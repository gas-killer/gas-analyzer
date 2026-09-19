/* gk-guest-crt implementation. Freestanding: provides its own mem*
 * intrinsics and keccak256; speaks SP1's syscall ABI directly (code in t0,
 * args in a0/a1, result in t0).
 */
#include "gkvm.h"

/* --- SP1 syscall ABI ---------------------------------------------------- */

#define SP1_HALT 0x00000000u
#define SP1_WRITE 0x00000002u
#define SP1_HINT_LEN 0x000000F0u
#define SP1_HINT_READ 0x000000F1u
/* SP1 v6 offsets its special fds by LOWEST_ALLOWED_FD = 10 (see
 * sp1-primitives consts::fd): public values ride fd 3 + 10. */
#define SP1_FD_PUBLIC_VALUES 13UL

static inline u64 sp1_hint_len(void) {
    register u64 t0 __asm__("t0") = SP1_HINT_LEN;
    __asm__ volatile("ecall" : "+r"(t0) : : "memory");
    return t0;
}

static inline void sp1_hint_read(void *ptr, u64 len) {
    register u64 t0 __asm__("t0") = SP1_HINT_READ;
    register u64 a0 __asm__("a0") = (u64)ptr;
    register u64 a1 __asm__("a1") = len;
    __asm__ volatile("ecall" : "+r"(t0) : "r"(a0), "r"(a1) : "memory");
}

static inline void sp1_write(u64 fd, const void *buf, u64 nbytes) {
    /* WRITE reads the byte count from a2. */
    register u64 t0 __asm__("t0") = SP1_WRITE;
    register u64 a0 __asm__("a0") = fd;
    register u64 a1 __asm__("a1") = (u64)buf;
    register u64 a2 __asm__("a2") = nbytes;
    __asm__ volatile("ecall" : "+r"(t0) : "r"(a0), "r"(a1), "r"(a2) : "memory");
}

_Noreturn static inline void sp1_halt(u32 code) {
    register u64 t0 __asm__("t0") = SP1_HALT;
    register u64 a0 __asm__("a0") = code;
    __asm__ volatile("ecall" : : "r"(t0), "r"(a0) : "memory");
    __builtin_unreachable();
}

/* Read the next hint buffer of exactly `len` bytes into dst. dst must have
 * room for the word-granular spill: len rounded up to 8, plus 8 more when
 * len is already a multiple of 8 (SP1 always writes one final word). */
static void hint_read_exact(void *dst, u64 len) {
    sp1_hint_read(dst, len);
}

static u64 hint_spill_size(u64 len) {
    return ((len + 8) / 8) * 8 + 8;
}

/* --- freestanding intrinsics -------------------------------------------- */

void *memset(void *dst, int value, unsigned long n) {
    u8 *d = (u8 *)dst;
    for (unsigned long i = 0; i < n; i++) d[i] = (u8)value;
    return dst;
}

/* Word access to memory declared as bytes (hint buffers, pages, digests). */
typedef u64 u64_alias __attribute__((may_alias));

void *memcpy(void *dst, const void *src, unsigned long n) {
    u8 *d = (u8 *)dst;
    const u8 *s = (const u8 *)src;
    if ((((u64)d | (u64)s) & 7) == 0) {
        /* Both 8-aligned (every verified page copy is): move words. */
        u64_alias *dw = (u64_alias *)d;
        const u64_alias *sw = (const u64_alias *)s;
        for (; n >= 8; n -= 8) *dw++ = *sw++;
        d = (u8 *)dw;
        s = (const u8 *)sw;
    }
    for (unsigned long i = 0; i < n; i++) d[i] = s[i];
    return dst;
}

void *memmove(void *dst, const void *src, unsigned long n) {
    u8 *d = (u8 *)dst;
    const u8 *s = (const u8 *)src;
    if (d < s)
        for (unsigned long i = 0; i < n; i++) d[i] = s[i];
    else
        for (unsigned long i = n; i > 0; i--) d[i - 1] = s[i - 1];
    return dst;
}

int memcmp(const void *a, const void *b, unsigned long n) {
    const u8 *x = (const u8 *)a;
    const u8 *y = (const u8 *)b;
    for (unsigned long i = 0; i < n; i++)
        if (x[i] != y[i]) return x[i] < y[i] ? -1 : 1;
    return 0;
}

/* --- keccak256 (keccak-f[1600], rate 136) ------------------------------- */

static const u64 KECCAK_RC[24] = {
    0x0000000000000001UL, 0x0000000000008082UL, 0x800000000000808aUL,
    0x8000000080008000UL, 0x000000000000808bUL, 0x0000000080000001UL,
    0x8000000080008081UL, 0x8000000000008009UL, 0x000000000000008aUL,
    0x0000000000000088UL, 0x0000000080008009UL, 0x000000008000000aUL,
    0x000000008000808bUL, 0x800000000000008bUL, 0x8000000000008089UL,
    0x8000000000008003UL, 0x8000000000008002UL, 0x8000000000000080UL,
    0x000000000000800aUL, 0x800000008000000aUL, 0x8000000080008081UL,
    0x8000000000008080UL, 0x0000000080000001UL, 0x8000000080008008UL,
};

/* n is always a constant; the mask keeps n == 0 (lane ba) a defined shift. */
#define ROL(x, n) (((x) << (n)) | ((x) >> ((64 - (n)) & 63)))

/* One round, lanes A -> lanes E, theta/rho/pi/chi/iota fused per output row.
 * Lane names are <row><column>: rows b g k m s = y 0..4, columns a e i o u =
 * x 0..4, so st[x + 5y]. Every lane is a scalar and every rotation a
 * constant: artifact verification is paid per page per answer, in metered
 * cycles, so the permutation is written to be register-allocated. */
#define KECCAK_ROW(A, E, row, l0, r0, l1, r1, l2, r2, l3, r3, l4, r4)        \
    B0 = ROL(A##l0, r0), B1 = ROL(A##l1, r1), B2 = ROL(A##l2, r2);           \
    B3 = ROL(A##l3, r3), B4 = ROL(A##l4, r4);                                \
    E##row##a = B0 ^ (~B1 & B2), E##row##e = B1 ^ (~B2 & B3);                \
    E##row##i = B2 ^ (~B3 & B4), E##row##o = B3 ^ (~B4 & B0);                \
    E##row##u = B4 ^ (~B0 & B1)

#define KECCAK_ROUND(A, E, rc)                                               \
    do {                                                                     \
        u64 B0, B1, B2, B3, B4;                                              \
        u64 Ca = A##ba ^ A##ga ^ A##ka ^ A##ma ^ A##sa;                      \
        u64 Ce = A##be ^ A##ge ^ A##ke ^ A##me ^ A##se;                      \
        u64 Ci = A##bi ^ A##gi ^ A##ki ^ A##mi ^ A##si;                      \
        u64 Co = A##bo ^ A##go ^ A##ko ^ A##mo ^ A##so;                      \
        u64 Cu = A##bu ^ A##gu ^ A##ku ^ A##mu ^ A##su;                      \
        u64 Da = Cu ^ ROL(Ce, 1), De = Ca ^ ROL(Ci, 1), Di = Ce ^ ROL(Co, 1); \
        u64 Do = Ci ^ ROL(Cu, 1), Du = Co ^ ROL(Ca, 1);                      \
        A##ba ^= Da, A##ga ^= Da, A##ka ^= Da, A##ma ^= Da, A##sa ^= Da;     \
        A##be ^= De, A##ge ^= De, A##ke ^= De, A##me ^= De, A##se ^= De;     \
        A##bi ^= Di, A##gi ^= Di, A##ki ^= Di, A##mi ^= Di, A##si ^= Di;     \
        A##bo ^= Do, A##go ^= Do, A##ko ^= Do, A##mo ^= Do, A##so ^= Do;     \
        A##bu ^= Du, A##gu ^= Du, A##ku ^= Du, A##mu ^= Du, A##su ^= Du;     \
        KECCAK_ROW(A, E, b, ba, 0, ge, 44, ki, 43, mo, 21, su, 14);          \
        E##ba ^= (rc);                                                       \
        KECCAK_ROW(A, E, g, bo, 28, gu, 20, ka, 3, me, 45, si, 61);          \
        KECCAK_ROW(A, E, k, be, 1, gi, 6, ko, 25, mu, 8, sa, 18);            \
        KECCAK_ROW(A, E, m, bu, 27, ga, 36, ke, 10, mi, 15, so, 56);         \
        KECCAK_ROW(A, E, s, bi, 62, go, 55, ku, 39, ma, 41, se, 2);          \
    } while (0)

/* no-schedule-insns: gcc 13's rv64 insn scheduler interleaves the rows and
 * spills ~160 lanes per round; without it the round is ~250 instructions
 * against an rv64im floor of 213 (no rol/andn). Carried as an attribute so
 * every build of the crt gets it, whatever its command line. */
__attribute__((optimize("no-schedule-insns")))
static void keccakf(u64 st[25]) {
    u64 Aba = st[0], Abe = st[1], Abi = st[2], Abo = st[3], Abu = st[4];
    u64 Aga = st[5], Age = st[6], Agi = st[7], Ago = st[8], Agu = st[9];
    u64 Aka = st[10], Ake = st[11], Aki = st[12], Ako = st[13], Aku = st[14];
    u64 Ama = st[15], Ame = st[16], Ami = st[17], Amo = st[18], Amu = st[19];
    u64 Asa = st[20], Ase = st[21], Asi = st[22], Aso = st[23], Asu = st[24];
    u64 Eba, Ebe, Ebi, Ebo, Ebu, Ega, Ege, Egi, Ego, Egu;
    u64 Eka, Eke, Eki, Eko, Eku, Ema, Eme, Emi, Emo, Emu;
    u64 Esa, Ese, Esi, Eso, Esu;
    for (int round = 0; round < 24; round += 2) {
        KECCAK_ROUND(A, E, KECCAK_RC[round]);
        KECCAK_ROUND(E, A, KECCAK_RC[round + 1]);
    }
    st[0] = Aba, st[1] = Abe, st[2] = Abi, st[3] = Abo, st[4] = Abu;
    st[5] = Aga, st[6] = Age, st[7] = Agi, st[8] = Ago, st[9] = Agu;
    st[10] = Aka, st[11] = Ake, st[12] = Aki, st[13] = Ako, st[14] = Aku;
    st[15] = Ama, st[16] = Ame, st[17] = Ami, st[18] = Amo, st[19] = Amu;
    st[20] = Asa, st[21] = Ase, st[22] = Asi, st[23] = Aso, st[24] = Asu;
}

/* The generic streaming hash. The block buffer is kept as lanes (rv64 is
 * little-endian, so its bytes are keccak's byte order), and whole blocks of
 * 8-aligned data are absorbed in place without passing through it. */
typedef struct {
    u64 st[25];
    u64 buf[17];
    u64 fill;
} keccak_ctx;

static void keccak_init(keccak_ctx *ctx) {
    memset(ctx, 0, sizeof *ctx);
}

static void keccak_update(keccak_ctx *ctx, const u8 *data, u64 len) {
    if (ctx->fill == 0 && ((u64)data & 7) == 0) {
        const u64_alias *lanes = (const u64_alias *)data;
        for (; len >= 136; len -= 136, lanes += 17) {
            for (int i = 0; i < 17; i++) ctx->st[i] ^= lanes[i];
            keccakf(ctx->st);
        }
        data = (const u8 *)lanes;
    }
    while (len > 0) {
        u64 take = 136 - ctx->fill;
        if (take > len) take = len;
        memcpy((u8 *)ctx->buf + ctx->fill, data, take);
        ctx->fill += take;
        data += take;
        len -= take;
        if (ctx->fill == 136) {
            for (int i = 0; i < 17; i++) ctx->st[i] ^= ctx->buf[i];
            keccakf(ctx->st);
            ctx->fill = 0;
        }
    }
}

static void keccak_final(keccak_ctx *ctx, u8 out[32]) {
    u8 *block = (u8 *)ctx->buf;
    memset(block + ctx->fill, 0, 136 - ctx->fill);
    block[ctx->fill] ^= 0x01;
    block[135] ^= 0x80;
    for (int i = 0; i < 17; i++) ctx->st[i] ^= ctx->buf[i];
    keccakf(ctx->st);
    memcpy(out, ctx->st, 32);
}

void gk_keccak256(const u8 *data, u64 len, u8 out[32]) {
    keccak_ctx ctx;
    keccak_init(&ctx);
    keccak_update(&ctx, data, len);
    keccak_final(&ctx, out);
}

/* out = keccak256(prefix || lanes[0..count)) — the shape of both Merkle
 * hashes (leaf = 0x00 || page, node = 0x01 || left || right). The one-byte
 * prefix shifts every lane by a byte; that shift is done on words (`carry`
 * is the byte pushed into the next lane) rather than by re-buffering the
 * message. 1 + 8·count bytes always leaves exactly the carry byte for the
 * final block, so the padding lands in the lane being filled. */
static void keccak_prefixed_lanes(u8 prefix, const u64_alias *lanes, u64 count, u64 out[4]) {
    u64 st[25];
    for (int i = 0; i < 25; i++) st[i] = 0;
    u64 carry = prefix;
    u64 lane = 0;
    for (u64 k = 0; k < count; k++) {
        u64 word = lanes[k];
        st[lane] ^= carry | (word << 8);
        carry = word >> 56;
        if (++lane == 17) {
            keccakf(st);
            lane = 0;
        }
    }
    st[lane] ^= carry | (0x01UL << 8);
    st[16] ^= 0x80UL << 56;
    keccakf(st);
    out[0] = st[0], out[1] = st[1], out[2] = st[2], out[3] = st[3];
}

/* --- hostcalls ----------------------------------------------------------- */

/* GKVM ABI v1 input framing: [artifactRoot][payload][manifest][pages…]. */

static u8 gk_root[40] __attribute__((aligned(8))); /* 32 + hint spill */
static int gk_payload_consumed;

const u8 *gk_artifact_root(void) { return gk_root; }

_Noreturn void gk_abort(u32 code, const char *msg, u32 msg_len) {
    /* Frame parsed from the stream tail by the host:
     * msg || code (u32 BE) || msg_len (u32 BE) || "GKTRAP01". */
    static const u8 magic[8] = {'G', 'K', 'T', 'R', 'A', 'P', '0', '1'};
    u8 word[4];
    if (msg_len > 0) sp1_write(SP1_FD_PUBLIC_VALUES, msg, msg_len);
    word[0] = (u8)(code >> 24), word[1] = (u8)(code >> 16);
    word[2] = (u8)(code >> 8), word[3] = (u8)code;
    sp1_write(SP1_FD_PUBLIC_VALUES, word, 4);
    word[0] = (u8)(msg_len >> 24), word[1] = (u8)(msg_len >> 16);
    word[2] = (u8)(msg_len >> 8), word[3] = (u8)msg_len;
    sp1_write(SP1_FD_PUBLIC_VALUES, word, 4);
    sp1_write(SP1_FD_PUBLIC_VALUES, magic, 8);
    sp1_halt(0xFA);
}

u64 gk_input_read(u8 *dst) {
    if (gk_payload_consumed) gk_abort(GK_TRAP_MANIFEST_INVALID, "payload re-read", 15);
    gk_payload_consumed = 1;
    u64 len = sp1_hint_len();
    if (len == (u64)-1) gk_abort(GK_TRAP_MANIFEST_INVALID, "no payload", 10);
    if (len > GK_INPUT_BYTES_CAP) gk_abort(GK_TRAP_INPUT_TOO_LARGE, "input over cap", 14);
    hint_read_exact(dst, len);
    return len;
}

void gk_output_write(const u8 *src, u64 len) {
    sp1_write(SP1_FD_PUBLIC_VALUES, src, len);
}

/* --- artifact manifest --------------------------------------------------- */

#define GK_MAX_ARTIFACT_FILES 8
#define GK_MAX_BRANCH 40

static struct {
    int loaded;
    u32 file_count;
    u64 lens[GK_MAX_ARTIFACT_FILES];
    u8 roots[GK_MAX_ARTIFACT_FILES][32];
} gk_manifest;

static void gk_manifest_load(void) {
    if (gk_manifest.loaded) return;
    if (!gk_payload_consumed)
        gk_abort(GK_TRAP_MANIFEST_INVALID, "artifact before input", 21);

    /* blob = fileCount (u32 BE) || (len u64 BE || root 32)*; the artifact
     * root is keccak(DOMAIN || blob), so one hash authenticates the whole
     * manifest and every later page check descends from it. */
    static u8 blob[4 + GK_MAX_ARTIFACT_FILES * 40 + 16] __attribute__((aligned(8)));
    u64 blob_len = sp1_hint_len();
    if (blob_len == (u64)-1 || blob_len < 4 || hint_spill_size(blob_len) > sizeof blob)
        gk_abort(GK_TRAP_MANIFEST_INVALID, "manifest missing", 16);
    hint_read_exact(blob, blob_len);

    static const char domain[] = "gaskiller.artifact.v3";
    keccak_ctx ctx;
    u8 digest[32];
    keccak_init(&ctx);
    keccak_update(&ctx, (const u8 *)domain, sizeof domain - 1);
    keccak_update(&ctx, blob, blob_len);
    keccak_final(&ctx, digest);
    if (memcmp(digest, gk_root, 32) != 0)
        gk_abort(GK_TRAP_MANIFEST_INVALID, "manifest root mismatch", 22);

    u32 count = ((u32)blob[0] << 24) | ((u32)blob[1] << 16) | ((u32)blob[2] << 8) | blob[3];
    if (count > GK_MAX_ARTIFACT_FILES || blob_len != 4 + (u64)count * 40)
        gk_abort(GK_TRAP_MANIFEST_INVALID, "manifest shape", 14);
    gk_manifest.file_count = count;
    for (u32 i = 0; i < count; i++) {
        const u8 *entry = blob + 4 + i * 40;
        u64 len = 0;
        for (int b = 0; b < 8; b++) len = (len << 8) | entry[b];
        gk_manifest.lens[i] = len;
        memcpy(gk_manifest.roots[i], entry + 8, 32);
    }
    gk_manifest.loaded = 1;
}

u64 gk_artifact_len(u32 kind) {
    gk_manifest_load();
    if (kind >= gk_manifest.file_count)
        gk_abort(GK_TRAP_ARTIFACT_RANGE, "unknown artifact kind", 21);
    return gk_manifest.lens[kind];
}

void gk_artifact_read(u32 kind, u64 page_idx, u8 *dst) {
    gk_manifest_load();
    if (kind >= gk_manifest.file_count)
        gk_abort(GK_TRAP_ARTIFACT_RANGE, "unknown artifact kind", 21);
    u64 page_count = (gk_manifest.lens[kind] + GK_ARTIFACT_PAGE_SIZE - 1) / GK_ARTIFACT_PAGE_SIZE;
    if (page_idx >= page_count)
        gk_abort(GK_TRAP_ARTIFACT_RANGE, "page out of range", 17);

    /* One hint buffer per scheduled page: page || branch (32 × k). */
    static u8 buf[GK_ARTIFACT_PAGE_SIZE + GK_MAX_BRANCH * 32 + 16] __attribute__((aligned(8)));
    u64 len = sp1_hint_len();
    if (len == (u64)-1 || len < GK_ARTIFACT_PAGE_SIZE ||
        (len - GK_ARTIFACT_PAGE_SIZE) % 32 != 0 || hint_spill_size(len) > sizeof buf)
        gk_abort(GK_TRAP_ARTIFACT_VERIFY, "page buffer shape", 17);
    hint_read_exact(buf, len);
    u64 branch_len = (len - GK_ARTIFACT_PAGE_SIZE) / 32;

    /* Fold leaf → root through the promotion-aware widths (a level with odd
     * width promotes its last node with no sibling). */
    /* buf is 8-aligned and the branch starts on a lane boundary, so page,
     * siblings and running node are all hashed as lanes. */
    u64 node[4];
    keccak_prefixed_lanes(0x00, (const u64_alias *)buf, GK_ARTIFACT_PAGE_SIZE / 8, node);
    const u64_alias *branch = (const u64_alias *)(buf + GK_ARTIFACT_PAGE_SIZE);
    u64 idx = page_idx;
    u64 width = page_count;
    u64 consumed = 0;
    while (width > 1) {
        if (!(idx == width - 1 && width % 2 == 1)) {
            if (consumed >= branch_len)
                gk_abort(GK_TRAP_ARTIFACT_VERIFY, "branch too short", 16);
            const u64_alias *sibling = branch + consumed * 4;
            consumed++;
            u64 pair[8];
            u64 *mine = idx % 2 == 0 ? pair : pair + 4;
            u64 *theirs = idx % 2 == 0 ? pair + 4 : pair;
            for (int i = 0; i < 4; i++) mine[i] = node[i], theirs[i] = sibling[i];
            keccak_prefixed_lanes(0x01, pair, 8, node);
        }
        idx /= 2;
        width = (width + 1) / 2;
    }
    if (consumed != branch_len || memcmp(node, gk_manifest.roots[kind], 32) != 0)
        gk_abort(GK_TRAP_ARTIFACT_VERIFY, "page verify failed", 18);

    memcpy(dst, buf, GK_ARTIFACT_PAGE_SIZE);
}

/* --- entry --------------------------------------------------------------- */

extern int main(void);

void __gk_start(void) {
    /* Buffer 0 of the input framing: the 32-byte artifact root. */
    u64 len = sp1_hint_len();
    if (len != 32) gk_abort(GK_TRAP_MANIFEST_INVALID, "no artifact root", 16);
    hint_read_exact(gk_root, 32);
    sp1_halt((u32)(main() & 0xff));
}

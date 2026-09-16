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

void *memcpy(void *dst, const void *src, unsigned long n) {
    u8 *d = (u8 *)dst;
    const u8 *s = (const u8 *)src;
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

static inline u64 rotl64(u64 x, unsigned n) { return (x << n) | (x >> (64 - n)); }

static void keccakf(u64 st[25]) {
    static const u8 rho[24] = {1,  3,  6,  10, 15, 21, 28, 36, 45, 55, 2,  14,
                               27, 41, 56, 8,  25, 43, 62, 18, 39, 61, 20, 44};
    static const u8 pi[24] = {10, 7,  11, 17, 18, 3, 5,  16, 8,  21, 24, 4,
                              15, 23, 19, 13, 12, 2, 20, 14, 22, 9,  6,  1};
    for (int round = 0; round < 24; round++) {
        u64 bc[5];
        for (int i = 0; i < 5; i++)
            bc[i] = st[i] ^ st[i + 5] ^ st[i + 10] ^ st[i + 15] ^ st[i + 20];
        for (int i = 0; i < 5; i++) {
            u64 t = bc[(i + 4) % 5] ^ rotl64(bc[(i + 1) % 5], 1);
            for (int j = 0; j < 25; j += 5) st[j + i] ^= t;
        }
        u64 t = st[1];
        for (int i = 0; i < 24; i++) {
            u64 next = st[pi[i]];
            st[pi[i]] = rotl64(t, rho[i]);
            t = next;
        }
        for (int j = 0; j < 25; j += 5) {
            for (int i = 0; i < 5; i++) bc[i] = st[j + i];
            for (int i = 0; i < 5; i++)
                st[j + i] ^= (~bc[(i + 1) % 5]) & bc[(i + 2) % 5];
        }
        st[0] ^= KECCAK_RC[round];
    }
}

typedef struct {
    u64 st[25];
    u8 buf[136];
    u64 fill;
} keccak_ctx;

static void keccak_init(keccak_ctx *ctx) {
    memset(ctx, 0, sizeof *ctx);
}

static void keccak_update(keccak_ctx *ctx, const u8 *data, u64 len) {
    while (len > 0) {
        u64 take = 136 - ctx->fill;
        if (take > len) take = len;
        memcpy(ctx->buf + ctx->fill, data, take);
        ctx->fill += take;
        data += take;
        len -= take;
        if (ctx->fill == 136) {
            for (int i = 0; i < 17; i++) {
                u64 word = 0;
                for (int b = 0; b < 8; b++) word |= (u64)ctx->buf[i * 8 + b] << (8 * b);
                ctx->st[i] ^= word;
            }
            keccakf(ctx->st);
            ctx->fill = 0;
        }
    }
}

static void keccak_final(keccak_ctx *ctx, u8 out[32]) {
    memset(ctx->buf + ctx->fill, 0, 136 - ctx->fill);
    ctx->buf[ctx->fill] ^= 0x01;
    ctx->buf[135] ^= 0x80;
    for (int i = 0; i < 17; i++) {
        u64 word = 0;
        for (int b = 0; b < 8; b++) word |= (u64)ctx->buf[i * 8 + b] << (8 * b);
        ctx->st[i] ^= word;
    }
    keccakf(ctx->st);
    for (int i = 0; i < 4; i++)
        for (int b = 0; b < 8; b++) out[i * 8 + b] = (u8)(ctx->st[i] >> (8 * b));
}

void gk_keccak256(const u8 *data, u64 len, u8 out[32]) {
    keccak_ctx ctx;
    keccak_init(&ctx);
    keccak_update(&ctx, data, len);
    keccak_final(&ctx, out);
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
    u8 node[32];
    {
        keccak_ctx ctx;
        static const u8 leaf_prefix = 0x00;
        keccak_init(&ctx);
        keccak_update(&ctx, &leaf_prefix, 1);
        keccak_update(&ctx, buf, GK_ARTIFACT_PAGE_SIZE);
        keccak_final(&ctx, node);
    }
    u64 idx = page_idx;
    u64 width = page_count;
    u64 consumed = 0;
    while (width > 1) {
        if (!(idx == width - 1 && width % 2 == 1)) {
            if (consumed >= branch_len)
                gk_abort(GK_TRAP_ARTIFACT_VERIFY, "branch too short", 16);
            const u8 *sibling = buf + GK_ARTIFACT_PAGE_SIZE + consumed * 32;
            consumed++;
            keccak_ctx ctx;
            static const u8 node_prefix = 0x01;
            keccak_init(&ctx);
            keccak_update(&ctx, &node_prefix, 1);
            if (idx % 2 == 0) {
                keccak_update(&ctx, node, 32);
                keccak_update(&ctx, sibling, 32);
            } else {
                keccak_update(&ctx, sibling, 32);
                keccak_update(&ctx, node, 32);
            }
            keccak_final(&ctx, node);
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

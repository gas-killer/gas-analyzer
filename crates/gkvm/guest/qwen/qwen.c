/* qwen: the UNBOUNDED_V3 flagship guest — Qwen3 greedy inference, integer
 * only, bit-exact with the engine-v2 spec that `Qwen3.sol` implements and
 * `tools/qwen3_int.py` defines (solidity-sdk, src/examples/onchain-llm):
 *   activations Q24 · int8 rows with a per-row power-of-two shift · int16
 *   norm gains with a per-tensor shift · KV cache int32 Q16 · exp via the
 *   Q32 bit-product kernel (LlamaMath.expQ32) · RoPE over Q30 tables ·
 *   greedy argmax, first maximum wins.
 *
 * Payload  = abi.encode(bytes32[3] packedConfig, uint32[] promptIds,
 *                       uint256 maxNewTokens)      (Qwen3Engine.chat's args)
 * Result   = abi.encode(string answer, uint32[] answerIds)   (its returns)
 * Artifact = kind 0: the engine-v2 weight blob, kind 1: the token table —
 *            the same bytes tools/qwen3_convert.py writes for V2.
 *
 * Weights are read ONCE, sequentially (kind 0 pages 0..n-1), into guest
 * memory before the first forward pass; the token table (kind 1 pages
 * 0..m-1) is read after generation, for the decode. That order is the
 * artifact schedule the host must serve.
 *
 * Where Solidity computes in int256 and this guest in int64/int128, every
 * narrowing is either proven by a range check or a deterministic trap
 * (QW_TRAP_NUMERIC_RANGE) — never a silent wrap.
 *
 * -DQWEN_EMIT_LOGITS builds the test twin: one forward pass of promptIds[0]
 * at position 0, result = abi.encode(int256[] logits) (vectors.json's
 * `logitsPos0`).
 */
#include "../crt/gkvm.h"

typedef signed char i8;
typedef long i64;
typedef __int128 i128;
typedef unsigned __int128 u128;

#define QW_TRAP_BAD_PAYLOAD 1u
#define QW_TRAP_BAD_CONFIG 2u
#define QW_TRAP_CONTEXT_OVERFLOW 3u
#define QW_TRAP_BAD_TOKEN 4u
#define QW_TRAP_ARTIFACT_LEN 5u
#define QW_TRAP_TOKEN_TABLE 6u
#define QW_TRAP_NUMERIC_RANGE 7u
#define QW_TRAP_OUT_OF_MEMORY 8u
#define QW_TRAP_OUTPUT_TOO_LARGE 9u

#define TRAP(code, msg) gk_abort((code), (msg), sizeof(msg) - 1)

#define I64_MAX 0x7fffffffffffffffL
#define PAGE GK_ARTIFACT_PAGE_SIZE

/* --- heap ------------------------------------------------------------------
 * Bump allocation above the image. Guest memory is zero-initialized, so
 * nothing is cleared. The ceiling keeps image + heap inside the pinned
 * 2^31-byte guest memory cap with room left for the stack, and makes running
 * out a guest-side trap rather than a host-side one. */

extern u8 _end[];
#define QW_HEAP_CEILING (0x78000000UL + 0x70000000UL)

static u64 heap_top;

static void *alloc(u64 bytes, u64 align) {
    if (heap_top == 0) heap_top = (u64)_end;
    u64 at = (heap_top + align - 1) & ~(align - 1);
    if (bytes > QW_HEAP_CEILING || at > QW_HEAP_CEILING - bytes)
        TRAP(QW_TRAP_OUT_OF_MEMORY, "model + cache exceed guest memory");
    heap_top = at + bytes;
    return (void *)at;
}

/* --- 128-bit helpers ---------------------------------------------------------
 * Only what gcc expands inline on rv64im is used on __int128 (add, sub,
 * compare, constant shifts, widening 64x64 multiplies); division and variable
 * shifts are spelled out here so nothing reaches for libgcc. */

static int clz64(u64 v) {
    int n = 0;
    while (!(v >> 63)) {
        v <<= 1;
        n++;
    }
    return n;
}

/* (u1:u0) / v for u1 < v — Hacker's Delight divlu, base 2^32 digits. */
static u64 divlu(u64 u1, u64 u0, u64 v) {
    const u64 b = 1UL << 32;
    int s = clz64(v);
    v <<= s;
    u64 vn1 = v >> 32, vn0 = v & 0xffffffffUL;
    u64 un32 = s ? (u1 << s) | (u0 >> (64 - s)) : u1;
    u64 un10 = u0 << s;
    u64 un1 = un10 >> 32, un0 = un10 & 0xffffffffUL;

    u64 q1 = un32 / vn1, rhat = un32 - q1 * vn1;
    while (q1 >= b || q1 * vn0 > b * rhat + un1) {
        q1--;
        rhat += vn1;
        if (rhat >= b) break;
    }
    u64 un21 = un32 * b + un1 - q1 * v;
    u64 q0 = un21 / vn1;
    rhat = un21 - q0 * vn1;
    while (q0 >= b || q0 * vn0 > b * rhat + un0) {
        q0--;
        rhat += vn1;
        if (rhat >= b) break;
    }
    return q1 * b + q0;
}

/* floor(n / d), d != 0 — EVM DIV on values that fit 128 bits. */
static u128 udiv128(u128 n, u64 d) {
    u64 hi = (u64)(n >> 64), lo = (u64)n;
    u64 qhi = hi / d;
    u64 qlo = divlu(hi - qhi * d, lo, d);
    return ((u128)qhi << 64) | qlo;
}

/* EVM SDIV (truncation toward zero), positive divisor. */
static i128 sdiv128(i128 n, u64 d) {
    if (n < 0) return -(i128)udiv128((u128)(-n), d);
    return (i128)udiv128((u128)n, d);
}

/* EVM SAR by a run-time amount. */
static i128 sar128(i128 v, u32 n) {
    if (n == 0) return v;
    if (n > 127) n = 127;
    i64 hi = (i64)(v >> 64);
    u64 lo = (u64)v;
    if (n >= 64) return (i128)(hi >> (n - 64));
    return ((i128)(hi >> n) << 64) | (u128)((lo >> n) | ((u64)hi << (64 - n)));
}

static i64 narrow(i128 v) {
    if (v > (i128)I64_MAX || v < -(i128)I64_MAX)
        TRAP(QW_TRAP_NUMERIC_RANGE, "activation leaves int64");
    return (i64)v;
}

/* floor(sqrt(x)) — LlamaMath.isqrt. */
static u64 isqrt128(u128 x) {
    u128 r = 0, bit = (u128)1 << 126;
    while (bit > x) bit >>= 2;
    while (bit != 0) {
        if (x >= r + bit) {
            x -= r + bit;
            r = (r >> 1) + bit;
        } else {
            r >>= 1;
        }
        bit >>= 2;
    }
    return (u64)r;
}

/* --- exp (LlamaMath.expQ32) -------------------------------------------------
 * EXP2_C[i] = 2^(2^-(i+1)) in Q64 is a 65-bit number 2^64 + c_i; the table
 * holds c_i. With acc = 2^64 + a:  (acc * C) >> 64 = 2^64 + a + c + mulhu(a,c)
 * exactly (the other partial products are multiples of 2^64), and the sum
 * stays below 2^65 because the true product is below 2. */

#define LOG2E_Q32 6196328018L

static const u64 EXP2_C_LOW[32] = {
    0x6A09E667F3BCC908UL, 0x306FE0A31B7152DEUL, 0x172B83C7D517ADCDUL, 0x0B5586CF9890F629UL,
    0x059B0D31585743AEUL, 0x02C9A3E778060EE6UL, 0x0163DA9FB33356D7UL, 0x00B1AFA5ABCBED60UL,
    0x0058C86DA1C09EA1UL, 0x002C605E2E8CEC4FUL, 0x00162F3904051FA0UL, 0x000B175EFFDC76B9UL,
    0x00058BA01FB9F96CUL, 0x0002C5CC37DA9491UL, 0x000162E525EE0546UL, 0x0000B17255775C03UL,
    0x000058B91B5BC9ADUL, 0x00002C5C89D5EC6CUL, 0x0000162E43F4F830UL, 0x00000B1721BCFC99UL,
    0x0000058B90CF1E6DUL, 0x000002C5C863B73EUL, 0x00000162E430E5A1UL, 0x000000B172183551UL,
    0x00000058B90C0B48UL, 0x0000002C5C8601CCUL, 0x000000162E42FFF0UL, 0x0000000B17217FBAUL,
    0x000000058B90BFCDUL, 0x00000002C5C85FE2UL, 0x0000000162E42FF0UL, 0x00000000B17217F7UL,
};

/* expQ32(z << 8) for a Q24 z <= 0; result Q32 in [0, 2^32]. */
static u64 exp_q32_of_q24(i64 z) {
    /* y = sar((z << 8) * LOG2E, 32) <= -(64 << 32) returns 0. LOG2E > 2^32,
     * so every z <= -2^30 lands there; past this line |z << 8| < 2^38. */
    if (z <= -(1L << 30)) return 0;
    i128 y = ((i128)(z * 256) * LOG2E_Q32) >> 32;
    if (y <= -((i128)64 << 32)) return 0;
    i64 y64 = (i64)y;
    i64 n = y64 >> 32; /* floor(y), in [-64, 0] */
    u64 f = (u64)(y64 - (n << 32));
    u64 a = 0;
    for (int i = 0; i < 32; i++) {
        if ((f >> (31 - i)) & 1) {
            u64 c = EXP2_C_LOW[i];
            a = a + c + (u64)(((u128)a * c) >> 64);
        }
    }
    u32 shift = (u32)(32 - n); /* in [32, 96] */
    if (shift > 64) return 0;
    if (shift == 64) return 1;
    return (a >> shift) + (1UL << (64 - shift));
}

/* --- model ------------------------------------------------------------------ */

static struct {
    u64 dim, hidden, n_layers, n_heads, n_kv, head_dim, vocab, seq_cap, tok_type, w_bits;
    u64 eps_q48, inv_sqrt_hd, weight_len, tok_len, stop0, stop1;
    u64 kvd, qd;
} cfg;

static struct {
    u64 emb_stride, layer_base, layer_len;
    u64 qn, kn, wq, wk, wv, wo, ln2, wg, wu, wd; /* relative to a layer's base */
    u64 norm, rope_cos, rope_sin;
} lay;

static const u8 *W; /* the weight blob, whole, in guest memory */

static struct {
    i64 *x, *xb, *q, *k, *v, *xatt, *g, *u, *scores;
    int *k_cache, *v_cache; /* Q16, [layer][pos][kvd] */
    u64 max_pos;
} buf;

static u64 be(const u8 *p, int n) {
    u64 v = 0;
    for (int i = 0; i < n; i++) v = (v << 8) | p[i];
    return v;
}

static void unpack_config(const u8 *w) {
    const u8 *w0 = w, *w1 = w + 32, *w2 = w + 64;
    cfg.dim = be(w0, 2);
    cfg.hidden = be(w0 + 2, 2);
    cfg.n_layers = w0[4];
    cfg.n_heads = w0[5];
    cfg.n_kv = w0[6];
    cfg.head_dim = be(w0 + 7, 2);
    cfg.vocab = be(w0 + 9, 4);
    cfg.seq_cap = be(w0 + 13, 2);
    cfg.tok_type = w0[15];
    cfg.w_bits = w0[16];
    cfg.eps_q48 = be(w1, 8);
    cfg.inv_sqrt_hd = be(w1 + 8, 8);
    cfg.weight_len = be(w1 + 16, 8);
    cfg.tok_len = be(w2, 4);
    cfg.stop0 = be(w2 + 4, 4);
    cfg.stop1 = be(w2 + 8, 4);
    cfg.kvd = cfg.n_kv * cfg.head_dim;
    cfg.qd = cfg.n_heads * cfg.head_dim;
    if (cfg.dim == 0 || cfg.hidden == 0 || cfg.n_layers == 0 || cfg.n_heads == 0 ||
        cfg.n_kv == 0 || cfg.head_dim == 0 || cfg.vocab == 0 || cfg.seq_cap == 0 ||
        cfg.n_heads % cfg.n_kv != 0 || cfg.head_dim % 2 != 0 || cfg.kvd % 8 != 0 ||
        (cfg.w_bits != 1 && cfg.w_bits != 2) || cfg.tok_type != 1 || cfg.eps_q48 == 0)
        TRAP(QW_TRAP_BAD_CONFIG, "bad config");
    /* Narrower than Qwen3.sol on purpose: the flagship blob is int8 rows, and
     * invSqrtHd (about 2^32 / sqrt(headDim)) takes part in a signed 64x64
     * multiply here. */
    if (cfg.w_bits != 1) TRAP(QW_TRAP_BAD_CONFIG, "int8 rows only");
    if (cfg.inv_sqrt_hd > (u64)I64_MAX) TRAP(QW_TRAP_BAD_CONFIG, "invSqrtHd range");
}

static void compute_layout(void) {
    u64 row = 1 + cfg.dim; /* [u8 shift][int8 x dim] */
    u64 at = 0;
    lay.emb_stride = row;
    lay.layer_base = cfg.vocab * row;
    at += 1 + cfg.dim * 2; /* ln1 at relative 0 */
    lay.qn = at, at += 1 + cfg.head_dim * 2;
    lay.kn = at, at += 1 + cfg.head_dim * 2;
    lay.wq = at, at += cfg.qd * row;
    lay.wk = at, at += cfg.kvd * row;
    lay.wv = at, at += cfg.kvd * row;
    lay.wo = at, at += cfg.dim * (1 + cfg.qd);
    lay.ln2 = at, at += 1 + cfg.dim * 2;
    lay.wg = at, at += cfg.hidden * row;
    lay.wu = at, at += cfg.hidden * row;
    lay.wd = at, at += cfg.dim * (1 + cfg.hidden);
    lay.layer_len = at;
    lay.norm = lay.layer_base + cfg.n_layers * lay.layer_len;
    lay.rope_cos = lay.norm + 1 + cfg.dim * 2;
    u64 rope_len = cfg.seq_cap * (cfg.head_dim / 2) * 4;
    lay.rope_sin = lay.rope_cos + rope_len;
    if (lay.rope_sin + rope_len != cfg.weight_len) TRAP(QW_TRAP_BAD_CONFIG, "bad config");
}

static const u8 *load_artifact(u32 kind, u64 len) {
    u64 pages = (len + PAGE - 1) / PAGE;
    u8 *dst = alloc(pages * PAGE, PAGE);
    for (u64 p = 0; p < pages; p++) gk_artifact_read(kind, p, dst + p * PAGE);
    return dst;
}

/* --- kernels ------------------------------------------------------------------ */

/* out_i = sar(sum_j w[i][j] * x_j, shift_i). The accumulator is int64: one
 * pass over x proves 128 * cols * max|x| < 2^63 first, or traps. */
static void matmul(const u8 *rows_at, u64 rows, u64 cols, const i64 *x, i64 *out) {
    i64 limit = I64_MAX / (i64)(128 * cols);
    for (u64 j = 0; j < cols; j++)
        if (x[j] > limit || x[j] < -limit)
            TRAP(QW_TRAP_NUMERIC_RANGE, "matmul input leaves the int64-safe range");

    for (u64 i = 0; i < rows; i++) {
        const u8 *row = rows_at + i * (cols + 1);
        u32 shift = row[0];
        const i8 *w = (const i8 *)(row + 1);
        const i64 *xp = x;
        i64 acc = 0;
        u64 j = cols;
        for (; j >= 8; j -= 8, w += 8, xp += 8) {
            acc += (i64)w[0] * xp[0] + (i64)w[1] * xp[1] + (i64)w[2] * xp[2] + (i64)w[3] * xp[3] +
                   (i64)w[4] * xp[4] + (i64)w[5] * xp[5] + (i64)w[6] * xp[6] + (i64)w[7] * xp[7];
        }
        for (; j > 0; j--, w++, xp++) acc += (i64)w[0] * xp[0];
        out[i] = acc >> (shift > 63 ? 63 : shift);
    }
}

/* out_i = sar(sdiv((x_i * g_i) << 24, isqrt(sum(x^2) / n + eps)), gShift);
 * g = [u8 shift][int16 BE x n]. out may alias x. */
static void rmsnorm(const u8 *g, const i64 *x, i64 *out, u64 n) {
    u128 ss = 0;
    for (u64 i = 0; i < n; i++) {
        i64 v = x[i];
        /* |v| < 2^47 keeps (v * g16) << 24 inside int128 and the sum of
         * squares far from 2^128 for any n the config can express. */
        if (v > (1L << 47) || v < -(1L << 47))
            TRAP(QW_TRAP_NUMERIC_RANGE, "rmsnorm input leaves the int128-safe range");
        ss += (u128)((i128)v * v);
    }
    u64 s = isqrt128(udiv128(ss, n) + cfg.eps_q48); /* Q24, > 0: eps > 0 */
    u32 gshift = g[0];
    for (u64 i = 0; i < n; i++) {
        i64 g16 = (short)((g[1 + 2 * i] << 8) | g[2 + 2 * i]);
        i128 num = ((i128)x[i] * g16) << 24;
        out[i] = narrow(sar128(sdiv128(num, s), gshift));
    }
}

static int rd_i32(const u8 *p) { return (int)be(p, 4); }

/* Interleaved-pair rotation by this position's Q30 angle rows. */
static void rope(i64 *arr, u64 heads, u64 pos) {
    u64 half = cfg.head_dim / 2;
    const u8 *cos_row = W + lay.rope_cos + pos * half * 4;
    const u8 *sin_row = W + lay.rope_sin + pos * half * 4;
    for (u64 p = 0; p < half; p++) {
        i64 c = rd_i32(cos_row + 4 * p), s = rd_i32(sin_row + 4 * p);
        for (u64 h = 0; h < heads; h++) {
            i64 *pair = arr + h * cfg.head_dim + 2 * p;
            i64 v0 = pair[0], v1 = pair[1];
            pair[0] = narrow(((i128)v0 * c - (i128)v1 * s) >> 30);
            pair[1] = narrow(((i128)v0 * s + (i128)v1 * c) >> 30);
        }
    }
}

/* Qwen3.cacheStore keeps the low 32 bits of sar(x, 8); a value outside int32
 * would wrap there, so here it traps. */
static int to_q16(i64 v) {
    i64 q = v >> 8;
    if (q > 0x7fffffffL || q < -0x80000000L)
        TRAP(QW_TRAP_NUMERIC_RANGE, "kv value leaves int32 Q16");
    return (int)q;
}

static void attend_head(u64 layer, u64 q_off, u64 kv_off, u64 steps) {
    const i64 *q = buf.q + q_off;
    const int *k_rows = buf.k_cache + layer * buf.max_pos * cfg.kvd + kv_off;
    const int *v_rows = buf.v_cache + layer * buf.max_pos * cfg.kvd + kv_off;
    u64 hd = cfg.head_dim;
    i64 *scores = buf.scores;

    i64 mx = 0;
    for (u64 t = 0; t < steps; t++) {
        const int *k = k_rows + t * cfg.kvd;
        i128 acc = 0;
        for (u64 j = 0; j < hd; j++) acc += (i128)q[j] * k[j];
        i64 sc = narrow(((i128)narrow(acc >> 16) * (i64)cfg.inv_sqrt_hd) >> 32);
        scores[t] = sc;
        if (t == 0 || sc > mx) mx = sc;
    }
    u64 tot = 0;
    for (u64 t = 0; t < steps; t++) {
        /* expQ24(d) = expQ32(d << 8) >> 8 with d = score - max <= 0; a
         * spread too wide for int64 is far below where exp reaches 0. */
        i64 d;
        u64 e = __builtin_sub_overflow(scores[t], mx, &d) ? 0 : exp_q32_of_q24(d) >> 8;
        scores[t] = (i64)e;
        tot += e; /* <= steps * 2^24 */
    }
    /* tot >= 2^24: the maximum contributes exp(0). */
    i64 *out = buf.xatt + q_off;
    for (u64 j = 0; j < hd; j++) {
        i128 acc = 0;
        for (u64 t = 0; t < steps; t++) acc += (i128)scores[t] * v_rows[t * cfg.kvd + j];
        out[j] = narrow(sdiv128(acc << 8, tot));
    }
}

/* g[i] = silu(g[i]) * u[i], all Q24. */
static void swiglu(i64 *g, const i64 *u, u64 n) {
    const u128 one32 = (u128)1 << 32;
    for (u64 i = 0; i < n; i++) {
        i64 z = g[i];
        if (z == -I64_MAX - 1) TRAP(QW_TRAP_NUMERIC_RANGE, "activation leaves int64");
        u64 sig;
        if (z >= 0) {
            u64 e = exp_q32_of_q24(-z);
            sig = (u64)udiv128(one32 << 32, (u64)(one32 + e));
        } else {
            u64 e = exp_q32_of_q24(z);
            sig = (u64)udiv128((u128)e << 32, (u64)(one32 + e));
        }
        /* sig <= 2^32, so |sz| <= |z|. */
        i64 sz = (i64)(((i128)z * (i64)sig) >> 32);
        g[i] = narrow(((i128)sz * u[i]) >> 24);
    }
}

static void add_into(i64 *x, const i64 *y, u64 n) {
    for (u64 i = 0; i < n; i++) {
        i64 sum;
        if (__builtin_add_overflow(x[i], y[i], &sum))
            TRAP(QW_TRAP_NUMERIC_RANGE, "activation leaves int64");
        x[i] = sum;
    }
}

/* One transformer pass; leaves the final normed hidden state in buf.xb. */
static void forward(u64 token, u64 pos) {
    const u8 *emb = W + token * lay.emb_stride;
    if (emb[0] > 24) TRAP(QW_TRAP_NUMERIC_RANGE, "embedding row shift > 24");
    i64 one = 1L << (24 - emb[0]);
    for (u64 i = 0; i < cfg.dim; i++) buf.x[i] = (i8)emb[1 + i] * one;

    u64 kv_mul = cfg.n_heads / cfg.n_kv;
    for (u64 l = 0; l < cfg.n_layers; l++) {
        const u8 *lb = W + lay.layer_base + l * lay.layer_len;

        rmsnorm(lb, buf.x, buf.xb, cfg.dim);
        matmul(lb + lay.wq, cfg.qd, cfg.dim, buf.xb, buf.q);
        matmul(lb + lay.wk, cfg.kvd, cfg.dim, buf.xb, buf.k);
        matmul(lb + lay.wv, cfg.kvd, cfg.dim, buf.xb, buf.v);
        for (u64 h = 0; h < cfg.n_heads; h++)
            rmsnorm(lb + lay.qn, buf.q + h * cfg.head_dim, buf.q + h * cfg.head_dim, cfg.head_dim);
        for (u64 h = 0; h < cfg.n_kv; h++)
            rmsnorm(lb + lay.kn, buf.k + h * cfg.head_dim, buf.k + h * cfg.head_dim, cfg.head_dim);
        rope(buf.q, cfg.n_heads, pos);
        rope(buf.k, cfg.n_kv, pos);

        int *k_row = buf.k_cache + (l * buf.max_pos + pos) * cfg.kvd;
        int *v_row = buf.v_cache + (l * buf.max_pos + pos) * cfg.kvd;
        for (u64 j = 0; j < cfg.kvd; j++) {
            k_row[j] = to_q16(buf.k[j]);
            v_row[j] = to_q16(buf.v[j]);
        }
        for (u64 h = 0; h < cfg.n_heads; h++)
            attend_head(l, h * cfg.head_dim, (h / kv_mul) * cfg.head_dim, pos + 1);

        matmul(lb + lay.wo, cfg.dim, cfg.qd, buf.xatt, buf.xb);
        add_into(buf.x, buf.xb, cfg.dim);

        rmsnorm(lb + lay.ln2, buf.x, buf.xb, cfg.dim);
        matmul(lb + lay.wg, cfg.hidden, cfg.dim, buf.xb, buf.g);
        matmul(lb + lay.wu, cfg.hidden, cfg.dim, buf.xb, buf.u);
        swiglu(buf.g, buf.u, cfg.hidden);
        matmul(lb + lay.wd, cfg.dim, cfg.hidden, buf.g, buf.xb);
        add_into(buf.x, buf.xb, cfg.dim);
    }
    rmsnorm(W + lay.norm, buf.x, buf.xb, cfg.dim);
}

static void new_buffers(u64 max_pos) {
    buf.max_pos = max_pos;
    buf.x = alloc(cfg.dim * 8, 8);
    buf.xb = alloc(cfg.dim * 8, 8);
    buf.q = alloc(cfg.qd * 8, 8);
    buf.k = alloc(cfg.kvd * 8, 8);
    buf.v = alloc(cfg.kvd * 8, 8);
    buf.xatt = alloc(cfg.qd * 8, 8);
    buf.g = alloc(cfg.hidden * 8, 8);
    buf.u = alloc(cfg.hidden * 8, 8);
    buf.scores = alloc(max_pos * 8, 8);
    buf.k_cache = alloc(cfg.n_layers * max_pos * cfg.kvd * 4, 8);
    buf.v_cache = alloc(cfg.n_layers * max_pos * cfg.kvd * 4, 8);
}

/* Tied classifier, one vocab-sized slice at a time; first maximum wins. */
#define CLS_SLICE 512

#ifndef QWEN_EMIT_LOGITS
static u64 argmax_classifier(void) {
    static i64 slice[CLS_SLICE];
    u64 best = 0;
    i64 best_val = 0;
    for (u64 row = 0; row < cfg.vocab; row += CLS_SLICE) {
        u64 rows = cfg.vocab - row < CLS_SLICE ? cfg.vocab - row : CLS_SLICE;
        matmul(W + row * lay.emb_stride, rows, cfg.dim, buf.xb, slice);
        for (u64 i = 0; i < rows; i++) {
            if ((row == 0 && i == 0) || slice[i] > best_val) {
                best_val = slice[i];
                best = row + i;
            }
        }
    }
    return best;
}
#endif

/* --- ABI ---------------------------------------------------------------------- */

static u8 input[GK_INPUT_BYTES_CAP + 8] __attribute__((aligned(8)));
static u8 output[GK_OUTPUT_BYTES_CAP];
static u64 out_len;

/* A uint256 word that must fit `bits`; anything wider is a malformed payload
 * (Solidity's decoder reverts on dirty high bits). */
static u64 word_at(u64 len, u64 off, int bits) {
    if (off > len || len - off < 32) TRAP(QW_TRAP_BAD_PAYLOAD, "payload truncated");
    const u8 *p = input + off;
    for (int i = 0; i < 32 - bits / 8; i++)
        if (p[i] != 0) TRAP(QW_TRAP_BAD_PAYLOAD, "payload word out of range");
    return be(p + 32 - bits / 8, bits / 8);
}

static void out_word(u64 v) {
    if (out_len + 32 > GK_OUTPUT_BYTES_CAP) TRAP(QW_TRAP_OUTPUT_TOO_LARGE, "answer over output cap");
    for (int i = 0; i < 8; i++) output[out_len + 24 + i] = (u8)(v >> (56 - 8 * i));
    out_len += 32; /* the high 24 bytes stay zero: output[] is never reused */
}

int main(void) {
    u64 len = gk_input_read(input);

    /* head: cfg[0] cfg[1] cfg[2] offset(promptIds) maxNewTokens */
    if (len < 160) TRAP(QW_TRAP_BAD_PAYLOAD, "payload truncated");
    unpack_config(input);
    compute_layout();
    u64 ids_off = word_at(len, 96, 64);
    u64 p_len = word_at(len, ids_off, 64);
    if (p_len > (len - ids_off - 32) / 32) TRAP(QW_TRAP_BAD_PAYLOAD, "payload truncated");
    /* maxNewTokens is clamped to the context anyway; saturate a wide one. */
    u64 max_new = cfg.seq_cap;
    {
        int wide = 0;
        for (int i = 0; i < 24; i++) wide |= input[128 + i];
        if (!wide && be(input + 152, 8) < max_new) max_new = be(input + 152, 8);
    }

    if (p_len == 0 || p_len >= cfg.seq_cap) TRAP(QW_TRAP_CONTEXT_OVERFLOW, "context overflow");
    const u8 *ids_at = input + ids_off + 32;
    for (u64 i = 0; i < p_len; i++)
        if (word_at(len, ids_off + 32 + 32 * i, 32) >= cfg.vocab)
            TRAP(QW_TRAP_BAD_TOKEN, "prompt token out of vocabulary");
    u64 max_pos = p_len + max_new;
    if (max_pos > cfg.seq_cap) max_pos = cfg.seq_cap;

    /* Qwen3Engine.checkArtifacts: the mounted bundle is the one the config
     * describes — settled before a single page is read. */
    if (gk_artifact_len(0) != cfg.weight_len || gk_artifact_len(1) != cfg.tok_len)
        TRAP(QW_TRAP_ARTIFACT_LEN, "artifact length != config");

    W = load_artifact(0, cfg.weight_len);
    new_buffers(max_pos);

#ifdef QWEN_EMIT_LOGITS
    forward(be(ids_at + 28, 4), 0);
    out_word(32);
    out_word(cfg.vocab);
    static i64 slice[CLS_SLICE];
    for (u64 row = 0; row < cfg.vocab; row += CLS_SLICE) {
        u64 rows = cfg.vocab - row < CLS_SLICE ? cfg.vocab - row : CLS_SLICE;
        matmul(W + row * lay.emb_stride, rows, cfg.dim, buf.xb, slice);
        for (u64 i = 0; i < rows; i++) {
            if (slice[i] < 0) {
                if (out_len + 32 > GK_OUTPUT_BYTES_CAP)
                    TRAP(QW_TRAP_OUTPUT_TOO_LARGE, "answer over output cap");
                for (int b = 0; b < 24; b++) output[out_len + b] = 0xff;
            }
            out_word((u64)slice[i]);
        }
    }
#else
    /* Qwen3.generate: teacher-forced prefill, then greedy decode. */
    u32 *gen = alloc((max_pos - p_len) * 4, 8);
    u64 n_gen = 0;
    u64 token = be(ids_at + 28, 4);
    for (u64 pos = 0; pos + 1 < max_pos; pos++) {
        forward(token, pos);
        u64 next;
        if (pos + 1 < p_len) {
            next = be(ids_at + 32 * (pos + 1) + 28, 4);
        } else {
            next = argmax_classifier();
            gen[n_gen++] = (u32)next;
            if (next == cfg.stop0 || next == cfg.stop1) break;
        }
        token = next;
    }

    /* Qwen3Engine._decode: [u8 type=1][u32 vocab][u32 stringsLen]
     * [(vocab+1) x u32 offsets][strings]; stop tokens are omitted. */
    const u8 *table = load_artifact(1, cfg.tok_len);
    if (cfg.tok_len < 9 || table[0] != 1) TRAP(QW_TRAP_TOKEN_TABLE, "malformed token table");
    u64 t_vocab = be(table + 1, 4);
    u64 strings_base = 9 + (t_vocab + 1) * 4;
    if (strings_base > cfg.tok_len) TRAP(QW_TRAP_TOKEN_TABLE, "malformed token table");
    u64 text_len = 0;
    for (u64 i = 0; i < n_gen; i++) {
        u64 id = gen[i];
        if (id >= t_vocab) TRAP(QW_TRAP_TOKEN_TABLE, "malformed token table");
        if (id == cfg.stop0 || id == cfg.stop1) continue;
        u64 so = be(table + 9 + id * 4, 4), eo = be(table + 9 + (id + 1) * 4, 4);
        if (eo < so || strings_base + eo > cfg.tok_len)
            TRAP(QW_TRAP_TOKEN_TABLE, "malformed token table");
        text_len += eo - so;
    }
    u64 text_padded = (text_len + 31) / 32 * 32;
    if (96 + text_padded + 32 + 32 * n_gen > GK_OUTPUT_BYTES_CAP)
        TRAP(QW_TRAP_OUTPUT_TOO_LARGE, "answer over output cap");

    /* abi.encode(string answer, uint32[] answerIds) */
    out_word(64);
    out_word(96 + text_padded);
    out_word(text_len);
    for (u64 i = 0; i < n_gen; i++) {
        u64 id = gen[i];
        if (id == cfg.stop0 || id == cfg.stop1) continue;
        u64 so = be(table + 9 + id * 4, 4), eo = be(table + 9 + (id + 1) * 4, 4);
        for (u64 j = so; j < eo; j++) output[out_len++] = table[strings_base + j];
    }
    out_len = 96 + text_padded;
    out_word(n_gen);
    for (u64 i = 0; i < n_gen; i++) out_word(gen[i]);
#endif

    gk_output_write(output, out_len);
    return 0;
}

/* bench: the M1 throughput guest. Runs a pure-integer xorshift loop whose
 * iteration count comes from the payload (u64 BE), then answers with the
 * final state — a few instructions per iteration, no memory traffic, so
 * Mcycles/s from `gk-run`'s report measures the executor tiers rather than
 * the guest. Also the cycle-budget test subject: the count in, the cycles
 * out, and OutOfCycles exactly when the budget says so.
 */
#include "../crt/gkvm.h"

static u8 input[64] __attribute__((aligned(8)));

int main(void) {
    u64 len = gk_input_read(input);
    if (len != 8) gk_abort(1, "bench wants a u64 BE iteration count", 36);
    u64 iters = 0;
    for (int i = 0; i < 8; i++) iters = (iters << 8) | input[i];

    u64 state = 0x9E3779B97F4A7C15UL;
    for (u64 i = 0; i < iters; i++) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
    }

    u8 out[8];
    for (int i = 0; i < 8; i++) out[i] = (u8)(state >> (56 - 8 * i));
    gk_output_write(out, 8);
    return 0;
}

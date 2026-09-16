/* hello: the M1 smoke guest. Reads the payload, answers with a fixed tag
 * followed by the payload reversed — enough to prove input framing, output
 * framing and the halt path, and cheap enough to run thousands of times in
 * the determinism matrix. The Rust twin (guest/hello-rs) implements the
 * exact same behavior, so one expected output checks both toolchains.
 */
#include "../crt/gkvm.h"

static u8 input[GK_INPUT_BYTES_CAP + 8] __attribute__((aligned(8)));

int main(void) {
    u64 len = gk_input_read(input);
    static const u8 tag[] = "GKVM-HELLO-V1\n";
    gk_output_write(tag, sizeof tag - 1);
    for (u64 i = 0; i < len / 2; i++) {
        u8 tmp = input[i];
        input[i] = input[len - 1 - i];
        input[len - 1 - i] = tmp;
    }
    gk_output_write(input, len);
    return 0;
}

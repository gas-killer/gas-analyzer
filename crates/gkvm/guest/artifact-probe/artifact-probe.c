/* artifact-probe: exercises the artifact hostcalls end to end. The payload
 * is a list of (kind u32 BE, page u64 BE) requests; for each, the guest
 * reads the page through the verifying SDK path and folds it into a running
 * keccak, answering with lens(each requested kind, first occurrence order
 * deduped is overkill — just each request's len) — kept simple: it answers
 * keccak256 over (len_0 BE8 || page_0 || len_1 BE8 || page_1 || …), which
 * the host-side test recomputes from the mount. A verification failure
 * inside gk_artifact_read aborts, which the negative test asserts.
 */
#include "../crt/gkvm.h"

static u8 input[GK_INPUT_BYTES_CAP + 8] __attribute__((aligned(8)));
static u8 page[GK_ARTIFACT_PAGE_SIZE];

int main(void) {
    u64 len = gk_input_read(input);
    if (len % 12 != 0) gk_abort(2, "requests are (kind u32, page u64)", 33);
    u64 count = len / 12;

    /* Streaming keccak over every served page, len-prefixed. */
    u8 acc[32];
    static u8 fold[8 + GK_ARTIFACT_PAGE_SIZE + 32] __attribute__((aligned(8)));
    u64 fold_len = 0;

    for (u64 i = 0; i < count; i++) {
        const u8 *req = input + i * 12;
        u32 kind = ((u32)req[0] << 24) | ((u32)req[1] << 16) | ((u32)req[2] << 8) | req[3];
        u64 page_idx = 0;
        for (int b = 0; b < 8; b++) page_idx = (page_idx << 8) | req[4 + b];

        u64 file_len = gk_artifact_len(kind);
        gk_artifact_read(kind, page_idx, page);

        /* fold = keccak(prev_acc? || len BE8 || page); chain via prefix. */
        fold_len = 0;
        if (i > 0) {
            for (int b = 0; b < 32; b++) fold[fold_len++] = acc[b];
        }
        for (int b = 0; b < 8; b++) fold[fold_len++] = (u8)(file_len >> (56 - 8 * b));
        for (u64 b = 0; b < GK_ARTIFACT_PAGE_SIZE; b++) fold[fold_len++] = page[b];
        gk_keccak256(fold, fold_len, acc);
    }

    gk_output_write(acc, 32);
    return 0;
}

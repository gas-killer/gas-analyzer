/* gk-guest-crt: the C face of GKVM_HOSTCALLS_V1.
 *
 * A closed, versioned hostcall surface: payload in, result out, verified
 * artifact pages, deterministic abort. No clock, no randomness, no
 * filesystem, no network. Everything here is implemented over SP1's native
 * syscalls (HINT_LEN/HINT_READ for input, WRITE to the public-values fd for
 * output), so a guest linking this crt runs unmodified under the operator
 * executor, the forge ffi sidecar, and the SP1 dispute guest.
 */
#ifndef GKVM_H
#define GKVM_H

typedef unsigned char u8;
typedef unsigned int u32;
typedef unsigned long u64;

/* Pinned caps — must match crates/gkvm/src/constants.rs. */
#define GK_INPUT_BYTES_CAP 131072UL
#define GK_OUTPUT_BYTES_CAP 131072UL
#define GK_ARTIFACT_PAGE_SIZE 4096UL

/* Guest-SDK trap codes (0xE… = raised by this crt; the host reserves 0xF…). */
#define GK_TRAP_INPUT_TOO_LARGE 0xE0000001u
#define GK_TRAP_ARTIFACT_VERIFY 0xE0000002u
#define GK_TRAP_ARTIFACT_RANGE 0xE0000003u
#define GK_TRAP_MANIFEST_INVALID 0xE0000004u

/* The committed artifact root for this execution (32 bytes; all-zero when
 * the consumer passed no artifact). Read by the crt before main. */
const u8 *gk_artifact_root(void);

/* Copy the gkExec payload into dst and return its length. dst must hold
 * GK_INPUT_BYTES_CAP + 8 bytes: SP1's hint writes are word-granular and can
 * spill one zero word past the payload. Traps if the payload would not fit
 * (the host also refuses such payloads before running). */
u64 gk_input_read(u8 *dst);

/* Append len bytes to the guest result (the SP1 public-values stream). */
void gk_output_write(const u8 *src, u64 len);

/* Deterministic abort: writes a GKTRAP01 frame and halts. */
_Noreturn void gk_abort(u32 code, const char *msg, u32 msg_len);

/* Byte length of artifact file `kind` (0 = weights, 1 = tokenizer, …),
 * from the manifest the crt verified against gk_artifact_root(). Traps on an
 * unknown kind. */
u64 gk_artifact_len(u32 kind);

/* Read one 4,096-byte page (zero-padded tail) into dst, verifying its
 * Merkle branch against the manifest — always, in every host: skipping
 * verification outside disputes would make operator and dispute cycle
 * counts diverge, a slashing bug by construction. Traps on verify failure. */
void gk_artifact_read(u32 kind, u64 page_idx, u8 *dst);

/* keccak256, exposed because guests routinely need it (commitment folds). */
void gk_keccak256(const u8 *data, u64 len, u8 out[32]);

#endif /* GKVM_H */

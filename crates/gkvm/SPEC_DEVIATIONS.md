# gkvm — deviations from UNBOUNDED_V3_NATIVE.md

The design doc (solidity-sdk, `src/examples/onchain-llm/UNBOUNDED_V3_NATIVE.md`) is the spec.
Where this implementation departs from it, the departure is listed here with its reason, so
the doc can be amended in one pass later. Each of these is consensus surface: every operator
and the dispute path must agree on it.

## 1. Guest hashing uses SP1's `KECCAK_PERMUTE` precompile (decided 2026-09-21)

**Spec:** `GKVM_HOSTCALLS_V1` is "stock SP1 syscalls" for I/O only (HALT, WRITE, HINT_LEN,
HINT_READ); everything else, hashing included, is guest instructions, and a cycle is one
retired rv64im instruction.

**Implemented:** `gk-guest-crt`'s keccak-f[1600] is one `ecall` with syscall code `0x00010109`
(a0 = pointer to the 25 little-endian u64 lanes, 8-byte aligned; a1 = 0). `gk_keccak256` and
the artifact Merkle verification (`gk_artifact_read`) are built on it.

**Why:** verified weight loading was the dominant cost of an answer. Measured on the same
bundles, both tiers identical:

| | software keccak-f (f733d2b) | KECCAK_PERMUTE |
|---|---|---|
| per artifact page (leaf + copy) | 191,939.3 cycles | 10,511.1 cycles |
| per Merkle branch node | 6,038.8 cycles | 269.1 cycles |
| real-size `load+1` (597 MB, 146,083 pages) | 46,465,876,532 cycles | 4,975,845,659 cycles |

Same outputs: `load+1`'s output keccak is unchanged (0xc415…39f1).

**What changes semantically:** a permutation retires as ONE cycle (SP1 `global_clk` counts
the `ecall`), so a cycle no longer means "one instruction of comparable cost" — hashing is
priced near zero relative to arithmetic. `UNBOUNDED_V3_CYCLES_PER_GAS` stays 4.

**Why it is sound:** the pinned executor (`sp1-core-executor =6.8.0`) services the ecall in
`minimal::precompiles::keccak` — one implementation (tiny-keccak) shared by the jit and the
portable tier through the `SyscallContext` trait, so the tiers cannot disagree; the dispute
guest proves the same ecall with SP1's keccak circuit. Nothing is proven per answer — the
operator path is plain execution.

**What moved:** every guest that links the crt got a new `programHash` when rebuilt (qwen ×2,
artifact-probe, every MicroPython image: hello.py, answer.py, stories260k.py). `hello-c.elf` /
`bench-c.elf` are committed bytes pinned by the sdk's golden vectors and were left alone.

## 2. `artifactRoot` binds each file's length

**Spec:** `artifactRoot = keccak(DOMAIN || fileCount || root_0 || root_1 || …)`.
**Implemented:** `keccak(DOMAIN || fileCount(u32 BE) || (len_i(u64 BE) || root_i)*)`.
**Why:** without the length, the byte length of a file inside its last page is whatever the
host says it is — two operators could serve the same root with different `gk_artifact_len`.

## 3. The cycle budget is enforced after the run, not during it

**Spec:** the ELF runs "under SP1's executor with that limit".
**Implemented:** SP1 v6's untraced executors run to completion in one call, so the verdict is
exact but post-hoc (deterministic for every halting guest); a non-halting guest is cut by an
operator-local wall-clock deadline, which is an abstain, never a signed result.

## 4. Artifact pages are served in a declared order, not by random access

The pinned executor has no pluggable hostcall surface, so `gk_artifact_read` consumes pages in
the order the caller declared, each verified in-guest against the manifest. A wrong or
reordered page is a deterministic trap.

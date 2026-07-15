# gk-fast-view

Phase-4 **revmc AOT/JIT fast executor** for overlay-mounted EVM view calls — the
productionized twin of the analyzer's revm-31 interpreter view path
(`evmsketch::call_view_local_multi`), built on the proven `revmc-harness` spike.

It ports `OverlayMount` / `OverlayMountSet` / `OverlayStateDb` and the view-call
execution to **revm 41** and dispatches bytecode execution through **revmc**: the
fixed seg-engine bytecode is JIT-compiled **once** (memoized by codehash), then
every segment view call runs on the compiled artifact. The interpreter is the
fallback for any un-compiled codehash (overlay chunks are STOP-prefixed data,
never executed as code).

## Consensus contract

Operators compare `keccak(returndata)`; gas may differ, returndata must not. The
returndata of this revm-41+revmc path is **byte-identical** to the revm-31
interpreter path — proven by `tests/consensus.rs` against committed golden
fixtures the revm-31 interpreter emits (see that file + the crate lib docs for
the two-stage cross-version methodology). Reverts/halts are loud `Err`s.

## Build & test

Requires LLVM 22 (same as `revmc-harness`). This crate is **excluded** from the
analyzer workspace (own `Cargo.lock`) so revm-41 + revmc + LLVM can never perturb
the revm-31 consensus-critical lockfile.

```sh
export LLVM_SYS_221_PREFIX=/opt/homebrew/opt/llvm@22
export PATH="/opt/homebrew/opt/llvm@22/bin:$PATH"
export DYLD_FALLBACK_LIBRARY_PATH=/opt/homebrew/opt/llvm@22/lib
cargo test -- --nocapture     # runs the consensus gate
```

Regenerate the golden fixtures from the revm-31 interpreter (in the analyzer):

```sh
GK_GEN_GOLDEN=1 cargo test -p gas-analyzer-evmsketch phase4_emit_consensus_golden_fixtures
```

## Sidecar binary + service integration

The service is pinned to **revm-31** (SP1/reth entanglement); this crate needs
**revm-41**. Two revm majors export incompatible copies of the same types
(`AccountInfo`, `Bytecode`, `ExecutionResult`, the `Database` traits) and cannot
be linked into one binary. Resolution: `gk-fast-view` ships a **sidecar binary**
the node shells out to.

```sh
# reads a `.job` (src/job.rs text format) on stdin (or a file arg),
# prints the raw returndata as one hex line on stdout:
gk-fast-view < segment.job
```

`common/src/shard.rs::local_view_call` dispatches to it when
`GK_SHARD_FAST_EXECUTOR=1`, else uses the interpreter (the default). Any fast-path
failure falls back to the interpreter, so the default build/behaviour is never at
risk. Config: `GK_FAST_VIEW_BIN` (binary path), `GK_FAST_VIEW_SPEC` (hardfork,
default `CANCUN`, must match the chain).

The node passes the engine bytecode inline (one `eth_getCode`) and the weights via
the same local `mount_files` it already mmap-mounts — an overlay-mode seg call
(`rootDirectory == address(0)`) reads only its own code + overlay chunks, so the
sidecar issues no RPC of its own.

### Remaining work for a general (non-overlay-mode) target

If a future consumer's view call touches arbitrary base chain state beyond its own
code, the sidecar would need an RPC-backed base `DatabaseRef`. revm-41 ships
`revm_database::AlloyDB` for exactly this; add it behind an `rpc` cargo feature
and pass `rpc_url` + `block` in the job. The overlay seg-engine path does not need
it.

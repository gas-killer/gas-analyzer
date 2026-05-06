# Benchmarks

Four criterion benchmark groups covering the gas estimation pipeline. Allocation counts (via `stats_alloc`) are printed to stderr alongside criterion's wall-time output.

## Running

```sh
# Offline benchmarks (no RPC required)
make bench

# End-to-end benchmark (requires a live Sepolia node)
make bench-rpc RPC_URL=https://your-sepolia-node

# Generate the trace_parsing fixture (once, then commit it)
make fixture RPC_URL=https://your-sepolia-node

# Generate the replay fixture (once, then commit it)
make replay-fixture RPC_URL=https://your-sepolia-node
```

## Comparing against a baseline

To measure the impact of a change, save a named baseline before making it:

```sh
# On the base branch (e.g. main) — saves results under the name "main"
make bench-save-baseline

# After making changes — prints a regression/improvement report vs. the saved baseline
make bench-compare
```

A custom name can be used if comparing two feature branches:

```sh
make bench-save-baseline BASELINE=before-optimisation
make bench-compare       BASELINE=before-optimisation
```

Criterion stores baselines in `target/criterion/` — they are local to the machine and not committed.

## Groups

### `trace_parsing` — `benches/trace_parsing.rs`

Benchmarks `compute_state_updates`: parsing a raw Geth struct-log trace into a `Vec<StateUpdate>`. Uses a pre-generated Sepolia fixture (`benches/fixtures/sepolia_trace.json`). Skips gracefully if the fixture is absent — run `make fixture` to generate it.

This isolates the ingestion step: no EVM execution, no network I/O.

### `gas_estimation` — `benches/gas_estimation.rs`

Two sub-benchmarks using a small canned input (3× `Store` + 1× `Log1`):

- **`build_calldata`** — ABI-encodes `Vec<StateUpdate>` into estimator calldata.
- **`estimate_gas_raw/EmptyDB`** — full revm EVM simulation against a fresh `CacheDB<EmptyDB>`. This is the CPU-bound core of the library.

No network I/O.

### `replay` — `benches/replay.rs`

Benchmarks `replay_preceding_transactions`: executing the transactions that precede the pinned Sepolia tx against a pre-populated `CacheDB<EmptyDB>`. This measures the mid-block state-replay step — the most CPU-intensive part of the preceding-tx path — in isolation, with no RPC or network I/O.

Requires pre-generated fixture files (`benches/fixtures/preceding_txs.json` and `benches/fixtures/pre_block_state.json`). Skips gracefully if absent — run `make replay-fixture` to generate them.

The fixture captures the `PrecedingTx` structs, the block-level sim_env, and the pre-block state (account balances, nonces, bytecode, and every storage slot touched during replay). On each bench iteration the pre-populated `CacheDB<EmptyDB>` is cloned in the setup closure, outside criterion's timed region.

### `end_to_end` — `benches/end_to_end.rs`

Benchmarks the full public API: `call_to_encoded_state_updates_with_evmsketch`. Fetches a pinned Sepolia transaction, traces it, parses the trace, encodes calldata, and runs the EVM simulation. Wall time is dominated by RPC latency. Skips if `RPC_URL` is not set.

Uses `sample_size(10)` since each iteration takes ~1–2 s.

## Flamegraph profiling

The `flamegraph` bench covers all three critical paths in a single binary with two modes:

### CPU flamegraph (latency)

```sh
make flamegraph                            # offline paths only
make flamegraph-online RPC_URL=<sepolia>   # includes end-to-end RPC pipeline
```

SVG per benchmark written to:
```
target/criterion/<group>/<bench>/profile/flamegraph.svg
```

Open in a browser — the SVGs are interactive. Click any frame to zoom into that subtree.

Uses `pprof` at 1000 Hz with frame-pointer unwinding. No elevated privileges required.

### Speedscope (time-order + sandwich view)

For a richer interactive UI — time-ordered view, sandwich mode, always-readable labels:

```sh
make flamegraph-speedscope                            # offline
make flamegraph-speedscope-online RPC_URL=<sepolia>   # includes end-to-end
```

Protobuf files written to:
```
target/criterion/<group>/<bench>/profile/profile.pb
```

Drag any `.pb` file into **https://speedscope.app** (runs in-browser, nothing is uploaded).
Use the **Time Order** view for the end-to-end bench to see the RPC → parse → EVM execution sequence. Use **Left Heavy** for the offline paths.

### Heap flamegraph (allocation call sites)

```sh
make flamegraph-heap
```

Outputs `dhat-heap.json` to the repo root. Open it at:
https://nnethercote.github.io/dh_view/dh_view.html

The heap mode compiles with `--features heap-profile` which activates the `dhat`
allocator. Running both modes separately keeps the CPU flamegraph free of allocator
overhead so the latency picture stays accurate.

### Online/offline

`RPC_URL` gates the end-to-end RPC benchmark. Without it, the bench skips that
function and profiles only the offline paths (`trace_parsing`, `gas_estimation`, `replay`).
`make flamegraph-online` sets `RPC_URL` for you.

---

## Baseline

See [`bench-baseline.md`](../../../bench-baseline.md) at the repo root for recorded numbers.

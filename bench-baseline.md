# Benchmark Baseline

To regenerate: `make bench` (offline) or `make bench-rpc RPC_URL=<node>` (end-to-end).

Alloc counts are printed to stderr alongside criterion's wall-time output.

---

## Machine

| field | value |
|-------|-------|
| CPU | Intel Core i5-8279U @ 2.40GHz |
| OS | macOS 15.7.5 |
| rustc | 1.94.0 (4a4ef493e 2026-03-02) |
| date | 2026-05-05 |

---

## `trace_parsing` / `compute_state_updates`

Fixture: `benches/fixtures/sepolia_trace.json` (141,855 struct-log entries)
SHA-256: `9b8ef97f1ae92cbbe49e726bacf75026fa1c382edcb463d81b49e16667740989`
(Sepolia tx `0x680e2abfbccaf6246b4bda0989fc55dee169d0f6aef2ca4c63a17c6a8a39d6cb` — run `make fixture RPC_URL=...` to regenerate)

| metric | value |
|--------|-------|
| wall time (median) | 280.88 ms |
| allocs/iter | 191,638 |
| bytes allocated/iter | 1,652,383 |

---

## `build_calldata` / `build_gas_estimation_calldata`

Input: 3× `Store` + 1× `Log1` (canned, no RPC)

| metric | value |
|--------|-------|
| wall time (median) | 2.98 µs |
| allocs/iter | 21 |
| bytes allocated/iter | 3,336 |

---

## `estimate_gas_raw` / `estimate_gas_raw/EmptyDB`

Input: same canned state updates, `CacheDB<EmptyDB>` (no RPC)

| metric | value |
|--------|-------|
| wall time (median) | 189.73 µs |
| allocs/iter | 383 |
| bytes allocated/iter | 389,319 |

---

## `replay` / `replay_preceding_transactions`

Fixture: `benches/fixtures/preceding_txs.json` + `benches/fixtures/pre_block_state.json`
(189 preceding txs, 579 accounts — Sepolia tx `0x680e2abfbccaf6246b4bda0989fc55dee169d0f6aef2ca4c63a17c6a8a39d6cb` — run `make replay-fixture RPC_URL=...` to regenerate)

| metric | value |
|--------|-------|
| wall time (median) | 20.01 ms |
| allocs/iter | 4,566 |
| bytes allocated/iter | 2,236,613 |

---

## `end_to_end` / `call_to_encoded_state_updates_with_evmsketch`

Pinned tx: `0x680e2abfbccaf6246b4bda0989fc55dee169d0f6aef2ca4c63a17c6a8a39d6cb` (Sepolia)
Run: `make bench-rpc RPC_URL=<sepolia-node>`

| metric | value |
|--------|-------|
| wall time (median) | 2.53 s |
| sample size | 10 |

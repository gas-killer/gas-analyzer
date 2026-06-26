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
| date | 2026-05-06 |

---

## `trace_parsing` / `compute_state_updates`

Fixture: `benches/fixtures/sepolia_trace.json` (141,855 struct-log entries)
SHA-256: `9b8ef97f1ae92cbbe49e726bacf75026fa1c382edcb463d81b49e16667740989`
(Sepolia tx `0x680e2abfbccaf6246b4bda0989fc55dee169d0f6aef2ca4c63a17c6a8a39d6cb` — run `make fixture RPC_URL=...` to regenerate)

| metric | value |
|--------|-------|
| wall time (median) | 200.46 ms |
| allocs/iter | 191,638 |
| bytes allocated/iter | 1,652,383 |

---

## `prestate_parsing` — extraction paths on the same logical diff

Synthetic, no RPC/fixture (always runs in CI). A heavy compute of `TOTAL_STEPS = 50,000` execution
steps that collapses to `CHANGED_SLOTS = 16` changed slots + 1 log. Both paths produce the **same 17
state updates**; only the representation differs — so the wall-time gap is purely
O(execution steps) vs O(changed slots).

| benchmark | wall time (median) | walks |
|-----------|--------------------|-------|
| `compute_state_updates_heavy` (struct-log path) | 967 µs | all 50,001 struct logs |
| `build_from_prestate` (prestate fast path) | 1.03 µs | the 16 changed slots |

≈ **940× faster** on this 50k-step workload, and the gap widens with trace length: the real
141,855-step `trace_parsing/compute_state_updates` fixture above is ~200 ms here. Absolute times are
machine-dependent (this file's machine is below); bench-CI compares both paths on the same runner.

---

## `build_calldata` / `build_gas_estimation_calldata`

Input: 3× `Store` + 1× `Log1` (canned, no RPC)

| metric | value |
|--------|-------|
| wall time (median) | 1.99 µs |
| allocs/iter | 21 |
| bytes allocated/iter | 3,336 |

---

## `estimate_gas_raw` / `estimate_gas_raw/EmptyDB`

Input: same canned state updates, `CacheDB<EmptyDB>` (no RPC)

| metric | value |
|--------|-------|
| wall time (median) | 26.10 µs |
| allocs/iter | 48 |
| bytes allocated/iter | 319,401 |

---

## `replay` / `replay_preceding_transactions`

Fixture: `benches/fixtures/preceding_txs.json` + `benches/fixtures/pre_block_state.json`
(189 preceding txs, 579 accounts — Sepolia tx `0x680e2abfbccaf6246b4bda0989fc55dee169d0f6aef2ca4c63a17c6a8a39d6cb` — run `make replay-fixture RPC_URL=...` to regenerate)

| metric | value |
|--------|-------|
| wall time (median) | 14.81 ms |
| allocs/iter | 4,567 |
| bytes allocated/iter | 2,253,912 |

---

## `end_to_end` / `call_to_encoded_state_updates_with_evmsketch`

Pinned tx: `0x680e2abfbccaf6246b4bda0989fc55dee169d0f6aef2ca4c63a17c6a8a39d6cb` (Sepolia)
Run: `make bench-rpc RPC_URL=<sepolia-node>`

| metric | value |
|--------|-------|
| wall time (median) | 2.71 s |
| sample size | 10 |

# Unbounded Simulation Mode (`SimProfile::Unbounded`)

**Status:** Implemented (analyzer side) — guest-side mirroring required before slashing ships
**Related:** [GAS_KILLER_SLASHING_SPEC.md](./GAS_KILLER_SLASHING_SPEC.md), [SP1_REVM_IMPLEMENTATION_SPEC.md](./SP1_REVM_IMPLEMENTATION_SPEC.md), solidity-sdk PRs [#51](https://github.com/gas-killer/solidity-sdk/pull/51) (single-slot commitment), [#47](https://github.com/gas-killer/solidity-sdk/pull/47)/[#48](https://github.com/gas-killer/solidity-sdk/pull/48) (multi-call forwarding)

## What it is

Gas Killer's core trade is: **computation happens off-chain, only the net state
diff lands on-chain.** Until now the analyzer simulated tracked functions under
the real chain's environment, so a tracked function was still bounded by the
block gas limit (and post-Osaka, the EIP-7825 2^24 tx cap) — even though nothing
about the *payload* required that bound.

`SimProfile::Unbounded` removes the bound. The tracked function is simulated
under pinned, protocol-versioned limits ~24,000× a mainnet block:

| Constant | Value | Meaning |
|---|---|---|
| `UNBOUNDED_BLOCK_GAS_LIMIT` | `1 << 40` (~1.1 Tgas) | block env gas limit during simulation |
| `UNBOUNDED_TX_GAS_LIMIT` | `1 << 40` | tx gas limit during simulation (EIP-7825 cap deliberately not applied) |

In exchange, the extracted payload must pass the **single-slot shape gate**
(`validate_unbounded_shape`):

- at most **one `Store`** — the consumer's commitment slot;
- **no `CREATE`/`CREATE2`** — initcode replays on-chain at real gas;
- any number of `Call` / `Log*` ops — but `Call`s re-execute on-chain at real
  gas prices, so compute inside them is *not* killed. Multi-call forwarding
  (`applyForwardedUpdates`, one commitment write per sibling contract) rides in
  this category.

The result: **unbounded Solidity, O(1) on-chain state.** A tracked function may
iterate a million-entry order book, verify a multi-megabyte witness against a
commitment, or run a whole matching epoch — what ships to `verifyAndUpdate` is
one SSTORE, some logs, and bounded calls. The intended consumer shape is the
single-slot commitment pattern of solidity-sdk PR #51, where the expanded state
travels as a calldata witness and only its hash lives in storage.

### Calldata

Neither revm nor the EVM protocol caps calldata *size*; calldata is priced in
gas (EIP-7623 floor). Lifting the gas limits therefore lifts the effective
calldata bound too — `1 << 40` gas admits ~27 GB of floor-priced calldata.
Multi-megabyte witnesses simulate fine even though they could never land in a
real transaction; only the signed payload must fit on-chain.

## How to use it

```rust
use gas_analyzer_core::SimProfile;
use gas_analyzer_evmsketch::{
    EvmSketchExecutorCache, call_to_encoded_state_updates_with_evmsketch_profiled,
};

let (payload, gas, is_heuristic, skipped) =
    call_to_encoded_state_updates_with_evmsketch_profiled(
        &cache, rpc_url, tx_request, block_number, SimProfile::Unbounded,
    ).await?;
```

Semantics under `Unbounded`:

1. Both extraction paths (prestate fast path *and* struct-log fallback) run the
   `debug_traceCall` with `tx.gas = 2^40` and `blockOverrides.gasLimit = 2^40`.
   The caller's `gas` field is overridden unconditionally — the profile is a
   pinned protocol environment, not a hint.
2. The extracted updates are validated against the shape gate; a violation is a
   **hard error** (a consumer writing N slots scales on-chain like a plain
   contract and defeats the mode).
3. The **gas estimate still uses the real chain env** — `verifyAndUpdate` lands
   in a real block, so applying the payload is priced under real limits.

### Node requirement

The node serving `debug_traceCall` has the last word on simulation gas:

| Node | Requirement |
|---|---|
| anvil | `--disable-block-gas-limit` |
| geth | `--rpc.gascap=0` (or ≥ the profile limit) |
| reth | `--rpc.gascap` raised accordingly |

A clamping node makes heavy calls OOG inside the tracer. This fails safe: the
reverted root frame forces the struct-log fallback classification (PR #165), so
a clamped simulation can produce an error — never an unsound diff.

## Determinism: why the constants are versioned

The environment override is **part of the protocol**, not a local tunable.
Three parties re-execute the same tracked function and must get bit-identical
results:

1. **Operators**, when extracting the update payload they BLS/ECDSA-sign.
2. **This analyzer**, when simulating on a user's behalf.
3. **The SP1 slashing guest** ([SP1_REVM_IMPLEMENTATION_SPEC.md](./SP1_REVM_IMPLEMENTATION_SPEC.md)),
   when a slasher re-executes the original function inside the zkVM to prove
   the signed updates wrong.

If the guest ran with the real header's gas limit while operators simulated
under lifted limits, a heavy-but-honest execution would OOG in the guest,
produce a different update set, and **falsely slash honest operators**.
Conversely, ad-hoc per-operator limits would fragment quorum signatures at the
boundary. Hence:

- The limits are `pub const` in `gas_analyzer_core::sim_profile` — the guest
  crate must import them from there (the crate is pure/WASM-safe and compiles
  in a zkVM guest).
- Any change to the values is a **new profile version** (`UnboundedV2`, …),
  coordinated across operators and the committed guest ELF — never a silent
  edit. The guest's public values should commit to the profile version
  alongside the chain-config hash so a proof under the wrong profile cannot
  satisfy the verifier.

### Fork support (implemented)

The BreadchainCoop `sp1-contract-call` fork (branch
`ron/unbounded-env-overrides`, targeting `cancun-v1`) provides the mechanism:

```rust
use sp1_cc_client_executor::EnvOverrides;
use gas_analyzer_core::UNBOUNDED_BLOCK_GAS_LIMIT;

let overrides = EnvOverrides::gas_limits(UNBOUNDED_BLOCK_GAS_LIMIT);

// Host: prefetch state under the SAME limits the guest will execute with —
// a host that OOGs early produces a witness the guest cannot complete on.
let out = sketch.call_raw_with_overrides(&input, overrides).await?;
let sketch_input = sketch.finalize().await?;

// Guest: re-execute under the identical env; the overrides are bound into
// `chainConfigHash` (ChainConfigWithEnvOverrides), so a proof under
// different limits cannot satisfy the verifier.
let executor = ClientExecutor::eth(&sketch_input)?;
executor.execute_and_commit_with_overrides(input, overrides);
```

Setting `tx_gas_limit` also lifts revm's EIP-7825 cap (2^24, Osaka+) to the
same value, so execution is identical on both sides of the hardfork boundary.
Regression tests in the fork pin the two invariants: a ~40M-gas call OOGs at
exactly the header limit without overrides, and succeeds (burning more than
any real block) under `gas_limits(1 << 40)`.

> **Latent bug fixed along the way:** upstream sp1-cc assigned the gas limit
> via `modify_tx_chained` on the context, but `Evm::transact` replaces the tx
> env with the converted `ContractInput` — whose `TxEnv::default()` carries
> revm's 2^24 builder default. Every sketch/guest execution was silently
> capped at **16,777,216 gas** regardless of the header. Any tracked function
> above ~16.7M gas would have OOG'd in host prefetch and guest re-execution
> even in "bounded" mode.

Remaining before slashing ships: merge the fork branch into `cancun-v1`, and
have the slashing guest program (per `SP1_REVM_IMPLEMENTATION_SPEC.md`) pass
`EnvOverrides::gas_limits(UNBOUNDED_BLOCK_GAS_LIMIT)` — importing the
constant from `gas-analyzer-core`, never restating it — and commit the profile
version in its public values.

## What this mode does *not* change

- **Trust model.** The quorum still signs the diff; the proof (when it ships)
  is a fraud proof, not a validity proof. Unbounded mode widens what operators
  can compute, not what users must trust.
- **On-chain application costs.** One commitment SSTORE (~5–22k gas) + BLS/ECDSA
  verification overhead per `verifyAndUpdate`, regardless of off-chain compute.
- **`Call` op costs.** Calls in the payload re-execute on-chain at real gas.
  A forwarded multi-call bundle costs one commitment write per sibling contract
  plus call overhead — still O(contracts), not O(compute).
- **The serialization model.** One transition per `transitionIndex` per
  consumer; a direct on-chain call to a tracked function still invalidates
  in-flight signed updates. Heavy simulations widen this race window
  (extraction + signing + proving all take longer than a cheap call) —
  consumers using unbounded mode should gate direct execution paths.

## Test coverage

- `gas_analyzer_core::sim_profile` unit tests: profile overrides, shape gate
  (single store / multi store / CREATE rejection / empty payload).
- `gas-analyzer-evmsketch` anvil-backed integration tests
  (`test_unbounded_profile_*`): a ~40M-gas busy-loop consumer OOGs under
  `Chain` (classified `Fallback`, per #165's revert soundness rule) and
  extracts exactly `[Store, Log1]` under `Unbounded` against a real anvil
  with `--disable-block-gas-limit`; a two-slot writer extracts but fails the
  shape gate with `TooManyStores { count: 2 }`.

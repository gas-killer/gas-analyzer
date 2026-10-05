# Historical state reads for tracked functions

**Status:** Implemented in the analyzer (Solidity interface, off-chain executor, CLI, tests).
Operator integration and fraud-proof support for historical reads are still to do — see
[What is not done yet](#what-is-not-done-yet).

## What it is

A tracked function never executes on-chain. Operators run it off-chain, sign the resulting state
diff, and only that diff lands, through `verifyAndUpdate`. The signed message is
`sha256(transitionIndex, contract, targetFunction, storageUpdates)`, so nothing on-chain depends on
*how* the diff was computed.

That makes it possible to give the off-chain execution something the EVM cannot offer: **read access
to state at earlier blocks.** A tracked function can ask "what was this storage slot, balance, or
view-call result at block N?" and use the answer in its computation. The use case that prompted this
is DeFi insurance — deciding a claim by comparing a vault's share price before and after a loss
event — which today has to run in a TEE because no contract can see past state.

The access point is a virtual precompile:

| | |
|---|---|
| Address | `0x45CA7275E900a8Cd49a62F59456402a57C192fC0` — the low 20 bytes of `keccak256("gaskiller.history")` |
| Interface | [`IGasKillerHistory`](../contracts/src/history/IGasKillerHistory.sol) |
| Library | [`GasKillerHistory`](../contracts/src/history/GasKillerHistory.sol) |
| Example | [`VaultLossOracle`](../contracts/src/history/examples/VaultLossOracle.sol) |

It exists only in the Gas Killer execution environment. On a real chain the address has no code, and
the library reverts with `HistoryUnavailable` rather than letting a function silently read zeros.

## Using it from Solidity

```solidity
import {GasKillerHistory} from "./history/GasKillerHistory.sol";

// The vault's share price at a past block.
(bool ok, bytes memory ret) = GasKillerHistory.callAt(
    vault, abi.encodeCall(IERC4626.convertToAssets, (1e18)), pastBlock
);

// A storage slot, a balance, or a block's timestamp at a past block.
bytes32 v   = GasKillerHistory.storageAt(token, slot, pastBlock);
uint256 bal = GasKillerHistory.balanceAt(account, pastBlock);
uint256 ts  = GasKillerHistory.blockTimestampAt(pastBlock);
```

| function | returns |
|---|---|
| `storageAt(account, slot, blockNumber)` | the slot's value |
| `balanceAt(account, blockNumber)` | the ether balance |
| `codeHashAt(account, blockNumber)` | the code hash (zero for an empty account) |
| `blockHashAt(blockNumber)` | the block hash |
| `blockTimestampAt(blockNumber)` | the block timestamp |
| `callAt(target, data, blockNumber)` | `(success, returnData)` of a read-only call run against that block |

## Semantics

- **"At block N" means after block N executed** — the state `eth_getStorageAt(…, N)` returns.
- **`blockNumber` must be at or before the execution block** (the block the tracked function runs at).
  A later block is refused: the precompile reverts with
  `GasKillerHistory: block X is after execution block Y`, which the library bubbles up.
- **Queries must be `STATICCALL`s.** A regular `CALL`, `DELEGATECALL` outside a static context, or a
  value transfer is refused. In addition, the executor refuses to produce any payload containing a
  `Call` op to the history address, so a non-static history call can never be signed.
- **`callAt`** runs in a separate EVM against the historical block, under that block's header and
  hardfork. `msg.sender` is the tracked contract. It has a fixed inner gas limit of 30,000,000, its
  state changes are discarded, and it cannot itself read history. A failing historical call reports
  `success = false`; it does not revert the tracked function.
- **The tracked function itself** runs exactly as `debug_traceCall` at the execution block would:
  against the state after that block, under its header.

### Gas schedule

Pinned in `gas_analyzer_core::history`. Every party that executes a tracked function — operators, the
analyzer, a future slashing guest — must charge the same, or an execution near its gas limit could
succeed for one and run out of gas for another.

| query | gas charged to the caller |
|---|---|
| `storageAt`, `balanceAt`, `codeHashAt`, `blockHashAt`, `blockTimestampAt` | 2,600 (`HISTORY_READ_GAS`) |
| `callAt` | 10,000 flat (`HISTORY_CALL_GAS`); the inner call's own gas does not count against the caller |

## How it is implemented

The default extraction path asks an RPC node to simulate the tracked function with
`debug_traceCall`. A node can only run code against one block and cannot be given new precompiles,
so history needs local execution:

1. **`gas_analyzer_core::history`** (pure, wasm-safe, zkVM-safe) — the ABI, address, gas schedule,
   query decoding and validation, answer encoding, and the read log with its commitment. This is the
   definition of a history read that every implementation shares.
2. **`gas_analyzer_evmsketch::history`** — runs the tracked function in revm over RPC-backed state,
   with the history precompile installed alongside the standard ones. Each query is answered from
   the requested block's state over the same RPC and appended to an ordered read log.
3. The local run produces the same geth-format traces a node would — a `callTracer` frame and a
   `prestateTracer` diff, or a struct-log trace — via `revm-inspectors`, and **feeds them to the
   existing extractors unchanged.** The unbounded-profile payload check, ABI encoding and gas
   estimate are shared with the default path.

Entry point:

```rust
use gas_analyzer_evmsketch::{call_to_encoded_state_updates_with_history, StateEncoding};

let out = call_to_encoded_state_updates_with_history(
    &cache, rpc_url, tx_request, block_number,
    StateEncoding::PrestateNet, SimProfile::Chain,
).await?;
out.encoded;           // EncodedStateUpdates — the payload to sign, as today
out.reads;             // Vec<HistoricalRead> — every query and its answer, in order
out.reads_commitment;  // keccak256(abi.encode(reads))
```

**For a function that does not touch the precompile, the payload is byte-identical to the default
path's.** The integration tests check this against a node-traced extraction under all three encodings,
for plain writes, delegatecall log ordering, a regular `CALL` (struct-log fallback) and a swallowed
revert. That property is what makes the new path safe to adopt: the only new behaviour is the
precompile itself.

### CLI

```sh
cargo run -- h request.json
```

```json
{
  "to": "0x…",
  "data": "0x…",
  "from": "0x…",
  "gas": 3000000,
  "block": 12345678,
  "encoding": "prestate-net",
  "profile": "chain"
}
```

Only `to` and `data` are required. `block` defaults to latest, `encoding` to `prestate-net`,
`profile` to `chain`. The output gives the extraction path, the payload, the gas to apply it, and
every history read with its answer and the commitment.

## Requirements

- **An archive node** for any block older than a full node's pruning window — 128 blocks on a default
  geth full node, and a few thousand blocks (well under a week) on the public endpoints tried. The RPC needs `eth_getProof` or the
  `eth_getBalance`/`eth_getStorageAt` family at historical blocks, plus `eth_getBlockByNumber`.
  It does **not** need `debug_traceCall`, because the tracked function executes locally.
- A **multi-threaded tokio runtime**, like the rest of the evmsketch crate (state is fetched from
  inside revm with `block_in_place`).
- A fetch failure is an **error**, never a guessed value. The tracked function must not run on data
  nobody fetched.

## What is not done yet

**1. Operator integration.** The operator software is not in this repository. Operators need to call
`call_to_encoded_state_updates_with_history` instead of the `debug_traceCall`-based path (for every
task, or only for consumers that opt in), and need archive RPCs. Because the payload is identical
when history is unused, switching the whole fleet over is possible, but that is a fleet-wide decision
like any encoding change.

**2. Fraud proofs for historical reads.** A slashing guest that re-executes a tracked function must
install the same precompile and, instead of trusting an RPC, verify each answer with a proof against
the corresponding block's state root. The pieces:

- **The read log is the witness.** Each `HistoricalRead` is a claim about state at a block. The guest
  checks a storage or account proof for each one against that block's state root. A `callAt` needs
  the whole historical call re-executed against a proven state witness for its block — the same
  thing `sp1-contract-call` already does for a single block.
- **Block headers must be anchored** to something the guest trusts. `sp1-contract-call`, already a
  dependency, provides this: a header anchor, an EIP-4788 beacon-root anchor (covers about 27 hours)
  and a chained beacon anchor for older blocks. How far back consumers read decides which one is
  needed and how expensive proving is.
- **Proving cost grows with the number of reads.** Each read is a Merkle proof; each `callAt` is a
  full historical execution.
- The pure half (`gas_analyzer_core::history`) already compiles in a guest, so query decoding, the gas
  schedule and answer encoding are shared rather than re-implemented.

**3. Binding the read log into the task.** `reads_commitment` lets operators compare logs cheaply. If
the slashing protocol needs the log to be part of what operators attest to, it has to be bound into
the signed task — a protocol change outside this repository.

## Limitations

- **Finality is the caller's job.** Reads are only as stable as the blocks they target, so the
  execution block should be finalized or the protocol must tolerate reorg-driven disagreement.
- **`callAt` cannot nest history reads.** A historical call runs with the standard precompiles only.
- **Throughput depends on the RPC.** Every new account or slot at a historical block is a network
  round-trip; reads are cached per block within one execution, not across executions.

# Gas Analyzer

Compute state update instructions for gas killer application and estimate gas savings.

This repository contains two surfaces:

- **CLI / library** (`crates/{core,gas-estimator,rpc,evmsketch,anvil,cli,wasm}`) — the original one-shot analyzer. See sections below.
- **Indexer service** (`crates/indexer-{api,rpc,store,resolver,service,web}`) — a persistent block-by-block indexer that pipes per-tx gas-savings into Postgres, plus a purpose-built `indexer-web` UI (axum + askama + htmx) on port 3000 for BD dashboards and admin actions. Architecture, deployment, and known limitations: [`docs/INDEXER.md`](docs/INDEXER.md).

## Implementation Notes
- Default mode uses EvmSketch for Anvil-free transaction simulation
- Legacy Anvil mode available via `--anvil` flag for precise gas estimation
- Note: Real blockchain traces may differ due to other transactions in block
- Ignores transactions that 
   - are below the gas limit
   - do not call a smart contract
   - create a smart contract

## Setup
1. Clone the repository
2. Copy the example environment file:
   ```bash
   cp .env.example .env
   ```
3. Fill in the required environment variables in `.env`:

## Tests
```bash
cargo test
```


## CLI (unstable)
The CLI supports analyzing single transactions and transaction requests. By default, it uses EvmSketch for analysis.

> **Note:** Examples are mainnet transactions. Ensure `.env` has a mainnet RPC set or update to transactions on your target network.

### Analyze a transaction
```bash
cargo run -- t 0x9add9d0f26bc6d867c1d6d41dda6287d9721a377cea42440250884f76d2a0fa7
```

Add `--debug` to print full error details when gas estimation or trace extraction fails:
```bash
cargo run -- t 0x9add9d0f26bc6d867c1d6d41dda6287d9721a377cea42440250884f76d2a0fa7 --debug
```

### Price a transaction as a nested settlement
Pass `--owned` with the contracts that would integrate the SDK alongside the root (the
transaction's `to`). If a root `A` calls `B` and `C`, and only `C` belongs to the same owner as `A`:
```bash
cargo run -- t <TX_HASH> --owned <C_ADDRESS>
```
The trace is split into one frame per owned contract that the root or another owned frame calls
directly, and each frame is measured against the block's state. The report compares the root-only
program with the nested tree, per signature scheme, and says why any owned contract got no frame:
- it was reached only through a contract outside the set, so it ran inside that contract's call;
- its own computation is smaller than what nesting costs under the versioned cost model;
- it reverted, or uses transient storage or `SELFDESTRUCT`.

The contracts never integrated the SDK, so this is an estimate: each frame is measured from the state
before the transaction, and the cost model's fixed overhead stands in for proofs and witnesses.

### Analyze a transaction request
```bash
cargo run -- r path/to/file.json
```

### Legacy Anvil Mode

To use the legacy Anvil-based implementation (requires running Anvil, provides precise gas estimates):

```bash
# Build with anvil feature
cargo build --features anvil

# Run with --anvil flag
cargo run --features anvil -- --anvil t 0x9add9d0f26bc6d867c1d6d41dda6287d9721a377cea42440250884f76d2a0fa7
```

### Block Analysis (Anvil only)

Block analysis requires Anvil for generating detailed reports:

```bash
cargo run --features anvil -- b 0x386725b93d39849e06d42c52b6ed492d98459f12db1f6c124ab483f5e7a64375
cargo run --features anvil -- b latest
```

The analysis report is written to the `OUTPUT_FILE`.

## Solidity Contracts

The [`contracts/`](contracts/README.md) directory contains the Solidity contracts used for on-chain gas estimation and integration testing.

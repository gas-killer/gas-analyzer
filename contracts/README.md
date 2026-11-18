# Contracts

Solidity contracts used by the Gas Analyzer for on-chain gas estimation and integration testing.

## Contents

- **`src/StateChangeHandlerGasEstimator.sol`** — Wrapper around `StateChangeHandlerLib` from [solidity-sdk](https://github.com/gas-killer/solidity-sdk). Deployed to an Anvil fork to measure the actual gas cost of replaying a transaction's state updates.
- **`src/AccessControlTestContracts.sol`** — Test contracts for verifying state update extraction across access-controlled calls.
- **`src/DelegateCallTestContracts.sol`** — Test contracts for verifying correct state update extraction with `DELEGATECALL` (only top-level context changes should be captured).
- **`src/SimpleStorageTestContracts.sol`** — `SimpleStorage` (plus the `DummyExternal` it calls): one function per basic state-update shape (SSTORE + LOG, an external CALL).
- **`script/`** — Foundry deployment scripts for the test contracts.

## Prerequisites

- [Foundry](https://book.getfoundry.sh/getting-started/installation)

## Usage

```bash
cd contracts

# Install dependencies (forge-std, solidity-sdk)
forge install

# Build
forge build

# Run Foundry tests (if any)
forge test
```

The compiled ABI artifacts (in `out/`) are referenced by the Rust `gas-analyzer-anvil` crate via the `abis/` directory at the workspace root.

The wasm crate's anvil integration tests (`crates/wasm/tests/anvil_integration.rs`) deploy
`DelegateCallTestMain` and `AccessControlTestMain` from creation bytecode in
`crates/wasm/tests/fixtures/`. After changing one of them, regenerate its fixture:

```bash
cd contracts
forge inspect DelegateCallTestMain bytecode > ../crates/wasm/tests/fixtures/DelegateCallTestMain.hex
```

`SimpleStorage` is deployed from the bytecode embedded in `crates/core/src/types.rs` (the
`#[sol(bytecode = ...)]` on its `sol!` block), so there is one copy to keep in step with the source.

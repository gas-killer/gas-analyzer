# Contracts

Solidity contracts used by the Gas Analyzer for on-chain gas estimation and integration testing.

## Contents

- **`src/StateChangeHandlerGasEstimator.sol`** — Wrapper around `StateChangeHandlerLib` from [gas-killer-avs-sol](https://github.com/BreadchainCoop/gas-killer-avs-sol). Deployed to an Anvil fork to measure the actual gas cost of replaying a transaction's state updates.
- **`src/AccessControlTestContracts.sol`** — Test contracts for verifying state update extraction across access-controlled calls.
- **`src/DelegateCallTestContracts.sol`** — Test contracts for verifying correct state update extraction with `DELEGATECALL` (only top-level context changes should be captured).
- **`script/`** — Foundry deployment scripts for the test contracts.

## Prerequisites

- [Foundry](https://book.getfoundry.sh/getting-started/installation)

## Usage

```bash
cd contracts

# Install dependencies (forge-std, gas-killer-avs-sol)
forge install

# Build
forge build

# Run Foundry tests (if any)
forge test
```

The compiled ABI artifacts (in `out/`) are referenced by the Rust `gas-analyzer-anvil` crate via the `abis/` directory at the workspace root.

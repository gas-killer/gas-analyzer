# Test fixtures

Runtime bytecode for the historical-state integration tests in `../history.rs`, compiled with
solc 0.8.35 from:

- `VaultLossOracle.runtime.hex` — `contracts/src/history/examples/VaultLossOracle.sol`
- `MockSharePriceVault.runtime.hex` — `contracts/src/HistoryTestContracts.sol`

Regenerate from `contracts/` with:

```sh
forge build --contracts src/history --out /tmp/out
forge build --contracts src/HistoryTestContracts.sol --out /tmp/out2
jq -r .deployedBytecode.object /tmp/out/VaultLossOracle.sol/VaultLossOracle.json > VaultLossOracle.runtime.hex
jq -r .deployedBytecode.object /tmp/out2/HistoryTestContracts.sol/MockSharePriceVault.json > MockSharePriceVault.runtime.hex
```

# Gas Killer × Scroll: Gas Savings Report

Oct 14, 2026

## Summary

Gas Killer could cut the Ethereum gas cost of Scroll's bundle finalisation by **65%**, about 293,000 of the 448,000 gas each one spends today. We measured this on real mainnet calls to the ScrollChain contract in October 2026. Almost all of the saving is the proof check, which Gas Killer moves off-chain. Scroll finalises about 14 bundles a day, so the saving recurs on each one.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All five bundle finalisations we measured save 65%. The saving is nearly identical every time, because the proof check costs the same regardless of the bundle.

| Transaction | Function | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0x375ae6ca…](https://etherscan.io/tx/0x375ae6cafaadb8456f89068f728f0c72993b935092d052d68a3edda055f6ab10) | bundle finalisation (`0xc1aa4e19`) | 448,139 | 292,851 | **65.4%** |
| [0x8b5cc84d…](https://etherscan.io/tx/0x8b5cc84d5134f990348eb5cd0d935ff8c06fc3c21124ceac82ab5f1e3d9f1324) | bundle finalisation (`0xc1aa4e19`) | 448,151 | 292,875 | **65.4%** |
| [0x355c5094…](https://etherscan.io/tx/0x355c5094a88eb2d9c40e6feb33727f7ec01680025a45653224c8e0fd4d1de049) | bundle finalisation (`0xc1aa4e19`) | 448,103 | 292,815 | **65.4%** |
| [0xe08628e4…](https://etherscan.io/tx/0xe08628e4ad9b2a73ad07652de9b6222ad87ca5d033ea8a7f6cacbb6f2819bf3f) | bundle finalisation (`0xc1aa4e19`) | 448,067 | 292,803 | **65.4%** |
| [0xe383dd19…](https://etherscan.io/tx/0xe383dd194cfdfbfbed5442f74354c57a861cca27bbd4863657f8abafd9833e87) | bundle finalisation (`0xc1aa4e19`) | 448,115 | 292,815 | **65.3%** |

Savings assume Gas Killer's Schnorr signature scheme.

## Where the saving comes from

The saving comes from the proof check in every bundle finalisation. ScrollChain passes each bundle's proof to its MultipleVersionRollupVerifier, and that check costs about 300,000 of the 448,000 gas.

- **What gets cheaper:** the proof check runs off-chain. On-chain, only the finalised batch index, state root and events are written.
- **What stays the same:** the state Scroll records on Ethereum is exactly what the contract produces today.
- **What integration involves:** one contract, ScrollChain, would use the Gas Killer SDK. The verifier stays as it is.
- **What saves nothing:** batch commits (`0x9bbaa2ba`) do no proof work, so they are left as they are.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions from October 2026, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** ScrollChain has not integrated Gas Killer, so the numbers estimate a future integration.
- **Consistent over time:** finalisations we measured in September 2026 saved 64.6–65.4%, the same as now.
- **Volume:** ScrollChain received 14 bundle finalisations and 14 batch commits in the day to block 26,191,901.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other Scroll transactions or contracts you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

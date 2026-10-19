# Gas Killer × YieldNest: Gas Savings Report

Oct 19, 2026

## Summary

Gas Killer could cut the gas cost of ynETH MAX's `processAccounting` by about **76–79%**: roughly 565,000 of the 715,000–745,000 gas each update spends today. We measured this on real Ethereum mainnet transactions to the ynETH MAX vault. The saving comes from pricing the vault's strategy positions, which Gas Killer moves off-chain, and it needs no change to what the accounting update writes.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All five `processAccounting` transactions we measured save over 75%, and the saving is almost the same every time.

| Transaction | Function | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0xcaf7bb74…](https://etherscan.io/tx/0xcaf7bb74b11de8ad34c96b73f2dd88ac6e2408cc5deb2b11f4dcba16145c6710) | `processAccounting` | 714,878 | 566,549 | **79.3%** |
| [0x3015ff44…](https://etherscan.io/tx/0x3015ff44b641b43591ec9fdf53807edf6fa30a66adadace8a94ee91d3cac6f7a) | `processAccounting` | 744,977 | 565,635 | **75.9%** |
| [0x1c576c22…](https://etherscan.io/tx/0x1c576c2297e3e44c5ce45da54e05e6dc152ba6a436bd6dc71a4bb3b4fac5391d) | `processAccounting` | 744,675 | 565,333 | **75.9%** |
| [0xe29d8160…](https://etherscan.io/tx/0xe29d81603083f244d9b0f12dd4c494993ee85472f8a5481b5affbd2b013408ac) | `processAccounting` | 744,448 | 565,106 | **75.9%** |
| [0xc6995839…](https://etherscan.io/tx/0xc69958393747a9481d4bde0087b6c48646c376644f99bf336bd25714464500e5) | `processAccounting` | 744,047 | 564,705 | **75.9%** |

We also measured four ynRWAx `requestWithdrawal` transactions. They save about 7%, because most of their gas goes to deploying a new withdrawal-request contract, which has to happen on-chain. Savings assume Gas Killer's Schnorr signature scheme.

## Where the saving comes from

The saving comes from how `processAccounting` values the vault. Each update reads the vault's position in every strategy it allocates to, then writes the new total. About 530,000 of the 744,000 gas is those reads.

- **What gets cheaper:** the position reads and the arithmetic over them run off-chain. On-chain, the update only writes the new totals.
- **What stays the same:** the vault's total assets, its share price and its events are exactly what the contract would produce today.
- **What integration involves:** one contract, the ynETH MAX vault, would use the Gas Killer SDK, and one sender, the keeper that calls `processAccounting` today, would submit through Gas Killer. The strategies it reads from stay as they are.
- **Who gains:** YieldNest pays less for every accounting update. The same approach applies to other YieldNest vaults built on the same `Vault` contract.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** the vault has not integrated Gas Killer, so the numbers estimate a future integration.
- **Check it yourself:** you can run any of these transactions through the Gas Killer analyzer at [gaskiller.xyz/analyzer](https://gaskiller.xyz/analyzer).
- **Volume:** we saw 53 `processAccounting` calls in the 30 days we sampled, about 1.8 a day. The total saving grows with how often the vault updates and with gas prices.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other YieldNest transactions or contracts you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

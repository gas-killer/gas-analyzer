# Gas Killer × Aave: Gas Savings Report

Oct 15, 2026

## Summary

Gas Killer could cut the gas cost of an Aave V3 `borrow` by **14–64%**: about 25% on a typical borrow, and up to 405,000 gas on borrowers with many assets. We measured this on real Ethereum mainnet borrows in October 2026. The saving comes from the price reads in the health-factor check. The Aave V3 Pool handles about 1,090 borrows a day, so the saving recurs on every one.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All six borrows we measured save gas, from 14% to 64%. They are V3 borrows, picked across the range of borrow sizes, from a smaller-than-typical borrow up to the largest in an 8-hour window. Borrows that cost more save more, because they check prices for more assets.

| Transaction | Function | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0xd7786c4e…](https://etherscan.io/tx/0xd7786c4e470f23e884bdfbfc14026eb157429bb0314f2eabe56d37f8a681f245) | `borrow` | 636,268 | 405,515 | **63.7%** |
| [0xb9e0a6c3…](https://etherscan.io/tx/0xb9e0a6c3c3c310d9250a0b0dcaf798fa96b1eec1a671cedfb5208f21b2244958) | `borrow` | 439,284 | 208,531 | **47.5%** |
| [0xf98c58f4…](https://etherscan.io/tx/0xf98c58f4867abe4e02fff584d872939307f40b0be7fd1e261f1b505f92c4d5f9) | `borrow` | 381,980 | 150,228 | **39.3%** |
| [0x5c713687…](https://etherscan.io/tx/0x5c713687b8789cc582430c5db317b07d3a04e2aecb8f0cfb7b71cd0d60a09824) | `borrow` | 333,343 | 101,507 | **30.5%** |
| [0x89123090…](https://etherscan.io/tx/0x89123090ed69760e08e4504365e796d5e923f40f78f100e99e2c7f7693a500b2) | `borrow` (typical size) | 306,943 | 75,143 | **24.5%** |
| [0x97151cb4…](https://etherscan.io/tx/0x97151cb479b8e17dbb84e299aca7ae58d16bfabac89d737e300ddd0b03d9f048) | `borrow` | 268,466 | 37,821 | **14.1%** |

Savings assume Gas Killer's Schnorr signature scheme.

### Aave V4

Aave V4 borrows save 17–46%, from the same health-factor price reads. We measured five V4 borrows from October 2026, sent to the Main and Bluechip spokes.

| Transaction | Spoke | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0xea1e8879…](https://etherscan.io/tx/0xea1e8879f3b85a7fd8750e96445f7755e1125bff1567d111518ecc8849e00598) | Bluechip | 488,737 | 225,920 | **46.2%** |
| [0x76c09b0b…](https://etherscan.io/tx/0x76c09b0bb9ad006ed16fc82cf0b20f141e34fd4196c09414a37b1bfb933e262a) | Main | 369,235 | 128,269 | **34.7%** |
| [0x557d5025…](https://etherscan.io/tx/0x557d502550b8edb48f663f25366db92a37b207cdf7decad649ff8db976e9e420) | Main | 339,583 | 114,701 | **33.8%** |
| [0x26456699…](https://etherscan.io/tx/0x26456699add1999853561856f68bd714a7ba846e3818d9307c266493d727b448) | Main | 252,804 | 53,686 | **21.2%** |
| [0xa6415540…](https://etherscan.io/tx/0xa64155401048c6bfab178ab827b30448de7de8dfcf7a81f752fe2a857ee8bf95) | Main (typical size) | 279,475 | 48,937 | **17.5%** |

In V4 each spoke would use the Gas Killer SDK. The hub stays as it is: its work is mostly storage writes and the token transfer, which have to be applied anyway.

## Where the saving comes from

The saving comes from the health-factor check in every borrow. Before it lends, the Pool reads the oracle price and balance of every asset in the borrower's position. The largest borrow we measured made 10 `getAssetPrice` calls and 24 price-feed reads, about 517,000 gas of reads. A typical borrow makes 2 `getAssetPrice` calls, about 114,000 gas of reads.

- **What gets cheaper:** the health-factor check runs off-chain. On-chain, only the debt, the index updates, the token transfer and the events are applied.
- **What stays the same:** the amount lent, the interest-rate update and every risk rule are exactly what the Pool enforces today.
- **What integration involves:** the Aave V3 Pool would use the Gas Killer SDK. The oracle and token contracts stay as they are.
- **Who gains:** borrowers, and most of all those with several collateral assets.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet borrows from October 2026, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** the Pool has not integrated Gas Killer, so the numbers estimate a future integration.
- **Consistent over time:** 10 borrows we measured in September 2026 saved 15–60%, the same range as now.
- **What saves nothing:** `supply`, `repay` and most `withdraw` calls do little work beyond their storage writes, so they save nothing.
- **Volume:** the Aave V3 Pool received 362 successful borrows in the 8 hours to block 26,199,227, about 1,090 a day. Aave V4's spokes received about 175 direct borrows in the day to block 26,199,586, plus more inside `multicall`.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other Aave transactions or spokes you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

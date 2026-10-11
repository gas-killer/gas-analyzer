# Gas Killer × Kelp: Gas Savings Report

Oct 8, 2026

## Summary

Gas Killer could cut the gas cost of an rsETH `depositETH` by about **81–83%**: roughly 700,000 of the 845,000–880,000 gas each deposit spends today. We measured this on real Ethereum mainnet deposits into Kelp's LRTDepositPool. The saving comes from pricing the pool, which Gas Killer moves off-chain, and it needs no change to how deposits behave.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All four `depositETH` transactions we measured save over 80%, and the results are very consistent.

| Transaction | Function | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0x5927773e…](https://etherscan.io/tx/0x5927773e7bf28ca0228333c07d5913192569982502caa320d594616403ac8581) | `depositETH` | 881,047 | 733,818 | **83.3%** |
| [0x3d793f5e…](https://etherscan.io/tx/0x3d793f5e6caf4cca5325c0a08586f09ea802e7155537c6268cae62a0d9b2fb90) | `depositETH` | 843,973 | 696,720 | **82.6%** |
| [0xed07bc6d…](https://etherscan.io/tx/0xed07bc6df666ead240eee3539cf2fbe198222bd5e77373dee853ed66799b8356) | `depositETH` | 860,124 | 695,771 | **80.9%** |
| [0x1de5a43e…](https://etherscan.io/tx/0x1de5a43e5be3c8909ee49f2b31d7da4fb6e5ae2820db7dcab45c7b54cd975db1) | `depositETH` | 860,124 | 695,771 | **80.9%** |

Savings assume Gas Killer's Schnorr signature scheme.

## Where the saving comes from

The saving comes from how `depositETH` prices the pool. Each deposit loops over the node delegators and strategies to total the pool's assets, and that loop is most of the gas.

- **What gets cheaper:** the pricing loop runs off-chain. On-chain, the deposit only mints rsETH and emits its event.
- **What stays the same:** the rsETH minted, the exchange rate and the deposit event are exactly what the contract would produce today.
- **What integration involves:** one contract, the LRTDepositPool, would use the Gas Killer SDK. The other contracts it reads from stay as they are.
- **Who gains:** every depositor pays less gas, so cheaper deposits are a user-facing improvement.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** the pool has not integrated Gas Killer, so the numbers estimate a future integration.
- **Volume:** we saw about 0.36 direct `depositETH` calls a day in our sample. The total saving grows with deposit volume and gas prices.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other Kelp transactions or contracts you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [gaskiller.xyz](https://gaskiller.xyz)

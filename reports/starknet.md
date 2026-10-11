# Gas Killer × Starknet: Gas Savings Report

Oct 8, 2026

## Summary

Gas Killer could cut the Ethereum gas cost of Starknet's state updates by **54–75%**, about 306,000 gas on an average `updateStateKzgDA`. We measured this on real mainnet calls to the Starknet Core contract. Starknet posts about 41 of these a day, so the saving recurs on every state update.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All four `updateStateKzgDA` calls we measured save over 54%. The on-chain part that remains is a fixed ~58,700 gas, so bigger updates save more gas in absolute terms.

| Transaction | Function | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0xe04e714d…](https://etherscan.io/tx/0xe04e714d933ecadcda0e76dd740aa2d0ad0c0a2e1fb867bd504e56f4424fcc15) | `updateStateKzgDA` | 430,536 | 321,864 | **74.8%** |
| [0xdf208a1e…](https://etherscan.io/tx/0xdf208a1efd933610f34fc9b758676d5d4de344c7dc86d24e4bca9f8d2f9f66a8) | `updateStateKzgDA` | 430,572 | 321,852 | **74.8%** |
| [0xa9c4f70f…](https://etherscan.io/tx/0xa9c4f70f98f57dbef8b36fce15f1b0d056ac46d8b2e0cb6fc898a11c20347c37) | `updateStateKzgDA` | 261,069 | 152,421 | **58.4%** |
| [0x89d7d4a5…](https://etherscan.io/tx/0x89d7d4a50e86de2334bbdafa0ada3abc673aa66d8ac69c02b83763d0a3ce6eb9) | `updateStateKzgDA` | 542,165 | 294,162 | **54.3%** |

Savings assume Gas Killer's Schnorr signature scheme.

At the gas prices we saw when measuring (0.15–0.35 gwei), the saving is about $244 a month across roughly 41 updates a day. At 20 gwei it would be about $18,500 a month.

## Where the saving comes from

The saving comes from processing each state update inside the Starknet Core contract. The function makes no external calls at all, so all of its computation can move off-chain.

- **What gets cheaper:** the work of checking and processing the update runs off-chain. On-chain, only the new state root, block number and messages are written.
- **What stays the same:** the state recorded on Ethereum and the events emitted are exactly what the contract produces today.
- **What integration involves:** one contract, Starknet Core, would use the Gas Killer SDK. No other contracts are involved.
- **Who gains:** the operator that posts state updates pays less for every one.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** Starknet Core has not integrated Gas Killer, so the numbers estimate a future integration.
- **Volume:** the counts come from an exact 7-day window of `updateStateKzgDA` calls, not a sample.
- **Open question:** a change to the core contract goes through Starknet's upgrade process, which we have not assessed.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other Starknet transactions or contracts you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [gaskiller.xyz](https://gaskiller.xyz)

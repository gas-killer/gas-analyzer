# Gas Killer × Polygon zkEVM: Gas Savings Report

Oct 8, 2026

## Summary

Gas Killer could cut the Ethereum gas cost of Polygon zkEVM batch verification by **54–72%**, about 350,000 gas per verification. We measured this on real mainnet calls to the zkEVM RollupManager, on both verification paths. About 49 of these run every day, so the saving recurs on every batch.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All five verifications we measured save over 54%, on both verification paths.

| Transaction | Function | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0x5d45dec8…](https://etherscan.io/tx/0x5d45dec824732cb577de414b76596943f90ac8171f2aa7560abf512deed70045) | batch verification (`0x6c766877`) | 518,713 | 372,236 | **71.8%** |
| [0x23b8eef0…](https://etherscan.io/tx/0x23b8eef0f5c1c42c6c5f84c94944afcbd1d9de147ba5b3a1353ec99a1842d8dd) | batch verification (`0x6c766877`) | 610,721 | 372,224 | **61.0%** |
| [0x287570a8…](https://etherscan.io/tx/0x287570a8c25d027d8bc8caafc22146ca85c4170f4eb6de29f092917b24d606f4) | batch verification (`0x6c766877`) | 699,367 | 404,042 | **57.8%** |
| [0x4796a155…](https://etherscan.io/tx/0x4796a155edc6c34003daedfb132057155cc5f62fe1d4fd50258eb1c6cbf59fb7) | batch verification (`0x6c766877`) | 702,121 | 404,054 | **57.5%** |
| [0x6d0af448…](https://etherscan.io/tx/0x6d0af448a99d15618509dd4802a00ac07be838bdbe12d5e5f08968315bb033c9) | `verifyBatchesTrustedAggregator` | 409,069 | 222,809 | **54.5%** |

Savings assume Gas Killer's Schnorr signature scheme.

At the gas prices we saw when measuring (0.15–0.35 gwei), the saving is about $77 a month across roughly 49 verifications a day. At 20 gwei it would be about $24,800 a month.

## Where the saving comes from

The saving comes from the work of verifying each batch inside the RollupManager. That work is mostly computation, and what it leaves behind is a few storage slots and events.

- **What gets cheaper:** the batch-verification work runs off-chain. On-chain, only the verified batch number, state root and events are written.
- **What stays the same:** the rollup state recorded on Ethereum is exactly what the contract produces today.
- **What integration involves:** the RollupManager would use the Gas Killer SDK. Both verification paths save from that one integration.
- **Who gains:** the aggregator that submits verifications pays less for every batch.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** the RollupManager has not integrated Gas Killer, so the numbers estimate a future integration.
- **Volume:** the counts come from an exact 7-day window of verification calls, not a sample.
- **Open question:** a change to the RollupManager goes through Polygon's upgrade process, which we have not assessed.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other Polygon zkEVM transactions or contracts you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

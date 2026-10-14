# Gas Killer × Polygon Agglayer: Gas Savings Report

Oct 14, 2026

## Summary

Gas Killer could cut the Ethereum gas cost of proof verification in Polygon's Agglayer rollup manager by **54–72%**, about 350,000 gas per verification. We measured this on real mainnet verifications for four chains connected to the Agglayer: X Layer, Katana, Lumia and Silicon. About 49 verifications run every day across the chains on the rollup manager, so the saving recurs on every one.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All five verifications we measured save over 54%, on both verification paths: pessimistic proofs (X Layer, Katana, Lumia) and batch proofs (Silicon).

| Transaction | Chain | Function | Gas today | Gas saved | Saving |
| --- | --- | --- | --: | --: | --: |
| [0x5d45dec8…](https://etherscan.io/tx/0x5d45dec824732cb577de414b76596943f90ac8171f2aa7560abf512deed70045) | Lumia | `verifyPessimisticTrustedAggregator` | 518,713 | 372,236 | **71.8%** |
| [0x23b8eef0…](https://etherscan.io/tx/0x23b8eef0f5c1c42c6c5f84c94944afcbd1d9de147ba5b3a1353ec99a1842d8dd) | X Layer | `verifyPessimisticTrustedAggregator` | 610,721 | 372,224 | **61.0%** |
| [0x287570a8…](https://etherscan.io/tx/0x287570a8c25d027d8bc8caafc22146ca85c4170f4eb6de29f092917b24d606f4) | Katana | `verifyPessimisticTrustedAggregator` | 699,367 | 404,042 | **57.8%** |
| [0x4796a155…](https://etherscan.io/tx/0x4796a155edc6c34003daedfb132057155cc5f62fe1d4fd50258eb1c6cbf59fb7) | Katana | `verifyPessimisticTrustedAggregator` | 702,121 | 404,054 | **57.5%** |
| [0x6d0af448…](https://etherscan.io/tx/0x6d0af448a99d15618509dd4802a00ac07be838bdbe12d5e5f08968315bb033c9) | Silicon | `verifyBatchesTrustedAggregator` | 409,069 | 222,809 | **54.5%** |

Savings assume Gas Killer's Schnorr signature scheme.

## Where the saving comes from

The saving comes from the proof verification inside the rollup manager (`PolygonRollupManager`, `0x5132A183…`), which every Agglayer chain shares. That work is mostly computation, and what it leaves behind is a few storage slots and events.

- **What gets cheaper:** the proof verification runs off-chain. On-chain, only the verified state, batch number and events are written.
- **What stays the same:** the state recorded on Ethereum for each chain is exactly what the contract produces today.
- **What integration involves:** the rollup manager would use the Gas Killer SDK. Both verification paths, and every chain on the Agglayer, save from that one integration.
- **Who gains:** whoever submits the verifications pays less for each one. All five transactions were sent by the same aggregator address (`0x20a53dcb…`) for four different chains.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** the rollup manager has not integrated Gas Killer, so the numbers estimate a future integration.
- **Volume:** the counts come from an exact 7-day window of verification calls, not a sample.
- **Open question:** a change to the rollup manager goes through Polygon's upgrade process, which we have not assessed. Polygon zkEVM itself was shut down on July 1, 2026; none of these transactions are for it.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other Agglayer chains, transactions or contracts you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

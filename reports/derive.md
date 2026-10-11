# Gas Killer × Derive: Gas Savings Report

Oct 11, 2026

## Summary

Gas Killer could cut the Ethereum gas cost of Derive V3's proof submissions by **35–65%**, about 290,000 gas on every submission. We measured this on real mainnet submissions to Derive's VerifiableApp contract in the first days after the V3 launch. At about 145 submissions a day, that is roughly 42 million gas saved every day.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All five proof submissions we measured save between 35% and 65%. The gas saved is nearly constant at about 290,000, so smaller batches save the larger share.

| Transaction | Function | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0xf83f7406…](https://etherscan.io/tx/0xf83f74064b25f3a5b0533d1f1e02a891eca4c91b5d4363b5e4dffed26eb22ebb) | proof submission (`0xcaf85453`) | 442,743 | 288,710 | **65.2%** |
| [0x60ff3054…](https://etherscan.io/tx/0x60ff3054b8337ce75a7e6995c81bb877e6831cfc61e6ad56fe24d65b1e598af2) | proof submission (`0xcaf85453`) | 448,335 | 290,698 | **64.8%** |
| [0xe66eaac6…](https://etherscan.io/tx/0xe66eaac6d541a0e548e35ddc543edcdb5b650fa3ce872ff8a3ed752a4dc3b4c3) | proof submission (`0xcaf85453`) | 448,395 | 290,722 | **64.8%** |
| [0x333c2069…](https://etherscan.io/tx/0x333c2069a02734879b6dd07c6444f778ba2373db7f089e5cab9692424d652cc8) | proof submission (`0xcaf85453`) | 573,819 | 292,750 | **51.0%** |
| [0x8a320a2a…](https://etherscan.io/tx/0x8a320a2a28bc2292c66817374e77889aef59662996702e52731c5db7e30ecfc0) | proof submission (`0xcaf85453`) | 842,171 | 298,986 | **35.5%** |

Savings assume Gas Killer's Schnorr signature scheme.

At ~0.25 gwei and the ETH price at our last measurement ($2,386), the saving is roughly $750 a month. At 20 gwei it would be roughly $60,000 a month. Both figures are approximate.

## Where the saving comes from

The saving comes from the ZK proof check in every submission. The VerifiableApp contract calls a separate verifier, and that check costs about 291,000–298,000 gas no matter how big the batch is.

- **What gets cheaper:** the proof check runs off-chain. On-chain, only the batch's resulting state and payouts are applied.
- **What stays the same:** custody, deposits, withdrawals and the state recorded on Ethereum are exactly what the contracts produce today.
- **What integration involves:** one contract, the VerifiableApp, would use the Gas Killer SDK. The verifier is only read from, so it needs no change.
- **Who gains:** the sequencer that posts proofs pays less on every batch, about 145 times a day.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** Derive has not integrated Gas Killer, so the numbers estimate a future integration.
- **Volume:** 281 proof submissions in the first ~47 hours after launch (blocks 26,135,150–26,149,120), all successful. Early volume may not reflect steady state.
- **Not measured:** deposits and withdrawals, which are mostly token transfers.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other Derive transactions or contracts you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [gaskiller.xyz](https://gaskiller.xyz)

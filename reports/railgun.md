# Gas Killer × Railgun: Gas Savings Report

Oct 13, 2026

## Summary

Gas Killer could cut the gas cost of Railgun's private transfers by **61–79%**, about 72% on a typical transaction. We measured this on 15 real mainnet calls to the Railgun smart wallet, and every one of them saved. Most of the gas goes to ZK proof verification and Poseidon hashing, which is the work Gas Killer moves off-chain.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All 15 direct calls we measured save between 61% and 79%. A sample is below, covering private transfers and shields. The last three rows are RelayAdapt bundles, which save 45–68%.

| Transaction | Function | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0xaa357a48…](https://etherscan.io/tx/0xaa357a4824001aed7173b3bc7d976f997fc7839def4c56ec070db97ec2121bc4) | private transfer / unshield | 1,085,832 | 854,737 | **78.7%** |
| [0xe7e605b6…](https://etherscan.io/tx/0xe7e605b6520aaf757f4531a88af4966866d86fee61ec06e93b3423b30a934adc) | private transfer / unshield | 1,615,755 | 1,247,915 | **77.2%** |
| [0x39a67a29…](https://etherscan.io/tx/0x39a67a2964b675ccd1be1939c513b46f62cae71c3f320bece6acdaf71934b3ae) | private transfer / unshield | 1,131,080 | 814,650 | **72.0%** |
| [0x0376edd3…](https://etherscan.io/tx/0x0376edd36b0c9211b1a38f41e5f1c04233478ca4dcf7d4852ef313deb1c57ade) | `shield` | 724,515 | 505,863 | **69.8%** |
| [0x6837bb8d…](https://etherscan.io/tx/0x6837bb8d629ee0dce5f045acfc6c1e296df4abe1907897820bf621462dc23425) | `shield` | 758,741 | 497,097 | **65.5%** |
| [0x792a8e57…](https://etherscan.io/tx/0x792a8e5783cec82cc456d83f0f1703a2bf71eafa05984a17896809bd952567cc) | private transfer / unshield | 484,303 | 294,581 | **60.8%** |
| [0x6f534f5a…](https://etherscan.io/tx/0x6f534f5af7fa26b66e0c6a5f49505f7125665e88b0d1af23efca86da88109f27) | RelayAdapt bundle | 1,297,603 | 884,393 | **68.2%** |
| [0x7c731150…](https://etherscan.io/tx/0x7c731150234add278ecae3ee9b6bcd35ee50435cc69bc47cce1a008e54c1f2ce) | RelayAdapt bundle | 1,120,780 | 749,350 | **66.9%** |
| [0x2d754e6f…](https://etherscan.io/tx/0x2d754e6f8a34058e5d07596e627cbc70e0c704279e9f745a4c0baef80389cca7) | RelayAdapt bundle | 523,496 | 237,154 | **45.3%** |

Savings assume Gas Killer's Schnorr signature scheme.

Railgun processes roughly 106 of these transactions a day, so the saving recurs on each one.

## Where the saving comes from

The saving comes from proof verification and Poseidon hashing inside the Railgun smart wallet. What a transaction leaves behind is small: commitment and nullifier slots, plus a few token transfers.

- **What gets cheaper:** the ZK verification and hashing run off-chain. On-chain, only the new commitments, nullifiers and transfers are applied.
- **What stays the same:** privacy guarantees, commitments and nullifiers are exactly what the contract produces today.
- **What integration involves:** the smart wallet would use the Gas Killer SDK. RelayAdapt bundles also save, at 45–68%, once RelayAdapt integrates alongside it.
- **Who gains:** users pay less to shield, transfer and unshield, and relayers pay less to submit.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** Railgun has not integrated Gas Killer, so the numbers estimate a future integration. The RelayAdapt figures also include an estimated fixed cost for the two-contract setup.
- **Check it yourself:** you can run any of these transactions through the Gas Killer analyzer at [gaskiller.xyz/analyzer](https://gaskiller.xyz/analyzer).
- **Volume:** in a 40,000-block sample, 53% of successful Railgun transactions went straight to the smart wallet and 39% went through RelayAdapt.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other Railgun transactions or contracts you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

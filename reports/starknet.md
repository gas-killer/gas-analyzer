# Gas Killer × Starknet: Gas Savings Report

Oct 8, 2026 · corrected Oct 21, 2026

## Summary

**Correction.** The first version of this report said Gas Killer could cut the cost of Starknet's `updateStateKzgDA` by 54–75%. That figure counted the proof-fact check and the blob checks as removable. Those checks are what make Starknet a validity rollup, so they have to stay on-chain. With them kept on-chain, Gas Killer saves about **2%** on a typical update and **nothing** on larger or smaller ones. We do not think `updateStateKzgDA` is a good fit for Gas Killer.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

The first column of savings is what the analyzer reported. The second keeps the proof-fact check and the blob checks on-chain.

| Transaction | Blobs | Gas today | Saving as first reported | Proof and blob checks | Saving with checks on-chain |
| --- | --: | --: | --: | --: | --: |
| [0xe04e714d…](https://etherscan.io/tx/0xe04e714d933ecadcda0e76dd740aa2d0ad0c0a2e1fb867bd504e56f4424fcc15) | 6 | 430,536 | 321,864 | 313,071 | **8,793 (2.0%)** |
| [0xdf208a1e…](https://etherscan.io/tx/0xdf208a1efd933610f34fc9b758676d5d4de344c7dc86d24e4bca9f8d2f9f66a8) | 6 | 430,572 | 321,852 | 313,071 | **8,781 (2.0%)** |
| [0xa9c4f70f…](https://etherscan.io/tx/0xa9c4f70f98f57dbef8b36fce15f1b0d056ac46d8b2e0cb6fc898a11c20347c37) | 3 | 261,069 | 152,421 | 163,071 | **0** |
| [0x89d7d4a5…](https://etherscan.io/tx/0x89d7d4a50e86de2334bbdafa0ada3abc673aa66d8ac69c02b83763d0a3ce6eb9) | 6 | 542,165 | 294,162 | 313,071 | **0** |

The last column is our arithmetic from the call traces: the reported saving minus the gas of the checks that must stay. Savings assume Gas Killer's Schnorr signature scheme.

## Where the gas goes

The earlier version said `updateStateKzgDA` makes no external calls. That was wrong. Every update makes:

- **One KZG point-evaluation precompile call per blob**, at a fixed 50,000 gas each: 300,000 gas for a six-blob update. This proves the blob data matches its commitment.
- **One `isValid` call on the fact registry** (13,071 gas), which checks that a STARK proof for this exact output was verified.
- **In some updates, a plain call** sending L1-to-L2 message fees to the fee collector.

Our analyzer counts read-only calls as removable, so it counted both checks as saving. The roughly 58,700 gas that stays on-chain today is the transaction's base cost, the new state root, block number and block hash, and the update's events.

## Why the checks have to stay

- **The fact registry check is the validity proof.** If operators attested to it instead, the new state root and the L1-to-L2 message counters would be backed by operator stake, not by a STARK proof. The Core contract has no delay, so StarkGate withdrawals could use those messages as soon as the update lands, before any fraud proof. The slashable stake would have to cover everything withdrawable.
- **`blobhash` only exists inside the blob transaction.** Operators replaying off-chain cannot read it, so the signed result would have to commit to the versioned hashes and the contract would still compare them on-chain.
- **What is left is small.** With the checks on-chain, only output parsing, hashing and the message loops could move off-chain. That work is smaller than the 50,000-gas signature check, which is why the saving is 2% or nothing.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions, replayed at the block they ran in.
- **Call traces:** the gas of each external call comes from the transaction's own call trace.
- **Check it yourself:** you can run any of these transactions through the Gas Killer analyzer at [gaskiller.xyz/analyzer](https://gaskiller.xyz/analyzer).
- **Thanks:** this correction follows a careful review on the Starknet forum.

## Next steps

We are not proposing a Gas Killer integration for `updateStateKzgDA`. If other Starknet contracts on Ethereum do heavy work that is not part of the proof or data-availability checks, we are happy to run the same analysis on them.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

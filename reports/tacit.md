# Gas Killer × Tacit: Gas Savings Report

Oct 20, 2026

## Summary

Gas Killer could cut the gas cost of a `transact` on the TacitEvmPool by about **47–55%**: roughly 200,000 of the 373,000–418,000 gas each one spends today. We measured this on real Ethereum mainnet transactions to the pool. The saving comes from the ZK proof check, which Gas Killer moves off-chain, and it needs no change to what the pool writes.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All four direct `transact` calls we measured save between 47% and 55%.

| Transaction | Function | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0xd5df306f…](https://etherscan.io/tx/0xd5df306f7533e8416a7cb202adf7a3a168742b95bb379579ad7c9fd9437fc90a) | `transact` | 373,030 | 203,283 | **54.5%** |
| [0x4e4ad66b…](https://etherscan.io/tx/0x4e4ad66b7c18eea384f92fa1254930a347c4719bec8e99be04874fd1957d0072) | `transact` | 375,380 | 203,303 | **54.2%** |
| [0x709868bd…](https://etherscan.io/tx/0x709868bd808bcc6549e93828fbf6dbce24e5b18faa78452fca15c55d797ad4cc) | `transact` | 390,785 | 196,407 | **50.3%** |
| [0x58c6fa26…](https://etherscan.io/tx/0x58c6fa2682fe4d88345524d87d00a3cbf9229c3db09d6cd4de5f35bfdbb519f6) | `transact` | 418,183 | 196,439 | **47.0%** |

The same saving also reaches withdrawals that go through the TacitEvmPoolRouter. A `withdrawAndCall` ([0x0991b9f3…](https://etherscan.io/tx/0x0991b9f32b006d39422c98fd4357a0142c0c613fe60584776b7b6dc5498e47dd), 690,894 gas) saves 172,071 gas (**24.9%**) when the pool, which the router calls, uses Gas Killer. Savings assume Gas Killer's Schnorr signature scheme.

## Where the saving comes from

The saving comes from how `transact` verifies its proof. Each call checks a Groth16 proof in the pool's verifier, which costs 254,510 gas every time and writes nothing.

- **What gets cheaper:** the proof check runs off-chain. On-chain, `transact` only records its nullifiers and commitments and moves funds.
- **What stays the same:** the nullifiers, commitments, transfers and events are exactly what the pool would produce today.
- **What integration involves:** one contract, the TacitEvmPool, would use the Gas Killer SDK. The router benefits without changes to how it calls the pool.
- **Who gains:** every shielded transfer and withdrawal costs its sender less gas.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** the pool has not integrated Gas Killer, so the numbers estimate a future integration.
- **Check it yourself:** you can run any of these transactions through the Gas Killer analyzer at [gaskiller.xyz/analyzer](https://gaskiller.xyz/analyzer).
- **Volume:** we saw 241 direct `transact` calls in the 60 days to October 20, 2026, about 4 a day, plus about 16 router calls. The total saving grows with usage and gas prices.

## Next steps

We would like a 30-minute call to walk through these numbers with you.

1. We run the same analysis on any other Tacit transactions or contracts you choose, including the Base and Robinhood Chain deployments.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

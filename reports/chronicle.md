# Gas Killer × Chronicle: Gas Savings Report

Oct 15, 2026

## Summary

Gas Killer could cut the gas cost of a Scribe price update by **29–32%**, about 36,000–40,000 of the 123,000–127,000 gas each poke spends today. We measured this on real Ethereum mainnet pokes across five feeds in October 2026. Chronicle posts about 90 direct pokes a day across 21 feeds, so the saving recurs on every update.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All five pokes we measured save 29–32%, one from each of five different feeds.

| Transaction | Feed | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0x5335dca0…](https://etherscan.io/tx/0x5335dca0d302a82e1f4f38e574ff3621a3a407f005b3ce1be45794451d0330d3) | WETH/USD | 127,195 | 40,231 | **31.6%** |
| [0xd2509fe2…](https://etherscan.io/tx/0xd2509fe24faf10d4dfc0edf75d7e62c7c7b8746f87e5147153340d9ba2ea0e4c) | WEETH/USD | 126,647 | 39,635 | **31.3%** |
| [0x56d3444a…](https://etherscan.io/tx/0x56d3444a5db83162f70d6c50d0732b8095c176d8039fd1ce17808dd2a1e761ca) | WSTETH/USD | 126,099 | 39,087 | **31.0%** |
| [0xf1d15ffd…](https://etherscan.io/tx/0xf1d15ffdc43b0312df3a75b04ae82fff89072adbd789564bd71d5db5d1a9ced9) | SPK/USD | 125,391 | 38,451 | **30.7%** |
| [0xaf329d56…](https://etherscan.io/tx/0xaf329d56b238f5f3392ebae121a85f6ea576806409adff1f7bcb118f4457901a) | ALETH/ETH | 122,877 | 35,889 | **29.2%** |

All five call `poke_optimized_7136211` (`0x00000082`). Savings assume Gas Killer's Schnorr signature scheme.

## Where the saving comes from

The saving comes from the work each poke does before it writes the price. Writing the new price and its event costs about 37,000 gas on every feed. The other 86,000–90,000 is decoding, the staleness check and Scribe's own signature verification. Gas Killer replaces that work with its own signature check, which costs 50,000 gas, and the difference is the saving.

- **What gets cheaper:** the poke's verification runs off-chain. On-chain, only the new price and its event are written.
- **What stays the same:** the price, age and event each feed records are exactly what Scribe produces today.
- **What integration involves:** each Scribe feed contract would use the Gas Killer SDK. Feeds share the same code, so one change covers all of them.
- **An honest limit:** Scribe is already built to keep verification cheap, so the saving is smaller here than on contracts that verify ZK proofs.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet pokes from October 2026, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of Gas Killer's signature check and the storage write.
- **An estimate:** Scribe has not integrated Gas Killer, so the numbers estimate a future integration.
- **Consistent over time:** 21 direct pokes we measured in September 2026 saved 28.6–32.6%, the same range as now.
- **Batched pokes:** pokes sent together through Multicall3 do not save, because each inner poke is a separate call. The saving applies to direct pokes.
- **Volume:** Scribe feeds received about 90 successful direct pokes across 21 feeds in the day to block 26,192,723.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other Chronicle feeds or contracts you choose.
2. Together we pick one feed for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

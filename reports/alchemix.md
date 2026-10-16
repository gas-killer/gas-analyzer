# Gas Killer × Alchemix: Gas Savings Report

Oct 16, 2026

## Summary

Gas Killer could cut the gas cost of an Alchemix V3 `mint` by **71–85%**, about 630,000–1,360,000 gas on each one. We measured this on real Ethereum mainnet mints on both Alchemists, alETH and alUSD, in October 2026. Almost all of the saving is the work of valuing the Mix-Yield Token across its strategies, which Gas Killer moves off-chain.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All three mints we replayed save over 70%. Mints on the alETH Alchemist cost more today, and save more.

| Transaction | Alchemist | Function | Gas today | Gas saved | Saving |
| --- | --- | --- | --: | --: | --: |
| [0x3ec144c4…](https://etherscan.io/tx/0x3ec144c4bffb5003e90c0a9574342cfc70024227fe20f87c286278c7fdc07fe5) | alETH | `mint` | 1,610,420 | 1,361,342 | **84.5%** |
| [0x0f0f4034…](https://etherscan.io/tx/0x0f0f403441c495f654d65ec69f615790594cf824f09bb31fe30d79f7159580b6) | alETH | `mint` | 1,600,587 | 1,351,497 | **84.4%** |
| [0x216dc160…](https://etherscan.io/tx/0x216dc160c441478483c6e83622d36c8265f842611954b22fecb31a5e80d17de3) | alUSD | `mint` | 881,980 | 627,945 | **71.2%** |

Savings assume Gas Killer's Schnorr signature scheme.

## Where the saving comes from

The saving comes from valuing the Mix-Yield Token before each mint. The Alchemist asks the Mix-Yield Token what a position is worth, and the token asks each of its strategies for its holdings. On the alETH mints we measured, that took seven strategy reads in one call and two more in a second, plus a price read: about 740,000 gas of read-only calls.

- **What gets cheaper:** the valuation runs off-chain. On-chain, only the new debt, the alAsset mint and the events are applied.
- **What stays the same:** the amount minted, the loan-to-value limits and every position rule are exactly what the Alchemist enforces today.
- **What integration involves:** each Alchemist would use the Gas Killer SDK. Both are upgradeable proxies that share one implementation, so one upgrade covers both, with no migration. The strategies stay as they are.
- **Who gains:** every user who mints alUSD or alETH against their deposit.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet mints from October 2026, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** the Alchemists have not integrated Gas Killer, so the numbers estimate a future integration.
- **Check it yourself:** you can run any of these transactions through the Gas Killer analyzer at [gaskiller.xyz/analyzer](https://gaskiller.xyz/analyzer).
- **Not yet measured:** `burn`, deposits through the AlchemistRouter, withdrawals and redemptions. Our replay could not complete a `burn` yet, so we have left it out.
- **Volume:** the two Alchemists had 104 mints, and about 313 transactions in all, in the 21 days to mid-October 2026.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on burns, deposits, withdrawals and redemptions, or any other Alchemix transactions you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

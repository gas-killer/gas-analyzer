# Gas Killer × Usual: Gas Savings Report

Oct 14, 2026

## Summary

Gas Killer could cut the gas cost of USD0 swaps and redemptions by **54–60%**, about 290,000–306,000 gas on each one. We measured this on real Ethereum mainnet transactions to Usual's DaoCollateral contract. Almost all of the saving is the collateral-backing check that runs inside every USD0 mint. DaoCollateral handles about 25 of these transactions a day, so the saving recurs on each one.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All five transactions we measured save over 53%. Swaps into USD0 save 60%; redemptions save 54%.

| Transaction | Function | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0xbc30d150…](https://etherscan.io/tx/0xbc30d150f39f43aac3125ceb96ec686d74ad5d765be3b6f446ec88e780e11a96) | `swap` | 509,290 | 306,446 | **60.2%** |
| [0x52d20bbb…](https://etherscan.io/tx/0x52d20bbb3f6415c82ac700dae1e47be4f8ce39f1f9276da283288c1a1b9189ca) | `swap` | 509,290 | 306,458 | **60.2%** |
| [0x59165947…](https://etherscan.io/tx/0x59165947367f4058061e7a7fba4edcd3182936a10efa6a5abd462a424ae242c3) | `swap` | 509,100 | 306,256 | **60.2%** |
| [0x894cba84…](https://etherscan.io/tx/0x894cba8428d835591f35d02f6c9027f62482c3b55e2b05ea44decfd3567761bb) | `redeem` | 536,673 | 289,521 | **54.0%** |
| [0xf0ea0b60…](https://etherscan.io/tx/0xf0ea0b601264eb1801d592d697005cf928a4a700b42a7e5c1f78fcb7b29b1e2c) | `redeem` | 536,388 | 289,236 | **53.9%** |

Savings assume Gas Killer's Schnorr signature scheme.

## Where the saving comes from

The saving comes from the collateral check inside every USD0 mint. When DaoCollateral mints USD0, the Usd0 contract confirms the collateral still backs the supply. It reads the oracle price of each collateral token, plus its balance and decimals. On a redemption, those reads cost about 300,000 of the mint's 363,000 gas, and seven `getPrice` calls alone cost 231,000.

- **What gets cheaper:** the backing check runs off-chain. On-chain, only the mint, the burn, the token transfers and their events are applied.
- **What stays the same:** the amounts minted, burned and paid out, and the backing rule itself, are exactly what the contracts enforce today.
- **What integration involves:** DaoCollateral and Usd0 would both use the Gas Killer SDK, since the check sits in the second contract. Both are Usual's own contracts.
- **Who gains:** every user who swaps into USD0 or redeems out of it.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions from October 2026, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** these contracts have not integrated Gas Killer, so the numbers estimate a future integration. The fixed costs of a two-contract setup are included as a fixed estimate.
- **Check it yourself:** you can run any of these transactions through the Gas Killer analyzer at [gaskiller.xyz/analyzer](https://gaskiller.xyz/analyzer).
- **Volume:** DaoCollateral handled 74 successful transactions in the 3 days to block 26,190,861: 54 redemptions and 20 swaps.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other Usual transactions or contracts you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

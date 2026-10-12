# Gas Killer × Securitize: Gas Savings Report

Oct 8, 2026

## Summary

Gas Killer could cut the gas cost of large BUIDL transfers by up to **88%**: about 1,006,000 of the 1,140,000 gas a single transfer spends today. We measured this on real Ethereum mainnet BUIDL transfers. Almost all of that gas goes to compliance checks in Securitize's compliance-service contract, and that is the work Gas Killer moves off-chain.

## What is Gas Killer

Gas Killer moves a contract's heavy computation off-chain without changing what the contract does.

1. A network of operators runs your function off-chain, against the same chain state it would see on-chain.
2. They sign the result: the exact storage changes the function would make.
3. Your contract checks one signature and writes those changes. The computation itself never runs on-chain.

You pay for the signature check and the storage writes, not the computation. If operators sign a wrong result, it can be proven wrong and they are slashed.

## What we measured

All five BUIDL transfers we could measure save gas, from 19% to 88%. Bigger transfers save more, because they spend more on compliance checks.

| Transaction | Function | Gas today | Gas saved | Saving |
| --- | --- | --: | --: | --: |
| [0x2f838199…](https://etherscan.io/tx/0x2f83819933c4a9d645f00d51e738b25a4181f6e49e9f0ed42f8372c2ecd6cb8c) | BUIDL `transfer` | 1,139,537 | 1,006,206 | **88.3%** |
| [0x8457c64c…](https://etherscan.io/tx/0x8457c64cf0289168cbb7a1095d3ceeef47883b59a9384259c72128b2f13bffa6) | BUIDL `transfer` | 1,006,353 | 864,643 | **85.9%** |
| [0x73e61f0f…](https://etherscan.io/tx/0x73e61f0feed41e55104d8d43f98074c2fdde26f096060790e5c7ab394c0a6acb) | BUIDL `transfer` | 247,639 | 47,694 | **19.3%** |
| [0xb1ae64fe…](https://etherscan.io/tx/0xb1ae64fe8f4c859d3935a84abbd48837b738ce29a9882fcd05bb7c435c7872d4) | BUIDL `transfer` | 223,465 | 48,187 | **21.6%** |
| [0x6c124186…](https://etherscan.io/tx/0x6c124186b7dad8c99db0278fdab0d059b042130ef705896d1bd8e7006d3476c8) | BUIDL `transfer` | 194,060 | 47,193 | **24.3%** |

Savings assume Gas Killer's Schnorr signature scheme.

## Where the saving comes from

The saving comes from the compliance check in every BUIDL transfer. The BUIDL token calls Securitize's compliance-service contract, and that call accounts for up to 1,060,000 of a transfer's 1,140,000 gas.

- **What gets cheaper:** the compliance rules run off-chain. On-chain, only their result is applied.
- **What stays the same:** balances, events and transfer rules behave exactly as today.
- **What integration involves:** both the BUIDL token and the compliance service would use the Gas Killer SDK, since the saving sits in the second contract. Both are Securitize's own contracts.
- **Likely reach:** the compliance service is part of Securitize's DS Protocol, so other DS tokens may save the same way. We have only measured BUIDL.

## How we measured

We replayed each transaction as Gas Killer would settle it and compared the gas against what it actually cost.

- **Real data:** real mainnet transactions, replayed at the block they ran in.
- **Includes all costs:** the gas saved is net of the signature check and every storage write.
- **An estimate:** these contracts have not integrated Gas Killer, so the numbers estimate a future integration. The fixed costs of a two-contract setup are included as a fixed estimate.
- **Volume:** BUIDL averaged about 1.8 transfers a day when we sampled it, so the total saving depends on transfer volume.

## Next steps

We would like a 30-minute call to walk through these numbers with your engineering team.

1. We run the same analysis on any other Securitize transactions or tokens you choose.
2. Together we pick one contract path for a testnet pilot.
3. You see the gas difference on your own transactions before committing to anything.

Contact: [contact@gaskiller.xyz](mailto:contact@gaskiller.xyz)

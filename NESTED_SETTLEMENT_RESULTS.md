# Nested settlement: re-running the call-blocked transactions

First run 2026-10-07/08 on branch `nested-settlement` (`889de30`). **Re-run 2026-10-12/13 on
`main` (`8aed5cf`)** after the nested-settlement branch was merged with fixes for most of the
failures in `NESTED_SETTLEMENT_FAILURES.md`. Every number below is from the re-run. The last
column shows the first run's result.

Both runs used `cargo run -- t <tx> --owned <addrs>`.

## What was run

The starting set was every transaction in `ALL_TRANSACTION_ANALYSES.md` that scored 0%: 355 transactions.

- **181** spent at least 60k gas inside regular `CALL`s to non-token contracts.
- **128** were run:
  - 115 from that set. Their callee belongs to the same protocol as the contract the
    transaction was sent to, and only those callees were passed as `--owned`. Contracts were
    identified through Sourcify names.
  - 13 hand-picked test transactions: Privacy Pools relays and deposits, Railgun RelayAdapt,
    the Morpho flash loan, Chainlink and Null.
- **54** were skipped because the callee is a third party: bots calling Morpho Blue, Safe
  wallets, ERC-4337 / ZeroDev wallets, CoW Swap, 1inch, Multicall3, EAS resolvers, UMA, and
  Null calling a curator's vault. Integration there depends on someone other than the
  protocol.

## What changed in the re-run

- **6 transactions that failed before now save:**
  - Railgun RelayAdapt ×2: 68.16% and 66.86%. Before, they failed on "gas price too low".
  - Privacy Pools USDT relay: 30.01%. PPRouter deposit: 12.97%. Before, both failed because
    frames were priced in isolation.
  - World ID: 51.33%. Before, it failed on an RPC error.
  - Securitize: 27.44%. Before, it timed out.
- **Small drops on everything that already saved, from more accurate accounting.** Frames are now
  priced in execution order on one shared state, so later work pays for the storage it
  actually touches.
  - Most transactions lost exactly 2,150 gas of saving: World ID, Securitize, and the
    Privacy Pools `ragequit` and relays. That is under 1.2 points each.
  - Usual lost 16,729 gas per transaction, 56.09% → 52.90% at best.
  - Linea and Chainlink did not change.
- **Railgun RelayAdapt `0x2d754e6f…` dropped from 54.64% to 45.30%.** The old version left
  out an operation RelayAdapt runs after the smart-wallet call returns. The new version keeps it
  in RelayAdapt's own frame, and that frame went from 61,495 to 110,356 gas. The dev added a
  test for this case (`5b02b4c`), so 45.30% is the correct figure.
- **Doppler `create`, Ondo instant mint and Grove** no longer fail, but they measure 0%.

## Results: 0% → savings

31 transactions save gas.

| protocol | tx | owned callee | gas used | saved (Schnorr) | % | before (`889de30`) |
|---|---|---|---:|---:|---:|---:|
| Securitize | [`0x2f838199…`](https://etherscan.io/tx/0x2f83819933c4a9d645f00d51e738b25a4181f6e49e9f0ed42f8372c2ecd6cb8c) | compliance service | 1,139,537 | 1,004,056 | **88.11%** | 88.30% |
| Securitize | [`0x8457c64c…`](https://etherscan.io/tx/0x8457c64cf0289168cbb7a1095d3ceeef47883b59a9384259c72128b2f13bffa6) | compliance service | 1,006,353 | 862,493 | **85.70%** | 85.92% |
| Railgun | [`0x6f534f5a…`](https://etherscan.io/tx/0x6f534f5af7fa26b66e0c6a5f49505f7125665e88b0d1af23efca86da88109f27) | RailgunSmartWallet (RelayAdapt) | 1,297,603 | 884,393 | **68.16%** | failed: gas price |
| Railgun | [`0x7c731150…`](https://etherscan.io/tx/0x7c731150234add278ecae3ee9b6bcd35ee50435cc69bc47cce1a008e54c1f2ce) | RailgunSmartWallet (RelayAdapt) | 1,120,780 | 749,350 | **66.86%** | failed: gas price |
| Linea | [`0x4546019a…`](https://etherscan.io/tx/0x4546019a7733a24736b222e6fc45bfceac2a2c1d8caae4ea99b696dd8d254858) | PlonkVerifierFull | 427,765 | 232,706 | **54.40%** | 54.40% |
| World ID | [`0xa447c2d3…`](https://etherscan.io/tx/0xa447c2d3d0786a32f8b23c0f571e714e91d4d812b575d7bee27864c7c3e8c556) | Verifier | 298,629 | 161,088 | **53.94%** | 54.66% |
| Usual | [`0x54164425…`](https://etherscan.io/tx/0x541644256d45b8418ad9a2312a71d81a89da806f514fcf2cc768e26e332f3ca0) | Usd0 | 524,715 | 277,563 | **52.90%** | 56.09% |
| Usual | [`0x22f4b076…`](https://etherscan.io/tx/0x22f4b076bdeb8a081fbedcb1fd6875f5bed16c30d5d2d0d695c30a6dd65ccc0c) | Usd0 | 522,184 | 275,032 | **52.67%** | 55.87% |
| World ID | [`0x36c09544…`](https://etherscan.io/tx/0x36c095445eb96f2ccaa2a2ec9544ac2cf72aa524c36c0ce23c0aecc2cf36b8b7) | Verifier | 285,261 | 147,708 | **51.78%** | 52.53% |
| Usual | [`0xddb1eaa6…`](https://etherscan.io/tx/0xddb1eaa6346cceece4a46a1e901d896945380a47a7a240d01e81f184b30d0cb1) | Usd0 | 536,239 | 275,559 | **51.39%** | 54.51% |
| World ID | [`0x6b2fb8d3…`](https://etherscan.io/tx/0x6b2fb8d32c1fc927e5c37ae0ca52d17cb122b949ec0140f75a8212164911f494) | Verifier | 282,573 | 145,032 | **51.33%** | failed: RPC |
| World ID | [`0xb2f5ba58…`](https://etherscan.io/tx/0xb2f5ba588077025662acd44f62ead62c4dc6da4faa30890d658542aedcaef3c5) | Verifier | 281,457 | 143,928 | **51.14%** | 51.90% |
| World ID | [`0xcd404a27…`](https://etherscan.io/tx/0xcd404a27462a9e60fdd5a17c024d758d809f860ad2da9f1709882d497276375a) | Verifier | 281,445 | 143,892 | **51.13%** | 51.89% |
| Privacy Pools | [`0xb120146b…`](https://etherscan.io/tx/0xb120146b4dd84f30f7c44cfb6f9fb5fca7c0b10f051a6259edd5c5c7b40d9da6) | CommitmentVerifier (`ragequit`) | 279,732 | 139,350 | **49.82%** | 50.58% |
| World ID | [`0x04ca8194…`](https://etherscan.io/tx/0x04ca81943592e11ddbce6e4fac96c0f84debb12c8d972bd3f910dc8bf77274de) | Verifier | 271,876 | 134,311 | **49.40%** | 50.19% |
| World ID | [`0x2fee0848…`](https://etherscan.io/tx/0x2fee084888a10a8cf80c30b36bf511e8ba499e517d49dbc0ca2a97d4c4e160e6) | Verifier | 271,816 | 134,251 | **49.39%** | 50.18% |
| Privacy Pools | [`0x15082298…`](https://etherscan.io/tx/0x150822981204592e4cfa340ba2e63e607a1c6ded490b988f9a8bd37c1f2b46d0) | pool + verifier (relay) | 620,208 | 281,144 | **45.33%** | 46.14% |
| Railgun | [`0x2d754e6f…`](https://etherscan.io/tx/0x2d754e6f8a34058e5d07596e627cbc70e0c704279e9f745a4c0baef80389cca7) | RailgunSmartWallet | 523,496 | 237,154 | **45.30%** | 54.64% |
| Privacy Pools | [`0xad4ac41d…`](https://etherscan.io/tx/0xad4ac41d7ad3ba9792d7c426631dba0d46a31e271f5105dbb6aa6df349c891a5) | pool + verifier (relay) | 604,233 | 257,055 | **42.54%** | 43.37% |
| Privacy Pools | [`0x03ebad9a…`](https://etherscan.io/tx/0x03ebad9a10bc3dc5ad36613de80975b7ee8061d7fa74367f1a9aa04e77cc1524) | pool + verifier (relay) | 604,245 | 257,019 | **42.54%** | 43.36% |
| Privacy Pools | [`0x4d8f00ee…`](https://etherscan.io/tx/0x4d8f00ee277c67f95049a43dfe604418d0a408fed40a6473bd5b154045c2e2e2) | pool + verifier (relay) | 577,042 | 174,365 | **30.22%** | 32.31% |
| Privacy Pools | [`0x2766c992…`](https://etherscan.io/tx/0x2766c992f22f5aec9bdfc1f16e394d4c4ed6b996bd002f64235b5234e9269cd2) | pool + verifier (USDT relay) | 558,846 | 167,700 | **30.01%** | failed: isolation |
| Securitize | [`0xe6a62fbc…`](https://etherscan.io/tx/0xe6a62fbc8f5ba4f8404ac6df89e1c2f0ec0a3e090bc84f8d217a083793036fe2) | compliance service | 183,672 | 50,401 | **27.44%** | timed out |
| Securitize | [`0x6c124186…`](https://etherscan.io/tx/0x6c124186b7dad8c99db0278fdab0d059b042130ef705896d1bd8e7006d3476c8) | compliance service | 194,060 | 45,043 | **23.21%** | 24.32% |
| Securitize | [`0xb1ae64fe…`](https://etherscan.io/tx/0xb1ae64fe8f4c859d3935a84abbd48837b738ce29a9882fcd05bb7c435c7872d4) | compliance service | 223,465 | 46,037 | **20.60%** | 21.56% |
| Securitize | [`0x73e61f0f…`](https://etherscan.io/tx/0x73e61f0feed41e55104d8d43f98074c2fdde26f096060790e5c7ab394c0a6acb) | compliance service | 247,639 | 45,544 | **18.39%** | 19.26% |
| Privacy Pools | [`0x46a8ff4b…`](https://etherscan.io/tx/0x46a8ff4b10a52df709860a908f1749cd66523b5e6a3b18e7eeb901f8b7cd97eb) | pool (Entrypoint deposit) | 381,540 | 55,660 | **14.59%** | 17.17% |
| Privacy Pools | [`0x1a18f555…`](https://etherscan.io/tx/0x1a18f5556a44d6c405964019ff6a1ea6020d155777303407240269ecd65ad95a) | Entrypoint + PoolVault (PPRouter deposit) | 872,799 | 113,227 | **12.97%** | failed: isolation |
| Chainlink | [`0x6fc803ec…`](https://etherscan.io/tx/0x6fc803ecc426f8d20c9bcbdcc6c8118a1d8f22395a9702a7145fd7586adf4d80) | DualAggregator | 183,165 | 13,088 | 7.15% | 7.15% |
| Chainlink | [`0x0937e5c9…`](https://etherscan.io/tx/0x0937e5c9c7070b119608b28b836efc1c435ce7327f78ff00fff7fcc9ac1eef5f) | OCR2Aggregator | 182,433 | 11,612 | 6.37% | 6.37% |
| Privacy Pools | [`0x53e6375e…`](https://etherscan.io/tx/0x53e6375e0156f40d6917e8d48d26f0af6e1fd197d54d0042b96138dfc449660f) | pool (Entrypoint deposit) | 393,615 | 6,979 | 1.77% | 4.28% |

### Takeaways

- **Securitize is still the biggest change in the survey.** A BUIDL `transfer` spends about 1.06M
  gas in Securitize's own compliance-service contract. Before nesting, it scored 0% because
  only 3,777 gas could be removed. Nested, the large transfers save 86–88%. All six measured
  Securitize transactions save.
- **Railgun RelayAdapt is now the second-best result.** The two large RelayAdapt bundles save
  67–68%, close to the 61–79% Railgun already measured on direct calls.
- **Separate proof verifiers now count.**
  - World ID (7 of 7 measured, 49–54%), Linea's second verify path (54%) and the Privacy Pools
    verifier all keep the proof in its own contract.
  - The old analyzer could not remove a verifier that sits behind a `CALL`. The new version can.
- **Usual `DaoCollateral` → `Usd0` mint** goes from 0% to 51–53%.
- **Every main Privacy Pools path now saves something.** Relays save 30–45% in ETH or USDT,
  `ragequit` saves 50%, and deposits save 2–15%.

## Still 0% (measured, nothing to nest)

The tool reports the callee as "cheaper as a plain CALL" in every one of these. The callee's
own work is mostly storage writes, logs and token moves, which a settlement has to apply
anyway. Nesting adds overhead without removing anything.

Aave (Pool → aToken), Aragon (→ DAO), Centrifuge, Chainlink (6 of 8), Doppler (all 13),
EigenLayer, ENS, Ether.fi, Euler (EVC → EVaults), Frax (router → pair), Grove, Lido, Maple
(Pool → PoolManager), Morpho (VaultV2 → adapters, reallocations), Null (→ its own adapter),
Ondo, Panther, Pendle (router → markets), Swell.

## Did not produce a result

| reason | txs |
|---|---|
| Morpho Bundler3: `GeneralAdapter1` reverts with "transfer amount exceeds allowance". Still failing after the execution-order fix. | Morpho [`0xdc74e020…`](https://etherscan.io/tx/0xdc74e020e296fbb968edfc2ffd630bad47d557c71dabe901938315be6329c5c9) |
| Morpho flash-loan callback (`0x7e5f4b61`) | Morpho [`0x16a0a31c…`](https://etherscan.io/tx/0x16a0a31c0547f2f35018c38f0c2fa3bdcf1320e6a75f998caaa957747e9dc568) |

## Who to contact

Only the protocols where nesting produced a saving are listed, ordered by result. ✅ means the
channel was seen on the organisation's own site or code. ⚠️ means it came from search or a
summary and still needs checking in a browser. Contacts for Railgun, Linea and Privacy Pools
are repeated from `OUTREACH_CONTACTS.md`.

### Securitize: 86–88% on BUIDL transfers
- **Who:** Securitize, Inc., the tokenization and transfer-agent platform behind BlackRock's BUIDL.
  The saving is in their own compliance-service contract, so this is a conversation with
  Securitize, not BlackRock.
- **Email:** `info@securitize.io` ✅ (in the site's own code). The site also has a contact form
  at `securitize.io/contact-form` and a `partner-ecosystem` page ✅. Both are routes inside the
  app; open them in a browser.
- **X** [`@Securitize`](https://twitter.com/Securitize) ✅, **LinkedIn**
  <https://www.linkedin.com/company/securitize/> ✅.
- **Developer docs:** <https://sec-connect-api-docs.securitize.io/> ⚠️
- **Pitch:** every transfer spends about 1M gas on compliance checks. Nested settlement removes
  86–88% of it.

### Usual: ~52% on USD0 minting
- **Who:** Usual Labs (France) ⚠️. Governance runs through the Usual DAO.
- **Contact form** <https://usual.money/form/contact-us> ✅
- **X** [`@usualmoney`](https://x.com/usualmoney) ✅, **LinkedIn**
  <https://www.linkedin.com/company/usualmoney> ✅
- **Discord** <https://discord.usual.money> ✅, Snapshot governance `usualmoney.eth` ✅,
  docs <https://docs.usual.money> ✅
- No governance forum or partnerships email found.

### World ID: 49–54% on `registerIdentities`
- **Who:** Tools for Humanity (main contributor) and the World Foundation
  (<https://foundation.world.org>).
- **Developers** <https://world.org/developers> ✅, docs <https://docs.world.org> ✅,
  GitHub <https://github.com/worldcoin> ✅. The identity-manager contracts live there, so a
  technical issue or discussion is a reasonable first contact.
- **X** [`@worldnetwork`](https://x.com/worldnetwork) ✅. Enterprise page
  <https://world.org/solutions> ✅, grants <https://world.org/grants> ✅.
- No Discord or BD email found.
- **Pitch:** the saving is entirely in the Groth16 verifier. Under the old analyzer this
  scored 0% only because the verifier sits behind a `CALL`.

### Railgun: 45–68% on RelayAdapt
- **Governance** <https://governance.railgun.org>. Posting needs staked RAIL, so find a delegate.
- **Telegram** `@railgunproject`, **GitHub** <https://github.com/Railgun-Community>. No email.
- Railgun already measured 78.72% on direct `transact` calls. This adds the RelayAdapt path,
  which used to score 0%.

### Linea: 54% on the second proof path
- **Contact:** "Speak to an expert" form at <https://linea.build/contact/inquiries> ✅
- Linea's direct path already measured 54.54%. Nested settlement brings its other verify path,
  which sends the proof to `PlonkVerifierFull`, up to the same level.

### Privacy Pools (0xbow): 2–50% across relays, deposits and `ragequit`
- **Who:** 0xbow <https://0xbow.io>. GitHub <https://github.com/0xbow-io/privacy-pools-core> ✅,
  docs <https://docs.privacypools.com> ✅.
- **X** `@0xbowio` ⚠️. 0xbow.io has a "Contact Us" item that needs a browser.
- **Pitch:** the Entrypoint relay used to score 0% and now saves 30–45%, in ETH or USDT.
  Combined with the 59.59% already measured on the pool, every main path now saves. Deposits
  save the least, 2–15%.

### Chainlink: 6–7% (low priority)
- **Contact page** <https://chain.link/contact> ✅ (loads; form contents not checked).
- Only 2 of 6 price updates save, and only slightly. Not worth leading with.

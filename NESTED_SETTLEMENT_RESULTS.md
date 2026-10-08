# Nested settlement: re-running the call-blocked transactions

Run 2026-10-07/08 with branch `nested-settlement` (`889de30`) and
`cargo run -- t <tx> --owned <addrs>`. Failures are written up in
`NESTED_SETTLEMENT_FAILURES.md`.

## What was run

The starting set was every transaction in `ALL_TRANSACTION_ANALYSES.md` that scored 0%: 355 transactions.

- **181** spent at least 60k gas inside regular `CALL`s to non-token contracts.
- **127** of those were run. Their callee belongs to the same protocol as the contract the
  transaction was sent to, and only those callees were passed as `--owned`. Contracts were
  identified through Sourcify names.
- **54** were skipped because the callee is a third party: bots calling Morpho Blue, Safe
  wallets, ERC-4337 / ZeroDev wallets, CoW Swap, 1inch, Multicall3, EAS resolvers, UMA, and
  Null calling a curator's vault. Integration there depends on someone other than the
  protocol.
- **3** Doppler `create` transactions (6.9M–14.6M gas) were not finished before the run's time limit.

## Results: 0% → savings

| protocol | tx | owned callee | gas used | saved (Schnorr) | % | BLS % |
|---|---|---|---:|---:|---:|---:|
| Securitize | [`0x2f838199…`](https://etherscan.io/tx/0x2f83819933c4a9d645f00d51e738b25a4181f6e49e9f0ed42f8372c2ecd6cb8c) | compliance service | 1,139,537 | 1,006,206 | **88.30%** | 70.75% |
| Securitize | [`0x8457c64c…`](https://etherscan.io/tx/0x8457c64cf0289168cbb7a1095d3ceeef47883b59a9384259c72128b2f13bffa6) | compliance service | 1,006,353 | 864,643 | **85.92%** | 66.04% |
| Usual | [`0x54164425…`](https://etherscan.io/tx/0x541644256d45b8418ad9a2312a71d81a89da806f514fcf2cc768e26e332f3ca0) | Usd0 | 524,715 | 294,292 | **56.09%** | 17.97% |
| Usual | [`0x22f4b076…`](https://etherscan.io/tx/0x22f4b076bdeb8a081fbedcb1fd6875f5bed16c30d5d2d0d695c30a6dd65ccc0c) | Usd0 | 522,184 | 291,761 | **55.87%** | 17.57% |
| World ID | [`0xa447c2d3…`](https://etherscan.io/tx/0xa447c2d3d0786a32f8b23c0f571e714e91d4d812b575d7bee27864c7c3e8c556) | Verifier | 298,629 | 163,238 | **54.66%** | 0% |
| Railgun | [`0x2d754e6f…`](https://etherscan.io/tx/0x2d754e6f8a34058e5d07596e627cbc70e0c704279e9f745a4c0baef80389cca7) | RailgunSmartWallet | 523,496 | 286,015 | **54.64%** | 16.43% |
| Usual | [`0xddb1eaa6…`](https://etherscan.io/tx/0xddb1eaa6346cceece4a46a1e901d896945380a47a7a240d01e81f184b30d0cb1) | Usd0 | 536,239 | 292,288 | **54.51%** | 17.21% |
| Linea | [`0x4546019a…`](https://etherscan.io/tx/0x4546019a7733a24736b222e6fc45bfceac2a2c1d8caae4ea99b696dd8d254858) | PlonkVerifierFull | 427,765 | 232,706 | **54.40%** | 7.65% |
| World ID | [`0x36c09544…`](https://etherscan.io/tx/0x36c095445eb96f2ccaa2a2ec9544ac2cf72aa524c36c0ce23c0aecc2cf36b8b7) | Verifier | 285,261 | 149,858 | **52.53%** | 0% |
| World ID | [`0xb2f5ba58…`](https://etherscan.io/tx/0xb2f5ba588077025662acd44f62ead62c4dc6da4faa30890d658542aedcaef3c5) | Verifier | 281,457 | 146,078 | **51.90%** | 0% |
| World ID | [`0xcd404a27…`](https://etherscan.io/tx/0xcd404a27462a9e60fdd5a17c024d758d809f860ad2da9f1709882d497276375a) | Verifier | 281,445 | 146,042 | **51.89%** | 0% |
| Privacy Pools | [`0xb120146b…`](https://etherscan.io/tx/0xb120146b4dd84f30f7c44cfb6f9fb5fca7c0b10f051a6259edd5c5c7b40d9da6) | CommitmentVerifier (`ragequit`) | 279,732 | 141,500 | **50.58%** | 0% |
| World ID | [`0x04ca8194…`](https://etherscan.io/tx/0x04ca81943592e11ddbce6e4fac96c0f84debb12c8d972bd3f910dc8bf77274de) | Verifier | 271,876 | 136,461 | **50.19%** | 0% |
| World ID | [`0x2fee0848…`](https://etherscan.io/tx/0x2fee084888a10a8cf80c30b36bf511e8ba499e517d49dbc0ca2a97d4c4e160e6) | Verifier | 271,816 | 136,401 | **50.18%** | 0% |
| Privacy Pools | [`0x15082298…`](https://etherscan.io/tx/0x150822981204592e4cfa340ba2e63e607a1c6ded490b988f9a8bd37c1f2b46d0) | pool + verifier (relay) | 620,208 | 286,147 | **46.14%** | 13.89% |
| Privacy Pools | [`0xad4ac41d…`](https://etherscan.io/tx/0xad4ac41d7ad3ba9792d7c426631dba0d46a31e271f5105dbb6aa6df349c891a5) | pool + verifier (relay) | 604,233 | 262,058 | **43.37%** | 10.27% |
| Privacy Pools | [`0x03ebad9a…`](https://etherscan.io/tx/0x03ebad9a10bc3dc5ad36613de80975b7ee8061d7fa74367f1a9aa04e77cc1524) | pool + verifier (relay) | 604,245 | 262,022 | **43.36%** | 10.26% |
| Privacy Pools | [`0x4d8f00ee…`](https://etherscan.io/tx/0x4d8f00ee277c67f95049a43dfe604418d0a408fed40a6473bd5b154045c2e2e2) | pool + verifier (relay) | 577,042 | 186,415 | **32.31%** | 0% |
| Securitize | [`0x6c124186…`](https://etherscan.io/tx/0x6c124186b7dad8c99db0278fdab0d059b042130ef705896d1bd8e7006d3476c8) | compliance service | 194,060 | 47,193 | **24.32%** | 0% |
| Securitize | [`0xb1ae64fe…`](https://etherscan.io/tx/0xb1ae64fe8f4c859d3935a84abbd48837b738ce29a9882fcd05bb7c435c7872d4) | compliance service | 223,465 | 48,187 | **21.56%** | 0% |
| Securitize | [`0x73e61f0f…`](https://etherscan.io/tx/0x73e61f0feed41e55104d8d43f98074c2fdde26f096060790e5c7ab394c0a6acb) | compliance service | 247,639 | 47,694 | **19.26%** | 0% |
| Privacy Pools | [`0x46a8ff4b…`](https://etherscan.io/tx/0x46a8ff4b10a52df709860a908f1749cd66523b5e6a3b18e7eeb901f8b7cd97eb) | pool (Entrypoint deposit) | 381,540 | 65,524 | **17.17%** | 0% |
| Chainlink | [`0x6fc803ec…`](https://etherscan.io/tx/0x6fc803ecc426f8d20c9bcbdcc6c8118a1d8f22395a9702a7145fd7586adf4d80) | DualAggregator | 183,165 | 13,088 | 7.15% | 0% |
| Chainlink | [`0x0937e5c9…`](https://etherscan.io/tx/0x0937e5c9c7070b119608b28b836efc1c435ce7327f78ff00fff7fcc9ac1eef5f) | OCR2Aggregator | 182,433 | 11,612 | 6.37% | 0% |
| Privacy Pools | [`0x53e6375e…`](https://etherscan.io/tx/0x53e6375e0156f40d6917e8d48d26f0af6e1fd197d54d0042b96138dfc449660f) | pool (Entrypoint deposit) | 393,615 | 16,842 | 4.28% | 0% |
| Lido | [`0xf9a0c484…`](https://etherscan.io/tx/0xf9a0c48463e92a01347aadfefbf9349ec72858550a8fa162e894f61e9e99a499) | — | 1,729,395 | 940 | 0.05% | 0% |
| Lido | [`0x1bad3438…`](https://etherscan.io/tx/0x1bad343834044681f393485bcf131863801ff082da4fe24a2095629a7332d517) | — | 1,721,688 | 456 | 0.03% | 0% |

### Takeaways

- **Securitize is the biggest change in the survey.** A BUIDL `transfer` spends about 1.06M gas
  in Securitize's own compliance-service contract. Before, it scored 0% because only 3,777 gas
  could be removed. Nested, the large transfers save 86–88% and still clear the BLS floor.
  Every measured Securitize transaction now saves.
- **Separate proof verifiers now count.**
  - World ID (6 of 6 measured, 50–55%), Linea's second verify path (54%) and the Privacy Pools
    verifier all keep the proof in its own contract.
  - The old analyzer could not remove a verifier that sits behind a `CALL`. The new version can.
- **Usual `DaoCollateral` → `Usd0` mint** goes from 0% to about 55%.
- **Wrappers in front of the real contract now work.** Railgun's RelayAdapt and the Privacy
  Pools Entrypoint relay save 32–55%, where both scored 0% before.

## Still 0% (measured, nothing to nest)

The tool reports the callee as "cheaper as a plain CALL" in every one of these. The callee's
own work is mostly storage writes, logs and token moves, which a settlement has to apply
anyway. Nesting adds overhead without removing anything.

Aave (Pool → aToken), Aragon (→ DAO), Centrifuge, EigenLayer, Ether.fi, ENS, Euler (EVC →
EVaults), Frax (router → pair), Grove, Maple (Pool → PoolManager), Morpho (VaultV2 → adapters,
reallocations), Ondo (1 tx), Panther, Pendle (router → markets), Swell, Chainlink (4 of 6),
Doppler (3 of 13).

## Did not produce a result

| reason | txs |
|---|---|
| a replayed call reverted because frames are priced in isolation (see failures doc) | Ondo ×2, Grove ×1, Morpho Bundler3 ×1, Privacy Pools USDT relay, PPRouter deposit |
| Doppler `create`: the token factory's `CREATE` fails (`0x30116425`) | Doppler ×7 |
| Railgun "gas price too low" | Railgun ×2 |
| timed out (15 min) | Centrifuge, ENS, Grove, Morpho, Securitize (1 each) |
| RPC error during replay of earlier txs in the block | World ID ×1 |

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
  86–88% of it, and it still clears the BLS floor.

### Usual: ~55% on USD0 minting
- **Who:** Usual Labs (France) ⚠️. Governance runs through the Usual DAO.
- **Contact form** <https://usual.money/form/contact-us> ✅
- **X** [`@usualmoney`](https://x.com/usualmoney) ✅, **LinkedIn**
  <https://www.linkedin.com/company/usualmoney> ✅
- **Discord** <https://discord.usual.money> ✅, Snapshot governance `usualmoney.eth` ✅,
  docs <https://docs.usual.money> ✅
- No governance forum or partnerships email found.

### World ID: 50–55% on `registerIdentities`
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

### Railgun: 55% on RelayAdapt
- **Governance** <https://governance.railgun.org>. Posting needs staked RAIL, so find a delegate.
- **Telegram** `@railgunproject`, **GitHub** <https://github.com/Railgun-Community>. No email.
- Railgun already measured 78.72% on direct `transact` calls. This adds the RelayAdapt path,
  which used to score 0%.

### Linea: 54% on the second proof path
- **Contact:** "Speak to an expert" form at <https://linea.build/contact/inquiries> ✅
- Linea's direct path already measured 54.54%. Nested settlement brings its other verify path,
  which sends the proof to `PlonkVerifierFull`, up to the same level.

### Privacy Pools (0xbow): 17–51% across relays, deposits and `ragequit`
- **Who:** 0xbow <https://0xbow.io>. GitHub <https://github.com/0xbow-io/privacy-pools-core> ✅,
  docs <https://docs.privacypools.com> ✅.
- **X** `@0xbowio` ⚠️. 0xbow.io has a "Contact Us" item that needs a browser.
- **Pitch:** the Entrypoint relay used to score 0% and now saves 32–46%. Combined with the
  59.59% already measured on the pool, every main path now saves. The USDT relay and the
  PPRouter deposit are still blocked by the analyzer bug in the failures report.

### Chainlink: 6–7% (low priority)
- **Contact page** <https://chain.link/contact> ✅ (loads; form contents not checked).
- Only 2 of 6 price updates save, and only slightly. Not worth leading with.

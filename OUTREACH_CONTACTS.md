# Outreach contacts for the protocols that measured savings

Companion to `ALL_TRANSACTION_ANALYSES.md` and `PROTOCOL_SURVEY.md`. Compiled 2026-09-17, updated 2026-10-05.

**22 of 50 surveyed protocols had at least one transaction clear the 50,000-gas Schnorr
floor**, plus two projects measured outside the survey (Tacit and USD8). They are not equally worth an email. This document ranks them by the *measured
dollar value* of the saving, routes each one to a contact channel, and flags what is
verified versus what still needs a human to confirm.

> **Correction (2026-10-14): "Polygon zkEVM" below is the Polygon Agglayer rollup manager.** The measured transactions go to `PolygonRollupManager` (`0x5132A183…`) but verify other Agglayer chains: X Layer (rollup 3), Lumia (7), Silicon (10) and Katana (20). None is Polygon zkEVM (rollup 1), which Polygon shut down on July 1, 2026. Selector `0x6c766877` is `verifyPessimisticTrustedAggregator`. The gas numbers are unchanged.

## Read this before sending anything

The survey's own conclusion applies to the outreach: **at the gas prices prevailing when
this was measured, the entire opportunity across all 48 protocols is ~$3,650/month.**
Telling Linea we can save them $21 a month is not a conversation. Telling them the same
integration is worth $1,348/month at 20 gwei — and that their proof verification is 54%
removable — is.

**The pitch is the percentage and the 20-gwei number, not today's dollars.** Lead with the
measurement (we ran your real mainnet transactions through our analyzer), not with the
savings claim.

---

## Tier 1 — worth a real outreach effort

Six protocols. Everything else is rounding error at any gas price.

| protocol | best % | $/mo now | $/mo @ 20 gwei | what to lead with |
|---|---:|---:|---:|---|
| **Aave V3** | 59.61% | $1,392 | $84,986 | 555 qualifying txs/day — highest volume in the survey |
| **Railgun** | 78.72% | $876 | $130,162 | best $/month at 20 gwei of anything measured |
| **Scroll** | 65.44% | $703 | $6,818 | proof verification; they pay the highest gas price of anyone measured |
| **Chronicle** | 32.64% | $304 | $6,041 | 189 qualifying txs/day; directly beats Chainlink, which scored 0.00% |
| **Starknet** | 74.76% | $244 | $18,549 | `updateStateKzgDA` has **no calls in the program at all** — cleanest result in the survey |
| **Polygon Agglayer** | 71.76% | $77 | $24,800 | two verify paths, both clear; 2nd-highest 20-gwei value |

### Aave
- **Best channel: the governance forum.** <https://governance.aave.com> — verified live. No
  dedicated integrations category; technical proposals go under **Governance** or **General**.
- Business/BD: <https://aave.com/build> → "Talk to Sales" (`aave.com/build#how-can-we-help`).
  Form only — no published BD email.
- Discord <https://discord.com/invite/aave>, GitHub <https://github.com/aave>.
- **Practical note:** Aave Labs runs partnerships; the DAO runs the protocol. A gas-saving
  integration touching the pool contracts is a DAO conversation, so the forum is the real
  door. Expect to need a delegate to sponsor it — the Aave Chan Initiative is the usual route.

### Railgun
- **Governance portal: <https://governance.railgun.org>** — DAO governance requires staked
  RAIL to post, which is a real barrier; budget for it or find a delegate.
- Telegram `@railgunproject`, Discord via the governance portal. GitHub org:
  <https://github.com/Railgun-Community>.
- **People (2026-10-14):**
  - Alan Scott, co-founder, "handling adoption and partnerships". X
    [`@tsu_kareta`](https://x.com/tsu_kareta) ⚠️, from IQ.wiki, a crowd-edited wiki.
  - Kieran (GitHub [`mesquka`](https://github.com/mesquka)) ✅, the only committer to
    `Railgun-Privacy/contract` in 2025–26. No socials on the profile.
  - Official X [`@railgun_project`](https://x.com/railgun_project) ✅, from the Railgun-Community GitHub org.
- **No published email found.** Railgun is the most decentralised target on this list and
  the hardest to reach through a conventional channel — but it has the single largest
  20-gwei value, so it is worth the effort of showing up in Telegram/Discord first.

### Scroll
- **Best channel: <https://tally.so/r/waxLBW>** — Scroll's own "Get in touch — reach out
  directly if you need more support for your project" form, linked from their docs.
  Verified. This is the intended BD/technical entry point.
- Discord <https://discord.gg/scroll>, GitHub <https://github.com/scroll-tech>.
- **No published email.** The Tally form is the route.
- **People (2026-10-14):** top committers to `scroll-tech/scroll-contracts` in 2025–26:
  Péter Garamvölgyi (GitHub [`Thegaram`](https://github.com/Thegaram), 25 commits) ✅ and Xi Lin
  (GitHub [`zimpha`](https://github.com/zimpha), 11) ✅. Neither GitHub links an X account.
- **Report:** `reports/scroll.md`. 65.4% on bundle finalisation, re-measured October 2026; Scroll is still live.

### Chronicle
- `hello@chroniclelabs.org` (general) and `gm@chroniclelabs.org` (partnerships) —
  **search-derived, NOT verified on-page**; chroniclelabs.org rate-limited every fetch
  attempt. Confirm before sending, or use Discord.
- Discord <https://discord.com/invite/CjgvJ9EspJ>. GitHub <https://github.com/chronicleprotocol>.
- **Strongest single story in the survey:** Chronicle measured 32.64% while Chainlink
  measured 0.00% on the same screening. That is a competitive-differentiation pitch, not
  just a cost pitch, and Chronicle already markets itself on gas efficiency.

### Starknet / StarkWare
- **Contact form at <https://starkware.co>** → "Contact Us" in the nav. The form has a
  **Topic dropdown including "Customers/Partner Inquiries"** — use that. Verified.
- `info@starkware.co` — **not on any StarkWare page checked**; only `legal@` appears (privacy policy). Use the form.
- GitHub <https://github.com/starkware-libs>, X `@StarkWareLtd`.
- **2026-10-13:** the StarkWare form returned "There was an error trying to send your message".
  `developerpartnerships@starknetfoundation.org` (from a March 2023 Foundation post) bounced.
  The live alternative is `info@starknet.io`, which StarkWare names in the starknet.io terms
  (updated November 2025). It is a general inbox, not a BD address.
- Also consider the **Starknet Foundation** separately from StarkWare — the L1 state-update
  transactions are posted by the sequencer operator, so confirm who actually owns that cost.

### Polygon Agglayer
- **`/get-access` form on <https://polygon.technology/contact-us>** — labelled "Business
  Enquiries", the primary route. Verified.
- Verified emails: `pr@polygon.technology` (press), `ir@polygon.technology` (investor
  relations). **Neither is right for this** — use the form.
- Telegram `@PolygonHQ` (founders support). Support portal:
  `support.polygon.technology`.
- `info@polygon.technology` ✅, listed on the Agglayer GitHub organization.
- **People (2026-10-14):**
  - Engineers on the rollup-manager contracts, from the `agglayer/agglayer-contracts` commits:
    - Carlos Matallana (GitHub `krlosMata`) ✅. Public member of the Agglayer org and a 2026
      committer. Devcon bios list him as protocol team lead for Polygon zkEVM. No X handle found.
    - Krishang (GitHub `kamuikatsurgi`) ✅. Public org member, company @0xPolygon. X
      [`@0xkrishang`](https://x.com/0xkrishang) and Telegram <https://t.me/kamuik16>, both
      linked from his GitHub.
    - GitHub `jesusholoo`, the top 2026 committer to `agglayer-contracts`. No name or socials
      on the profile.
  - Leadership: Sandeep Nailwal, CEO of the Polygon Foundation, which took over the Agglayer in
    2025. X `@sandeepnailwal` ⚠️. Marc Boiron, CEO of Polygon Labs, X `@marcboiron` ⚠️.
    Brendan Farmer, co-founder who led the Agglayer design, X `@_bfarmer` ⚠️. These handles
    are from memory and still need checking.
- **Context:** in 2026 Polygon Labs cut staff twice and repositioned as a payments company
  (Coinme and Sequence acquisitions). Agglayer sits under the Foundation, so pitch it there.

---

## Tier 2 — measured savings, but not worth a cold email on economics alone

Contact them only if there is a strategic reason (logo, case study, ecosystem intro).

| protocol | best % | $/mo | channel | verified? |
|---|---:|---:|---|---|
| **Linea** | 54.54% | $21 ($1,348 @20) | `linea.build/contact/inquiries` — "Speak to an expert" | ✅ |
| **Pyth** | 52.06% | $12 ($2,245 @20) | DAO forum <https://forum.pyth.network>; Douro Labs interest form `data.dourolabs.xyz/pyth-interest-form` | ✅ |
| **Kelp** | 83.29% | $25 | Now under KernelDAO. **Mihailo Maksa**, who leads Kelp's on-chain engineering per his GitHub: X [`@MihailoMaksa`](https://x.com/MihailoMaksa) ✅ (linked from his GitHub). Kelp's own site: X `@KelpDAO`, Discord `discord.gg/khSpqCFvGq`, Telegram `t.me/KelpDAOxyz` ✅. Context: rsETH lost ~$292M in an April 18, 2026 LayerZero bridge exploit; the deposit pool we measured was paused, then reopened (our deposits are from Aug 2026) | ✅ site and GitHub |
| **Symbiotic** | 57.04% | $14 | X `@symbioticfi` (only real channel; no Discord, email or form); Telegram is announce-only; or intro via Mellow | ✅ docs.symbiotic.fi |
| **Mellow** | 41.55% | $3 | Discord <https://discord.gg/mellow>; X `@Mellowprotocol`; no email or form | ✅ mellow.finance |
| **Rocket Pool** | 15.66% | — | <https://dao.rocketpool.net> — **has a dedicated `Integration` category** | ✅ best-structured forum of any target |
| **Morpho** | 40.43% | — | see *Added since 2026-09-17* — pitch the vault curators, not only Morpho | ✅ |
| **Aragon** | 24.15% | ~$3.62 | Discord `discord.gg/aragonorg`; `press@aragon.org` (press only) | ✅ |
| **Ondo** | 12.09% | — | `support@ondo.finance`, `press@ondo.finance`, contact form | ✅ |
| **Ether.fi** | 11.09% | — | Discord `discord.gg/etherfi`; <https://governance.ether.fi> | ✅ |
| **Midas** | 23.54% | — | midas.app and docs both 403. Co-founder Dennis Dinkelmeyer (public X/LinkedIn); open the midas.app footer in a browser | ❌ still unverified |
| **Renzo** | 62.97% | — | Site moved to renzofinance.com. Discord `discord.com/invite/renzoprotocol`, X `@RenzoProtocol`, Telegram `t.me/RenzoProtocolChat`; no email or form | ⚠️ search-derived |
| **Puffer** | 7.06% | — | **Partnerships form** <https://forms.gle/9QdfbmGuCc934KJh6>; forum <https://governance.puffer.fi>; Discord `discord.com/invite/pufferfi`; X `@Puffer_Finance` | ✅ puffer.fi footer |
| **Privacy Pools** | 59.59% | — | see *Added since 2026-09-17* | ⚠️ partial |
| **Pendle** | 2.60% | — | No forum exists (`forum.` and `governance.pendle.finance` both fail); governance is sPENDLE snapshot votes. Use the Discord linked in the pendle.finance footer | ❌ links need a browser |
| **Doppler** | 13.73% | — | see *Added since 2026-09-17* | ✅ |

Renzo and Puffer are flagged in the survey as **inferred-only contract identifications** —
their percentages rest on contracts that were never positively identified. Do not quote
those numbers to them without re-verifying first.

---

## Added since 2026-09-17

These were measured after the first version of this file. None has a monthly-volume figure
yet, so they are not ranked by dollars. Every number below is a measured replay, not the
heuristic fallback.

| project | best % (Schnorr) | best tx | what saves |
|---|---:|---|---|
| **Privacy Pools** | 59.59% | [`0x8cc80eae…`](https://etherscan.io/tx/0x8cc80eae520f1c61fe39f18ab0ea82e4afbbb6ed4c0ddca71d26b5627628f92a) | PoolVault `ragequit`; the proof check lives in the pool. The PPRouter wrapper saves nothing |
| **Tacit** | 54.50% | [`0xe98ed820…`](https://etherscan.io/tx/0xe98ed82065ba594d6b51dd7833c3b7d0a91026c6be4a30b6901c762490698a53) | `TacitEvmPool` ZK proof check (254k gas `STATICCALL` to `TransactVerifier`); no regular calls. Only one tx measured |
| **Morpho VaultV2** | 40.43% | [`0x2e91acd5…`](https://etherscan.io/tx/0x2e91acd5286eeda304294f8f2c9803b7d140dd0cf7426e16ee285e9f6b1f0fca) | `redeem`/`deposit` on VaultV2; 15 wins across 3 vaults and 3 curators |
| **Doppler** | 13.73% | [`0x7b08cf0d…`](https://etherscan.io/tx/0x7b08cf0df79aaaf918bc03d62d402bceb70e3ac68e662a18464c3097d02071ff) | only the v4 hook's `collectFees`; `Airlock.create` saves nothing |
| **USD8** | ~17.5% | Sepolia only | Treasury `0xd2840edb`; approximate (replayed against current state). **Not a gas pitch** — see below |
| **Derive** | 65.21% | [`0xf83f7406…`](https://etherscan.io/tx/0xf83f74064b25f3a5b0533d1f1e02a891eca4c91b5d4363b5e4dffed26eb22ebb) | V3 proof submissions (`0xcaf85453`) to the VerifiableApp; the ~290k-gas verifier is a `STATICCALL`. About 145 a day. Report: `reports/derive.md` |
| Null | 0.00% | [`0x36b6116b…`](https://etherscan.io/tx/0x36b6116b621b2cfaa9f082f1f4295c4aca95435a105ed9f2eccce42232c555db) | not a candidate: `depositMarket` is a router, all its gas is in the vault `CALL` |

### Privacy Pools (0xbow)
- Builder: **0xbow** (<https://0xbow.io>). Press coverage names Ameen Soleimani (CTO) and
  Zak Cole — search-derived.
- X `@0xbowio` (search-derived). GitHub <https://github.com/0xbow-io/privacy-pools-core>,
  docs <https://docs.privacypools.com> (verified).
- 0xbow.io has a "Contact Us" item, but no email, Discord or form showed up when fetched.
  **Open it in a browser**, or DM `@0xbowio`.
- Lead with the pool-level result (59.59%), not the router — their router is a CALL wrapper and
  scores 0.

### Tacit
- `TacitEvmPool` is the shielded ETH pool of **Tacit** (Bitcoin metaprotocol with a ZK bridge to
  Ethereum). Repo <https://github.com/z0r0z/tacit> lists the pool address in `docs/DEPLOYMENTS.md`
  (verified). App: <https://tacit.finance> (not loaded).
- Team: GitHub user **`z0r0z`**; no company named. Also deployed on Base and Robinhood Chain.
- No X, email or Telegram found. **Best route: a GitHub issue/discussion on `z0r0z/tacit`**, or
  check its `SECURITY.md` for a contact.
- Do not confuse with "Tacit Protocol", an unrelated Chainlink hackathon project.

### Morpho VaultV2 — pitch the curators
The saving sits in the vault contract, and curators deploy and run the vaults. They are the
people who would adopt it.
- **Morpho** (Association + Morpho Labs): forum <https://forum.morpho.org> (verified; has
  vault-provider categories), docs <https://docs.morpho.org>. No BD email found.
- **Steakhouse Financial** (steakUSDC): "Get in touch" form
  <https://steakhouse.notion.site/258a4ef3031c809e88e7c50331ca1236>, X `@SteakhouseFi`,
  LinkedIn `linkedin.com/company/steakhouse-financial` — verified.
- **Sentora** (ex-IntoTheBlock; senRLUSDv2): contact page <https://sentora.com/contact>
  (verified), X `@SentoraHQ` (search-derived).
- **Gauntlet** (gtUSDCp): X `@gauntlet_xyz` and the Gauntlet category on the Morpho forum
  <https://forum.morpho.org/t/about-the-gauntlet-category/551> (verified);
  `gov@gauntlet.xyz` (search-derived, unconfirmed).
- The 40.43% row is Sentora's vault, so Sentora is the natural first email.

### Doppler (Whetstone Research)
- Builder: **Whetstone Research** <https://whetstone.cc>. X <https://x.com/dopplerprotocol>,
  Telegram <https://doppler.lol/telegram/>, GitHub <https://github.com/whetstoneresearch>,
  docs <https://docs.doppler.lol> — all verified on doppler.lol. No email.
- Weak pitch on gas alone: only the hook's fee collection saves.

### Derive
- **Who:** Derive (formerly Lyra), an options and perps exchange. The team page at
  <https://www.derive.xyz/about> links each person's X account ✅:
  - Josh Kim, head of engineering: [`@josh_pwrk`](https://x.com/josh_pwrk)
  - Dominic Romanowski, co-founder (Foundation), engineer: [`@domrom__`](https://x.com/domrom__)
  - Sean Dawson, head of research: [`@SeanNotShorn`](https://x.com/SeanNotShorn)
  - Hitesh Donga, head of product: [`@HD_Derive`](https://x.com/HD_Derive)
  - Nick Forster, co-founder & CEO: [`@nickforster`](https://x.com/nickforster)
- **Discord** <https://discord.com/invite/Derive> ✅ (linked from the governance site).
- **Governance forum** <https://forums.derive.xyz> ✅, governance site <https://gov.derive.xyz> ✅.
- **X** [`@derivexyz`](https://x.com/derivexyz) ⚠️ (linked from Derive's own blog post, not seen on the site).
- **Docs** <https://docs.derive.xyz> ✅, help center <https://help.derive.xyz/en/> ✅.
- **No email or partnership form found.** The site's "Contact Us" only jumps to its integrations section.
- Synthetix's May 2025 acquisition proposal (SIP-415) was withdrawn and is marked Rejected, so
  Derive is still independent.

### USD8
- Already in contact with the founder. Gas is not their problem. They want off-chain compute
  over **historical** chain data, which they run in a TEE today.
- The answer is the historical-reads feature on branch `claude/historical-state-reads`
  (`docs/HISTORICAL_STATE.md`, example `VaultLossOracle`). Pitch that, not a percentage.

---

## Warm paths

Two warm-intro paths exist and are held outside this repository (they involve private
correspondence). See the session notes rather than this file.

**No existing correspondence with any of the 22 surveyed protocols exists.** USD8 is the
only project already in conversation. Every direct contact
listed above would be cold.

---

## Suggested sequencing

1. **Chronicle first.** Clearest story (beat Chainlink 32.64% to 0.00%), they already
   compete on gas, and they are small enough to answer a cold email.
2. **Scroll second.** Purpose-built intake form, highest gas price paid of anyone measured,
   and the proof-verification result is the strongest technical finding in the survey.
3. **Starknet and Polygon Agglayer** in parallel — both have working partner-inquiry forms.
4. **Warm intros in parallel** with all of the above, aimed at Aave and Railgun
   specifically, since those two have the highest value and the hardest front doors.
5. **Aave and Railgun last**, once there is a reference customer or an intro. Both are DAO
   governance processes, not sales conversations, and both will go better with a name.

---

## What still needs doing

- `gm@chroniclelabs.org` is still unverified (site rate-limits every fetch). `info@starkware.co`
  is **not** on StarkWare's site — use the contact form ("Customers/Partner Inquiries").
- Needs a real browser: midas.app footer, pendle.finance footer (Discord/BD), 0xbow.io contact,
  morpho.org footer, renzofinance.com.
- Tacit and the Morpho curators: measure transaction volume so they can be ranked in dollars.
- Decide whether Starknet's L1 costs are StarkWare's or the Starknet Foundation's.

# Outreach contacts for the protocols that measured savings

Companion to `ALL_TRANSACTION_ANALYSES.md` and `PROTOCOL_SURVEY.md`. Compiled 2026-09-17.

**21 of 48 surveyed protocols had at least one transaction clear the 50,000-gas Schnorr
floor.** They are not equally worth an email. This document ranks them by the *measured
dollar value* of the saving, routes each one to a contact channel, and flags what is
verified versus what still needs a human to confirm.

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
| **Polygon zkEVM** | 71.76% | $77 | $24,800 | two verify paths, both clear; 2nd-highest 20-gwei value |

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
- **No published email found.** Railgun is the most decentralised target on this list and
  the hardest to reach through a conventional channel — but it has the single largest
  20-gwei value, so it is worth the effort of showing up in Telegram/Discord first.

### Scroll
- **Best channel: <https://tally.so/r/waxLBW>** — Scroll's own "Get in touch — reach out
  directly if you need more support for your project" form, linked from their docs.
  Verified. This is the intended BD/technical entry point.
- Discord <https://discord.gg/scroll>, GitHub <https://github.com/scroll-tech>.
- **No published email.** The Tally form is the route.

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
- `info@starkware.co` — search-derived, **not verified**; the site lists no email directly.
- GitHub <https://github.com/starkware-libs>, X `@StarkWareLtd`.
- Also consider the **Starknet Foundation** separately from StarkWare — the L1 state-update
  transactions are posted by the sequencer operator, so confirm who actually owns that cost.

### Polygon zkEVM
- **`/get-access` form on <https://polygon.technology/contact-us>** — labelled "Business
  Enquiries", the primary route. Verified.
- Verified emails: `pr@polygon.technology` (press), `ir@polygon.technology` (investor
  relations). **Neither is right for this** — use the form.
- Telegram `@PolygonHQ` (founders support). Support portal:
  `support.polygon.technology`.

---

## Tier 2 — measured savings, but not worth a cold email on economics alone

Contact them only if there is a strategic reason (logo, case study, ecosystem intro).

| protocol | best % | $/mo | channel | verified? |
|---|---:|---:|---|---|
| **Linea** | 54.54% | $21 ($1,348 @20) | `linea.build/contact/inquiries` — "Speak to an expert" | ✅ |
| **Pyth** | 52.06% | $12 ($2,245 @20) | DAO forum <https://forum.pyth.network>; Douro Labs interest form `data.dourolabs.xyz/pyth-interest-form` | ✅ |
| **Kelp** | 83.29% | $25 | Now under KernelDAO — <https://kerneldao.com/kelp/> | ⚠️ no contact found in footer |
| **Symbiotic** | 57.04% | $14 | <https://symbiotic.fi> — no contact channels published | ❌ needs a pass |
| **Mellow** | 41.55% | $3 | — | ❌ needs a pass |
| **Rocket Pool** | 15.66% | — | <https://dao.rocketpool.net> — **has a dedicated `Integration` category** | ✅ best-structured forum of any target |
| **Morpho** | 0.51% | — | <https://forum.morpho.org> | ✅ |
| **Aragon** | 24.15% | ~$3.62 | Discord `discord.gg/aragonorg`; `press@aragon.org` (press only) | ✅ |
| **Ondo** | 12.09% | — | `support@ondo.finance`, `press@ondo.finance`, contact form | ✅ |
| **Ether.fi** | 11.09% | — | Discord `discord.gg/etherfi`; <https://governance.ether.fi> | ✅ |
| **Midas** | 23.54% | — | midas.app returned 403 | ❌ needs a pass |
| **Renzo** | 62.97% | — | — | ❌ needs a pass |
| **Puffer** | 7.06% | — | — | ❌ needs a pass |
| **Privacy Pools** | 19.47% | — | — | ❌ needs a pass |
| **Pendle** | 2.60% | — | `forum.pendle.finance` does not resolve | ❌ needs a pass |

Renzo and Puffer are flagged in the survey as **inferred-only contract identifications** —
their percentages rest on contracts that were never positively identified. Do not quote
those numbers to them without re-verifying first.

---

## Warm paths

Two warm-intro paths exist and are held outside this repository (they involve private
correspondence). See the session notes rather than this file.

**No existing correspondence with any of the 21 protocols exists.** Every direct contact
listed above would be cold.

---

## Suggested sequencing

1. **Chronicle first.** Clearest story (beat Chainlink 32.64% to 0.00%), they already
   compete on gas, and they are small enough to answer a cold email.
2. **Scroll second.** Purpose-built intake form, highest gas price paid of anyone measured,
   and the proof-verification result is the strongest technical finding in the survey.
3. **Starknet and Polygon zkEVM** in parallel — both have working partner-inquiry forms.
4. **Warm intros in parallel** with all of the above, aimed at Aave and Railgun
   specifically, since those two have the highest value and the hardest front doors.
5. **Aave and Railgun last**, once there is a reference customer or an intro. Both are DAO
   governance processes, not sales conversations, and both will go better with a name.

---

## What still needs doing

- Verify `gm@chroniclelabs.org` and `info@starkware.co` before use — both are search-derived.
- Find contacts for Symbiotic, Mellow, Midas, Renzo, Puffer, Privacy Pools, Pendle.
- Decide whether Starknet's L1 costs are StarkWare's or the Starknet Foundation's.

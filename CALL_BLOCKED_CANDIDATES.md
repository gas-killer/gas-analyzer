# Call-blocked candidates — transactions worth revisiting

> **Revised for the 50,000-gas Schnorr floor (2026-09-04).** The floor was raised from
> 27,000 on `main` (`87aaf1c`). Savings cells here were recomputed arithmetically from the
> recorded gas and replay-cost columns; gas used and replay cost are unchanged. See
> `ALL_TRANSACTION_ANALYSES.md` for the derivation and the full revision.

Transactions that **score at or near zero today** because the expensive work sits behind an external call the analyzer re-executes rather than strips out. Measured results live in `MORPHO_CANDIDATES.md` and `PROTOCOL_SURVEY.md`; this is the watchlist of what is currently unreachable.

> **Correction (this revision).** An earlier version of this file implied the "unlocked" figures were achievable through the repo's prestate/net-form encoder. They are not. `crates/core/src/prestate.rs` states that a net form exists only when a transaction *"makes no regular `CALL`"* — every row here contains one, so all of them fall back to the struct-log encoder. These estimates describe a **call-aware encoder that does not exist yet**, not a mode that can be switched on today.

## Why these are blocked

GasKiller replaces executing a transaction with applying its final state changes plus one signature check. `crates/core/src/trace.rs:255` drops everything nested inside a `CALL`, keeping the `CALL` itself as a single instruction:

```rust
// Filter out all state-changing operations (CALL, SSTORE, LOG*) that are nested within any CALL
// (they'll be executed as part of the outer CALL, so we can't optimize them)
if !call_stack.is_empty() { continue; }
```

Replaying that instruction re-executes the callee at full price, so the work inside is never removed. `DELEGATECALL` is followed through and is unaffected; `STATICCALL` is ignored entirely. Only a regular `CALL` into another contract's storage is opaque.

A transaction's score therefore depends on **which address it was sent to**. Two protocols demonstrate this with the same operation scoring both ways:

| | direct | via wrapper | measured saving |
|---|---|---|---:|
| **Railgun** `transact` | smart wallet | RelayAdapt (calls `0xd8ae136a`, the same function) | **78.72%** vs **0%** |
| **Privacy Pools** withdraw | pool | Entrypoint `relay` | **19.47%** vs **0%** |

## Three cases, needing different fixes

**A — the callee changes no storage** (a pure verifier). There is no diff to apply; the call could simply be dropped. World ID is the clean example: `verifyProof(uint256[8],uint256[1])` at a 4,812-byte contract that never appears in the transaction's post-state, yet accounts for ~217k of its 263k GasKiller cost.

**B — the callee changes storage and belongs to the same protocol.** Both contracts could carry the SDK and the callee's diff applied directly. Chainlink (forwarder → aggregator, forwarder changes **zero** slots), Privacy Pools (Entrypoint → pool, Entrypoint changes **zero** slots) and Ether.fi's oracle report (5 of its own contracts) are this case.

**C — the callee is a third party.** Cannot assume the SDK. **Safe is entirely this case** — a Safe changes one slot (its nonce) and contributes only 5,407–31,268 of its own work; everything else belongs to whatever it called, which in one sampled transaction was 13 contracts including Morpho Blue and Uniswap V4. There is no "integrate Safe" — the value belongs to the protocols underneath it.

**Not on this list: calls hiding bookkeeping.** Where the callee is only moving money and writing records, reaching past the call gains nothing. Ether.fi's `claimWithdraw` spends 43,868 gas per claim on ~25,000 of storage writes, 3,768 of logs and ~15,100 of calls — leaving ~0 for computation.

## The watchlist

Sorted by the pessimistic bound. `slots` = storage slots that actually changed on chain (measured, `debug_traceTransaction` / `prestateTracer` diffMode). `contracts` = how many contracts they span, which is the real integration cost. `unlocked` prices writes at the warm rate (5,000) and the cold rate (22,100), plus exact log costs, plus the 50,000 Schnorr floor.

| tx | protocol | what | gas used | today | slots | ctrs | logs | unlocked (pessimistic → optimistic) |
|---|---|---|---:|---:|---:|---:|---:|---:|
| [`0x16a0a31c…`](https://etherscan.io/tx/0x16a0a31c0547f2f35018c38f0c2fa3bdcf1320e6a75f998caaa957747e9dc568) | Morpho | flash-loan deleverage (1 Call) | 1,312,558 | -265,037 | 12 | 5 | 41 | +937,044 → +1,142,244 (71–87%) |
| [`0x7c731150…`](https://etherscan.io/tx/0x7c731150234add278ecae3ee9b6bcd35ee50435cc69bc47cce1a008e54c1f2ce) | Railgun | RelayAdapt (wraps the same transact call) | 1,120,780 | +0 | 14 | 2 | 7 | +767,015 → +1,006,415 (68–90%) |
| [`0x9a5fecc6…`](https://etherscan.io/tx/0x9a5fecc64422a0e8f8edb3b914eaed17cd675a09f1e2811a79d0cf0181893851) | Morpho | liquidation (7702 sender) | 2,661,881 | *unmeasurable* | 84 | 21 | 79 | +621,163 → +2,057,563 (23–77%) |
| [`0xff646682…`](https://etherscan.io/tx/0xff6466828843a8e795e4b6ae1b29644a148141dd48784b4be99c58b0ad3be268) | Chainlink | price update, large (1 Call) | 716,016 | -2,266 | 4 | 3 | 6 | +579,310 → +647,710 (81–90%) |
| [`0x2d754e6f…`](https://etherscan.io/tx/0x2d754e6f8a34058e5d07596e627cbc70e0c704279e9f745a4c0baef80389cca7) | Railgun | RelayAdapt (wraps the same transact call) | 523,496 | +0 | 4 | 2 | 6 | +396,601 → +465,001 (76–89%) |
| [`0x03ebad9a…`](https://etherscan.io/tx/0x03ebad9a10bc3dc5ad36613de80975b7ee8061d7fa74367f1a9aa04e77cc1524) | Privacy Pools | relayed withdrawal | 604,245 | -11,869 | 13 | 1 | 4 | +282,766 → +505,066 (47–84%) |
| [`0xa447c2d3…`](https://etherscan.io/tx/0xa447c2d3d0786a32f8b23c0f571e714e91d4d812b575d7bee27864c7c3e8c556) | World ID | registerIdentities (1 Call = Groth16 verifier) | 298,629 | +8,578 | 2 | 1 | 1 | +225,554 → +259,754 (76–87%) |
| [`0xcd404a27…`](https://etherscan.io/tx/0xcd404a27462a9e60fdd5a17c024d758d809f860ad2da9f1709882d497276375a) | World ID | registerIdentities (1 Call = Groth16 verifier) | 281,445 | +0 | 2 | 1 | 1 | +208,370 → +242,570 (74–86%) |
| [`0x2766c992…`](https://etherscan.io/tx/0x2766c992f22f5aec9bdfc1f16e394d4c4ed6b996bd002f64235b5234e9269cd2) | Privacy Pools | relayed withdrawal (USDT) | 558,846 | -12,162 | 16 | 2 | 6 | +167,180 → +440,780 (30–79%) |
| [`0x4d8f00ee…`](https://etherscan.io/tx/0x4d8f00ee277c67f95049a43dfe604418d0a408fed40a6473bd5b154045c2e2e2) | Privacy Pools | relayed withdrawal | 577,042 | -11,726 | 17 | 2 | 6 | +163,276 → +453,976 (28–79%) |
| [`0x641b76f4…`](https://etherscan.io/tx/0x641b76f483f45a02815116bb7b0213530d0f2aee019b7cc4840a2d71a2940f0e) | Morpho | liquidation | 721,933 | *unmeasurable* | 24 | 7 | 23 | +119,429 → +529,829 (17–73%) |
| [`0xdcc894ec…`](https://etherscan.io/tx/0xdcc894ec22dc799bd1cd8c24caa4bcd2a2d7f35ae2ba8ab7c431a07f76e5ba21) | Safe | execTransaction -> 13 third-party contracts | 1,004,511 | +0 | 34 | 13 | 57 | +97,942 → +679,342 (10–68%) |
| [`0xf6ebba6b…`](https://etherscan.io/tx/0xf6ebba6ba0e5e5f003598b5e05efbb737fb5e00e6a1818e58d1e565316f96b74) | Safe | execTransaction -> 5 third-party contracts | 214,221 | +0 | 5 | 5 | 5 | +68,316 → +153,816 (32–72%) |
| [`0xa676c243…`](https://etherscan.io/tx/0xa676c24374af6324558937b595e3a94fca0fb817823fd22a24ea0f783ebffc6c) | Chainlink | price update, typical (1 Call) | 136,046 | -2,319 | 2 | 1 | 3 | +53,297 → +87,497 (39–64%) |
| [`0x09fd0f6e…`](https://etherscan.io/tx/0x09fd0f6eb66388ce7cdc484b2020d300b5c6d519df89c5bddc73307d9e68bd80) | Morpho | liquidation | 619,024 | *unmeasurable* | 32 | 11 | 34 | -185,495 → +361,705 (-30–58%) |
| [`0x1338ba16…`](https://etherscan.io/tx/0x1338ba16b0a7f61988caf43896fde0e32edac97cd7dab32bb6136bf9e77f0302) | Morpho | flash-loan MEV bot (1 Call) | 405,201 | -18,088 | 24 | 6 | 21 | -194,945 → +215,455 (-48–53%) |
| [`0x1c71eb76…`](https://etherscan.io/tx/0x1c71eb76549cc6a80467e06e8bc938b7fc1e67e9575c2aece8d98345243bb218) | Morpho | 9-market reallocation (1 Call) | 725,295 | +25,527 | 44 | 3 | 48 | -368,705 → +383,695 (-51–53%) |
| [`0xc3081e6f…`](https://etherscan.io/tx/0xc3081e6f850f214a3df7ccf069f0233a4d07fd08fede69f3e904c45e644789cf) | Ether.fi | third-party aggregator (1 Call) | 4,904,702 | -52,101 | 235 | 16 | 378 | -1,041,056 → +2,977,444 (-21–61%) |
| [`0x8951b058…`](https://etherscan.io/tx/0x8951b058f41486ff7d9c5806d187af52f7d969ae69ddbb85c1e1be04171dae04) | Safe | execTransaction, 251 logs (log-cost bound) | 3,251,629 | +4,268 | 252 | 2 | 251 | -2,784,952 → +1,524,248 (-86–47%) |

### Priority — robust and cheap to integrate

Positive even at the pessimistic bound, spanning at most 3 contracts:

- **`0x7c731150234a…`** — Railgun, RelayAdapt (wraps the same transact call). 14 slots / 2 contract(s). Unlocks **767,015–1,006,415** (68–90%). Today: +0.
- **`0xff6466828843…`** — Chainlink, price update, large (1 Call). 4 slots / 3 contract(s). Unlocks **579,310–647,710** (81–90%). Today: -2,266.
- **`0x2d754e6f8a34…`** — Railgun, RelayAdapt (wraps the same transact call). 4 slots / 2 contract(s). Unlocks **396,601–465,001** (76–89%). Today: +0.
- **`0x03ebad9a10bc…`** — Privacy Pools, relayed withdrawal. 13 slots / 1 contract(s). Unlocks **282,766–505,066** (47–84%). Today: -11,869.
- **`0xa447c2d3d078…`** — World ID, registerIdentities (1 Call = Groth16 verifier). 2 slots / 1 contract(s). Unlocks **225,554–259,754** (76–87%). Today: +8,578.
- **`0xcd404a27462a…`** — World ID, registerIdentities (1 Call = Groth16 verifier). 2 slots / 1 contract(s). Unlocks **208,370–242,570** (74–86%). Today: +0.
- **`0x2766c992f22f…`** — Privacy Pools, relayed withdrawal (USDT). 16 slots / 2 contract(s). Unlocks **167,180–440,780** (30–79%). Today: -12,162.
- **`0x4d8f00ee277c…`** — Privacy Pools, relayed withdrawal. 17 slots / 2 contract(s). Unlocks **163,276–453,976** (28–79%). Today: -11,726.
- **`0xa676c24374af…`** — Chainlink, price update, typical (1 Call). 2 slots / 1 contract(s). Unlocks **53,297–87,497** (39–64%). Today: -2,319.

**World ID and Chainlink are the cheapest integrations on this list** — 2 slots in 1 contract each. Chainlink additionally has no direct path at all (0 of 242 price updates across 187 aggregators went direct), so it either gets the integration or stays at zero. Railgun's RelayAdapt rows are the least urgent despite good numbers, since 53% of Railgun traffic already takes the direct path that scores 65–81% today.

### Deprioritise

- `0x16a0a31c0547…` — Morpho, flash-loan deleverage (1 Call). 12 slots / **5 contracts**, 41 logs. +937,044 → +1,142,244.
- `0x9a5fecc64422…` — Morpho, liquidation (7702 sender). 84 slots / **21 contracts**, 79 logs. +621,163 → +2,057,563.
- `0x641b76f483f4…` — Morpho, liquidation. 24 slots / **7 contracts**, 23 logs. +119,429 → +529,829.
- `0xdcc894ec22dc…` — Safe, execTransaction -> 13 third-party contracts. 34 slots / **13 contracts**, 57 logs. +97,942 → +679,342.
- `0xf6ebba6ba0e5…` — Safe, execTransaction -> 5 third-party contracts. 5 slots / **5 contracts**, 5 logs. +68,316 → +153,816.
- `0x09fd0f6eb663…` — Morpho, liquidation. 32 slots / **11 contracts**, 34 logs. -185,495 → +361,705.
- `0x1338ba16b0a7…` — Morpho, flash-loan MEV bot (1 Call). 24 slots / **6 contracts**, 21 logs. -194,945 → +215,455.
- `0x1c71eb76549c…` — Morpho, 9-market reallocation (1 Call). 44 slots / **3 contracts**, 48 logs. -368,705 → +383,695.
- `0xc3081e6f850f…` — Ether.fi, third-party aggregator (1 Call). 235 slots / **16 contracts**, 378 logs. -1,041,056 → +2,977,444.
- `0x8951b058f414…` — Safe, execTransaction, 251 logs (log-cost bound). 252 slots / **2 contracts**, 251 logs. -2,784,952 → +1,524,248.

Two ceilings are visible here that have nothing to do with calls:

- **Logs never compress.** Every event must be re-emitted. Safe's `0x8951b058…` emits 251 logs costing **440,381 gas** — that alone bounds it, which is why it goes negative pessimistically despite touching only 2 contracts. Ether.fi's `0xc3081e6f…` has 378 logs.
- **Contract sprawl.** Writing another contract's storage needs that contract to trust the handler. One-contract rows are plausible; the 13-, 16- and 21-contract rows are not, whatever their headline number.

## Measured vs estimated

| claim | status |
|---|---|
| gas used, today's GasKiller score | **measured** — analyzer output (`cargo run -- t <hash>`) |
| slots changed, contracts touched, log counts | **measured** — `debug_traceTransaction` prestateTracer diffMode |
| log costs | **measured** — computed from real topics/data with the module's own constants |
| the "unlocked" range | **estimated** — my arithmetic applying the repo's cost model by hand |

The unlocked column has never been produced by the analyzer, and per the correction above cannot be produced by any mode that exists today.

## Caveats

- **The estimate assumes every call disappears.** It prices only storage writes, logs and the signature floor. Any integration that still needs a call adds that cost back.
- **Case A means dropping a verification, not relocating it.** For World ID the elided call is the ZK proof check. GasKiller's model is that an attestor already validated the transition and the signature floor pays for that trust — but this is a protocol-design conversation, and the likeliest source of pushback.
- **Warm vs cold pricing swings several rows across zero.** Refunds for slots cleared to zero are ignored, and the 20%-of-gas refund cap may bite differently in a smaller replay.
- **Small opportunistic samples.** These came from surveying eight protocols, not a systematic sweep. Three rows could not be measured by the analyzer at all — see `MORPHO_CANDIDATES.md` for the reasons (an EIP-7702 sender the simulator rejects, reproducible mid-replay reverts).

## Screening question

Two questions have correctly predicted every result across Morpho, Privacy Pools, Ether.fi, Chainlink, Pendle, World ID, Railgun and Safe:

> **1. Does the user call the contract that does the heavy computation, or does something call it for them?**

> **2. Is the work behind that call computation, or bookkeeping?**

Called for them + computation → belongs on this list. Called for them + bookkeeping → no integration helps. Safe passes question 1 and fails on ownership instead: its callees are third parties, so the opportunity is never Safe's to grant.

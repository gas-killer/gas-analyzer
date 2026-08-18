# Call-blocked candidates — transactions worth revisiting

A watchlist of transactions that **look like good GasKiller candidates but score zero today**, because the expensive work sits behind an external call the analyzer cannot reach past.

This is a tracker, not a results file. Measured results live in `MORPHO_CANDIDATES.md`. Nothing here is a verified saving — every figure in the "unlocked" column is an estimate, and the section below is explicit about which numbers are measured and which are not.

## Why these are blocked

GasKiller replaces *executing* a transaction with *applying its final state changes* plus one signature check. The analyzer builds that list of state changes from the trace, and `crates/core/src/trace.rs:255` drops everything nested inside a `CALL`:

```rust
// Filter out all state-changing operations (CALL, SSTORE, LOG*) that are nested within any CALL
// (they'll be executed as part of the outer CALL, so we can't optimize them)
if !call_stack.is_empty() { continue; }
```

So a `CALL` survives as a single opaque instruction, and replaying it re-executes the callee at full price. Whatever computation happened inside is never stripped out. `DELEGATECALL` is followed through and is *not* affected — only real calls into another contract's storage are opaque.

The consequence: a transaction's score depends on **which address it was sent to**, not on how much compressible work it did. The clearest proof is in Privacy Pools, where the same job on the same pool scores either way depending only on the entry point:

| | relayed via Entrypoint | called pool directly |
|---|---:|---:|
| gas used | 604,245 | 587,069 |
| GasKiller cost (measured) | 616,114 | 422,768 |
| **saving (measured)** | **0** | **137,301 (23.39%)** |

## Two kinds of call-blocking — only one is worth chasing

**Calls hiding computation → track these.** The callee burns gas on math, hashing or signature checks that leave almost nothing behind. Reach past the call and the saving is real. Chainlink is the extreme case: 136,046 gas of oracle-signature verification and median math that changes **2 storage slots**.

**Calls hiding bookkeeping → dead end, do not track.** The callee is just moving money and writing records. Reaching past the call gains nothing because there was no hidden computation. Ether.fi's `batchClaimWithdraw` is the reference case: solving its cost model against a single claim gives a per-claim on-chain cost of 43,868, which decomposes as ~25,000 of storage writes + 3,768 of logs + ~15,100 of calls — leaving **~0 for computation**. Its only saving was a one-time 26,207 of transaction startup overhead, against a 17,334 loss on every claim, so it breaks even at 1.5 claims and a batch of 13 loses 199,139. **Batching is actively harmful and no integration fixes it.**

## The watchlist

Sorted by the pessimistic bound — i.e. by how much survives if every assumption goes against us.

`slots` = storage slots that actually changed on chain (measured via `debug_traceTransaction` / `prestateTracer` in diffMode). `contracts` = how many distinct contracts those slots span, which is the practical integration cost. `unlocked` is bounded by pricing every write at the warm rate (5,000, optimistic) and at the cold rate (22,100, pessimistic), plus exact log costs, plus the 27,000 Schnorr floor.

| tx | protocol | what | gas used | today (measured) | slots | contracts | unlocked (pessimistic → optimistic) |
|---|---|---|---:|---:|---:|---:|---:|
| [`0x16a0a31c…`](https://etherscan.io/tx/0x16a0a31c0547f2f35018c38f0c2fa3bdcf1320e6a75f998caaa957747e9dc568) | Morpho | flash-loan deleverage (1 Call) | 1,312,558 | -265,037 | 12 | 5 | +937,044 → +1,142,244 (71–87%) |
| [`0x9a5fecc6…`](https://etherscan.io/tx/0x9a5fecc64422a0e8f8edb3b914eaed17cd675a09f1e2811a79d0cf0181893851) | Morpho | liquidation (7702 sender) | 2,661,881 | *unmeasurable* | 84 | 21 | +621,163 → +2,057,563 (23–77%) |
| [`0xff646682…`](https://etherscan.io/tx/0xff6466828843a8e795e4b6ae1b29644a148141dd48784b4be99c58b0ad3be268) | Chainlink | price update, large (1 Call) | 716,016 | -2,266 | 4 | 3 | +579,310 → +647,710 (81–90%) |
| [`0x03ebad9a…`](https://etherscan.io/tx/0x03ebad9a10bc3dc5ad36613de80975b7ee8061d7fa74367f1a9aa04e77cc1524) | Privacy Pools | relayed withdrawal | 604,245 | -11,869 | 13 | 1 | +282,766 → +505,066 (47–84%) |
| [`0x2766c992…`](https://etherscan.io/tx/0x2766c992f22f5aec9bdfc1f16e394d4c4ed6b996bd002f64235b5234e9269cd2) | Privacy Pools | relayed withdrawal (USDT) | 558,846 | -12,162 | 16 | 2 | +167,180 → +440,780 (30–79%) |
| [`0x4d8f00ee…`](https://etherscan.io/tx/0x4d8f00ee277c67f95049a43dfe604418d0a408fed40a6473bd5b154045c2e2e2) | Privacy Pools | relayed withdrawal | 577,042 | -11,726 | 17 | 2 | +163,276 → +453,976 (28–79%) |
| [`0x641b76f4…`](https://etherscan.io/tx/0x641b76f483f45a02815116bb7b0213530d0f2aee019b7cc4840a2d71a2940f0e) | Morpho | liquidation | 721,933 | *unmeasurable* | 24 | 7 | +119,429 → +529,829 (17–73%) |
| [`0xa676c243…`](https://etherscan.io/tx/0xa676c24374af6324558937b595e3a94fca0fb817823fd22a24ea0f783ebffc6c) | Chainlink | price update, typical (1 Call) | 136,046 | -2,319 | 2 | 1 | +53,297 → +87,497 (39–64%) |
| [`0x09fd0f6e…`](https://etherscan.io/tx/0x09fd0f6eb66388ce7cdc484b2020d300b5c6d519df89c5bddc73307d9e68bd80) | Morpho | liquidation | 619,024 | *unmeasurable* | 32 | 11 | -185,495 → +361,705 (-30–58%) |
| [`0x1338ba16…`](https://etherscan.io/tx/0x1338ba16b0a7f61988caf43896fde0e32edac97cd7dab32bb6136bf9e77f0302) | Morpho | flash-loan MEV bot (1 Call) | 405,201 | -18,088 | 24 | 6 | -194,945 → +215,455 (-48–53%) |
| [`0x1c71eb76…`](https://etherscan.io/tx/0x1c71eb76549cc6a80467e06e8bc938b7fc1e67e9575c2aece8d98345243bb218) | Morpho | 9-market reallocation (1 Call) | 725,295 | +25,527 | 44 | 3 | -368,705 → +383,695 (-51–53%) |
| [`0xc3081e6f…`](https://etherscan.io/tx/0xc3081e6f850f214a3df7ccf069f0233a4d07fd08fede69f3e904c45e644789cf) | Ether.fi | third-party aggregator (1 Call) | 4,904,702 | -52,101 | 235 | 16 | -1,041,056 → +2,977,444 (-21–61%) |

### Priority: robust *and* cheap to integrate

Positive even at the pessimistic bound, and spanning few contracts:

- **`0xff6466828843…`** — Chainlink, price update, large (1 Call). 4 slots across 3 contract(s). Unlocks **579,310–647,710** (81–90%). Today: -2,266.
- **`0x03ebad9a10bc…`** — Privacy Pools, relayed withdrawal. 13 slots across 1 contract(s). Unlocks **282,766–505,066** (47–84%). Today: -11,869.
- **`0x2766c992f22f…`** — Privacy Pools, relayed withdrawal (USDT). 16 slots across 2 contract(s). Unlocks **167,180–440,780** (30–79%). Today: -12,162.
- **`0x4d8f00ee277c…`** — Privacy Pools, relayed withdrawal. 17 slots across 2 contract(s). Unlocks **163,276–453,976** (28–79%). Today: -11,726.
- **`0xa676c24374af…`** — Chainlink, price update, typical (1 Call). 2 slots across 1 contract(s). Unlocks **53,297–87,497** (39–64%). Today: -2,319.

Chainlink's typical feed update is the standout on *cost of integration*: **2 slots, 1 contract**. Its large-feed sibling spans 3 contracts and unlocks 579,310–647,710 (81–90%), the biggest robust number here. Chainlink also routes **100% of traffic through forwarders** — 0 of 242 price updates across 187 aggregators went direct — so there is no shortcut: it needs the integration or it stays at zero.

### Deprioritise: many contracts, or negative at the pessimistic bound

- `0x16a0a31c0547…` — Morpho, flash-loan deleverage (1 Call). 12 slots across **5 contracts**. +937,044 → +1,142,244.
- `0x9a5fecc64422…` — Morpho, liquidation (7702 sender). 84 slots across **21 contracts**. +621,163 → +2,057,563.
- `0x641b76f483f4…` — Morpho, liquidation. 24 slots across **7 contracts**. +119,429 → +529,829.
- `0x09fd0f6eb663…` — Morpho, liquidation. 32 slots across **11 contracts**. -185,495 → +361,705.
- `0x1338ba16b0a7…` — Morpho, flash-loan MEV bot (1 Call). 24 slots across **6 contracts**. -194,945 → +215,455.
- `0x1c71eb76549c…` — Morpho, 9-market reallocation (1 Call). 44 slots across **3 contracts**. -368,705 → +383,695.
- `0xc3081e6f850f…` — Ether.fi, third-party aggregator (1 Call). 235 slots across **16 contracts**. -1,041,056 → +2,977,444.

The 4.9 M-gas Ether.fi aggregator transaction is the cautionary entry: enormous gas and a single `Call` update, so it looks maximally blocked — but its 235 changed slots span 16 contracts, and at cold pricing it goes **negative**. Big gas alone is not the signal.

## What is measured and what is not

| claim | status |
|---|---|
| gas used, and today's GasKiller score | **measured** — analyzer output (`cargo run -- t <hash>`) |
| storage slots changed, contracts touched | **measured** — `debug_traceTransaction` prestateTracer, diffMode |
| log costs | **measured** — computed from actual log topics/data with the module's own constants |
| the "unlocked" range | **estimated** — my arithmetic applying the repo's cost model by hand |
| Privacy Pools direct withdraw 23.39%, Ether.fi oracle report 18.99% | **measured** (see `MORPHO_CANDIDATES.md`) |

The unlocked column has never been produced by the analyzer. The tool always measures from a transaction's `to` address, and re-pointing it at an inner contract needs a code change that has not been made.

## Caveats that could shrink these numbers

- **The estimate assumes *every* call disappears.** It prices only storage writes, logs and the signature floor. A real integration that still needs one call — paying a transmitter, sending ETH to a recipient — adds that cost back. This makes the optimistic bound genuinely optimistic.
- **For some entries, the stripped computation *is* the security mechanism.** Chainlink's gas is largely spent verifying a quorum of oracle signatures; substituting one GasKiller attestation changes the protocol's trust model. Same for Privacy Pools if the Groth16 `verifyProof` call were elided — note the measured 23.39% win does **not** do this, it keeps paying for the verifier. This is a protocol-design conversation, not an engineering one, and it is the likeliest source of pushback.
- **Warm vs cold storage pricing swings the answer.** Several rows are positive at 5,000/slot and negative at 22,100/slot. Which applies depends on access patterns not modelled here. Refunds for slots cleared to zero are also ignored, and the 20%-of-gas refund cap may bite differently in a smaller replay.
- **Integration cost scales with contracts touched, not slots.** Writing another contract's storage requires that contract to trust the handler. One-contract entries are plausible; sixteen-contract entries are not.
- **Small samples.** These 12 transactions came from opportunistic windows while surveying four protocols, not a systematic sweep. Three could not be measured by the analyzer at all, for reasons documented in `MORPHO_CANDIDATES.md` (an EIP-7702 sender the simulator rejects, and reproducible mid-replay reverts).

## Suggested next step

Add a flag letting the analyzer measure from a nominated inner contract instead of the transaction's `to`. That converts every "unlocked" estimate here into a measured number, and turns this from a judgement call into a screening tool — useful well beyond these 12, since forwarder and relayer patterns are everywhere. Until then this list is a set of leads, not findings.

## Screening question for new protocols

One question has correctly predicted every result across Morpho, Privacy Pools, Ether.fi and Chainlink:

> **When a user performs the main action, do they call the contract that does the heavy computation — or does something call it for them?**

If it is called for them, expect zero today and add it here. Then ask the follow-up that separates the two categories above: **is the work behind that call computation, or bookkeeping?** Only computation is worth integrating for.

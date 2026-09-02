# Every transaction analysed, in one place

208 Ethereum mainnet transactions across 17 protocols, all run through this repo's analyzer (`gas-analyzer-cli t <hash>`). Five more could not be run at all; they are listed at the end.

**Read the dollar-value section first.** The percentages in this file are large and the money behind them is not: the whole opportunity across the three best protocols is roughly $2,600 a month at the gas prices measured.

Every number in the transaction tables is the tool's own output. The dollar-value section is the one exception — it extrapolates measured gas prices and volumes to a month, and says so.

## What the columns mean

- **gas used** — what the transaction actually cost on chain.
- **GasKiller cost** — what the tool says it would cost to skip the execution and just write the final result.
- **surplus** — gas used minus GasKiller cost. This is the work GasKiller removes.
- **Schnorr saved** — surplus minus 27,000, the cost of the one signature check GasKiller adds. If surplus is under 27,000, the saving is zero.
- **BLS** — same idea but the signature check costs 250,000. Almost nothing clears it.
- **updates** — how many storage writes, logs and calls are in the final result GasKiller has to write back. More is worse.

## Why a transaction saves nothing

Three reasons, and they are different problems:

| code | what it means | fixable? |
|---|---|---|
| **external calls** | The expensive work happens in a *different* contract. The tool keeps that call whole and runs it again, so none of the gas inside it is saved. | Yes, if the other contract also runs GasKiller. This is the integration problem. |
| too many writes | The transaction changes so many storage slots that writing them all back costs more than the original transaction. | No. The transaction is bookkeeping, not computation. |
| under the floor | The transaction's own work is real but smaller than the 27,000-gas signature check. | Only by making the signature check cheaper. |

Across all 208 transactions, **68 show a saving** and 140 show none.

Of the 68 savings, **66 are properly measured and 2 are suspect** — the two ERC-4337
EntryPoint rows at 80.66% and 86.54%, which are probably artifacts of the replay defect
described below. **15 rows are still tagged `heur`** and are not results; re-run them
serially before using them.

Of the 140 that save nothing:

- **108 are blocked by external calls**
- 18 have too many writes
- 61 fall under the floor

Those add up to more than the total because a transaction can hit more than one at once — the tables below list every blocker that applies.

**External calls are the single biggest blocker in this data set.** Fixing that — by getting the called contracts onto GasKiller too — unlocks more transactions than any other change.

## What it's worth in dollars — read this before the percentages

Every other number in this file is a percentage. Percentages turned out to be the wrong
unit, and this is the correction.

Measured 2026-09-02. Mainnet base fee was **0.15–0.35 gwei**, blocks running 11–42M gas
against a 60M limit — abundant spare capacity, so gas is nearly free. ETH/USD $2,386, read
from the Chainlink aggregator on-chain.

| protocol | best % | qualifying txs/day | gas saved each | **savings/month** | at 20 gwei |
|---|---:|---:|---:|---:|---:|
| Aave V3 | 63.64% | 555 | 130,000 | **$1,691** | $103,254 |
| Railgun | 80.84% | 106 | 877,737 | **$900** | $133,664 |
| Pyth | 63.94% | 29 | 54,614–123,861 | **$12–25** | ~$2,245 |
| | | | | **~$2,600** | **~$239,000** |

**The ranking inverts against percentage.** Railgun has the best percentage in this file by
a wide margin and saves *less money* than Aave, because Aave qualifies five times more
transactions per day. Pyth — the cleanest, most reachable result here — is worth about
twenty dollars a month.

**The product's value is a leveraged bet on the fee market, not on the percentage saved.**
Nothing about the engineering differs between the last two columns; only gas prices do.

So when using this file to pick targets, rank by:

1. **The fee market.** Two orders of magnitude, entirely outside anyone's control.
2. **Qualifying transactions per day.** This is what makes Aave beat Railgun on money.
3. **Absolute gas saved per transaction**, not the ratio.
4. The percentage — which is only a presentation of #3 and adds no information.

Caveats: sampling was one ~1.4-day window per protocol, full log enumeration plus 180
random transactions to establish the direct-call fraction and median gas price actually
paid, extrapolated to 30 days. **An earlier version of this calculation undercounted Aave
by 119x** by capping the hash scan at 400 and treating the survivors as the population.
Aave's 25% qualifying share and 130,000 gas per transaction are carried from the
per-function work, not re-measured. A quiet window understates the average, and gas spikes
are exactly when the product is worth most. Fees are paid by the *users* who send the
transactions, not by the protocols — which affects who the counterparty in any deal is.

## Scoreboard

Best and typical figures use only properly measured runs. They exclude the two Ondo rows that turned out to be other people's traffic (a 13.37% MEV bot and a 1.64% aggregator that merely touched Ondo), and the two Morpho rows still tagged `heur`.

| protocol | txs | save gas (measured) | best Schnorr | typical win | blockers on the rest |
|---|---:|---:|---:|---:|---|
| **Railgun** | 18 | 15 | **80.84%** | 74.06% | external calls (3), under the floor (2) |
| **Pyth** | 13 | **13** | **63.94%** | 28.01% | none — every transaction saves |
| **Aave** | 20 | 10 | **63.64%** | 44.12% | external calls (9), under the floor (8), too many writes (1) |
| **Privacy Pools** | 11 | 2 | 23.39% | 21.96% | external calls (8), too many writes (1) |
| **Ether.fi** | 13 | 3 | 18.99% | 7.34% | external calls (8), under the floor (4), too many writes (1) |
| **Safe** | 12 | 3 | 9.99% | 8.98% | external calls (9), under the floor (8) |
| **Ethena** | 22 | 1 | 8.99% | 8.99% | under the floor (15), external calls (6) |
| **EigenLayer** | 13 | 6 | 5.15% | 0.91% | external calls (6), too many writes (4), under the floor (3) |
| **Pendle** | 8 | 1 | 3.94% | 3.94% | external calls (7), under the floor (5) |
| **World ID** | 7 | 1 | 2.87% | 2.87% | external calls (6), under the floor (6) |
| **Morpho** | 25 | 2 | 2.60% | 2.21% | external calls (20), too many writes (6), under the floor (3) |
| **Ondo** | 8 | 3 | 2.01% | 1.59% | external calls (3), too many writes (1) |
| **Euler** | 8 | 1 | 1.31% | 1.31% | external calls (7), too many writes (3), under the floor (2) |
| **Lido** | 13 | 2 | 1.13% | 1.13% | too many writes (10), under the floor (1) |
| **Chainlink** | 8 | 0 | 0.00% | — | external calls (8) |
| **Panther** | 1 | 0 | 0.00% | — | external calls (1), too many writes (1) |
| **ERC-4337 EntryPoint** | 8 | 2 *(suspect)* | *86.54%* | *83.60%* | external calls (6); **both wins are probably replay artifacts — do not quote** |

## What actually predicts a win

Three rules. Every result in this file follows them.

**1. The transaction has to be sent straight to the contract doing the expensive work.**

The clearest proof is Aave against Euler. Both run the same per-asset risk maths on a borrow. Aave lets users call its Pool contract directly and a borrow saves up to 63.64%. Euler forces every user through its EVC router — 0 of 191 transactions reached a vault directly — and the best result is 1.31%. Same computation, ~49× difference, decided entirely by routing.

**2. The expensive work has to be computation, not bookkeeping.**

Railgun verifies zero-knowledge proofs: 65–81% saved. Morpho updates share balances: 2.60% at best. Cryptography and per-asset oracle loops win. Merkle hashing, allowlist checks and share accounting do not, because the writes they produce cost as much as the work they did.

**3. Screen per entry point, not per protocol.**

Aave's `borrow` won 9 out of 9. Aave's `supply` and `repay` won 0 out of 6 — they skip the health-factor check, so there is no computation to remove. One protocol can be both a great candidate and a dead end depending on which function is called.

### What that means for targeting

Look for protocols that:

- do elliptic-curve cryptography on chain (ZK proof verification, BLS/Groth16 checks, signature aggregation);
- loop over per-asset oracle reads on every transaction;
- let users call the contract directly instead of forcing a router or relayer.

Avoid protocols whose transactions are mostly transfers, share-balance updates, merkle-tree appends or allowlist lookups — and avoid anything where a relayer, batcher or safe sits in front, until the external-call problem is solved.

## Near misses — blocked by calls but already close

These transactions have real compressible work left over and still lost. If the contract behind the call also ran GasKiller, these are the ones that flip first.

| protocol | tx | function | gas used | surplus | short of the floor by | calls in the result |
|---|---|---|---:|---:|---:|---:|
| Morpho | [`0x1c71eb76…`](https://etherscan.io/tx/0x1c71eb76549cc6a80467e06e8bc938b7fc1e67e9575c2aece8d98345243bb218) | *9-market reallocation* `0xeb7499cf` | 725,295 | +25,527 | 1,473 | 1 |
| Safe | [`0x5688590b…`](https://etherscan.io/tx/0x5688590bc26e704d720d7bb2185b195aabb310ba67dddd944f64509b1cf70513) | `execTransaction` ✓ `0x6a761202` | 322,338 | +25,210 | 1,790 | 2 |
| Pendle | [`0xa2c6de73…`](https://etherscan.io/tx/0xa2c6de73e89c7bd707a9fe308d09c54eb16d2ddf9e3ff09f866d8a68e3948afd) | *third-party aggregator* `0xc685f647` | 1,183,159 | +25,192 | 1,808 | 5 |
| Safe | [`0x2666f99a…`](https://etherscan.io/tx/0x2666f99a3b1ad225cf8dcc40fbafb1959a6efb5bb3c561b3d2d5a3b242ca4a29) | `execTransaction` ✓ `0x6a761202` | 96,660 | +23,309 | 3,691 | 1 |
| World ID | [`0x36c09544…`](https://etherscan.io/tx/0x36c095445eb96f2ccaa2a2ec9544ac2cf72aa524c36c0ce23c0aecc2cf36b8b7) | `registerIdentities` ✓ `0x2217b211` | 285,261 | +22,186 | 4,814 | 1 |
| EigenLayer | [`0xaa858fcd…`](https://etherscan.io/tx/0xaa858fcd781a6a5afa142ccd9a802d0df1e2f68292f0abf8546426cda3fe6644) | *EigenLayer rewards claim* `0x3ccc861d` | 156,150 | +20,985 | 6,015 | 2 |
| Safe | [`0xb286eeb1…`](https://etherscan.io/tx/0xb286eeb15c4cb3445a73e275e89336f7563eb46c2e9ec97a82aeefb5a4485430) | `execTransaction` ✓ `0x6a761202` | 463,470 | +20,526 | 6,474 | 6 |
| World ID | [`0x6b2fb8d3…`](https://etherscan.io/tx/0x6b2fb8d32c1fc927e5c37ae0ca52d17cb122b949ec0140f75a8212164911f494) | `registerIdentities` ✓ `0x2217b211` | 282,573 | +19,534 | 7,466 | 1 |
| World ID | [`0xb2f5ba58…`](https://etherscan.io/tx/0xb2f5ba588077025662acd44f62ead62c4dc6da4faa30890d658542aedcaef3c5) | `registerIdentities` ✓ `0x2217b211` | 281,457 | +18,406 | 8,594 | 1 |
| World ID | [`0xcd404a27…`](https://etherscan.io/tx/0xcd404a27462a9e60fdd5a17c024d758d809f860ad2da9f1709882d497276375a) | `registerIdentities` ✓ `0x2217b211` | 281,445 | +18,394 | 8,606 | 1 |
| Safe | [`0x909681b5…`](https://etherscan.io/tx/0x909681b5131c5ccc56e6f6791f152efe41cf270172c5a4645fc0067567bd8651) | `execTransaction` ✓ `0x6a761202` | 103,575 | +18,056 | 8,944 | 1 |
| Euler | [`0x7d1e2345…`](https://etherscan.io/tx/0x7d1e2345c39b884a16eb6b2e06c1c4a4177c5639fe1c91474a356c3eef04fd87) | *third-party contract* `0x3271ba8d` | 2,701,108 | +17,630 | 9,370 | 3 |
| Euler | [`0xa32effd3…`](https://etherscan.io/tx/0xa32effd3b31a02343d8cf4362c4fee2e806ea9dcf00fdea5eb1a2b054ae0ea4a) | `batch` ✓ `0xc16ae7a4` | 499,524 | +16,691 | 10,309 | 5 |
| Aave | [`0x9905c35e…`](https://etherscan.io/tx/0x9905c35e963d0c36e66b33d6f2319085daebf2920a8572ce26c01e201d0bbc16) | `withdraw` ✓ `0x69328dec` | 183,126 | +16,416 | 10,584 | 1 |

**One caveat on the Safe rows.** A Safe's own work is verifying owner signatures — about 3,000 gas each — and it writes one slot, its nonce. So its surplus is capped by owner count no matter what the transaction does. The gas inside the call belongs to whatever the Safe called, and those protocols would get that saving with or without a Safe in front. Safe is a thin wrapper over other candidates, not a candidate itself.

`CALL_BLOCKED_CANDIDATES.md` has the deeper write-up of this group.

## Warning: `heur` rows are usually rate limiting, not real results

When the analyzer cannot replay a block it falls back to a heuristic estimator and prints
a normal-looking result with a small `(heuristic - measured estimation failed)` note. The
estimator prices `Call` at **zero gas**, so it fails in one direction only: it *overstates*
savings.

During the Ethena runs, 9 of 21 transactions hit this path. The cause was not the
transactions — it was the RPC provider returning `HTTP 429: 50/second request limit
reached` mid-replay. Re-running them serially recovered real measurements for 6, and
**every single one collapsed to 0%**:

| tx | heuristic claimed | true measured |
|---|---:|---:|
| `0xae6a3e25…` | 36.96% | 0% |
| `0xaf1a2df7…` | 34.99% | 0% |
| `0xd2475759…` | 33.91% | 0% |
| `0x91e950a3…` | 33.90% | 0% |
| `0xb3a29f2b…` | 30.95% | 0% |
| `0x6f12cb87…` | 3.94% | 0% |

On `0xaf1a2df7…` the heuristic was wrong by 86,378 gas.

**Practical rules.** Never quote a `heur` row. Re-run it serially — one analyzer process at
a time — before drawing any conclusion. The remaining `heur` rows in the table below,
including those inherited from earlier protocols, were produced under the same throttling
and should be re-run before use rather than treated as data.

## The replay defect — why 11 transactions can't be measured, and one silent failure mode

Separate from rate limiting, 11 runs failed with `RevertingContext CALL #n ... reverted`.
These cluster in **Morpho (7), Euler (2) and Railgun (2)** — and not at all in Lido,
Chainlink or Pyth.

**What happens.** GasKiller records a transaction as a list of state updates and replays
them to price it. But the recorded steps depend on each other's side effects, and the
recording does not preserve them. A Euler liquidation
(`0x575f2fb2224cf67a0a3a8cca18def46bbf327eb72e1ac1414771dffd6f4a67f3`, 3,036,470 gas)
reduces to four calls:

1. Morpho Blue `0xe0232b42`
2. WETH `withdraw(uint256)` (`0x2e1a7d4d`, keccak-verified)
3. an ETH transfer
4. an ETH transfer to a builder

On replay, step 2 reverts: `WETH.withdraw` requires the caller to already hold that WETH,
and step 1 is what delivered it in the original transaction. The recording says only *call
Morpho Blue with these bytes* — the balance change that call produced is not carried
forward.

This hits transactions that **chain** operations — borrow, then swap, then repay — which is
exactly what liquidations and bundled DeFi calls do. Simple transactions never trip it,
which is why Lido and Pyth measured first time.

**The detected case is safe.** When the revert escapes, the tool notices, refuses to
report a measurement and says why. That is correct behaviour.

**The undetected case is not.** If the contract catches its own errors, the revert never
escapes. The tool sees a call that "succeeded" very cheaply and reports a large saving.
This is the suspected explanation for the two ERC-4337 EntryPoint rows showing 80.66% and
86.54%: EntryPoint is designed to catch a failing userOp rather than revert. Two
structurally identical EntryPoint transactions — same eight updates, same ordering, same
`validateUserOp` and `innerHandleOp` calls — replay for 201,100 and 2,205,725 gas
respectively, a 10x divergence with no structural difference.

| replay fails and… | tool reports | risk |
|---|---|---|
| the revert escapes | "estimation failed" | none — honest |
| the contract catches it | a large saving | **you quote it to a customer** |

Nothing in the output distinguishes the second case from a real win. Both are labelled
`measured via StateChangeHandler` with no warning.

**One-sentence version:** GasKiller's replay doesn't carry forward the intermediate state
between a transaction's recorded steps, so chained calls fail on replay — and when a
contract swallows that failure internally, the analyzer reports it as a large false saving.

## Every transaction

Sorted by protocol (best protocol first), then by savings.

`heur` marks a transaction where the real replay failed and the tool fell back to a rough estimate. **Those numbers are not reliable** — the estimator prices calls at zero gas, and the one time it could be checked against a real measurement it was off by 95,744 gas.

A **✓** on a function name means I confirmed the 4-byte selector by hashing the signature myself. An *italic* name was guessed from the events the transaction emitted — treat it as a description, not an identification.

| protocol | tx | function | gas used | GasKiller cost | surplus | Schnorr saved | BLS saved | why 0 | updates | notes |
|---|---|---|---:|---:|---:|---:|---:|---|---:|---|
| Railgun | [`0xaa357a48…`](https://etherscan.io/tx/0xaa357a4824001aed7173b3bc7d976f997fc7839def4c56ec070db97ec2121bc4) | *Railgun private transfer / unshield* `0xd8ae136a` | 1,085,832 | 181,095 | +904,737 | **877,737** (80.84%) | 654,737 | — | 14 (11S/3L1) |  |
| Railgun | [`0x32daef80…`](https://etherscan.io/tx/0x32daef80a0c5180dcf45b15130e10ecced485242f0d3305b30942ebeb5aec364) | *Railgun private transfer / unshield* `0xd8ae136a` | 1,056,246 | 197,859 | +858,387 | **831,387** (78.71%) | 608,387 | — | 15 (12S/3L1) |  |
| Railgun | [`0xe7e605b6…`](https://etherscan.io/tx/0xe7e605b6520aaf757f4531a88af4966866d86fee61ec06e93b3423b30a934adc) | *Railgun private transfer / unshield* `0xd8ae136a` | 1,615,755 | 317,840 | +1,297,915 | **1,270,915** (78.66%) | 1,047,915 | — | 22 (15S/5L1/2C) |  |
| Railgun | [`0xa2177825…`](https://etherscan.io/tx/0xa21778257ba78ce1354a4ff25a48ee07854e5e858f4facfe3e3c3893d9a1945f) | *Railgun private transfer / unshield* `0xd8ae136a` | 1,561,573 | 312,286 | +1,249,287 | **1,222,287** (78.27%) | 999,287 | — | 23 (16S/5L1/2C) |  |
| Railgun | [`0x73a1d71e…`](https://etherscan.io/tx/0x73a1d71e2cfa678b63c934c8c04eb6b6069de89c85faf16d0376241ecabfb0ec) | *Railgun private transfer / unshield* `0xd8ae136a` | 1,097,058 | 213,396 | +883,662 | **856,662** (78.09%) | 633,662 | — | 17 (14S/3L1) |  |
| Railgun | [`0xa3a010fa…`](https://etherscan.io/tx/0xa3a010fa890d910e669fba1ea7b681a4c1f464b754c5b7bff7d62ae15ae25e71) | *Railgun private transfer / unshield* `0xd8ae136a` | 1,113,545 | 241,625 | +871,920 | **844,920** (75.88%) | 621,920 | — | 18 (12S/4L1/2C) |  |
| Railgun | [`0x04736806…`](https://etherscan.io/tx/0x04736806756593b664eb29591a2ea046b2261c5a4fa1bdd1707a6dc5abf54f07) | *Railgun private transfer / unshield* `0xd8ae136a` | 1,136,044 | 266,554 | +869,490 | **842,490** (74.16%) | 619,490 | — | 19 (13S/4L1/2C) |  |
| Railgun | [`0x39a67a29…`](https://etherscan.io/tx/0x39a67a2964b675ccd1be1939c513b46f62cae71c3f320bece6acdaf71934b3ae) | *Railgun private transfer / unshield* `0xd8ae136a` | 1,131,080 | 266,430 | +864,650 | **837,650** (74.06%) | 614,650 | — | 20 (14S/4L1/2C) |  |
| Railgun | [`0xaf36e218…`](https://etherscan.io/tx/0xaf36e2187e095fa88299a8859632963594414b716f36aae541318d4ce71740e6) | *Railgun private transfer / unshield* `0xd8ae136a` | 1,177,014 | 282,477 | +894,537 | **867,537** (73.71%) | 644,537 | — | 21 (15S/4L1/2C) |  |
| Railgun | [`0x0376edd3…`](https://etherscan.io/tx/0x0376edd36b0c9211b1a38f41e5f1c04233478ca4dcf7d4852ef313deb1c57ade) | `shield` ✓ `0x044a40c3` | 724,515 | 168,652 | +555,863 | **528,863** (73.00%) | 305,863 | — | 14 (10S/2C/2L1) |  |
| Railgun | [`0xab44777b…`](https://etherscan.io/tx/0xab44777b4e3a37169ea03163ec5d14dbf668bcb18219c3a95cc2313c2be55a3d) | `shield` ✓ `0x044a40c3` | 736,786 | 177,753 | +559,033 | **532,033** (72.21%) | 309,033 | — | 13 (9S/2C/2L1) |  |
| Railgun | [`0x27ce0220…`](https://etherscan.io/tx/0x27ce022039ef027d473c61f3140da32c8e58f0a8daaee826e6f04a58744db447) | `shield` ✓ `0x044a40c3` | 742,727 | 185,294 | +557,433 | **530,433** (71.42%) | 307,433 | — | 14 (10S/2C/2L1) |  |
| Railgun | [`0x3d54cfc3…`](https://etherscan.io/tx/0x3d54cfc375e626acd71432b2ab59a94bde2f6ee2490528ca8259417091e3ab97) | `shield` ✓ `0x044a40c3` | 752,878 | 197,812 | +555,066 | **528,066** (70.14%) | 305,066 | — | 15 (11S/2C/2L1) |  |
| Railgun | [`0x6837bb8d…`](https://etherscan.io/tx/0x6837bb8d629ee0dce5f045acfc6c1e296df4abe1907897820bf621462dc23425) | `shield` ✓ `0x044a40c3` | 758,741 | 211,644 | +547,097 | **520,097** (68.55%) | 297,097 | — | 18 (14S/2C/2L1) |  |
| Railgun | [`0x792a8e57…`](https://etherscan.io/tx/0x792a8e5783cec82cc456d83f0f1703a2bf71eafa05984a17896809bd952567cc) | *Railgun private transfer / unshield* `0xd8ae136a` | 484,303 | 139,722 | +344,581 | **317,581** (65.57%) | 94,581 | — | 7 (3L1/2C/2S) |  |
| Railgun | [`0x6f534f5a…`](https://etherscan.io/tx/0x6f534f5af7fa26b66e0c6a5f49505f7125665e88b0d1af23efca86da88109f27) `heur` | *Railgun RelayAdapt bundle* `0x28223a77` | 1,297,603 | 1,287,231 | +10,372 | 0 | 0 | **external calls** · under the floor | 5 (3C/2S) |  |
| Railgun | [`0x7c731150…`](https://etherscan.io/tx/0x7c731150234add278ecae3ee9b6bcd35ee50435cc69bc47cce1a008e54c1f2ce) `heur` | *Railgun RelayAdapt bundle* `0x28223a77` | 1,120,780 | 1,116,659 | +4,121 | 0 | 0 | **external calls** · under the floor | 5 (3C/2S) |  |
| Railgun | [`0x2d754e6f…`](https://etherscan.io/tx/0x2d754e6f8a34058e5d07596e627cbc70e0c704279e9f745a4c0baef80389cca7) | *Railgun RelayAdapt bundle* `0x28223a77` | 523,496 | 533,777 | -10,281 | 0 | 0 | **external calls** | 5 (3C/2S) |  |
| Aave | [`0xa7a8c34f…`](https://etherscan.io/tx/0xa7a8c34fee3795db241bcdf3e5c0b8b279dcb78f62ceee2f3530c9f38dd1ebec) | `borrow` ✓ `0xa415bcad` | 571,333 | 180,741 | +390,592 | **363,592** (63.64%) | 140,592 | — | 11 (7S/2C/1L2/1L4) |  |
| Aave | [`0xa36db142…`](https://etherscan.io/tx/0xa36db1427faa3a36a7461bf00e80bc12bd07d9d7c87fbab54ee49bec92b9fec7) | `borrow` ✓ `0xa415bcad` | 450,205 | 163,828 | +286,377 | **259,377** (57.61%) | 36,377 | — | 10 (6S/2C/1L2/1L4) |  |
| Aave | [`0x1b708eba…`](https://etherscan.io/tx/0x1b708eba95e753a84558374c488f1557907935272fabc2ffe91f6d8844c06370) | `withdraw` ✓ `0x69328dec` | 408,792 | 180,921 | +227,871 | **200,871** (49.14%) | 0 | — | 12 (8S/1C/1L2/1L3/1L4) |  |
| Aave | [`0xa70a6bbd…`](https://etherscan.io/tx/0xa70a6bbd833f973defbcc4a0b1ad281f4708fa1960a39635d784a855a8fac9e1) | `borrow` ✓ `0xa415bcad` | 371,912 | 173,408 | +198,504 | **171,504** (46.11%) | 0 | — | 12 (8S/2C/1L2/1L4) |  |
| Aave | [`0xc62d052c…`](https://etherscan.io/tx/0xc62d052c8385a546ab9f2f4d6a9fc194e45c3c9c7bf69e22454e38d9c55cb0cb) | `borrow` ✓ `0xa415bcad` | 360,781 | 170,263 | +190,518 | **163,518** (45.32%) | 0 | — | 11 (7S/2C/1L2/1L4) |  |
| Aave | [`0xe73dcebf…`](https://etherscan.io/tx/0xe73dcebf75429339193a24c45a37e377948bf6f69fa77b8f50931d1e26ebeeea) | `borrow` ✓ `0xa415bcad` | 345,794 | 170,335 | +175,459 | **148,459** (42.93%) | 0 | — | 11 (7S/2C/1L2/1L4) |  |
| Aave | [`0x000d4a32…`](https://etherscan.io/tx/0x000d4a32a076758c9ca1271e2909e2c4e1acaf988ff36ac8e8dc513f5fa64332) | `borrow` ✓ `0xa415bcad` | 365,438 | 181,716 | +183,722 | **156,722** (42.89%) | 0 | — | 11 (7S/2C/1L2/1L4) |  |
| Aave | [`0x2cab2e0f…`](https://etherscan.io/tx/0x2cab2e0f215f22a30249a1198f6604bdcfcc131183aeaaa7c1257fb939ec239f) | `borrow` ✓ `0xa415bcad` | 346,272 | 206,699 | +139,573 | **112,573** (32.51%) | 0 | — | 12 (8S/2C/1L2/1L4) |  |
| Aave | [`0xe65a59b9…`](https://etherscan.io/tx/0xe65a59b9f5d42dfd18018dd8c42850e6ea38c49d4a61718888c072e4f13dcc77) | `borrow` ✓ `0xa415bcad` | 316,682 | 205,592 | +111,090 | **84,090** (26.55%) | 0 | — | 12 (8S/2C/1L2/1L4) |  |
| Aave | [`0xb33162e1…`](https://etherscan.io/tx/0xb33162e1275b4e14a4a0de3d327949fb520949cadd898d17718bb813547ca0fb) | `borrow` ✓ `0xa415bcad` | 252,529 | 164,712 | +87,817 | **60,817** (24.08%) | 0 | — | 11 (7S/2C/1L2/1L4) |  |
| Aave | [`0xeff004bd…`](https://etherscan.io/tx/0xeff004bd1fb2851258ac5aa69997def87176f40716ecd4fe3d68904e1d6c3669) | `withdraw` ✓ `0x69328dec` | 211,120 | 194,806 | +16,314 | **0** (0.00%) | 0 | under the floor | 10 (1C) | **re-measured** — heuristic claimed 11.09% |
| Aave | [`0x9287f808…`](https://etherscan.io/tx/0x9287f80894eeeacc8af236609401da9f14ce0e655cbb9748e0db2e405aeb2ef5) | *third-party liquidator bot* `0x7e809076` | 1,165,207 | 1,156,574 | +8,633 | 0 | 0 | **external calls** · under the floor | 6 (4C/2S) |  |
| Aave | [`0x324633c1…`](https://etherscan.io/tx/0x324633c1f5cf10b86a58aad0d48ac39673afd6ee24e2b391488ac84e4c6baaa0) | `supplyWithPermit` ✓ `0x02c205f0` | 206,695 | 207,356 | -661 | 0 | 0 | **external calls** · too many writes | 11 (5S/3C/1L2/1L3/1L4) |  |
| Aave | [`0x4eba5f8d…`](https://etherscan.io/tx/0x4eba5f8de29afdcac5703668d41a1f4bee734545dcdb37a129c6341b41c6ea44) | `supply` ✓ `0x617ba037` | 183,352 | 178,041 | +5,311 | 0 | 0 | **external calls** · under the floor | 11 (7S/2C/1L2/1L4) |  |
| Aave | [`0x9905c35e…`](https://etherscan.io/tx/0x9905c35e963d0c36e66b33d6f2319085daebf2920a8572ce26c01e201d0bbc16) | `withdraw` ✓ `0x69328dec` | 183,126 | 166,710 | +16,416 | 0 | 0 | **external calls** · under the floor | 10 (7S/1C/1L2/1L4) |  |
| Aave | [`0x52b373f5…`](https://etherscan.io/tx/0x52b373f5701425f48c42e3779527b6b7da51e7c764317c07cb40446377113649) | `supplyWithPermit` ✓ `0x02c205f0` | 179,856 | 179,187 | +669 | 0 | 0 | **external calls** · under the floor | 12 (7S/3C/1L2/1L4) |  |
| Aave | [`0xe41dd331…`](https://etherscan.io/tx/0xe41dd331b63a80add54ec1ef340f252c623538cfe35b69a65b190651aa636691) | `withdraw` ✓ `0x69328dec` | 167,589 | 160,020 | +7,569 | 0 | 0 | **external calls** · under the floor | 12 (8S/1C/1L2/1L3/1L4) |  |
| Aave | [`0x7f281bd6…`](https://etherscan.io/tx/0x7f281bd663289348bccaeb5c1616459ab424f557a65eaebfb603a1d445267ad4) | `supply` ✓ `0x617ba037` | 155,576 | 150,490 | +5,086 | 0 | 0 | **external calls** · under the floor | 11 (7S/2C/1L2/1L4) |  |
| Aave | [`0x055c232c…`](https://etherscan.io/tx/0x055c232ce489f003f64d5f05a5f77706c8a282f921d60a4264b5eeb21a6c6736) | `supply` ✓ `0x617ba037` | 147,675 | 142,925 | +4,750 | 0 | 0 | **external calls** · under the floor | 11 (7S/2C/1L2/1L4) |  |
| Aave | [`0x6b7563d1…`](https://etherscan.io/tx/0x6b7563d1e117aa90cc1e65b19b10ee525da3bdc89a26d0498a2a7084ab7867b1) | `repay` ✓ `0x573ade81` | 144,130 | 142,631 | +1,499 | 0 | 0 | **external calls** · under the floor | 11 (7S/2C/1L2/1L4) |  |
| Privacy Pools | [`0xe894abc7…`](https://etherscan.io/tx/0xe894abc79ca19fae8e3ef2a98b9570da0037d6f47ce531351002770d16ffe11f) | `withdraw` ✓ `0x30c0766d` | 587,069 | 422,768 | +164,301 | **137,301** (23.39%) | 0 | — | 16 (12S/2C/1L1/1L2) |  |
| Privacy Pools | [`0x67deaa0e…`](https://etherscan.io/tx/0x67deaa0e50f0db65925f464c05168927db7ce336079334100aba5cdbeb701e64) | `withdraw` ✓ `0x30c0766d` | 576,177 | 430,907 | +145,270 | **118,270** (20.53%) | 0 | — | 17 (13S/2C/1L1/1L2) |  |
| Privacy Pools | [`0x15082298…`](https://etherscan.io/tx/0x150822981204592e4cfa340ba2e63e607a1c6ded490b988f9a8bd37c1f2b46d0) | `relay` ✓ `0x8a44121e` | 620,208 | 632,053 | -11,845 | 0 | 0 | **external calls** | 6 (3C/2S/1L4) |  |
| Privacy Pools | [`0x03ebad9a…`](https://etherscan.io/tx/0x03ebad9a10bc3dc5ad36613de80975b7ee8061d7fa74367f1a9aa04e77cc1524) | `relay` ✓ `0x8a44121e` | 604,245 | 616,114 | -11,869 | 0 | 0 | **external calls** | 6 (3C/2S/1L4) |  |
| Privacy Pools | [`0xad4ac41d…`](https://etherscan.io/tx/0xad4ac41d7ad3ba9792d7c426631dba0d46a31e271f5105dbb6aa6df349c891a5) | `relay` ✓ `0x8a44121e` | 604,233 | 616,066 | -11,833 | 0 | 0 | **external calls** | 6 (3C/2S/1L4) |  |
| Privacy Pools | [`0x4d8f00ee…`](https://etherscan.io/tx/0x4d8f00ee277c67f95049a43dfe604418d0a408fed40a6473bd5b154045c2e2e2) | `relay` ✓ `0x8a44121e` | 577,042 | 588,768 | -11,726 | 0 | 0 | **external calls** | 6 (3C/2S/1L4) |  |
| Privacy Pools | [`0x2766c992…`](https://etherscan.io/tx/0x2766c992f22f5aec9bdfc1f16e394d4c4ed6b996bd002f64235b5234e9269cd2) | `relay` ✓ `0x8a44121e` | 558,846 | 571,008 | -12,162 | 0 | 0 | **external calls** | 6 (3C/2S/1L4) |  |
| Privacy Pools | [`0x53e6375e…`](https://etherscan.io/tx/0x53e6375e0156f40d6917e8d48d26f0af6e1fd197d54d0042b96138dfc449660f) | `deposit` ✓ `0x0efe6a8b` | 393,615 | 402,757 | -9,142 | 0 | 0 | **external calls** | 6 (3S/2C/1L3) |  |
| Privacy Pools | [`0x46a8ff4b…`](https://etherscan.io/tx/0x46a8ff4b10a52df709860a908f1749cd66523b5e6a3b18e7eeb901f8b7cd97eb) | `deposit` ✓ `0xb6b55f25` | 381,540 | 387,061 | -5,521 | 0 | 0 | **external calls** | 5 (3S/1C/1L3) |  |
| Privacy Pools | [`0xb120146b…`](https://etherscan.io/tx/0xb120146b4dd84f30f7c44cfb6f9fb5fca7c0b10f051a6259edd5c5c7b40d9da6) | `ragequit` ✓ `0x71235b34` | 279,732 | 287,612 | -7,880 | 0 | 0 | **external calls** | 4 (2C/1L2/1S) |  |
| Privacy Pools | [`0x0b27aa2c…`](https://etherscan.io/tx/0x0b27aa2ce30ee4e0f9e34a3f28537115d38c5a5fd499937b04fe437cfa3537f8) | `updateRoot` ✓ `0x87bf00f0` | 149,961 | 163,803 | -13,842 | 0 | 0 | too many writes | 7 (6S/1L1) |  |
| Ether.fi | [`0x5ab8d02a…`](https://etherscan.io/tx/0x5ab8d02ae1b7a79bcfb65057b14e874fcd9165b5bcae85253f58958e25e1477b) | *(selector not captured)* | 291,228 | 208,918 | +82,310 | **55,310** (18.99%) | 0 | — | 6 (3C/2S/1L3) |  |
| Ether.fi | [`0xab780dc7…`](https://etherscan.io/tx/0xab780dc7c7c59d079462a88428b4e172c84a05eeec892195a02787a856214583) | *Ether.fi oracle-report execution* `0x2e03931e` | 259,254 | 208,906 | +50,348 | **23,348** (9.01%) | 0 | — | 6 (3C/2S/1L3) |  |
| Ether.fi | [`0xadb81164…`](https://etherscan.io/tx/0xadb81164fb568da41e4876b7695f287df7caaec92e4fb4f11579ac6a8aa61af8) | *Ether.fi oracle-report execution* `0x2e03931e` | 250,124 | 208,906 | +41,218 | **14,218** (5.68%) | 0 | — | 6 (3C/2S/1L3) |  |
| Ether.fi | [`0x9fe2db76…`](https://etherscan.io/tx/0x9fe2db76e575eba316f1c1133b9522b1084631a7140b9895272fb782ea65cb8d) | *Ether.fi oracle-report execution* `0x2e03931e` | 245,559 | 208,894 | +36,665 | **9,665** (3.94%) | 0 | — | 6 (3C/2S/1L3) |  |
| Ether.fi | [`0xdce4440b…`](https://etherscan.io/tx/0xdce4440b1e1cd6ae69eaa3331085bcbe38c6ab23c8d53401f7c49c63d7ec06ce) | `batchClaimWithdraw` ✓ `0x24fccdcf` | 205,965 | 209,933 | -3,968 | **0** (0.00%) | 0 | **external calls** · too many writes | 18 (4C) | **re-measured** — heuristic claimed 2.27% |
| Ether.fi | [`0xc3081e6f…`](https://etherscan.io/tx/0xc3081e6f850f214a3df7ccf069f0233a4d07fd08fede69f3e904c45e644789cf) | *third-party aggregator* `0x146ee74d` | 4,904,702 | 4,956,803 | -52,101 | 0 | 0 | **external calls** | 1 (1C) |  |
| Ether.fi | [`0x87b26753…`](https://etherscan.io/tx/0x87b26753364ee01e9c442fbf5c2a37443f28e00d3e34591cd4a9147611fe4eb7) | `batchClaimWithdraw` ✓ `0x24fccdcf` | 679,714 | 878,853 | -199,139 | 0 | 0 | **external calls** · too many writes | 117 (65S/26C/13L2/13L4) | 13 withdrawals batched — 117 state updates |
| Ether.fi | [`0x7ee0a346…`](https://etherscan.io/tx/0x7ee0a34642b5380eea636f02ce6ee35f109390a95bd2d19b8ff1794512829bb1) | `requestWithdraw` ✓ `0x397a1b28` | 193,099 | 198,205 | -5,106 | 0 | 0 | **external calls** | 3 (2C/1L2) |  |
| Ether.fi | [`0x4964cf5a…`](https://etherscan.io/tx/0x4964cf5adfaaf0e7d0d48d4cd2cc02bf792493653550293dbb22aa7909a18eda) | `requestWithdraw` ✓ `0x397a1b28` | 193,075 | 198,133 | -5,058 | 0 | 0 | **external calls** | 3 (2C/1L2) |  |
| Ether.fi | [`0xd6ec9fc5…`](https://etherscan.io/tx/0xd6ec9fc5c359167794fd0c692b578a346ae978b73ed8dc503aa5e6350ed84b63) | `claimWithdraw` ✓ `0xb13acedd` | 153,292 | 144,419 | +8,873 | 0 | 0 | **external calls** · under the floor | 9 (5S/2C/1L2/1L4) |  |
| Ether.fi | [`0xd14eb830…`](https://etherscan.io/tx/0xd14eb830b3308feb3dee287e11752e9400d4aaf8b1c85cacb04d1ef9321a2a84) | `wrap` ✓ `0xea598cb0` | 134,730 | 130,194 | +4,536 | 0 | 0 | **external calls** · under the floor | 4 (2S/1C/1L3) |  |
| Ether.fi | [`0x2cc9a112…`](https://etherscan.io/tx/0x2cc9a112936e0cfda1b4e9afd88ac5a861f2392cb9e20d2598f255a509b6d929) | `unwrap` ✓ `0xde0e9a3e` | 122,474 | 117,487 | +4,987 | 0 | 0 | **external calls** · under the floor | 4 (2S/1C/1L3) |  |
| Ether.fi | [`0xbe33787e…`](https://etherscan.io/tx/0xbe33787eb89c203cacaee0fb05da574bfef9c0aef4b80bcd8eed779a57f86222) | `deposit` ✓ `0xd0e30db0` | 120,178 | 128,056 | -7,878 | 0 | 0 | **external calls** | 4 (2S/1C/1L2) |  |
| EigenLayer | [`0x4ce09132…`](https://etherscan.io/tx/0x4ce0913231fcb4ac1351f81c6632f53165a5a80dc1f44fc7ea9c31021e5c7b04) | *EigenPod checkpoint-proof verification* `0xf074ba62` | 229,856 | 191,015 | +38,841 | **11,841** (5.15%) | 0 | — | 11 (7S/1C/1L1/1L2/1L3) |  |
| EigenLayer | [`0x03d50d7f…`](https://etherscan.io/tx/0x03d50d7f761256ff7ddcab530c0535f352ad8101d5d2912d67339e65e3c61b5d) | `forwardEigenPodCall` ✓ `0x297fdfe4` | 134,366 | 103,398 | +30,968 | **3,968** (2.95%) | 0 | — | 1 (1C) |  |
| EigenLayer | [`0xc770332c…`](https://etherscan.io/tx/0xc770332c7e516902481cc309756988fbe452f58f89a1144713150fe083c1105e) | `forwardEigenPodCall` ✓ `0x297fdfe4` | 417,090 | 386,305 | +30,785 | **3,785** (0.91%) | 0 | — | 1 (1C) |  |
| EigenLayer | [`0xbbc8abcb…`](https://etherscan.io/tx/0xbbc8abcb0eeb0f311b36ab4274e8629bd2b8c1395ffe79ffdebbcbf4536fbe29) | `forwardEigenPodCall` ✓ `0x297fdfe4` | 417,028 | 386,243 | +30,785 | **3,785** (0.91%) | 0 | — | 1 (1C) |  |
| EigenLayer | [`0x95fa2d07…`](https://etherscan.io/tx/0x95fa2d07b579aaac9fa7e0fbd3545bcdb408c197956e0b4e34b9b29fa0f1cd9c) | *EigenLayer rewards claim* `0x3ccc861d` | 108,528 | 81,086 | +27,442 | **442** (0.41%) | 0 | — | 5 (3S/1C/1L4) |  |
| EigenLayer | [`0x6fa0fbbf…`](https://etherscan.io/tx/0x6fa0fbbf87072c3315c4801db2ac55a1e6c58b009d9ff07234e9c84b7a84801e) | *EigenLayer rewards claim* `0x3ccc861d` | 125,638 | 98,174 | +27,464 | **464** (0.37%) | 0 | — | 5 (3S/1C/1L4) |  |
| EigenLayer | [`0xf09b9b69…`](https://etherscan.io/tx/0xf09b9b69b3640f7b018509045180218c1c7acff17eb665fbc40bb9ad2003ed0b) | `queueWithdrawals` ✓ `0x0dd8dd02` | 440,373 | 471,942 | -31,569 | 0 | 0 | **external calls** · too many writes | 24 (20S/2L1/1C/1L2) |  |
| EigenLayer | [`0x065a18a2…`](https://etherscan.io/tx/0x065a18a2af1e2d80514ad79067794b7c34e92ba2c2754c5cb7ddb5157eb111b8) | `queueWithdrawals` ✓ `0x0dd8dd02` | 439,868 | 469,142 | -29,274 | 0 | 0 | **external calls** · too many writes | 24 (20S/2L1/1C/1L2) |  |
| EigenLayer | [`0x5ce19358…`](https://etherscan.io/tx/0x5ce19358229301efda44bc79908632dfc69c1609477d52438c3cd9add6950792) | `completeQueuedWithdrawal` ✓ `0xe4cc3f90` | 190,728 | 190,504 | +224 | 0 | 0 | **external calls** · under the floor | 17 (15S/1C/1L1) |  |
| EigenLayer | [`0xfb18ffe1…`](https://etherscan.io/tx/0xfb18ffe1164630f6d3a2c818f28404fcdc1782103870eadeaea75aaa0de4f0b9) | `completeQueuedWithdrawals` ✓ `0x9435bb43` | 186,142 | 188,312 | -2,170 | 0 | 0 | **external calls** · too many writes | 17 (15S/1C/1L1) |  |
| EigenLayer | [`0xe5de910b…`](https://etherscan.io/tx/0xe5de910bc96de735b7bc040855d99c5b565baa1de8e14c373af2febb5076d19b) | `completeQueuedWithdrawals` ✓ `0x9435bb43` | 183,232 | 185,594 | -2,362 | 0 | 0 | **external calls** · too many writes | 17 (15S/1C/1L1) |  |
| EigenLayer | [`0xaa858fcd…`](https://etherscan.io/tx/0xaa858fcd781a6a5afa142ccd9a802d0df1e2f68292f0abf8546426cda3fe6644) | *EigenLayer rewards claim* `0x3ccc861d` | 156,150 | 135,165 | +20,985 | 0 | 0 | **external calls** · under the floor | 8 (4S/2C/2L4) |  |
| EigenLayer | [`0x22b2c58c…`](https://etherscan.io/tx/0x22b2c58ced9f209d9ec2d21415436e9395067b377764d600927c8b54625f6d4f) | `startCheckpoint` ✓ `0x88676cad` | 76,444 | 52,517 | +23,927 | 0 | 0 | under the floor | 4 (3S/1L3) |  |
| Safe | [`0xa9eca9f1…`](https://etherscan.io/tx/0xa9eca9f1a7075c8eb971e7ee2a1a3ee514cf3cf6b1077d6ddcc4f60f8ebf4eaa) | `execTransaction` ✓ `0x6a761202` | 104,612 | 67,158 | +37,454 | **10,454** (9.99%) | 0 | — | 3 (1C/1L2/1S) |  |
| Safe | [`0x5c559278…`](https://etherscan.io/tx/0x5c5592787da61ec8c46326d93089c14e667276fab45703077168e72dcbbee99e) | `execTransaction` ✓ `0x6a761202` | 115,958 | 78,540 | +37,418 | **10,418** (8.98%) | 0 | — | 3 (1C/1L2/1S) |  |
| Safe | [`0x8951b058…`](https://etherscan.io/tx/0x8951b058f41486ff7d9c5806d187af52f7d969ae69ddbb85c1e1be04171dae04) | `execTransaction` ✓ `0x6a761202` | 3,251,629 | 3,220,361 | +31,268 | **4,268** (0.13%) | 0 | — | 3 (1C/1L2/1S) |  |
| Safe | [`0xf20b6df4…`](https://etherscan.io/tx/0xf20b6df4702b6537ba520316a35552b6d1da7b696e3c69e34986ed901402bd56) | `execTransaction` ✓ `0x6a761202` | 11,414,223 | 11,400,916 | +13,307 | 0 | 0 | **external calls** · under the floor | 3 (1C/1L2/1S) | 11.4M gas, 3 state updates — all of it inside one call |
| Safe | [`0xdcc894ec…`](https://etherscan.io/tx/0xdcc894ec22dc799bd1cd8c24caa4bcd2a2d7f35ae2ba8ab7c431a07f76e5ba21) | `execTransaction` ✓ `0x6a761202` | 1,004,511 | 1,005,210 | -699 | 0 | 0 | **external calls** | 8 (6C/1L2/1S) |  |
| Safe | [`0xb286eeb1…`](https://etherscan.io/tx/0xb286eeb15c4cb3445a73e275e89336f7563eb46c2e9ec97a82aeefb5a4485430) | `execTransaction` ✓ `0x6a761202` | 463,470 | 442,944 | +20,526 | 0 | 0 | **external calls** · under the floor | 8 (6C/1L1/1S) |  |
| Safe | [`0x4e13a401…`](https://etherscan.io/tx/0x4e13a40100c52346b4e971697a347030ffd0cc5bd47d521e8b4a9baf1a984872) | `execTransaction` ✓ `0x6a761202` | 458,625 | 452,215 | +6,410 | 0 | 0 | **external calls** · under the floor | 9 (7C/1L2/1S) |  |
| Safe | [`0x5688590b…`](https://etherscan.io/tx/0x5688590bc26e704d720d7bb2185b195aabb310ba67dddd944f64509b1cf70513) | `execTransaction` ✓ `0x6a761202` | 322,338 | 297,128 | +25,210 | 0 | 0 | **external calls** · under the floor | 4 (2C/1L1/1S) |  |
| Safe | [`0xf6ebba6b…`](https://etherscan.io/tx/0xf6ebba6ba0e5e5f003598b5e05efbb737fb5e00e6a1818e58d1e565316f96b74) | `execTransaction` ✓ `0x6a761202` | 214,221 | 202,578 | +11,643 | 0 | 0 | **external calls** · under the floor | 6 (4C/1L2/1S) |  |
| Safe | [`0x909681b5…`](https://etherscan.io/tx/0x909681b5131c5ccc56e6f6791f152efe41cf270172c5a4645fc0067567bd8651) | `execTransaction` ✓ `0x6a761202` | 103,575 | 85,519 | +18,056 | 0 | 0 | **external calls** · under the floor | 4 (2L1/1C/1S) |  |
| Safe | [`0x2666f99a…`](https://etherscan.io/tx/0x2666f99a3b1ad225cf8dcc40fbafb1959a6efb5bb3c561b3d2d5a3b242ca4a29) | `execTransaction` ✓ `0x6a761202` | 96,660 | 73,351 | +23,309 | 0 | 0 | **external calls** · under the floor | 3 (1C/1L2/1S) |  |
| Safe | [`0xb2b17e51…`](https://etherscan.io/tx/0xb2b17e51681c60f3d66aa112fa13f070c5ea76c466e794e3ba360c8b833b5a55) | `execTransaction` ✓ `0x6a761202` | 75,536 | 70,129 | +5,407 | 0 | 0 | **external calls** · under the floor | 3 (1C/1L2/1S) |  |
| Pendle | [`0x75aceefb…`](https://etherscan.io/tx/0x75aceefb54f607c4de446308146e0b84ff405ec56a6e5bc24ccb88b3b5397991) | *third-party aggregator* `0xc685f647` | 1,719,271 | 1,624,535 | +94,736 | **67,736** (3.94%) | 0 | — | 9 (5C/1L2/1L3/1L4/1S) |  |
| Pendle | [`0xa2c6de73…`](https://etherscan.io/tx/0xa2c6de73e89c7bd707a9fe308d09c54eb16d2ddf9e3ff09f866d8a68e3948afd) | *third-party aggregator* `0xc685f647` | 1,183,159 | 1,157,967 | +25,192 | 0 | 0 | **external calls** · under the floor | 9 (5C/1L2/1L3/1L4/1S) |  |
| Pendle | [`0x8bd2985a…`](https://etherscan.io/tx/0x8bd2985af25143dbf0d3aa60f0e36bc50195d2702e4beeaf1576f8f4464200d7) | *Pendle router action* `0x60fc8466` | 1,018,948 | 1,020,097 | -1,149 | 0 | 0 | **external calls** | 8 (7C/1L4) |  |
| Pendle | [`0xa01ba5b6…`](https://etherscan.io/tx/0xa01ba5b6e8c9f78e7d4d324bd2b03da78a7d02cdf9fa852aa82b60dc400a586c) | *Pendle router action* `0xed48907e` | 768,674 | 762,631 | +6,043 | 0 | 0 | **external calls** · under the floor | 7 (6C/1L4) |  |
| Pendle | [`0x90476689…`](https://etherscan.io/tx/0x9047668966adcd340cf4b69b727a17c5df6c6f34d4bad1994f9cf5331e2329f7) | *Pendle router action* `0xed48907e` | 531,449 | 524,603 | +6,846 | 0 | 0 | **external calls** · under the floor | 7 (6C/1L4) |  |
| Pendle | [`0xa3ec8aea…`](https://etherscan.io/tx/0xa3ec8aea5dd4b01271f7dd979830a444ec2b3fa618f5e0afc550a2bd03af65a3) | *Pendle router action* `0xed48907e` | 370,167 | 359,084 | +11,083 | 0 | 0 | **external calls** · under the floor | 5 (4C/1L4) |  |
| Pendle | [`0xec4d1fdf…`](https://etherscan.io/tx/0xec4d1fdf22d55bdde9d358e329af22a6c3345032ce969c79afb9d5dea8487556) | *Pendle router action* `0x594a88cc` | 205,329 | 213,408 | -8,079 | 0 | 0 | **external calls** | 4 (3C/1L4) |  |
| Pendle | [`0xc503cb6f…`](https://etherscan.io/tx/0xc503cb6f653908693bc666cf764b7fbd18d25d1f2982bde550107e98c81559a0) | `swapSyForExactPt` ✓ `0x5b709f17` | 153,140 | 148,995 | +4,145 | 0 | 0 | **external calls** · under the floor | 10 (5S/3C/1L2/1L3) |  |
| Morpho | [`0x641b76f4…`](https://etherscan.io/tx/0x641b76f483f45a02815116bb7b0213530d0f2aee019b7cc4840a2d71a2940f0e) | *liquidator bot* `0xc2df23ef` | 721,933 | 707,735 | +14,198 | **0** (0.00%) | 0 | **external calls** · under the floor | 4 (3C) | **re-measured** — heuristic claimed 16.32% |
| Morpho | [`0xcd59750e…`](https://etherscan.io/tx/0xcd59750e91859ec6af4209c588997b61962dcec2aad6c549ef35324476870bdf) | `reallocate` ✓ `0x7299aa31` | 324,608 | 349,625 | -25,017 | **0** (0.00%) | 0 | **external calls** | 15 (10C) | **re-measured** — heuristic claimed 13.47% |
| Morpho | [`0x9a5fecc6…`](https://etherscan.io/tx/0x9a5fecc64422a0e8f8edb3b914eaed17cd675a09f1e2811a79d0cf0181893851) `heur` | *liquidator bot* `0x3f462f8f` | 2,661,881 | 2,335,360 | +326,521 | **299,521** (11.25%) | 76,521 | — | 8 (8C) |  |
| Morpho | [`0x80329618…`](https://etherscan.io/tx/0x80329618f5c5261829097e2a8a079c765c6ae0ce35f6d98e09a4d246a694c8bf) | `multicall` ✓ `0xac9650d8` | 380,049 | 343,154 | +36,895 | **9,895** (2.60%) | 0 | — | 18 (10S/4C/3L3/1L1) |  |
| Morpho | [`0x6ed1eaa3…`](https://etherscan.io/tx/0x6ed1eaa33eff8b3b992ee3fc55d5548d2072edd5f32ba437854d784dc9e62946) `heur` | *liquidator bot* `0xca8bd1f9` | 307,371 | 273,856 | +33,515 | **6,515** (2.12%) | 0 | — | 3 (3C) |  |
| Morpho | [`0x4e547494…`](https://etherscan.io/tx/0x4e547494fcf332b50465117a6467c8cb097787e4b54fd5b97ff6ff5cfec96ceb) | *flash-loan leverage* `0x642ba7a7` | 1,779,190 | 1,720,040 | +59,150 | **32,150** (1.81%) | 0 | — | 6 (3S/2C/1L1) |  |
| Morpho | [`0x16a0a31c…`](https://etherscan.io/tx/0x16a0a31c0547f2f35018c38f0c2fa3bdcf1320e6a75f998caaa957747e9dc568) `heur` | *flash-loan deleverage* `0x2f5066dd` | 1,312,558 | 1,577,595 | -265,037 | 0 | 0 | **external calls** | 1 (1C) |  |
| Morpho | [`0xb1bf36be…`](https://etherscan.io/tx/0xb1bf36beaf1aeeb69e575a1230468d917ef4646c6416ab465201bca70d8c7a72) | `multicall` ✓ `0xac9650d8` | 1,278,050 | 1,457,568 | -179,518 | 0 | 0 | **external calls** · too many writes | 84 (45S/22C/12L3/5L1) |  |
| Morpho | [`0x1c71eb76…`](https://etherscan.io/tx/0x1c71eb76549cc6a80467e06e8bc938b7fc1e67e9575c2aece8d98345243bb218) | *9-market reallocation* `0xeb7499cf` | 725,295 | 699,768 | +25,527 | 0 | 0 | **external calls** · under the floor | 1 (1C) | missed the 27,000 floor by 1,473 gas |
| Morpho | [`0x09fd0f6e…`](https://etherscan.io/tx/0x09fd0f6eb66388ce7cdc484b2020d300b5c6d519df89c5bddc73307d9e68bd80) `heur` | *liquidator bot* `0x1a28e979` | 619,024 | 733,358 | -114,334 | 0 | 0 | **external calls** | 2 (1C/1L3) |  |
| Morpho | [`0x482fb3b2…`](https://etherscan.io/tx/0x482fb3b2dfb2d336237a0112285a14d7847af91006e64e6fd442f11864360e9c) | *liquidator bot* `0xc35d5cb3` | 457,129 | 462,926 | -5,797 | 0 | 0 | **external calls** | 4 (4C) |  |
| Morpho | [`0x9a08a526…`](https://etherscan.io/tx/0x9a08a526e05f5fe827840f0ec4e3d1ce31906fa5a7a9bda7677098e4d78903df) `heur` | *flash-loan MEV bot* `0x03f00196` | 420,293 | 414,721 | +5,572 | 0 | 0 | **external calls** · under the floor | 5 (5C) |  |
| Morpho | [`0xbeffded8…`](https://etherscan.io/tx/0xbeffded8df725752edea428f171d8ec2a842dcdb645977ea9cf5bedba14ca414) | `reallocate` ✓ `0x7299aa31` | 416,744 | 451,366 | -34,622 | 0 | 0 | **external calls** | 18 (12C/6L3) |  |
| Morpho | [`0x1338ba16…`](https://etherscan.io/tx/0x1338ba16b0a7f61988caf43896fde0e32edac97cd7dab32bb6136bf9e77f0302) | *flash-loan MEV bot* `0x99999999` | 405,201 | 423,289 | -18,088 | 0 | 0 | **external calls** | 1 (1C) |  |
| Morpho | [`0xe520cf76…`](https://etherscan.io/tx/0xe520cf761e3fd61b115b0f31bd7f182a9cce43ec747b1aeaf77bf3457ebdc91f) | `multicall` ✓ `0xac9650d8` | 366,689 | 393,924 | -27,235 | 0 | 0 | **external calls** · too many writes | 24 (13S/6C/3L3/2L1) |  |
| Morpho | [`0x584c52d9…`](https://etherscan.io/tx/0x584c52d957c165432f32e42e0ebacc4683d0e6f9cb251d926060701f8f71322b) | *flash-loan liquidation* `0xeddba708` | 362,143 | 382,655 | -20,512 | 0 | 0 | **external calls** · too many writes | 8 (6S/2C) |  |
| Morpho | [`0x3dffb38b…`](https://etherscan.io/tx/0x3dffb38b03f52c07073a0ad32f336c5d3106640c2462f72204c9d6fe02534ed1) | `multicall` ✓ `0xac9650d8` | 360,323 | 371,754 | -11,431 | 0 | 0 | **external calls** | 8 (6C/2S) |  |
| Morpho | [`0x8e4616ac…`](https://etherscan.io/tx/0x8e4616acfaf812a41b471e139924a1bc906e03e8e1203760ae6117113682b760) | `multicall` ✓ `0xac9650d8` | 312,814 | 314,184 | -1,370 | 0 | 0 | **external calls** · too many writes | 18 (10S/4C/3L3/1L1) |  |
| Morpho | [`0x50305a21…`](https://etherscan.io/tx/0x50305a216cbeabbc02ad2262619090e91c6928ecc30def46af4eaab7bde99e9b) | `multicall` ✓ `0xac9650d8` | 179,644 | 183,470 | -3,826 | 0 | 0 | **external calls** | 5 (3C/2S) |  |
| Morpho | [`0x8d0c4018…`](https://etherscan.io/tx/0x8d0c40187a36dd2de2b64800b87d8db9b235624479d35202a11c2b2fb98fd76a) | `multicall` ✓ `0xac9650d8` | 157,756 | 161,582 | -3,826 | 0 | 0 | **external calls** | 5 (3C/2S) |  |
| Morpho | [`0xdc74e020…`](https://etherscan.io/tx/0xdc74e020e296fbb968edfc2ffd630bad47d557c71dabe901938315be6329c5c9) `heur` | *router* `0x374f435d` | 134,366 | 148,620 | -14,254 | 0 | 0 | **external calls** | 2 (2C) |  |
| Morpho | [`0x441cd851…`](https://etherscan.io/tx/0x441cd85183e88986305c0721c98bdd3c25edbe5ecc4578baeb964f09b8b42686) | `withdrawCollateral` ✓ `0x8720316d` | 132,478 | 158,129 | -25,651 | 0 | 0 | **external calls** | 8 (4S/2C/1L2/1L4) |  |
| Morpho | [`0x2d7cebfe…`](https://etherscan.io/tx/0x2d7cebfe726192fe692ccfa905b401fdedb108f5a3ecdf34aff20d2e77b3c320) `heur` | *router* `0x374f435d` | 122,812 | 126,798 | -3,986 | 0 | 0 | **external calls** | 3 (3C) |  |
| Morpho | [`0x4419f117…`](https://etherscan.io/tx/0x4419f1176b254fa8b1f0cb0daa7093b223379894701addb921fa02f00f373f8d) | `withdraw` ✓ `0x5c2bea49` | 111,792 | 142,279 | -30,487 | 0 | 0 | **external calls** · too many writes | 10 (6S/2C/1L2/1L4) |  |
| Morpho | [`0x8a27bff6…`](https://etherscan.io/tx/0x8a27bff6e7606ee0f89b63f01241c1a6a8d5cff37d726ee413df165b243c2f64) | `supply` ✓ `0xa99aad89` | 99,912 | 130,941 | -31,029 | 0 | 0 | **external calls** · too many writes | 10 (6S/2C/1L2/1L4) |  |
| World ID | [`0xa447c2d3…`](https://etherscan.io/tx/0xa447c2d3d0786a32f8b23c0f571e714e91d4d812b575d7bee27864c7c3e8c556) | `registerIdentities` ✓ `0x2217b211` | 298,629 | 263,051 | +35,578 | **8,578** (2.87%) | 0 | — | 4 (2S/1C/1L4) |  |
| World ID | [`0x36c09544…`](https://etherscan.io/tx/0x36c095445eb96f2ccaa2a2ec9544ac2cf72aa524c36c0ce23c0aecc2cf36b8b7) | `registerIdentities` ✓ `0x2217b211` | 285,261 | 263,075 | +22,186 | 0 | 0 | **external calls** · under the floor | 4 (2S/1C/1L4) |  |
| World ID | [`0x6b2fb8d3…`](https://etherscan.io/tx/0x6b2fb8d32c1fc927e5c37ae0ca52d17cb122b949ec0140f75a8212164911f494) | `registerIdentities` ✓ `0x2217b211` | 282,573 | 263,039 | +19,534 | 0 | 0 | **external calls** · under the floor | 4 (2S/1C/1L4) |  |
| World ID | [`0xb2f5ba58…`](https://etherscan.io/tx/0xb2f5ba588077025662acd44f62ead62c4dc6da4faa30890d658542aedcaef3c5) | `registerIdentities` ✓ `0x2217b211` | 281,457 | 263,051 | +18,406 | 0 | 0 | **external calls** · under the floor | 4 (2S/1C/1L4) |  |
| World ID | [`0xcd404a27…`](https://etherscan.io/tx/0xcd404a27462a9e60fdd5a17c024d758d809f860ad2da9f1709882d497276375a) | `registerIdentities` ✓ `0x2217b211` | 281,445 | 263,051 | +18,394 | 0 | 0 | **external calls** · under the floor | 4 (2S/1C/1L4) |  |
| World ID | [`0x04ca8194…`](https://etherscan.io/tx/0x04ca81943592e11ddbce6e4fac96c0f84debb12c8d972bd3f910dc8bf77274de) | `deleteIdentities` ✓ `0xea10fbbe` | 271,876 | 263,087 | +8,789 | 0 | 0 | **external calls** · under the floor | 4 (2S/1C/1L4) |  |
| World ID | [`0x2fee0848…`](https://etherscan.io/tx/0x2fee084888a10a8cf80c30b36bf511e8ba499e517d49dbc0ca2a97d4c4e160e6) | `deleteIdentities` ✓ `0xea10fbbe` | 271,816 | 263,075 | +8,741 | 0 | 0 | **external calls** · under the floor | 4 (2S/1C/1L4) |  |
| Ondo | [`0x4551339c…`](https://etherscan.io/tx/0x4551339cc87aabe9c57e492856f0af4c5cc00666be7bbfbd00ee8117d6b2327c) | *flash-loan MEV bot* `0x24c12020` | 1,799,146 | 1,531,651 | +267,495 | **240,495** (13.37%) | 17,495 | — | 11 (8S/2C/1L2) | not really Ondo — MEV bot, only 4 of 31 logs are Ondo |
| Ondo | [`0x10fdab16…`](https://etherscan.io/tx/0x10fdab165e2a37d70223b9546f80e9f7a248e1a0ccacd6638bbc66d565de8c35) | *Ondo instant mint* `0xd8780161` | 350,502 | 316,467 | +34,035 | **7,035** (2.01%) | 0 | — | 10 (7C/2S/1L3) |  |
| Ondo | [`0x197c9a14…`](https://etherscan.io/tx/0x197c9a14ed634444154e66fb75bfa167b874dfbba78b463c62f5b390063c8965) | *third-party aggregator* `0x146ee74d` | 3,290,733 | 3,209,798 | +80,935 | **53,935** (1.64%) | 0 | — | 1 (1C) | not really Ondo — aggregator, only 5 of 190 logs are Ondo |
| Ondo | [`0x089be390…`](https://etherscan.io/tx/0x089be390aab1f47562520e714e3d96b270c279b0c2a7b2876c933ee390a264b0) | *Ondo instant mint* `0xd8780161` | 447,366 | 413,235 | +34,131 | **7,131** (1.59%) | 0 | — | 10 (7C/2S/1L3) |  |
| Ondo | [`0x23ac6eda…`](https://etherscan.io/tx/0x23ac6eda7ddb46309b5d61628b5f24ced1b1c1cb608470d451f6dfb869fc6242) | *Ondo instant redeem* `0x22d4a175` | 432,419 | 400,729 | +31,690 | **4,690** (1.08%) | 0 | — | 10 (7C/2S/1L3) |  |
| Ondo | [`0xe16fb5bc…`](https://etherscan.io/tx/0xe16fb5bcb8d2d4d2a2a9d3c7b92d7388e8e8af784f771d3bd0f3250daeb54e42) | `settle` ✓ `0x13d79a0b` | 3,611,533 | 3,660,592 | -49,059 | 0 | 0 | **external calls** | 1 (1C) |  |
| Ondo | [`0xfccb2eda…`](https://etherscan.io/tx/0xfccb2eda50342893f6f1e93b0fb08df57dff595d17ac74414a688d1bfe3aaa78) | *third-party router* `0x81a794cb` | 819,503 | 843,764 | -24,261 | 0 | 0 | **external calls** · too many writes | 15 (9S/5C/1L4) |  |
| Ondo | [`0xd8ea9b21…`](https://etherscan.io/tx/0xd8ea9b2158d7bf3407ea618ad07e3f7debc096076e108f6c166ca9fa53e9ec37) | `settle` ✓ `0x13d79a0b` | 519,037 | 542,092 | -23,055 | 0 | 0 | **external calls** | 13 (5C/5L2/3S) |  |
| Euler | [`0x0a06e978…`](https://etherscan.io/tx/0x0a06e9783d4bc563d8b4674112b2dd427597c80a1095c377e6b88e307b31927c) | `batch` ✓ `0xc16ae7a4` | 548,011 | 513,820 | +34,191 | **7,191** (1.31%) | 0 | — | 29 (16S/5C/3L3/3L4/2L2) |  |
| Euler | [`0x575f2fb2…`](https://etherscan.io/tx/0x575f2fb2224cf67a0a3a8cca18def46bbf327eb72e1ac1414771dffd6f4a67f3) `heur` | *third-party contract* `0x1d23a9c4` | 3,036,470 | 3,319,285 | -282,815 | 0 | 0 | **external calls** | 4 (4C) |  |
| Euler | [`0x1fd83b06…`](https://etherscan.io/tx/0x1fd83b06274079376b85f63220bbfb16cdfe1279ad5b8d07bd613a68b7639ca3) `heur` | *third-party contract* `0x1d23a9c4` | 2,715,594 | 3,069,384 | -353,790 | 0 | 0 | **external calls** | 4 (4C) |  |
| Euler | [`0x7d1e2345…`](https://etherscan.io/tx/0x7d1e2345c39b884a16eb6b2e06c1c4a4177c5639fe1c91474a356c3eef04fd87) | *third-party contract* `0x3271ba8d` | 2,701,108 | 2,683,478 | +17,630 | 0 | 0 | **external calls** · under the floor | 7 (3C/3S/1L2) |  |
| Euler | [`0xf6e16544…`](https://etherscan.io/tx/0xf6e16544c8c04199f8318649d05ab68f3ddd4ea2c1f7dee9cab98a0301afd0ad) `heur` | `batch` ✓ `0xc16ae7a4` | 1,531,193 | 1,608,623 | -77,430 | 0 | 0 | **external calls** · too many writes | 152 (80S/33C/31L4/6L3/2L2) |  |
| Euler | [`0xc3543837…`](https://etherscan.io/tx/0xc3543837e22a84fb06798cb626d7a1cc716329fdf15fb675eafc587911090206) | `batch` ✓ `0xc16ae7a4` | 1,415,274 | 1,871,874 | -456,600 | 0 | 0 | **external calls** · too many writes | 158 (82S/37C/35L4/2L2/2L3) | worst result measured anywhere |
| Euler | [`0xa32effd3…`](https://etherscan.io/tx/0xa32effd3b31a02343d8cf4362c4fee2e806ea9dcf00fdea5eb1a2b054ae0ea4a) | `batch` ✓ `0xc16ae7a4` | 499,524 | 482,833 | +16,691 | 0 | 0 | **external calls** · under the floor | 29 (16S/5C/3L3/3L4/2L2) |  |
| Euler | [`0x10c755ee…`](https://etherscan.io/tx/0x10c755eea1865f9761e49f2e52dd700d8ecc4e0037057bb2ea05df24bb946095) | `batch` ✓ `0xc16ae7a4` | 203,620 | 244,538 | -40,918 | 0 | 0 | **external calls** · too many writes | 14 (8S/3C/2L4/1L2) |  |
| Chainlink | [`0xff646682…`](https://etherscan.io/tx/0xff6466828843a8e795e4b6ae1b29644a148141dd48784b4be99c58b0ad3be268) | `forward` ✓ `0x6fadcf72` | 716,016 | 718,282 | -2,266 | 0 | 0 | **external calls** | 1 (1C) |  |
| Chainlink | [`0x6fc803ec…`](https://etherscan.io/tx/0x6fc803ecc426f8d20c9bcbdcc6c8118a1d8f22395a9702a7145fd7586adf4d80) | `forward` ✓ `0x6fadcf72` | 183,165 | 185,665 | -2,500 | 0 | 0 | **external calls** | 1 (1C) |  |
| Chainlink | [`0x0937e5c9…`](https://etherscan.io/tx/0x0937e5c9c7070b119608b28b836efc1c435ce7327f78ff00fff7fcc9ac1eef5f) | `forward` ✓ `0x6fadcf72` | 182,433 | 184,933 | -2,500 | 0 | 0 | **external calls** | 1 (1C) |  |
| Chainlink | [`0x973a53f5…`](https://etherscan.io/tx/0x973a53f585cbf82e951c96c24b58f1639a88f2935e6964ef4ac084ea10fcd278) | `forward` ✓ `0x6fadcf72` | 145,183 | 147,547 | -2,364 | 0 | 0 | **external calls** | 1 (1C) |  |
| Chainlink | [`0x9da346d0…`](https://etherscan.io/tx/0x9da346d0b79f055e1ac769f289ec0d449c4a20711c86300fa9230e817e069c95) | `forward` ✓ `0x6fadcf72` | 145,015 | 147,379 | -2,364 | 0 | 0 | **external calls** | 1 (1C) |  |
| Chainlink | [`0x4b8d2ac4…`](https://etherscan.io/tx/0x4b8d2ac44a640f4ce2b9fb274f5d1e3760ca324395aeaa3774190e99e41a10c9) | `forward` ✓ `0x6fadcf72` | 136,406 | 138,725 | -2,319 | 0 | 0 | **external calls** | 1 (1C) |  |
| Chainlink | [`0xf200bdfd…`](https://etherscan.io/tx/0xf200bdfd609fdb4228a02f6279e2968da2fc4c27590a3eadf5371843c5b5c6d0) | `forward` ✓ `0x6fadcf72` | 136,394 | 138,713 | -2,319 | 0 | 0 | **external calls** | 1 (1C) |  |
| Chainlink | [`0xa676c243…`](https://etherscan.io/tx/0xa676c24374af6324558937b595e3a94fca0fb817823fd22a24ea0f783ebffc6c) | `forward` ✓ `0x6fadcf72` | 136,046 | 138,365 | -2,319 | 0 | 0 | **external calls** | 1 (1C) |  |

Update shorthand: `S` storage write, `C` call, `L0`–`L4` log with that many topics, `Cr` contract creation.

| Panther | [`0x63338b98…`](https://etherscan.io/tx/0x63338b98cb3c6bf3390a7f7dcb84e25766424b7a1c6444b4b1b3c89f9059d134) | *staking* `0x7f678334` | 207,193 | 226,374 | -19,181 | **0** (0.00%) | 0 | **external calls** · too many writes | 11 (8S/1L3/2C) | only Panther mainnet tx in 200k blocks |

| Ethena | [`0x5055ea7f…`](https://etherscan.io/tx/0x5055ea7ff088407138215ddbe45b9cedfc334cda7e791d0eba4061aea819015c) | *mint* `0x96eea750` | 241,268 | 192,569 | +48,699 | **21,699** (8.99%) | 0 | — | — | **900B order — 1 of 309 mints** |
| Ethena | [`0xb3a29f2b…`](https://etherscan.io/tx/0xb3a29f2bdcb1573d2f3b7d613a08415854df6ea60d356c9611248856474a8a21) | *mint* `0x96eea750` | 219,295 | 204,750 | +14,545 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0xae6a3e25…`](https://etherscan.io/tx/0xae6a3e25f711d88ab06a1729ec60acf771c33e83877b2ac7885661cfe0d4d253) | *mint* `0x96eea750` | 210,560 | 192,497 | +18,063 | **0** (0.00%) | 0 | under the floor | — | heuristic first claimed 36.96% |
| Ethena | [`0xd2475759…`](https://etherscan.io/tx/0xd2475759c71f278896ea7ff15ca213bb17dc9c3e0b29ddfdc8e1e01424995ae0) | *mint* `0x96eea750` | 208,058 | 190,820 | +17,238 | **0** (0.00%) | 0 | under the floor | — | heuristic first claimed 33.91% |
| Ethena | [`0x91e950a3…`](https://etherscan.io/tx/0x91e950a3e5497de1908db495bd8432a9068865314a8bba6e2ee3f800eb9c0e41) | *mint* `0x96eea750` | 208,046 | 190,808 | +17,238 | **0** (0.00%) | 0 | under the floor | — | heuristic first claimed 33.90% |
| Ethena | [`0x2bdf6644…`](https://etherscan.io/tx/0x2bdf664447b22de7c275becd81ff56775417e0544812417ee0dadc95232a6fd4) | *mint* `0x96eea750` | 207,927 | 192,581 | +15,346 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0xe6f15a94…`](https://etherscan.io/tx/0xe6f15a94892f1ec9a52263ba0a8869c081db57bc9c48558b31e9cc1e3609e126) | *mint* `0x96eea750` | 205,401 | 190,844 | +14,557 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0xba225f3e…`](https://etherscan.io/tx/0xba225f3ef9ad52f911771e967f5e81e83fc0cdebe5eb1cd996012e4aa543ef21) | *mint* `0x96eea750` | 202,207 | 187,626 | +14,581 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0xe67e1c84…`](https://etherscan.io/tx/0xe67e1c8420c330116b81cb6e9bd73d5016058267d390701dd8fab89e592877d0) | *mint* `0x96eea750` | 202,195 | 187,602 | +14,593 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0x1c4c1779…`](https://etherscan.io/tx/0x1c4c1779b4b03b196c1e2891be88fb9771f33931fc356597ae5be7f54433b264) | *mint* `0x96eea750` | 200,589 | 186,044 | +14,545 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0xaf1a2df7…`](https://etherscan.io/tx/0xaf1a2df790dc89d457abe7cfb14a5be4e6d32d92a547e2f991479dcbdd7321f3) | *mint* `0x96eea750` | 200,023 | 189,422 | +10,601 | **0** (0.00%) | 0 | under the floor | — | heuristic first claimed 34.99% |
| Ethena | [`0x0e4976bf…`](https://etherscan.io/tx/0x0e4976bf08241feb978fc57172e36ea04a066b0bd242c3952ce06fabfd03ade8) | *mint* `0x96eea750` | 200,023 | 189,422 | +10,601 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0x2b762d27…`](https://etherscan.io/tx/0x2b762d27b09d8f0b8a221a71bc3e3544e55b7c34d9a44edfd566fa5ebc739ed7) | *mint* `0x96eea750` | 200,011 | 189,446 | +10,565 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0xcfea9cfa…`](https://etherscan.io/tx/0xcfea9cfa79f814a7ff93a431c56dde3b9feeb10a86e8dde41c00ce3adf6d7ec2) | *mint* `0x96eea750` | 185,047 | 170,454 | +14,593 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0x9786d142…`](https://etherscan.io/tx/0x9786d142e2a872dda2f8af0c108635e8f638b9779b62170fd8692eb3ed633632) | *sUSDe* | 407,179 | 409,604 | -2,425 | **0** (0.00%) | 0 | **external calls** | — |  |
| Ethena | [`0xc371b418…`](https://etherscan.io/tx/0xc371b418f7a88c1bcbce3ebee77145ac28e6f9764569d11de55a2a86fc7fd0fb) | `cooldownShares` ✓ `0x9343d9e1` | 89,471 | 103,472 | -14,001 | **0** (0.00%) | 0 | **external calls** | — |  |
| Ethena | [`0xd6e74eef…`](https://etherscan.io/tx/0xd6e74eef135030ef66c96c65247c0595d81d8de790923139eabe419800752aba) | `cooldownShares` ✓ `0x9343d9e1` | 89,459 | 103,460 | -14,001 | **0** (0.00%) | 0 | **external calls** | — |  |
| Ethena | [`0x6f12cb87…`](https://etherscan.io/tx/0x6f12cb8706d5c2f6fa999ff37dd5728cd9530d0d9c256efc715800df45d72403) | `deposit` ✓ `0x6e553f65` | 88,423 | 99,316 | -10,893 | **0** (0.00%) | 0 | **external calls** | — | heuristic first claimed 3.94% |
| Ethena | [`0x20437c45…`](https://etherscan.io/tx/0x20437c4593fc6e80acdd78578e134c336cc0a1f827e56cb36a18e8becb5d62e0) | *sUSDe* | 84,342 | 83,451 | +891 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0xe9eebd35…`](https://etherscan.io/tx/0xe9eebd353a8623401f2106e17a73cf433f31c3f6ac45722ffe61c2f70965e5cd) `heur` | `deposit` ✓ `0x6e553f65` | 83,659 | 57,942 | +25,717 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0x7a4241aa…`](https://etherscan.io/tx/0x7a4241aa594bf958bdb4c5fa93ef04a12f6cc1f6b854584000349f75c809b11d) `heur` | `cooldownShares` ✓ `0x9343d9e1` | 72,371 | 65,909 | +6,462 | **0** (0.00%) | 0 | under the floor | — |  |
| Ethena | [`0x03c37967…`](https://etherscan.io/tx/0x03c37967a8003d273e3a8b8518689d304fd03c93d429754f0df4a66e663355af) `heur` | `deposit` ✓ `0x6e553f65` | 66,559 | 57,942 | +8,617 | **0** (0.00%) | 0 | under the floor | — |  |

| ERC-4337 EntryPoint | [`0x030b4fd3…`](https://etherscan.io/tx/0x030b4fd3776594fc57df6451e83b61e916554227a0c7208f2f6a039f9a2bc312) | `handleOps` | 1,694,622 | 201,100 | +1,493,522 | *1,466,522* (*86.54%*) | — | **suspect** | — | **SUSPECT** — see EntryPoint section |
| ERC-4337 EntryPoint | [`0xb752f16b…`](https://etherscan.io/tx/0xb752f16bd51240342af289dfffebd5276e28ec313eb12ba8bf4ad654794bd807) | `handleOps` | 1,103,781 | 186,440 | +917,341 | *890,341* (*80.66%*) | — | **suspect** | — | **SUSPECT** — reproduced twice, but see EntryPoint section |
| ERC-4337 EntryPoint | [`0x112f2b10…`](https://etherscan.io/tx/0x112f2b10d8e6fc37032a3103957c8324db6cee480bf030da56bbbcc5bec5816e) | `handleOps` | 2,137,036 | 2,205,725 | -68,689 | **0** (0.00%) | — | **external calls** | — | structurally identical to the 86.54% row |
| ERC-4337 EntryPoint | [`0x98949778…`](https://etherscan.io/tx/0x989497784755fae4ffaefe945aa8309269455dc966784e3d0a8a8224cf5c27a4) | `handleOps` | 1,141,959 | 1,208,050 | -66,091 | **0** (0.00%) | — | **external calls** | — |  |
| ERC-4337 EntryPoint | [`0xe64e4eb1…`](https://etherscan.io/tx/0xe64e4eb1fc3306f4eb081b65fc8b3bdf3e2e21c7478980fa9c13aa70540e8e2b) | `handleOps` | 177,986 | 228,198 | -50,212 | **0** (0.00%) | — | **external calls** | — |  |
| ERC-4337 EntryPoint | [`0xf177394e…`](https://etherscan.io/tx/0xf177394e40b6e52101c309fa9be07aa66711250e6e981c996c2080324a1a9c89) | `handleOps` | 177,986 | 228,174 | -50,188 | **0** (0.00%) | — | **external calls** | — |  |
| ERC-4337 EntryPoint | [`0x92f6ec5b…`](https://etherscan.io/tx/0x92f6ec5bede29c44849b7bd1f12f86ff4965022525ada840029eaa676c4ceedb) | `handleOps` | 165,688 | 207,785 | -42,097 | **0** (0.00%) | — | **external calls** | — | EntryPoint v0.6 |
| ERC-4337 EntryPoint | [`0x773e691e…`](https://etherscan.io/tx/0x773e691ea71b9e0b99e8599fa29e9ea9097c73de92e66d25bad564f17a264dad) | `handleOps` | 145,170 | 183,888 | -38,718 | **0** (0.00%) | — | **external calls** | — | EntryPoint v0.6 |

| Lido | [`0x69d40f3d…`](https://etherscan.io/tx/0x69d40f3d0e599963cd619a61cbf60a1c3e847e9f2f87796a139561b53cec4424) | `addSigningKeysOperatorBH` ✓ `0x805911ae` | 5,887,344 | 6,739,086 | -851,742 | **0** (0.00%) | 0 | too many writes | 306 (·/1C) | largest tx in survey; 306 writes |
| Lido | [`0x21fa690f…`](https://etherscan.io/tx/0x21fa690fbbbbefec96c3fb533b2e0b6a275e9f0bad3fa7f6c71ccec9de398461) | `addSigningKeysOperatorBH` ✓ `0x805911ae` | 5,887,248 | 6,739,014 | -851,766 | **0** (0.00%) | 0 | too many writes | 306 (·/1C) |  |
| Lido | [`0xf9a0c484…`](https://etherscan.io/tx/0xf9a0c48463e92a01347aadfefbf9349ec72858550a8fa162e894f61e9e99a499) | *oracle report* `0x11a78d23` | 1,729,395 | 1,682,837 | +46,558 | **19,558** (1.13%) | 0 | — | 9 (·/4C) | only Lido function that pays |
| Lido | [`0x1bad3438…`](https://etherscan.io/tx/0x1bad343834044681f393485bcf131863801ff082da4fe24a2095629a7332d517) | *oracle report* `0x11a78d23` | 1,721,688 | 1,675,339 | +46,349 | **19,349** (1.12%) | 0 | — | 9 (·/4C) |  |
| Lido | [`0x341d60b7…`](https://etherscan.io/tx/0x341d60b7870a57c2fa6d31a6935bfa143f0cff41685fa038661fd8721e7b4a92) | *registry* `0x8f73c5ae` | 978,934 | 1,038,649 | -59,715 | **0** (0.00%) | 0 | too many writes | 73 (·/37C) |  |
| Lido | [`0x0df2c733…`](https://etherscan.io/tx/0x0df2c7332dd34a84ee404631b5431648ecce7e791ddb93f37c3fd9a3ce7206c9) | *registry* `0x8f73c5ae` | 978,934 | 1,038,632 | -59,698 | **0** (0.00%) | 0 | too many writes | 73 (·/37C) | heuristic first claimed 25.84% |
| Lido | [`0x6e2453d1…`](https://etherscan.io/tx/0x6e2453d1b4b55a31ffb9fd94b38a6475c6fc68727e83edbc30078829274b4446) | `claimWithdrawals` ✓ `0xacf41e4d` | 261,492 | 275,614 | -14,122 | **0** (0.00%) | 0 | too many writes | 10 (·/2C) |  |
| Lido | [`0xdfbaead0…`](https://etherscan.io/tx/0xdfbaead0d3c07378a6265a5093d029987518e7c75ede86ceb7b41d7f6fead305) | `claimWithdrawals` ✓ `0xacf41e4d` | 261,468 | 275,626 | -14,158 | **0** (0.00%) | 0 | too many writes | 10 (·/2C) |  |
| Lido | [`0x07dfd955…`](https://etherscan.io/tx/0x07dfd9551205d17c73cd997e2ccf76708f60eae947999636a7e96b5f45f908f2) | *wstETH withdrawal* `0x7951b76f` | 258,886 | 276,945 | -18,059 | **0** (0.00%) | 0 | too many writes | 11 (·/3C) |  |
| Lido | [`0x8af05a00…`](https://etherscan.io/tx/0x8af05a00ef89fdbc3c3cd246e5274c935f7a45869cba96ebbfd1aaea451db50f) | `requestWithdrawals` ✓ `0xd6681042` | 215,719 | 225,836 | -10,117 | **0** (0.00%) | 0 | too many writes | 9 (·/1C) |  |
| Lido | [`0x954e6c05…`](https://etherscan.io/tx/0x954e6c05327ab8ac475eb27d646450b600826ed279ca5c99ca3ab90e1f953177) | `requestWithdrawals` ✓ `0xd6681042` | 215,683 | 225,752 | -10,069 | **0** (0.00%) | 0 | too many writes | 9 (·/1C) |  |
| Lido | [`0x9f518648…`](https://etherscan.io/tx/0x9f51864813f2f5744a5114d7345653ef2c1b8d87b9e479317020a14153e9b490) | `claimWithdrawals` ✓ `0xe3afe0a3` | 131,269 | 171,727 | -40,458 | **0** (0.00%) | 0 | too many writes | 26 (·/3C) |  |
| Lido | [`0x076a9688…`](https://etherscan.io/tx/0x076a9688c14bf3e1af823eb525fe3243176492d5c246567cbe6b5680bdef7844) | `claimWithdrawals` ✓ `0xe3afe0a3` | 81,977 | 80,590 | +1,387 | **0** (0.00%) | 0 | under the floor | 8 (·/1C) |  |


| **Pyth** | [`0x8874d5a5…`](https://etherscan.io/tx/0x8874d5a5dd22f4be257985b269f6c6ded6f28e08f0a572280d49d2db9a3a347a) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 279,375 | 113,369 | +166,006 | **139,006** (49.76%) | 0 | — | 12 | 2884B calldata. recovered from heuristic (claimed 65.79%) |
| **Pyth** | [`0xbf483557…`](https://etherscan.io/tx/0xbf4835576f7677f4d93a6f96fe75671c0dd19362652bb1cf5c8622cec1d54fb9) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 278,401 | 113,441 | +164,960 | **137,960** (49.55%) | 0 | — | 12 | 2884B calldata.  |
| **Pyth** | [`0x616ba1cd…`](https://etherscan.io/tx/0x616ba1cd828ee4f0e63a5be3e09df5d8ef28e0661e8c396f45658cdca477fea9) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 193,725 | 42,864 | +150,861 | **123,861** (63.94%) | 0 | — | 3 | 1636B calldata. **best** — 1 feed written; recovered from heuristic |
| **Pyth** | [`0xf8800e04…`](https://etherscan.io/tx/0xf8800e04ea63017f9a4ffe500a4399c72a5606f243230a351c5758828568757b) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 192,005 | 42,864 | +149,141 | **122,141** (63.61%) | 0 | — | 3 | 1636B calldata. 1 feed written |
| **Pyth** | [`0xa8ae7011…`](https://etherscan.io/tx/0xa8ae70119c42ea2fb646f18a81b58f4263ae9cce624e0d44301abee3960ba7d4) | `executeGovernanceInstruction` ✓ `0xb6ed701e` | 194,705 | 55,548 | +139,157 | **112,157** (57.60%) | 0 | — | 3 | 1028B calldata. governance instruction |
| **Pyth** | [`0xab298442…`](https://etherscan.io/tx/0xab2984424844b5a4120660896eddea19589458b583b7a7279b8d951ddb2e90db) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 195,034 | 113,393 | +81,641 | **54,641** (28.02%) | 0 | — | 12 | 2148B calldata. typical shape |
| **Pyth** | [`0x32913147…`](https://etherscan.io/tx/0x329131473471b39de257f0392204b6ddc7ac7560c94e8d2407d4d33590f420f9) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 194,986 | 113,357 | +81,629 | **54,629** (28.02%) | 0 | — | 12 | 2148B calldata.  |
| **Pyth** | [`0xfd6656e5…`](https://etherscan.io/tx/0xfd6656e58db697736c638a01a1abe0d3065a1b72627a1c9368b108e052cb03e3) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 195,031 | 113,405 | +81,626 | **54,626** (28.01%) | 0 | — | 12 | 2148B calldata.  |
| **Pyth** | [`0xacc36012…`](https://etherscan.io/tx/0xacc36012498cc4c26e3e79d5f6ee1d0bc579e2ccf03828f34c1b60f77c5e239c) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 194,992 | 113,369 | +81,623 | **54,623** (28.01%) | 0 | — | 12 | 2148B calldata.  |
| **Pyth** | [`0xe2795d24…`](https://etherscan.io/tx/0xe2795d246d11e909653880ca648a00e2b658b4c252dcf9bfd237cd41bab1112f) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 195,007 | 113,393 | +81,614 | **54,614** (28.01%) | 0 | — | 12 | 2148B calldata.  |
| **Pyth** | [`0x3862ae1b…`](https://etherscan.io/tx/0x3862ae1b964adcdc8932bfd4583854ad2fb749ed85cce0ae8d0e85440f41a571) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 195,004 | 113,393 | +81,611 | **54,611** (28.01%) | 0 | — | 12 | 2148B calldata.  |
| **Pyth** | [`0x90b34b5d…`](https://etherscan.io/tx/0x90b34b5df172d1d050f207ab34da927e4f81f7e710ae1edc1aa945e0fbc887d9) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 194,995 | 113,405 | +81,590 | **54,590** (28.00%) | 0 | — | 12 | 2148B calldata.  |
| **Pyth** | [`0x9f3e0810…`](https://etherscan.io/tx/0x9f3e08104c3a853674bb012ae9331fccb6f00dacd90c0661bf5d3de97079c7ee) | `updatePriceFeedsIfNecessary` ✓ `0xb9256d28` | 194,989 | 113,405 | +81,584 | **54,584** (27.99%) | 0 | — | 12 | 2148B calldata.  |

## Transactions that could not be measured at all

The tool produced no output and exited cleanly. Every one is a very large trace, so the surveys here systematically miss the biggest and most interesting transactions.

| protocol | tx | why |
|---|---|---|
| Safe | [`0x1cce897f…`](https://etherscan.io/tx/0x1cce897f987a2ca1cf11e83b94daeb0f116ef8bc4c8b06757332c33705bc54a8) | ~6.4M gas |
| Safe | [`0x62dede44…`](https://etherscan.io/tx/0x62dede445b69352285dc30558e87fa5c5b664b8157109625772e60655759872f) | ~5.9M gas |
| Safe | [`0xfde7c414…`](https://etherscan.io/tx/0xfde7c414c1bd53ec908140e72afe5eb34f717e5d8188a8231fcd6b55f4c280e9) | ~6.5M gas |
| EigenLayer | [`0x6508d5bf…`](https://etherscan.io/tx/0x6508d5bfc34439a4005d3c0e8967fe62cf65df140eae5171cb9faa35b8ccc4ac) | checkpoint proof, ~3.4M gas, 85 KB calldata |
| EigenLayer | [`0xe92e3dad…`](https://etherscan.io/tx/0xe92e3dadee2f12722936bb8fc0bf19527e305e4c51a6e63e9f10d481d067e23d) | checkpoint proof, oversized trace |

Four Morpho liquidations failed the real replay for other reasons and appear in the table above with `heur` numbers you should not trust: one sender is an EIP-7702 smart EOA the simulator rejects outright, two revert reproducibly part-way through the replay, and one was rate-limited. `MORPHO_CANDIDATES.md` has the detail.

## What to be careful about

- **16 of the results are `heur` fallbacks.** Ignore their savings figures. The three biggest apparent Morpho wins in this file (16.32%, 13.47%, 11.25%) are all fallbacks, and the one that was later re-measured properly dropped from 13.47% to 0%.
- **The five unmeasurable transactions are all large.** EigenLayer's real ceiling is unknown for this reason.
- **Two Ondo rows are mislabelled traffic**, flagged in the notes column — an MEV bot and an aggregator that happen to touch Ondo tokens. Ondo's own mint and redeem transactions save 1–2%.
- **Sample sizes are small per protocol** (7–26 transactions, drawn from a few thousand recent blocks). The direction of each result is solid; the exact percentages are not a population average.

Deeper write-ups: `PROTOCOL_SURVEY.md` (per protocol), `MORPHO_CANDIDATES.md` (Morpho and liquidations), `CALL_BLOCKED_CANDIDATES.md` (the external-call group).


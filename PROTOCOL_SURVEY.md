# Protocol survey — GasKiller candidates beyond Morpho

Measured with this repository's analyzer (`cargo run -- t <hash>`, EvmSketch backend) against Ethereum mainnet. Companion to `MORPHO_CANDIDATES.md` (Morpho) and `CALL_BLOCKED_CANDIDATES.md` (transactions that score zero because of external calls).

Every number in the results tables is analyzer output. Where a figure is my own arithmetic it says so. The appendix at the end indexes all 114 measured transactions by their 4-byte selector, separating keccak-verified function names from labels inferred only from emitted events.

## Headline: Railgun

**15 direct calls to the Railgun smart wallet, all trace-measured, all clearing both signature floors.** Schnorr savings range **65.57%–80.84%** (median 74.06%). BLS savings range 19.53%–64.86%.

In aggregate: 15,174,097 gas used on chain becomes 3,765,439 under GasKiller with Schnorr — a saving of **11,408,658 gas (75.2%)**.

Railgun is the only protocol where **every** transaction clears the BLS floor. Aave manages it on 2 of 9 borrows; across the remaining seven protocols, 0 of ~60 transactions cleared it.

### Why it works

Railgun verifies its ZK proofs and computes its Poseidon hashes **in its own contract code**, and leaves behind only commitment and nullifier slots. The diffs are `Store`-dominated with few or no calls — the best result has **11 stores, 3 logs and zero calls**, so 1,085,832 gas of work replays for 181,095.

Contrast the same protocol reached through its RelayAdapt contract. Its diff is 2 stores plus **3 calls**, and the first call is `0xd8ae136a` — the *same* `transact` function the direct transactions call. Replaying it re-runs the entire shielded transaction:

| | direct to smart wallet | via RelayAdapt |
|---|---:|---:|
| gas used | 484,303 – 1,615,755 | 523,496 / 1,120,780 |
| GasKiller cost | 139,722 – 317,840 | 533,777 / 1,116,659 |
| **Schnorr saving** | **65.6–80.8%** | **0%** |

All 3 RelayAdapt transactions analyzed scored zero.

**The good path is also the common path.** Of 704 successful Railgun transactions found in a 40,000-block window, **374 (53%) went directly to the smart wallet** and 273 went via RelayAdapt. That distinguishes Railgun from Privacy Pools (2 direct withdrawals in 17 days) and Chainlink (0 of 242 direct).

### Railgun results

| tx | gas used | GasKiller cost | Schnorr saved | BLS saved | state updates |
|---|---:|---:|---:|---:|---|
| [`0xaa357a48…`](https://etherscan.io/tx/0xaa357a4824001aed7173b3bc7d976f997fc7839def4c56ec070db97ec2121bc4) | 1,085,832 | 181,095 | **877,737** (80.84%) | 654,737 (60.30%) | `Store`×11, `Log1`×3 |
| [`0x32daef80…`](https://etherscan.io/tx/0x32daef80a0c5180dcf45b15130e10ecced485242f0d3305b30942ebeb5aec364) | 1,056,246 | 197,859 | **831,387** (78.71%) | 608,387 (57.60%) | `Store`×12, `Log1`×3 |
| [`0xe7e605b6…`](https://etherscan.io/tx/0xe7e605b6520aaf757f4531a88af4966866d86fee61ec06e93b3423b30a934adc) | 1,615,755 | 317,840 | **1,270,915** (78.66%) | 1,047,915 (64.86%) | `Store`×15, `Log1`×5, `Call`×2 |
| [`0xa2177825…`](https://etherscan.io/tx/0xa21778257ba78ce1354a4ff25a48ee07854e5e858f4facfe3e3c3893d9a1945f) | 1,561,573 | 312,286 | **1,222,287** (78.27%) | 999,287 (63.99%) | `Store`×16, `Log1`×5, `Call`×2 |
| [`0x73a1d71e…`](https://etherscan.io/tx/0x73a1d71e2cfa678b63c934c8c04eb6b6069de89c85faf16d0376241ecabfb0ec) | 1,097,058 | 213,396 | **856,662** (78.09%) | 633,662 (57.76%) | `Store`×14, `Log1`×3 |
| [`0xa3a010fa…`](https://etherscan.io/tx/0xa3a010fa890d910e669fba1ea7b681a4c1f464b754c5b7bff7d62ae15ae25e71) | 1,113,545 | 241,625 | **844,920** (75.88%) | 621,920 (55.85%) | `Store`×12, `Log1`×4, `Call`×2 |
| [`0x04736806…`](https://etherscan.io/tx/0x04736806756593b664eb29591a2ea046b2261c5a4fa1bdd1707a6dc5abf54f07) | 1,136,044 | 266,554 | **842,490** (74.16%) | 619,490 (54.53%) | `Store`×13, `Log1`×4, `Call`×2 |
| [`0x39a67a29…`](https://etherscan.io/tx/0x39a67a2964b675ccd1be1939c513b46f62cae71c3f320bece6acdaf71934b3ae) | 1,131,080 | 266,430 | **837,650** (74.06%) | 614,650 (54.34%) | `Store`×14, `Log1`×4, `Call`×2 |
| [`0xaf36e218…`](https://etherscan.io/tx/0xaf36e2187e095fa88299a8859632963594414b716f36aae541318d4ce71740e6) | 1,177,014 | 282,477 | **867,537** (73.71%) | 644,537 (54.76%) | `Store`×15, `Log1`×4, `Call`×2 |
| [`0x0376edd3…`](https://etherscan.io/tx/0x0376edd36b0c9211b1a38f41e5f1c04233478ca4dcf7d4852ef313deb1c57ade) | 724,515 | 168,652 | **528,863** (73.00%) | 305,863 (42.22%) | `Store`×10, `Log1`×2, `Call`×2 |
| [`0xab44777b…`](https://etherscan.io/tx/0xab44777b4e3a37169ea03163ec5d14dbf668bcb18219c3a95cc2313c2be55a3d) | 736,786 | 177,753 | **532,033** (72.21%) | 309,033 (41.94%) | `Store`×9, `Log1`×2, `Call`×2 |
| [`0x27ce0220…`](https://etherscan.io/tx/0x27ce022039ef027d473c61f3140da32c8e58f0a8daaee826e6f04a58744db447) | 742,727 | 185,294 | **530,433** (71.42%) | 307,433 (41.39%) | `Store`×10, `Log1`×2, `Call`×2 |
| [`0x3d54cfc3…`](https://etherscan.io/tx/0x3d54cfc375e626acd71432b2ab59a94bde2f6ee2490528ca8259417091e3ab97) | 752,878 | 197,812 | **528,066** (70.14%) | 305,066 (40.52%) | `Store`×11, `Log1`×2, `Call`×2 |
| [`0x6837bb8d…`](https://etherscan.io/tx/0x6837bb8d629ee0dce5f045acfc6c1e296df4abe1907897820bf621462dc23425) | 758,741 | 211,644 | **520,097** (68.55%) | 297,097 (39.16%) | `Store`×14, `Log1`×2, `Call`×2 |
| [`0x792a8e57…`](https://etherscan.io/tx/0x792a8e5783cec82cc456d83f0f1703a2bf71eafa05984a17896809bd952567cc) | 484,303 | 139,722 | **317,581** (65.57%) | 94,581 (19.53%) | `Log1`×3, `Store`×2, `Call`×2 |

By function:

- **private transfer / unshield** (`0xd8ae136a`) — 10 txs, 65.6–80.8% Schnorr, gas 484,303–1,615,755. **This selector was not verified**: no signature I constructed matched it, so the name is inferred from the `Nullified` and `Unshield` events it emits, not from the selector.
- **`shield`** (`0x044a40c3`, `shield(((bytes32,(uint8,address,uint256),uint120),(bytes32[3],bytes32))[])`) — 5 txs, 68.5–73.0% Schnorr, gas 724,515–758,741

On the RelayAdapt side, only 1 of the 3 completed a real replay (`0x2d754e6f…`, surplus −10,281). The other two fell back to heuristic — one of them reverted mid-replay against the smart wallet on all 3 attempts. All three scored zero either way, and their heuristic surpluses (+4,121 and +10,372) are well under the 27,000 floor, so the fallbacks do not change the conclusion.

## All protocols surveyed

Best trace-measured Schnorr saving per protocol, and whether the winning shape is reachable in practice.

| protocol | best measured | BLS | winning shape | how common |
|---|---:|---:|---|---|
| **Railgun** | **80.84%** | **60.30%** | direct call to smart wallet | 374 of 704 (53%) |
| **Aave** | **63.64%** | **yes (2 txs)** | direct `borrow` (any borrower); `withdraw` if multi-asset | ~25% of txs are direct borrows |
| Privacy Pools | 23.39% | 0% | direct call to pool | 2 in 17 days |
| Ether.fi | 18.99% | 0% | EtherFiAdmin oracle report | every ~4 hours |
| EigenLayer | 5.15% | 0% | EigenPod checkpoint proof | 4 of 5 pod checkpoints |
| Pendle | 3.94% | 0% | one large aggregator tx | marginal |
| World ID | 2.87% | 0% | direct, but verifier call dominates | all txs direct |
| Morpho | 2.60% | 0% | bundler multicall reallocation | marginal |
| Safe | 9.99% | 0% | 5-signature execTransaction | 2 of 565 (0.4%) |
| **Euler** | 1.31% | 0% | none — EVC router mandatory | **0 of 191 direct** |
| **Ondo** | 2.01% | 0% | mint/redeem via manager | 52 mint/burns in 60k blocks |
| Chainlink | 0% | 0% | none — all traffic via forwarder | 0 of 242 direct |
| Ethena | 8.99% | 0% | one 900-byte mint order | **1 of 309 mints (0.3%)** |
| ERC-4337 EntryPoint | *86.54%?* | *60%?* | **suspect — likely estimator false positive** | 2 of 8 measured |
| Panther | 0% | 0% | none — shielded pool is not on mainnet | 0 of 1 |

## What predicts a good candidate

Two conditions, both necessary. Every result above is explained by them. Aave adds a third dimension: the two conditions can hold for one *function* of a protocol and fail for another — `borrow` clears the floor 9 times out of 9 while `supply` and `repay` never do, because only the former runs a per-reserve health-factor loop. Screen per entry point, not per protocol.

**1. The expensive work must happen in the contract the transaction is sent to.** The sharpest evidence is Aave vs Euler: identical per-asset risk math, 63.64% vs 1.31%, because Aave permits direct `Pool` calls and Euler mandates the EVC router. The analyzer keeps a regular `CALL` as one instruction and re-executes it on replay, so anything inside it is never stripped (`crates/core/src/trace.rs:255`; `crates/core/src/prestate.rs` documents the same constraint for the net form). Railgun direct vs Railgun RelayAdapt is the same operation on the same contract scoring 80% vs 0% on this alone.

**2. The work must actually be computation, not bookkeeping.** Pendle satisfies condition 1 on its one direct-to-market swap and still scores ~2.7% surplus, because a swap is mostly storage writes. Ether.fi's `claimWithdraw` is the clearest case: its 43,868 gas per claim decomposes as ~25,000 of storage writes + 3,768 of logs + ~15,100 of calls, leaving ~0 for computation.

The winners are cryptographic: Railgun (ZK proofs + Poseidon in its own code), Privacy Pools (Poseidon over a merkle tree), Ether.fi (oracle report processing). Ordinary DeFi arithmetic — swap curves, share conversions, interest accrual — is never large enough to dominate a transaction's gas.

## Aave — `borrow` is a reliable candidate

20 transactions measured, all trace-based. **11 of 20 clear the floor.** What decides it is **which operation is called**, not who calls it:

| operation | measured | clear the floor | Schnorr range |
|---|---:|---:|---|
| **`borrow`** | 9 | **9** | **24.08% – 63.64%** (median 42.93%) |
| `withdraw` | 4 | 2 | 0% – 49.14% |
| `supply` / `supplyWithPermit` / `repay` | 6 | **0** | 0% |
| `liquidationCall` (via 3rd party) | 1 | 0 | 0% |

### Every borrow wins, and reserve count sets how much

| reserves held | gas used | GasKiller cost | Schnorr saved |
|---:|---:|---:|---:|
| 8 | 571,333 | 180,741 | **363,592** (63.64%) |
| 6 | 450,205 | 163,828 | **259,377** (57.61%) |
| 3 | 371,912 | 173,408 | **171,504** (46.11%) |
| 4 | 360,781 | 170,263 | **163,518** (45.32%) |
| 3 | 345,794 | 170,335 | **148,459** (42.93%) |
| 4 | 365,438 | 181,716 | **156,722** (42.89%) |
| 2 | 346,272 | 206,699 | **112,573** (32.51%) |
| 1 | 316,682 | 205,592 | **84,090** (26.55%) |
| 2 | 252,529 | 164,712 | **60,817** (24.08%) |

**GasKiller's cost barely moves: 163,828–206,699, while gas used runs 252,529–571,333.** The diff is effectively fixed and the computation on top varies — the same fixed-diff/variable-workload signature seen in Ether.fi's oracle report and World ID's identity registration.

Two borrows also clear the **BLS** floor (140,592 and 36,377) — only Railgun has managed that elsewhere in this survey.

### Why the operation decides it

`borrow` and `withdraw` must check the health factor, which walks every reserve the user holds and reads an oracle price per asset. Those reads are `STATICCALL`s — real gas that leaves nothing in the diff, and which the extractor ignores entirely (it tracks only `CALL`, `SSTORE`, `LOG*`, `CREATE`). `supply` and `repay` skip the check, because neither can worsen your own health factor.

Median gas by operation and reserve count, across 471 direct-to-Pool transactions, shows the loop clearly — and shows it is absent from `supply` and `repay`:

| operation | 0–1 reserves | 2–3 | 4+ |
|---|---:|---:|---:|
| `withdraw` | 176,867 | 278,596 | **360,926** |
| `borrow` | 303,803 | 284,079 | **387,564** |
| `supply` | 184,880 | 150,987 | 160,388 |
| `repay` | — | 152,022 | 154,444 |

Three `withdraw` calls with **identical diff shapes** (8 stores, 1 call, 3 logs) scored 49.14%, 0% and 0%. Their opcode profiles isolate the mechanism:

| | winner | zero | zero |
|---|---:|---:|---:|
| gas used | 408,792 | 167,589 | 183,126 |
| GasKiller cost | 180,921 | 160,020 | 166,710 |
| `STATICCALL` | **25** | 4 | 4 |
| `SLOAD` | 129 | 47 | 47 |
| total opcodes | 17,929 | 7,038 | 6,980 |
| user's active reserves | **5** | 1 | 0 |
| **Schnorr saved** | **49.14%** | 0 | 0 |

Six times the oracle reads, ~2.5× the opcodes, and the same 12 state updates at the end.

> **Revision.** An earlier version of this section, written from an 8-transaction sample that happened to contain only one `borrow`, claimed savings depend on the borrower's position rather than the operation, and that "single-asset users save nothing". Widening to 20 transactions reversed both: a 1-reserve `borrow` saves 26.55%, and 9 of 9 borrows clear the floor regardless of position size. Reserve count is a multiplier, not the gate.

### Practical significance

In the sampled 3,000-block window, direct-to-Pool `borrow` calls were **120 of 471** classified transactions (~25%). At a median 42.93%, that is a large recurring segment rather than an edge case — and Aave is the largest lending market on mainnet, so ~25% of its flow likely exceeds Railgun's entire volume in absolute gas.

This also corrects a conclusion drawn from Morpho. Morpho's 2.60% suggested lending was structurally poor for GasKiller; Aave shows that was about Morpho's design (storage-optimal, little per-transaction computation), not about lending as a category.

The one liquidation in range (1,165,207 gas) went through a third-party liquidator contract, so its diff is 6 updates dominated by one call — 0%. The Aave transaction with the most computation was reached through a wrapper.
## Safe — not worth pursuing, and the reason is a hard ceiling

12 transactions measured, plus 3 above ~5M gas that could not be measured at all (empty analyzer output on every attempt — a tool limit on very large struct-log traces).

**2 of 12 clear the floor**, both at 5 signatures:

| tx | sigs | gas used | GasKiller cost | surplus | Schnorr saved |
|---|---:|---:|---:|---:|---:|
| [`0xa9eca9f1…`](https://etherscan.io/tx/0xa9eca9f1a7075c8eb971e7ee2a1a3ee514cf3cf6b1077d6ddcc4f60f8ebf4eaa) | 5 | 104,612 | 67,158 | +37,454 | **10,454** (9.99%) |
| [`0x5c559278…`](https://etherscan.io/tx/0x5c5592787da61ec8c46326d93089c14e667276fab45703077168e72dcbbee99e) | 5 | 115,958 | 78,540 | +37,418 | **10,418** (8.98%) |
| [`0x8951b058…`](https://etherscan.io/tx/0x8951b058f41486ff7d9c5806d187af52f7d969ae69ddbb85c1e1be04171dae04) | — | 3,251,629 | 3,220,361 | +31,268 | **4,268** (0.13%) |
| [`0x5688590b…`](https://etherscan.io/tx/0x5688590bc26e704d720d7bb2185b195aabb310ba67dddd944f64509b1cf70513) | 3 | 322,338 | 297,128 | +25,210 | 0 |
| [`0x2666f99a…`](https://etherscan.io/tx/0x2666f99a3b1ad225cf8dcc40fbafb1959a6efb5bb3c561b3d2d5a3b242ca4a29) | 3 | 96,660 | 73,351 | +23,309 | 0 |
| [`0xb286eeb1…`](https://etherscan.io/tx/0xb286eeb15c4cb3445a73e275e89336f7563eb46c2e9ec97a82aeefb5a4485430) | 4 | 463,470 | 442,944 | +20,526 | 0 |
| [`0x909681b5…`](https://etherscan.io/tx/0x909681b5131c5ccc56e6f6791f152efe41cf270172c5a4645fc0067567bd8651) | 4 | 103,575 | 85,519 | +18,056 | 0 |
| [`0xf20b6df4…`](https://etherscan.io/tx/0xf20b6df4702b6537ba520316a35552b6d1da7b696e3c69e34986ed901402bd56) | 2 | 11,414,223 | 11,400,916 | +13,307 | 0 |
| [`0xf6ebba6b…`](https://etherscan.io/tx/0xf6ebba6ba0e5e5f003598b5e05efbb737fb5e00e6a1818e58d1e565316f96b74) | — | 214,221 | 202,578 | +11,643 | 0 |
| [`0x4e13a401…`](https://etherscan.io/tx/0x4e13a40100c52346b4e971697a347030ffd0cc5bd47d521e8b4a9baf1a984872) | 3 | 458,625 | 452,215 | +6,410 | 0 |
| [`0xb2b17e51…`](https://etherscan.io/tx/0xb2b17e51681c60f3d66aa112fa13f070c5ea76c466e794e3ba360c8b833b5a55) | 1 | 75,536 | 70,129 | +5,407 | 0 |
| [`0xdcc894ec…`](https://etherscan.io/tx/0xdcc894ec22dc799bd1cd8c24caa4bcd2a2d7f35ae2ba8ab7c431a07f76e5ba21) | — | 1,004,511 | 1,005,210 | -699 | 0 |

### Safe's own compressible work is capped by owner count

A Safe changes **one slot** — its nonce. Everything else it does is verify signatures (~3,000 gas per owner via `ecrecover`) and emit `ExecutionSuccess`. That caps its surplus regardless of what the transaction does:

| signatures | observed surplus |
|---:|---:|
| 5 | ~37,400 |
| 4 | 18,056 – 20,526 |
| 3 | 6,410 – 25,210 |
| 2 | 13,307 |
| 1 | 5,407 |

Against a 27,000 floor, **5 signatures is roughly break-even**. Signature counts across 565 sampled transactions (read from the `signatures` field length in the `execTransaction` calldata):

| signatures | txs | share |
|---:|---:|---:|
| 1 | 282 | 50% |
| 2 | 212 | 38% |
| 3 | 43 | 8% |
| 4 | 4 | 0.7% |
| 5 | 2 | 0.4% |

So Safe's addressable share is roughly **0.4% of its transactions at ~9–10% savings**. Compare Railgun: 53% of transactions at 65–81%.

### The two failures are separable, and only one is the call problem

The clearest single data point in this survey: `0xf20b6df4…` burned **11,414,223 gas** and its surplus is **13,307**. Its diff is *three* state updates. Everything is inside one call, so GasKiller captures 0.1%.

But solving that would not help Safe, because the gas inside the call is never Safe's:

- **Gas inside the call** belongs to whatever the Safe called — third-party contracts in every case checked (one sampled transaction touched 13 contracts including Morpho Blue and Uniswap V4). Those protocols get that saving whether a Safe sits in front of them or not.
- **Safe's own work** is signature verification, capped at ~37,000 by owner count.

Safe is therefore not a blocked candidate but a thin wrapper in front of other candidates. There is no integration to do with the Safe team that produces savings. The longlist calls it a "near-ideal match for aggregate-signature verification"; the measurements do not support that.

## Euler — the architectural counter-example to Aave

5 transactions measured (3 unmeasurable). Best result **1.31%**; the rest zero.

| tx | gas used | GasKiller cost | surplus | Schnorr saved | state updates |
|---|---:|---:|---:|---:|---:|
| [`0x0a06e978…`](https://etherscan.io/tx/0x0a06e9783d4bc563d8b4674112b2dd427597c80a1095c377e6b88e307b31927c) | 548,011 | 513,820 | +34,191 | **7,191** (1.31%) | 29 |
| [`0x7d1e2345…`](https://etherscan.io/tx/0x7d1e2345c39b884a16eb6b2e06c1c4a4177c5639fe1c91474a356c3eef04fd87) | 2,701,108 | 2,683,478 | +17,630 | 0 | 7 |
| [`0xa32effd3…`](https://etherscan.io/tx/0xa32effd3b31a02343d8cf4362c4fee2e806ea9dcf00fdea5eb1a2b054ae0ea4a) | 499,524 | 482,833 | +16,691 | 0 | 29 |
| [`0x10c755ee…`](https://etherscan.io/tx/0x10c755eea1865f9761e49f2e52dd700d8ecc4e0037057bb2ea05df24bb946095) | 203,620 | 244,538 | −40,918 | 0 | 14 |
| [`0xc3543837…`](https://etherscan.io/tx/0xc3543837e22a84fb06798cb626d7a1cc716329fdf15fb675eafc587911090206) | 1,415,274 | 1,871,874 | **−456,600** | 0 | 158 |

### This is an architectural problem, not a computation problem

Euler does the **same expensive work as Aave** — per-asset risk calculations on every borrow and repay, the exact mechanism that earns Aave's `borrow` 24–64%. The difference is entirely in how users reach it.

Euler V2 mandates the **Ethereum Vault Connector** as its coordination layer. All 20 vaults found in the scan report the same `EVC()` address (`0x0c9a3dd6b8f28529d72d7f9ce918d493519ee383`, discovered on-chain, not from documentation). Users reach vaults through `batch((address,address,uint256,bytes)[])` on the EVC (selector `0xc16ae7a4`, keccak-verified) or through a third-party contract:

| entry point | txs | share |
|---|---:|---:|
| EVC `batch(...)` | 58 | 57% |
| third-party contracts | 44 | 43% |
| **directly to a vault** | **0** | **0%** |

**Zero of 191 borrow/repay transactions called a vault directly.** So the risk calculation sits behind a `CALL` in every single Euler transaction, and the analyzer re-executes it rather than stripping it out.

The two protocols side by side:

| | Aave | Euler |
|---|---|---|
| per-asset risk math on borrow | yes | yes |
| direct calls to the lending contract | **52% of txs** | **0 of 191** |
| borrows clearing the Schnorr floor | 9 of 9 | 1 of 5 |
| best measured | **63.64%** | **1.31%** |

Same category, same computation, ~49× difference in outcome — decided by whether the protocol lets users call the contract that does the work. Euler belongs in `CALL_BLOCKED_CANDIDATES.md` rather than being written off: the compressible work exists, it is simply unreachable through the current encoder.

Two further observations:

- **Routing through a batcher compounds both problems.** `0xc3543837…` is a large EVC batch producing **158 state updates**, so GasKiller costs 1,871,874 to replay a 1,415,274-gas transaction — a surplus of −456,600, the worst measured anywhere in this survey.
- **Size does not help.** `0x7d1e2345…` burns 2,701,108 gas and yields a 7-update diff with +17,630 surplus. Everything is inside calls.

Three transactions could not be measured: one rate-limited, two reverting reproducibly at a WETH call (`0xC02aaA39…`) during replay on all 3 attempts each.
## Ondo — weak, and the volume is thin

8 transactions measured, all trace-based. Genuine Ondo mint/redeem operations land at **1.08–2.01%**.

| tx | what it is | gas used | GasKiller cost | Schnorr saved |
|---|---|---:|---:|---:|
| [`0x10fdab16…`](https://etherscan.io/tx/0x10fdab165e2a37d70223b9546f80e9f7a248e1a0ccacd6638bbc66d565de8c35) | mint/redeem via manager | 350,502 | 316,467 | **7,035** (2.01%) |
| [`0x089be390…`](https://etherscan.io/tx/0x089be390aab1f47562520e714e3d96b270c279b0c2a7b2876c933ee390a264b0) | mint/redeem via manager | 447,366 | 413,235 | **7,131** (1.59%) |
| [`0x23ac6eda…`](https://etherscan.io/tx/0x23ac6eda7ddb46309b5d61628b5f24ced1b1c1cb608470d451f6dfb869fc6242) | mint/redeem via manager | 432,419 | 400,729 | **4,690** (1.08%) |
| [`0xd8ea9b21…`](https://etherscan.io/tx/0xd8ea9b2158d7bf3407ea618ad07e3f7debc096076e108f6c166ca9fa53e9ec37) | via CowSwap settlement | 519,037 | 542,092 | 0 |
| [`0xfccb2eda…`](https://etherscan.io/tx/0xfccb2eda50342893f6f1e93b0fb08df57dff595d17ac74414a688d1bfe3aaa78) | third-party | 819,503 | 843,764 | 0 |
| [`0xe16fb5bc…`](https://etherscan.io/tx/0xe16fb5bcb8d2d4d2a2a9d3c7b92d7388e8e8af784f771d3bd0f3250daeb54e42) | third-party, 153 logs | 3,611,533 | 3,660,592 | 0 |

### Two results excluded as false attributions

Two transactions in the sample scored higher but **are not Ondo operations** and are excluded from the protocol's figure:

| tx | apparent | why excluded |
|---|---:|---|
| [`0x4551339c…`](https://etherscan.io/tx/0x4551339cc87aabe9c57e492856f0af4c5cc00666be7bbfbd00ee8117d6b2327c) | 13.37% | MEV/arb bot entering through a 133-byte contract; only **4 of 31 logs** are Ondo tokens, across 10 contracts |
| [`0x197c9a14…`](https://etherscan.io/tx/0x197c9a14ed634444154e66fb75bfa167b874dfbba78b463c62f5b390063c8965) | 1.64% | aggregator; only **5 of 190 logs** are Ondo tokens, across 25 contracts |

Their savings come from the bot's own routing work, not from anything Ondo does. Counting them would inflate Ondo to 13.37% on the strength of a transaction that merely touches USDY in passing. Worth noting as a general hazard when a protocol is discovered by token-log scanning: large third-party transactions incidentally touch the token and can dominate a small sample.

### Why it is weak

**Compliance checks are storage reads, not computation.** A tokenised-treasury mint is an allowlist lookup plus a mint — a handful of `SLOAD`s and 2–3 writes. The longlist anticipated "compliance logic" as heavy compute; it is not. This is the same reason the longlist itself correctly flags EAS as a weak fit: the write *is* the payload, with little surrounding computation to strip.

**Volume is thin.** 302 USDY transactions in a 60,000-block window (~8 days), of which 52 involve a mint or burn; OUSG had **8 transactions in total**. Even at a good percentage the absolute gas is orders of magnitude below Aave or Railgun.

On this evidence the RWA category looks unpromising as a whole — Midas, Centrifuge, Maple and Backed share the mint/redeem-plus-allowlist shape that produced 1–2% here.
## EigenLayer — the beacon-chain proofs are the only thing worth anything

13 transactions measured, 2 more too large for the tool to handle at all. **6 clear the floor**, the best at **5.15%**. Everything that isn't a beacon-chain proof loses money.

| tx | function | gas used | GasKiller cost | surplus | Schnorr saved | updates |
|---|---|---:|---:|---:|---:|---:|
| [`0x4ce09132…`](https://etherscan.io/tx/0x4ce0913231fcb4ac1351f81c6632f53165a5a80dc1f44fc7ea9c31021e5c7b04) | *checkpoint-proof verification* | 229,856 | 191,015 | +38,841 | **11,841** (5.15%) | 11 |
| [`0x03d50d7f…`](https://etherscan.io/tx/0x03d50d7f761256ff7ddcab530c0535f352ad8101d5d2912d67339e65e3c61b5d) | `forwardEigenPodCall` ✓ | 134,366 | 103,398 | +30,968 | **3,968** (2.95%) | 1 |
| [`0xbbc8abcb…`](https://etherscan.io/tx/0xbbc8abcb0eeb0f311b36ab4274e8629bd2b8c1395ffe79ffdebbcbf4536fbe29) | `forwardEigenPodCall` ✓ | 417,028 | 386,243 | +30,785 | **3,785** (0.91%) | 1 |
| [`0xc770332c…`](https://etherscan.io/tx/0xc770332c7e516902481cc309756988fbe452f58f89a1144713150fe083c1105e) | `forwardEigenPodCall` ✓ | 417,090 | 386,305 | +30,785 | **3,785** (0.91%) | 1 |
| [`0x95fa2d07…`](https://etherscan.io/tx/0x95fa2d07b579aaac9fa7e0fbd3545bcdb408c197956e0b4e34b9b29fa0f1cd9c) | *rewards claim* | 108,528 | 81,086 | +27,442 | **442** (0.41%) | 5 |
| [`0x6fa0fbbf…`](https://etherscan.io/tx/0x6fa0fbbf87072c3315c4801db2ac55a1e6c58b009d9ff07234e9c84b7a84801e) | *rewards claim* | 125,638 | 98,174 | +27,464 | **464** (0.37%) | 5 |
| [`0x065a18a2…`](https://etherscan.io/tx/0x065a18a2af1e2d80514ad79067794b7c34e92ba2c2754c5cb7ddb5157eb111b8) | `queueWithdrawals` ✓ | 439,868 | 469,142 | -29,274 | 0 | 24 |
| [`0x22b2c58c…`](https://etherscan.io/tx/0x22b2c58ced9f209d9ec2d21415436e9395067b377764d600927c8b54625f6d4f) | `startCheckpoint` ✓ | 76,444 | 52,517 | +23,927 | 0 | 4 |
| [`0x5ce19358…`](https://etherscan.io/tx/0x5ce19358229301efda44bc79908632dfc69c1609477d52438c3cd9add6950792) | `completeQueuedWithdrawal` ✓ | 190,728 | 190,504 | +224 | 0 | 17 |
| [`0xaa858fcd…`](https://etherscan.io/tx/0xaa858fcd781a6a5afa142ccd9a802d0df1e2f68292f0abf8546426cda3fe6644) | *rewards claim* | 156,150 | 135,165 | +20,985 | 0 | 8 |
| [`0xe5de910b…`](https://etherscan.io/tx/0xe5de910bc96de735b7bc040855d99c5b565baa1de8e14c373af2febb5076d19b) | `completeQueuedWithdrawals` ✓ | 183,232 | 185,594 | -2,362 | 0 | 17 |
| [`0xf09b9b69…`](https://etherscan.io/tx/0xf09b9b69b3640f7b018509045180218c1c7acff17eb665fbc40bb9ad2003ed0b) | `queueWithdrawals` ✓ | 440,373 | 471,942 | -31,569 | 0 | 24 |
| [`0xfb18ffe1…`](https://etherscan.io/tx/0xfb18ffe1164630f6d3a2c818f28404fcdc1782103870eadeaea75aaa0de4f0b9) | `completeQueuedWithdrawals` ✓ | 186,142 | 188,312 | -2,170 | 0 | 17 |

### The proof work is real but it sits on a fixed ceiling

The five EigenPod checkpoint transactions have surpluses of **23,927 / 30,785 / 30,785 / 30,968 / 38,841** regardless of how much gas they burn (76,444 up to 417,090). That is a fixed cost — verifying beacon-chain Merkle proofs and BLS-adjacent balance checks — sitting just above the 27,000 floor. Four of five clear it, but only by 4,000–12,000 gas.

Withdrawals go the other way. `queueWithdrawals` produces 24 state updates for a 440,000-gas transaction, so GasKiller costs **31,569 more** than the transaction did. Share accounting again: many writes, little compute.

### Two more things

- **The two biggest checkpoint proofs could not be measured** — ~3.4M gas and 85 KB of calldata, and the tool produces no output on traces that size. EigenLayer's real ceiling is therefore unknown, and probably above 5.15%.
- **129 of 136 pod transactions in the scan window came through Ether.fi's `EtherFiNodesManager` (`0x8b71140a…`) via `forwardEigenPodCall`**, not from pod owners directly. So most EigenPod traffic already has a wrapper in front of it — the same routing problem that sinks Euler, arriving from a different direction.
## ERC-4337 EntryPoint — a likely false-positive in the analyzer

**This section replaces an earlier conclusion that was wrong.** An initial run of eight
`handleOps` transactions reported 0% across the board and was written up as a structural
blocker. Most of those runs were heuristic fallbacks caused by RPC rate limiting. On clean
re-measurement the picture inverted, and what it exposes is a probable defect in the
estimator rather than a verdict on the protocol.

**Scope caveat.** These are transactions to the shared ERC-4337 EntryPoint
(`0x0000000071727De22E5E9d8BAf0edAc6f37da032` v0.7,
`0x5FF137D4b0FDCD49DcA30c7CF57E578a026d2789` v0.6). The wallet vendor behind each userOp
was never identified. ZeroDev specifically has **not** been measured.

### The measurements

Cleanly measured, zero rate-limit errors:

| tx | gas used | base estimate | Schnorr | updates |
|---|---:|---:|---:|---:|
| `0x030b4fd3776594fc57df6451e83b61e916554227a0c7208f2f6a039f9a2bc312` | 1,694,622 | **201,100** | **86.54%** | 8 (3C) |
| `0xb752f16bd51240342af289dfffebd5276e28ec313eb12ba8bf4ad654794bd807` | 1,103,781 | **186,440** | **80.66%** | 8 (3C) |
| `0x112f2b10d8e6fc37032a3103957c8324db6cee480bf030da56bbbcc5bec5816e` | 2,137,036 | 2,205,725 | 0% | 8 (3C) |
| `0x989497784755fae4ffaefe945aa8309269455dc966784e3d0a8a8224cf5c27a4` | 1,141,959 | 1,208,050 | 0% | 8 (3C) |
| `0xe64e4eb1fc3306f4eb081b65fc8b3bdf3e2e21c7478980fa9c13aa70540e8e2b` | 177,986 | 228,198 | 0% | 9 (4C) |
| `0xf177394e40b6e52101c309fa9be07aa66711250e6e981c996c2080324a1a9c89` | 177,986 | 228,174 | 0% | 9 (4C) |
| `0x92f6ec5bede29c44849b7bd1f12f86ff4965022525ada840029eaa676c4ceedb` | 165,688 | 207,785 | 0% | 9 (4C) |
| `0x773e691ea71b9e0b99e8599fa29e9ea9097c73de92e66d25bad564f17a264dad` | 145,170 | 183,888 | 0% | 9 (4C) |

`0xb752f16b…` was measured twice in independent runs and returned a base of **186,440 both
times** — the result is deterministic, not flaky.

### Why the two 80%+ results should not be trusted

`0x030b4fd3…` (86.54%) and `0x112f2b10…` (0%) are **structurally identical**: eight state
updates each, same ordering, same three calls, same selectors — `0x19822f7c`
(`validateUserOp`) into the smart account and `0x0042dc53` (`innerHandleOp`) back into
EntryPoint. Same shape, same protocol, same code path.

Yet one replays for 201,100 gas and the other for 2,205,725. A **10x divergence between
transactions with no structural difference** is not a property of the workload. It means
the replay is not doing the same work in both cases.

The most likely explanation is EntryPoint's own error handling. EntryPoint is *designed*
to catch a failing userOp rather than revert — it emits `UserOperationRevertReason` and
continues. During replay the state updates are applied before `innerHandleOp` runs, so the
userOp's inner execution can fail on already-consumed nonces or already-moved balances.
EntryPoint swallows that failure and returns successfully and cheaply. The estimator sees a
successful call and a small gas number, and records a large apparent saving.

If that is right, the implication is general and matters well beyond 4337: **any contract
that try/catches internally can produce a false saving under this estimator**, because a
silently-caught failure during replay is indistinguishable from cheap success. Nothing in
the current output flags it — no revert is reported, and the run is labelled
`measured via StateChangeHandler`.

This is unresolved. It needs someone with the estimator internals to confirm or refute,
and it should be treated as a **known-suspect result**, not as an 86% win to take to a
customer.

### What still holds

EntryPoint is immutable, shared, unowned infrastructure. Even if the savings were real,
there is no counterparty who can integrate it — not ZeroDev, not any wallet vendor, not a
customer. Volume is genuine (23,336 userOps in 10,000 blocks on v0.7) but unreachable
commercially.

## Ethena — one win in 309, and a lesson about the tool

**22 transactions measured, 19 of them cleanly. One real win: 8.99%.**

`EthenaMinting.mint` (`0x96eea750`, selector unverified) reduces to four storage writes,
one `Log4`, and two calls — a token `transferFrom` and a USDe `mint`. Both calls are plain
ERC-20 operations, so nothing large is hiding inside them. The measured surplus really is
Ethena's own compute: the EIP-712 check over the signed order, plus order bookkeeping.

Across 11 cleanly measured mints that surplus sits in a tight band of **10,565–18,063**,
against a 27,000 Schnorr floor. Ethena is not blocked by external calls and not blocked by
write-heaviness. It simply does not do enough work.

| tx | gas used | base estimate | surplus | Schnorr |
|---|---:|---:|---:|---:|
| `0x5055ea7ff088407138215ddbe45b9cedfc334cda7e791d0eba4061aea819015c` | 241,268 | 192,569 | **+48,699** | **8.99%** |
| `0xb3a29f2bdcb1573d2f3b7d613a08415854df6ea60d356c9611248856474a8a21` | 219,295 | 204,750 | +14,545 | 0% |
| `0xae6a3e25f711d88ab06a1729ec60acf771c33e83877b2ac7885661cfe0d4d253` | 210,560 | 192,497 | +18,063 | 0% |
| `0xd2475759c71f278896ea7ff15ca213bb17dc9c3e0b29ddfdc8e1e01424995ae0` | 208,058 | 190,820 | +17,238 | 0% |
| `0x91e950a3e5497de1908db495bd8432a9068865314a8bba6e2ee3f800eb9c0e41` | 208,046 | 190,808 | +17,238 | 0% |
| `0x2bdf664447b22de7c275becd81ff56775417e0544812417ee0dadc95232a6fd4` | 207,927 | 192,581 | +15,346 | 0% |
| `0xe6f15a94892f1ec9a52263ba0a8869c081db57bc9c48558b31e9cc1e3609e126` | 205,401 | 190,844 | +14,557 | 0% |
| `0xba225f3ef9ad52f911771e967f5e81e83fc0cdebe5eb1cd996012e4aa543ef21` | 202,207 | 187,626 | +14,581 | 0% |
| `0xe67e1c8420c330116b81cb6e9bd73d5016058267d390701dd8fab89e592877d0` | 202,195 | 187,602 | +14,593 | 0% |
| `0x1c4c1779b4b03b196c1e2891be88fb9771f33931fc356597ae5be7f54433b264` | 200,589 | 186,044 | +14,545 | 0% |
| `0xaf1a2df790dc89d457abe7cfb14a5be4e6d32d92a547e2f991479dcbdd7321f3` | 200,023 | 189,422 | +10,601 | 0% |
| `0x0e4976bf08241feb978fc57172e36ea04a066b0bd242c3952ce06fabfd03ade8` | 200,023 | 189,422 | +10,601 | 0% |
| `0x2b762d27b09d8f0b8a221a71bc3e3544e55b7c34d9a44edfd566fa5ebc739ed7` | 200,011 | 189,446 | +10,565 | 0% |
| `0xcfea9cfa79f814a7ff93a431c56dde3b9feeb10a86e8dde41c00ce3adf6d7ec2` | 185,047 | 170,454 | +14,593 | 0% |

Staking is lighter still and several rows are net-negative: `cooldownShares` (`0x9343d9e1`,
keccak-verified) at 89,471 gas measures a base of 103,472, and `deposit(uint256,address)`
(`0x6e553f65`, keccak-verified) at 88,423 gas measures 99,316. GasKiller would cost more
than the transaction does.

### The one win is a statistical freak, not a segment

`0x5055ea7f…` is real — measured via `StateChangeHandler`, and reproduced with identical
numbers on a second independent run. Its surplus is 48,699, more than triple the
next-highest. The difference is order size: **900 bytes of calldata against the usual 836**,
one extra collateral asset. The state-update shape is unchanged (8 updates, 2 calls), so
the extra ~33,000 gas is pure verification work that writes nothing — precisely what the
co-processor strips.

That sounds like a targetable segment until you count them. Over 30,000 blocks
(25,854,266–25,884,265), **309 mints: 308 at 836 bytes, exactly one at 900 bytes.**

**0.3%.** One freak order in a month. Ethena is a no.

### What this cost to find out — a warning about `heur` rows

Nine of the first twenty-one runs fell back to the heuristic estimator, and every one of
them reported a large, plausible-looking saving. All were false:

| tx | heuristic said | true measured value |
|---|---:|---:|
| `0xae6a3e25…` | 36.96% | **0%** |
| `0xaf1a2df7…` | 34.99% | **0%** |
| `0xd2475759…` | 33.91% | **0%** |
| `0x91e950a3…` | 33.90% | **0%** |
| `0xb3a29f2b…` | 30.95% | **0%** |
| `0x6f12cb87…` | 3.94% | **0%** |

The cause was not a defect in the transactions. It was **RPC rate limiting** — the provider
returned `HTTP 429, 50/second request limit reached` while replaying preceding
transactions, the replay aborted, and the tool silently substituted the heuristic. The
heuristic prices `Call` at zero gas, so it always errs toward *overstating* savings; on
`0xaf1a2df7…` it was wrong by 86,378 gas.

Two consequences worth acting on:

1. **Never report a `heur` row as a result.** Re-run it serially first. Every one of the six
   above collapsed to 0% on retry.
2. **The 16 `heur` rows carried in `ALL_TRANSACTION_ANALYSES.md` from earlier protocols are
   suspect for the same reason** and may be recoverable as real measurements. They were
   produced under the same throttling conditions, some of them while two analyzer batches
   ran concurrently.

## Panther — the target is not on this chain

Panther's MASP runs on Polygon. On Ethereum mainnet only the ZKP token is live, and its
traffic is overwhelmingly bridge deposits (`0xe3dec8fb` to the Polygon PoS bridge) and
router swaps — that is, the token *leaving* for the chain where the protocol actually runs.

Across 200,000 blocks (~27 days): the staking contract
`0xf4d06d72dACdD8393FA4eA72FdcC10049711F899` saw **one** transaction and RewardMaster
`0x347a58878D04951588741d4d16d54B742c7f60fC` saw **none**.

That single transaction (`0x63338b98cb3c6bf3390a7f7dcb84e25766424b7a1c6444b4b1b3c89f9059d134`,
selector `0x7f678334`) measured **0%** — 207,193 gas used against a 226,374 base estimate,
from eight storage writes, one `Log3`, and two calls. Staking bookkeeping, no computation.

Not a technology-fit failure. There is simply nothing on mainnet to target.

## Limitations

- **Opportunistic samples.** Railgun is the largest at 15 direct transactions from one 40,000-block window; the others are 7–20 transactions each. None is a systematic survey and none should be read as a population statistic.
- **Contract addresses came from web search, then were verified on chain.** For each protocol, event topic0 values and function selectors were matched against keccak hashes computed locally (pure-Python keccak, self-tested against the empty-string digest and the ERC-20 `Transfer` topic) before any conclusion was drawn. Unmatched selectors are named as unidentified rather than guessed.
- **Some transactions cannot be measured at all**, independent of rate limits: senders with EIP-7702 delegation designators are rejected by the simulator (`RejectCallerWithCode`), and some transactions reproducibly revert mid-replay. See `MORPHO_CANDIDATES.md`.
- **The prestate/net-form encoder was not exercised.** The CLI's `t` command uses the struct-log encoder. The net form collapses repeated writes to the same slot, which materially changes batched transactions — Ether.fi's `batchClaimWithdraw` has 65 struct-log stores but only 5 net changed slots. Everything measured here is the struct-log path.
- **Savings are the analyzer's cost model**, not on-chain outcomes; nothing was executed through a deployed GasKiller.
- **Some very large transactions cannot be measured.** Three Safe transactions (5.9M, 6.4M and 6.5M gas, each with 250–500 logs) produced empty analyzer output on every attempt, apparently a struct-log trace size limit. Size alone is not the trigger — an 11.4M-gas Safe transaction measured fine — but any survey weighted toward large batched transactions will be biased by this.
- **Aave's 25% traffic-share figure is a single-window estimate.** Direct-to-Pool borrows were 120 of 471 classified transactions in one 3,000-block window. Borrow share will vary with market conditions, and the 20 analyzed transactions are not a random sample of them — they were chosen to span the operation/reserve-count space.

## Appendix — every successfully measured transaction, by function

All 114 transactions that produced a number, labelled with the 4-byte selector actually present in their calldata.

A **✓** means the selector was confirmed by matching a locally computed keccak-256 hash of a candidate signature against the calldata prefix. An *italic* label was inferred from the transaction's emitted events only — the selector never matched any signature I could construct, and I did not decompile the callee. Treat those names as descriptions of observed effects, not verified function identities.

`heur` marks a heuristic fallback rather than a real `StateChangeHandler` replay; those figures are much softer.

| protocol | tx | function (4-byte) | gas used | GasKiller cost | Schnorr saved |
|---|---|---|---:|---:|---:|
| Railgun | [`0xaa357a48…`](https://etherscan.io/tx/0xaa357a4824001aed7173b3bc7d976f997fc7839def4c56ec070db97ec2121bc4) | `0xd8ae136a` *Railgun private transfer / unshield* | 1,085,832 | 181,095 | **877,737** (80.84%) |
| Railgun | [`0x32daef80…`](https://etherscan.io/tx/0x32daef80a0c5180dcf45b15130e10ecced485242f0d3305b30942ebeb5aec364) | `0xd8ae136a` *Railgun private transfer / unshield* | 1,056,246 | 197,859 | **831,387** (78.71%) |
| Railgun | [`0xe7e605b6…`](https://etherscan.io/tx/0xe7e605b6520aaf757f4531a88af4966866d86fee61ec06e93b3423b30a934adc) | `0xd8ae136a` *Railgun private transfer / unshield* | 1,615,755 | 317,840 | **1,270,915** (78.66%) |
| Railgun | [`0xa2177825…`](https://etherscan.io/tx/0xa21778257ba78ce1354a4ff25a48ee07854e5e858f4facfe3e3c3893d9a1945f) | `0xd8ae136a` *Railgun private transfer / unshield* | 1,561,573 | 312,286 | **1,222,287** (78.27%) |
| Railgun | [`0x73a1d71e…`](https://etherscan.io/tx/0x73a1d71e2cfa678b63c934c8c04eb6b6069de89c85faf16d0376241ecabfb0ec) | `0xd8ae136a` *Railgun private transfer / unshield* | 1,097,058 | 213,396 | **856,662** (78.09%) |
| Railgun | [`0xa3a010fa…`](https://etherscan.io/tx/0xa3a010fa890d910e669fba1ea7b681a4c1f464b754c5b7bff7d62ae15ae25e71) | `0xd8ae136a` *Railgun private transfer / unshield* | 1,113,545 | 241,625 | **844,920** (75.88%) |
| Railgun | [`0x04736806…`](https://etherscan.io/tx/0x04736806756593b664eb29591a2ea046b2261c5a4fa1bdd1707a6dc5abf54f07) | `0xd8ae136a` *Railgun private transfer / unshield* | 1,136,044 | 266,554 | **842,490** (74.16%) |
| Railgun | [`0x39a67a29…`](https://etherscan.io/tx/0x39a67a2964b675ccd1be1939c513b46f62cae71c3f320bece6acdaf71934b3ae) | `0xd8ae136a` *Railgun private transfer / unshield* | 1,131,080 | 266,430 | **837,650** (74.06%) |
| Railgun | [`0xaf36e218…`](https://etherscan.io/tx/0xaf36e2187e095fa88299a8859632963594414b716f36aae541318d4ce71740e6) | `0xd8ae136a` *Railgun private transfer / unshield* | 1,177,014 | 282,477 | **867,537** (73.71%) |
| Railgun | [`0x0376edd3…`](https://etherscan.io/tx/0x0376edd36b0c9211b1a38f41e5f1c04233478ca4dcf7d4852ef313deb1c57ade) | `0x044a40c3` `shield` ✓ | 724,515 | 168,652 | **528,863** (73.00%) |
| Railgun | [`0xab44777b…`](https://etherscan.io/tx/0xab44777b4e3a37169ea03163ec5d14dbf668bcb18219c3a95cc2313c2be55a3d) | `0x044a40c3` `shield` ✓ | 736,786 | 177,753 | **532,033** (72.21%) |
| Railgun | [`0x27ce0220…`](https://etherscan.io/tx/0x27ce022039ef027d473c61f3140da32c8e58f0a8daaee826e6f04a58744db447) | `0x044a40c3` `shield` ✓ | 742,727 | 185,294 | **530,433** (71.42%) |
| Railgun | [`0x3d54cfc3…`](https://etherscan.io/tx/0x3d54cfc375e626acd71432b2ab59a94bde2f6ee2490528ca8259417091e3ab97) | `0x044a40c3` `shield` ✓ | 752,878 | 197,812 | **528,066** (70.14%) |
| Railgun | [`0x6837bb8d…`](https://etherscan.io/tx/0x6837bb8d629ee0dce5f045acfc6c1e296df4abe1907897820bf621462dc23425) | `0x044a40c3` `shield` ✓ | 758,741 | 211,644 | **520,097** (68.55%) |
| Railgun | [`0x792a8e57…`](https://etherscan.io/tx/0x792a8e5783cec82cc456d83f0f1703a2bf71eafa05984a17896809bd952567cc) | `0xd8ae136a` *Railgun private transfer / unshield* | 484,303 | 139,722 | **317,581** (65.57%) |
| Railgun | [`0x6f534f5a…`](https://etherscan.io/tx/0x6f534f5af7fa26b66e0c6a5f49505f7125665e88b0d1af23efca86da88109f27) `heur` | `0x28223a77` *unidentified* | 1,297,603 | 1,287,231 | 0 |
| Railgun | [`0x7c731150…`](https://etherscan.io/tx/0x7c731150234add278ecae3ee9b6bcd35ee50435cc69bc47cce1a008e54c1f2ce) `heur` | `0x28223a77` *unidentified* | 1,120,780 | 1,116,659 | 0 |
| Railgun | [`0x2d754e6f…`](https://etherscan.io/tx/0x2d754e6f8a34058e5d07596e627cbc70e0c704279e9f745a4c0baef80389cca7) | `0x28223a77` *unidentified* | 523,496 | 533,777 | 0 |
| Aave | [`0xa7a8c34f…`](https://etherscan.io/tx/0xa7a8c34fee3795db241bcdf3e5c0b8b279dcb78f62ceee2f3530c9f38dd1ebec) | `0xa415bcad` `borrow` ✓ | 571,333 | 180,741 | **363,592** (63.64%) |
| Aave | [`0xa36db142…`](https://etherscan.io/tx/0xa36db1427faa3a36a7461bf00e80bc12bd07d9d7c87fbab54ee49bec92b9fec7) | `0xa415bcad` `borrow` ✓ | 450,205 | 163,828 | **259,377** (57.61%) |
| Aave | [`0x1b708eba…`](https://etherscan.io/tx/0x1b708eba95e753a84558374c488f1557907935272fabc2ffe91f6d8844c06370) | `0x69328dec` `withdraw` ✓ | 408,792 | 180,921 | **200,871** (49.14%) |
| Aave | [`0xa70a6bbd…`](https://etherscan.io/tx/0xa70a6bbd833f973defbcc4a0b1ad281f4708fa1960a39635d784a855a8fac9e1) | `0xa415bcad` `borrow` ✓ | 371,912 | 173,408 | **171,504** (46.11%) |
| Aave | [`0xc62d052c…`](https://etherscan.io/tx/0xc62d052c8385a546ab9f2f4d6a9fc194e45c3c9c7bf69e22454e38d9c55cb0cb) | `0xa415bcad` `borrow` ✓ | 360,781 | 170,263 | **163,518** (45.32%) |
| Aave | [`0xe73dcebf…`](https://etherscan.io/tx/0xe73dcebf75429339193a24c45a37e377948bf6f69fa77b8f50931d1e26ebeeea) | `0xa415bcad` `borrow` ✓ | 345,794 | 170,335 | **148,459** (42.93%) |
| Aave | [`0x000d4a32…`](https://etherscan.io/tx/0x000d4a32a076758c9ca1271e2909e2c4e1acaf988ff36ac8e8dc513f5fa64332) | `0xa415bcad` `borrow` ✓ | 365,438 | 181,716 | **156,722** (42.89%) |
| Aave | [`0x2cab2e0f…`](https://etherscan.io/tx/0x2cab2e0f215f22a30249a1198f6604bdcfcc131183aeaaa7c1257fb939ec239f) | `0xa415bcad` `borrow` ✓ | 346,272 | 206,699 | **112,573** (32.51%) |
| Aave | [`0xe65a59b9…`](https://etherscan.io/tx/0xe65a59b9f5d42dfd18018dd8c42850e6ea38c49d4a61718888c072e4f13dcc77) | `0xa415bcad` `borrow` ✓ | 316,682 | 205,592 | **84,090** (26.55%) |
| Aave | [`0xb33162e1…`](https://etherscan.io/tx/0xb33162e1275b4e14a4a0de3d327949fb520949cadd898d17718bb813547ca0fb) | `0xa415bcad` `borrow` ✓ | 252,529 | 164,712 | **60,817** (24.08%) |
| Aave | [`0xeff004bd…`](https://etherscan.io/tx/0xeff004bd1fb2851258ac5aa69997def87176f40716ecd4fe3d68904e1d6c3669) `heur` | `0x69328dec` `withdraw` ✓ | 211,120 | 160,699 | **23,421** (11.09%) |
| Aave | [`0x9287f808…`](https://etherscan.io/tx/0x9287f80894eeeacc8af236609401da9f14ce0e655cbb9748e0db2e405aeb2ef5) | `0x7e809076` *third-party liquidator bot* | 1,165,207 | 1,156,574 | 0 |
| Aave | [`0x324633c1…`](https://etherscan.io/tx/0x324633c1f5cf10b86a58aad0d48ac39673afd6ee24e2b391488ac84e4c6baaa0) | `0x02c205f0` `supplyWithPermit` ✓ | 206,695 | 207,356 | 0 |
| Aave | [`0x4eba5f8d…`](https://etherscan.io/tx/0x4eba5f8de29afdcac5703668d41a1f4bee734545dcdb37a129c6341b41c6ea44) | `0x617ba037` `supply` ✓ | 183,352 | 178,041 | 0 |
| Aave | [`0x9905c35e…`](https://etherscan.io/tx/0x9905c35e963d0c36e66b33d6f2319085daebf2920a8572ce26c01e201d0bbc16) | `0x69328dec` `withdraw` ✓ | 183,126 | 166,710 | 0 |
| Aave | [`0x52b373f5…`](https://etherscan.io/tx/0x52b373f5701425f48c42e3779527b6b7da51e7c764317c07cb40446377113649) | `0x02c205f0` `supplyWithPermit` ✓ | 179,856 | 179,187 | 0 |
| Aave | [`0xe41dd331…`](https://etherscan.io/tx/0xe41dd331b63a80add54ec1ef340f252c623538cfe35b69a65b190651aa636691) | `0x69328dec` `withdraw` ✓ | 167,589 | 160,020 | 0 |
| Aave | [`0x7f281bd6…`](https://etherscan.io/tx/0x7f281bd663289348bccaeb5c1616459ab424f557a65eaebfb603a1d445267ad4) | `0x617ba037` `supply` ✓ | 155,576 | 150,490 | 0 |
| Aave | [`0x055c232c…`](https://etherscan.io/tx/0x055c232ce489f003f64d5f05a5f77706c8a282f921d60a4264b5eeb21a6c6736) | `0x617ba037` `supply` ✓ | 147,675 | 142,925 | 0 |
| Aave | [`0x6b7563d1…`](https://etherscan.io/tx/0x6b7563d1e117aa90cc1e65b19b10ee525da3bdc89a26d0498a2a7084ab7867b1) | `0x573ade81` `repay` ✓ | 144,130 | 142,631 | 0 |
| Privacy Pools | [`0xe894abc7…`](https://etherscan.io/tx/0xe894abc79ca19fae8e3ef2a98b9570da0037d6f47ce531351002770d16ffe11f) | `0x30c0766d` `withdraw` ✓ | 587,069 | 422,768 | **137,301** (23.39%) |
| Privacy Pools | [`0x67deaa0e…`](https://etherscan.io/tx/0x67deaa0e50f0db65925f464c05168927db7ce336079334100aba5cdbeb701e64) | `0x30c0766d` `withdraw` ✓ | 576,177 | 430,907 | **118,270** (20.53%) |
| Privacy Pools | [`0x15082298…`](https://etherscan.io/tx/0x150822981204592e4cfa340ba2e63e607a1c6ded490b988f9a8bd37c1f2b46d0) | `0x8a44121e` `relay` ✓ | 620,208 | 632,053 | 0 |
| Privacy Pools | [`0x03ebad9a…`](https://etherscan.io/tx/0x03ebad9a10bc3dc5ad36613de80975b7ee8061d7fa74367f1a9aa04e77cc1524) | `0x8a44121e` `relay` ✓ | 604,245 | 616,114 | 0 |
| Privacy Pools | [`0xad4ac41d…`](https://etherscan.io/tx/0xad4ac41d7ad3ba9792d7c426631dba0d46a31e271f5105dbb6aa6df349c891a5) | `0x8a44121e` `relay` ✓ | 604,233 | 616,066 | 0 |
| Privacy Pools | [`0x4d8f00ee…`](https://etherscan.io/tx/0x4d8f00ee277c67f95049a43dfe604418d0a408fed40a6473bd5b154045c2e2e2) | `0x8a44121e` `relay` ✓ | 577,042 | 588,768 | 0 |
| Privacy Pools | [`0x2766c992…`](https://etherscan.io/tx/0x2766c992f22f5aec9bdfc1f16e394d4c4ed6b996bd002f64235b5234e9269cd2) | `0x8a44121e` `relay` ✓ | 558,846 | 571,008 | 0 |
| Privacy Pools | [`0x53e6375e…`](https://etherscan.io/tx/0x53e6375e0156f40d6917e8d48d26f0af6e1fd197d54d0042b96138dfc449660f) | `0x0efe6a8b` `deposit` ✓ | 393,615 | 402,757 | 0 |
| Privacy Pools | [`0x46a8ff4b…`](https://etherscan.io/tx/0x46a8ff4b10a52df709860a908f1749cd66523b5e6a3b18e7eeb901f8b7cd97eb) | `0xb6b55f25` `deposit` ✓ | 381,540 | 387,061 | 0 |
| Privacy Pools | [`0xb120146b…`](https://etherscan.io/tx/0xb120146b4dd84f30f7c44cfb6f9fb5fca7c0b10f051a6259edd5c5c7b40d9da6) | `0x71235b34` `ragequit` ✓ | 279,732 | 287,612 | 0 |
| Privacy Pools | [`0x0b27aa2c…`](https://etherscan.io/tx/0x0b27aa2ce30ee4e0f9e34a3f28537115d38c5a5fd499937b04fe437cfa3537f8) | `0x87bf00f0` `updateRoot` ✓ | 149,961 | 163,803 | 0 |
| Ether.fi | [`0xab780dc7…`](https://etherscan.io/tx/0xab780dc7c7c59d079462a88428b4e172c84a05eeec892195a02787a856214583) | `0x2e03931e` *EtherFiAdmin oracle-report execution* | 259,254 | 208,906 | **235,906** (23348.00%) |
| Ether.fi | [`0xdce4440b…`](https://etherscan.io/tx/0xdce4440b1e1cd6ae69eaa3331085bcbe38c6ab23c8d53401f7c49c63d7ec06ce) `heur` | `0x24fccdcf` `batchClaimWithdraw` ✓ | 205,965 | 174,295 | **201,295** (4670.00%) |
| Ether.fi | [`0xc3081e6f…`](https://etherscan.io/tx/0xc3081e6f850f214a3df7ccf069f0233a4d07fd08fede69f3e904c45e644789cf) | `0x146ee74d` *third-party aggregator* | 4,904,702 | 4,956,803 | **4,983,803** (0.00%) |
| Ether.fi | [`0x87b26753…`](https://etherscan.io/tx/0x87b26753364ee01e9c442fbf5c2a37443f28e00d3e34591cd4a9147611fe4eb7) | `0x24fccdcf` `batchClaimWithdraw` ✓ | 679,714 | 878,853 | **905,853** (0.00%) |
| Ether.fi | [`0x7ee0a346…`](https://etherscan.io/tx/0x7ee0a34642b5380eea636f02ce6ee35f109390a95bd2d19b8ff1794512829bb1) | `0x397a1b28` `requestWithdraw` ✓ | 193,099 | 198,205 | **225,205** (0.00%) |
| Ether.fi | [`0x4964cf5a…`](https://etherscan.io/tx/0x4964cf5adfaaf0e7d0d48d4cd2cc02bf792493653550293dbb22aa7909a18eda) | `0x397a1b28` `requestWithdraw` ✓ | 193,075 | 198,133 | **225,133** (0.00%) |
| Ether.fi | [`0xd6ec9fc5…`](https://etherscan.io/tx/0xd6ec9fc5c359167794fd0c692b578a346ae978b73ed8dc503aa5e6350ed84b63) | `0xb13acedd` `claimWithdraw` ✓ | 153,292 | 144,419 | **171,419** (0.00%) |
| Ether.fi | [`0xd14eb830…`](https://etherscan.io/tx/0xd14eb830b3308feb3dee287e11752e9400d4aaf8b1c85cacb04d1ef9321a2a84) | `0xea598cb0` `wrap` ✓ | 134,730 | 130,194 | **157,194** (0.00%) |
| Ether.fi | [`0x2cc9a112…`](https://etherscan.io/tx/0x2cc9a112936e0cfda1b4e9afd88ac5a861f2392cb9e20d2598f255a509b6d929) | `0xde0e9a3e` `unwrap` ✓ | 122,474 | 117,487 | **144,487** (0.00%) |
| Ether.fi | [`0xbe33787e…`](https://etherscan.io/tx/0xbe33787eb89c203cacaee0fb05da574bfef9c0aef4b80bcd8eed779a57f86222) | `0xd0e30db0` `deposit` ✓ | 120,178 | 128,056 | **155,056** (0.00%) |
| Safe | [`0xa9eca9f1…`](https://etherscan.io/tx/0xa9eca9f1a7075c8eb971e7ee2a1a3ee514cf3cf6b1077d6ddcc4f60f8ebf4eaa) | `0x6a761202` `execTransaction` ✓ | 104,612 | 67,158 | **10,454** (9.99%) |
| Safe | [`0x5c559278…`](https://etherscan.io/tx/0x5c5592787da61ec8c46326d93089c14e667276fab45703077168e72dcbbee99e) | `0x6a761202` `execTransaction` ✓ | 115,958 | 78,540 | **10,418** (8.98%) |
| Safe | [`0x8951b058…`](https://etherscan.io/tx/0x8951b058f41486ff7d9c5806d187af52f7d969ae69ddbb85c1e1be04171dae04) | `0x6a761202` `execTransaction` ✓ | 3,251,629 | 3,220,361 | **4,268** (0.13%) |
| Safe | [`0xf20b6df4…`](https://etherscan.io/tx/0xf20b6df4702b6537ba520316a35552b6d1da7b696e3c69e34986ed901402bd56) | `0x6a761202` `execTransaction` ✓ | 11,414,223 | 11,400,916 | 0 |
| Safe | [`0xdcc894ec…`](https://etherscan.io/tx/0xdcc894ec22dc799bd1cd8c24caa4bcd2a2d7f35ae2ba8ab7c431a07f76e5ba21) | `0x6a761202` `execTransaction` ✓ | 1,004,511 | 1,005,210 | 0 |
| Safe | [`0xb286eeb1…`](https://etherscan.io/tx/0xb286eeb15c4cb3445a73e275e89336f7563eb46c2e9ec97a82aeefb5a4485430) | `0x6a761202` `execTransaction` ✓ | 463,470 | 442,944 | 0 |
| Safe | [`0x4e13a401…`](https://etherscan.io/tx/0x4e13a40100c52346b4e971697a347030ffd0cc5bd47d521e8b4a9baf1a984872) | `0x6a761202` `execTransaction` ✓ | 458,625 | 452,215 | 0 |
| Safe | [`0x5688590b…`](https://etherscan.io/tx/0x5688590bc26e704d720d7bb2185b195aabb310ba67dddd944f64509b1cf70513) | `0x6a761202` `execTransaction` ✓ | 322,338 | 297,128 | 0 |
| Safe | [`0xf6ebba6b…`](https://etherscan.io/tx/0xf6ebba6ba0e5e5f003598b5e05efbb737fb5e00e6a1818e58d1e565316f96b74) | `0x6a761202` `execTransaction` ✓ | 214,221 | 202,578 | 0 |
| Safe | [`0x909681b5…`](https://etherscan.io/tx/0x909681b5131c5ccc56e6f6791f152efe41cf270172c5a4645fc0067567bd8651) | `0x6a761202` `execTransaction` ✓ | 103,575 | 85,519 | 0 |
| Safe | [`0x2666f99a…`](https://etherscan.io/tx/0x2666f99a3b1ad225cf8dcc40fbafb1959a6efb5bb3c561b3d2d5a3b242ca4a29) | `0x6a761202` `execTransaction` ✓ | 96,660 | 73,351 | 0 |
| Safe | [`0xb2b17e51…`](https://etherscan.io/tx/0xb2b17e51681c60f3d66aa112fa13f070c5ea76c466e794e3ba360c8b833b5a55) | `0x6a761202` `execTransaction` ✓ | 75,536 | 70,129 | 0 |
| Pendle | [`0x75aceefb…`](https://etherscan.io/tx/0x75aceefb54f607c4de446308146e0b84ff405ec56a6e5bc24ccb88b3b5397991) | `0xc685f647` *third-party aggregator* | 1,719,271 | 1,624,535 | **67,736** (3.94%) |
| Pendle | [`0xa2c6de73…`](https://etherscan.io/tx/0xa2c6de73e89c7bd707a9fe308d09c54eb16d2ddf9e3ff09f866d8a68e3948afd) | `0xc685f647` *third-party aggregator* | 1,183,159 | 1,157,967 | 0 |
| Pendle | [`0x8bd2985a…`](https://etherscan.io/tx/0x8bd2985af25143dbf0d3aa60f0e36bc50195d2702e4beeaf1576f8f4464200d7) | `0x60fc8466` *Pendle router action* | 1,018,948 | 1,020,097 | 0 |
| Pendle | [`0xa01ba5b6…`](https://etherscan.io/tx/0xa01ba5b6e8c9f78e7d4d324bd2b03da78a7d02cdf9fa852aa82b60dc400a586c) | `0xed48907e` *Pendle router action* | 768,674 | 762,631 | 0 |
| Pendle | [`0x90476689…`](https://etherscan.io/tx/0x9047668966adcd340cf4b69b727a17c5df6c6f34d4bad1994f9cf5331e2329f7) | `0xed48907e` *Pendle router action* | 531,449 | 524,603 | 0 |
| Pendle | [`0xa3ec8aea…`](https://etherscan.io/tx/0xa3ec8aea5dd4b01271f7dd979830a444ec2b3fa618f5e0afc550a2bd03af65a3) | `0xed48907e` *Pendle router action* | 370,167 | 359,084 | 0 |
| Pendle | [`0xec4d1fdf…`](https://etherscan.io/tx/0xec4d1fdf22d55bdde9d358e329af22a6c3345032ce969c79afb9d5dea8487556) | `0x594a88cc` *Pendle router action* | 205,329 | 213,408 | 0 |
| Pendle | [`0xc503cb6f…`](https://etherscan.io/tx/0xc503cb6f653908693bc666cf764b7fbd18d25d1f2982bde550107e98c81559a0) | `0x5b709f17` `swapSyForExactPt` ✓ | 153,140 | 148,995 | 0 |
| World ID | [`0xa447c2d3…`](https://etherscan.io/tx/0xa447c2d3d0786a32f8b23c0f571e714e91d4d812b575d7bee27864c7c3e8c556) | `0x2217b211` `registerIdentities` ✓ | 298,629 | 263,051 | **8,578** (2.87%) |
| World ID | [`0x36c09544…`](https://etherscan.io/tx/0x36c095445eb96f2ccaa2a2ec9544ac2cf72aa524c36c0ce23c0aecc2cf36b8b7) | `0x2217b211` `registerIdentities` ✓ | 285,261 | 263,075 | 0 |
| World ID | [`0x6b2fb8d3…`](https://etherscan.io/tx/0x6b2fb8d32c1fc927e5c37ae0ca52d17cb122b949ec0140f75a8212164911f494) | `0x2217b211` `registerIdentities` ✓ | 282,573 | 263,039 | 0 |
| World ID | [`0xb2f5ba58…`](https://etherscan.io/tx/0xb2f5ba588077025662acd44f62ead62c4dc6da4faa30890d658542aedcaef3c5) | `0x2217b211` `registerIdentities` ✓ | 281,457 | 263,051 | 0 |
| World ID | [`0xcd404a27…`](https://etherscan.io/tx/0xcd404a27462a9e60fdd5a17c024d758d809f860ad2da9f1709882d497276375a) | `0x2217b211` `registerIdentities` ✓ | 281,445 | 263,051 | 0 |
| World ID | [`0x04ca8194…`](https://etherscan.io/tx/0x04ca81943592e11ddbce6e4fac96c0f84debb12c8d972bd3f910dc8bf77274de) | `0xea10fbbe` `deleteIdentities` ✓ | 271,876 | 263,087 | 0 |
| World ID | [`0x2fee0848…`](https://etherscan.io/tx/0x2fee084888a10a8cf80c30b36bf511e8ba499e517d49dbc0ca2a97d4c4e160e6) | `0xea10fbbe` `deleteIdentities` ✓ | 271,816 | 263,075 | 0 |
| Morpho | [`0x80329618…`](https://etherscan.io/tx/0x80329618f5c5261829097e2a8a079c765c6ae0ce35f6d98e09a4d246a694c8bf) | `0xac9650d8` `multicall` ✓ | 380,049 | 343,154 | **9,895** (2.60%) |
| Morpho | [`0x4e547494…`](https://etherscan.io/tx/0x4e547494fcf332b50465117a6467c8cb097787e4b54fd5b97ff6ff5cfec96ceb) | `0x642ba7a7` *flash-loan leverage* | 1,779,190 | 1,720,040 | **32,150** (1.81%) |
| Morpho | [`0x16a0a31c…`](https://etherscan.io/tx/0x16a0a31c0547f2f35018c38f0c2fa3bdcf1320e6a75f998caaa957747e9dc568) `heur` | `0x2f5066dd` *flash-loan deleverage* | 1,312,558 | 1,577,595 | 0 |
| Morpho | [`0xb1bf36be…`](https://etherscan.io/tx/0xb1bf36beaf1aeeb69e575a1230468d917ef4646c6416ab465201bca70d8c7a72) | `0xac9650d8` `multicall` ✓ | 1,278,050 | 1,457,568 | 0 |
| Morpho | [`0x1c71eb76…`](https://etherscan.io/tx/0x1c71eb76549cc6a80467e06e8bc938b7fc1e67e9575c2aece8d98345243bb218) | `0xeb7499cf` *9-market reallocation* | 725,295 | 699,768 | 0 |
| Morpho | [`0x09fd0f6e…`](https://etherscan.io/tx/0x09fd0f6eb66388ce7cdc484b2020d300b5c6d519df89c5bddc73307d9e68bd80) `heur` | `0x1a28e979` *liquidator bot* | 619,024 | 733,358 | 0 |
| Morpho | [`0x9a08a526…`](https://etherscan.io/tx/0x9a08a526e05f5fe827840f0ec4e3d1ce31906fa5a7a9bda7677098e4d78903df) `heur` | `0x03f00196` *flash-loan MEV bot* | 420,293 | 414,721 | 0 |
| Morpho | [`0xbeffded8…`](https://etherscan.io/tx/0xbeffded8df725752edea428f171d8ec2a842dcdb645977ea9cf5bedba14ca414) | `0x7299aa31` `reallocate` ✓ | 416,744 | 451,366 | 0 |
| Morpho | [`0x1338ba16…`](https://etherscan.io/tx/0x1338ba16b0a7f61988caf43896fde0e32edac97cd7dab32bb6136bf9e77f0302) | `0x99999999` *flash-loan MEV bot* | 405,201 | 423,289 | 0 |
| Morpho | [`0xe520cf76…`](https://etherscan.io/tx/0xe520cf761e3fd61b115b0f31bd7f182a9cce43ec747b1aeaf77bf3457ebdc91f) | `0xac9650d8` `multicall` ✓ | 366,689 | 393,924 | 0 |
| Morpho | [`0x3dffb38b…`](https://etherscan.io/tx/0x3dffb38b03f52c07073a0ad32f336c5d3106640c2462f72204c9d6fe02534ed1) | `0xac9650d8` `multicall` ✓ | 360,323 | 371,754 | 0 |
| Morpho | [`0xcd59750e…`](https://etherscan.io/tx/0xcd59750e91859ec6af4209c588997b61962dcec2aad6c549ef35324476870bdf) | `0x7299aa31` `reallocate` ✓ | 324,608 | 349,625 | 0 |
| Morpho | [`0x8e4616ac…`](https://etherscan.io/tx/0x8e4616acfaf812a41b471e139924a1bc906e03e8e1203760ae6117113682b760) | `0xac9650d8` `multicall` ✓ | 312,814 | 314,184 | 0 |
| Morpho | [`0x50305a21…`](https://etherscan.io/tx/0x50305a216cbeabbc02ad2262619090e91c6928ecc30def46af4eaab7bde99e9b) | `0xac9650d8` `multicall` ✓ | 179,644 | 183,470 | 0 |
| Morpho | [`0x8d0c4018…`](https://etherscan.io/tx/0x8d0c40187a36dd2de2b64800b87d8db9b235624479d35202a11c2b2fb98fd76a) | `0xac9650d8` `multicall` ✓ | 157,756 | 161,582 | 0 |
| Morpho | [`0xdc74e020…`](https://etherscan.io/tx/0xdc74e020e296fbb968edfc2ffd630bad47d557c71dabe901938315be6329c5c9) `heur` | `0x374f435d` *router* | 134,366 | 148,620 | 0 |
| Morpho | [`0x441cd851…`](https://etherscan.io/tx/0x441cd85183e88986305c0721c98bdd3c25edbe5ecc4578baeb964f09b8b42686) | `0x8720316d` `withdrawCollateral` ✓ | 132,478 | 158,129 | 0 |
| Morpho | [`0x2d7cebfe…`](https://etherscan.io/tx/0x2d7cebfe726192fe692ccfa905b401fdedb108f5a3ecdf34aff20d2e77b3c320) `heur` | `0x374f435d` *router* | 122,812 | 126,798 | 0 |
| Morpho | [`0x4419f117…`](https://etherscan.io/tx/0x4419f1176b254fa8b1f0cb0daa7093b223379894701addb921fa02f00f373f8d) | `0x5c2bea49` `withdraw` ✓ | 111,792 | 142,279 | 0 |
| Morpho | [`0x8a27bff6…`](https://etherscan.io/tx/0x8a27bff6e7606ee0f89b63f01241c1a6a8d5cff37d726ee413df165b243c2f64) | `0xa99aad89` `supply` ✓ | 99,912 | 130,941 | 0 |
| Chainlink | [`0xff646682…`](https://etherscan.io/tx/0xff6466828843a8e795e4b6ae1b29644a148141dd48784b4be99c58b0ad3be268) | `0x6fadcf72` `forward` ✓ | 716,016 | 718,282 | 0 |
| Chainlink | [`0x6fc803ec…`](https://etherscan.io/tx/0x6fc803ecc426f8d20c9bcbdcc6c8118a1d8f22395a9702a7145fd7586adf4d80) | `0x6fadcf72` `forward` ✓ | 183,165 | 185,665 | 0 |
| Chainlink | [`0x0937e5c9…`](https://etherscan.io/tx/0x0937e5c9c7070b119608b28b836efc1c435ce7327f78ff00fff7fcc9ac1eef5f) | `0x6fadcf72` `forward` ✓ | 182,433 | 184,933 | 0 |
| Chainlink | [`0x973a53f5…`](https://etherscan.io/tx/0x973a53f585cbf82e951c96c24b58f1639a88f2935e6964ef4ac084ea10fcd278) | `0x6fadcf72` `forward` ✓ | 145,183 | 147,547 | 0 |
| Chainlink | [`0x9da346d0…`](https://etherscan.io/tx/0x9da346d0b79f055e1ac769f289ec0d449c4a20711c86300fa9230e817e069c95) | `0x6fadcf72` `forward` ✓ | 145,015 | 147,379 | 0 |
| Chainlink | [`0x4b8d2ac4…`](https://etherscan.io/tx/0x4b8d2ac44a640f4ce2b9fb274f5d1e3760ca324395aeaa3774190e99e41a10c9) | `0x6fadcf72` `forward` ✓ | 136,406 | 138,725 | 0 |
| Chainlink | [`0xf200bdfd…`](https://etherscan.io/tx/0xf200bdfd609fdb4228a02f6279e2968da2fc4c27590a3eadf5371843c5b5c6d0) | `0x6fadcf72` `forward` ✓ | 136,394 | 138,713 | 0 |
| Chainlink | [`0xa676c243…`](https://etherscan.io/tx/0xa676c24374af6324558937b595e3a94fca0fb817823fd22a24ea0f783ebffc6c) | `0x6fadcf72` `forward` ✓ | 136,046 | 138,365 | 0 |

### Verified signatures in full

Selectors above marked ✓, with the signature whose keccak hash matched:

| 4-byte | signature |
|---|---|
| `0x02c205f0` | `supplyWithPermit(address,uint256,address,uint16,uint256,uint8,bytes32,bytes32)` |
| `0x044a40c3` | `shield(((bytes32,(uint8,address,uint256),uint120),(bytes32[3],bytes32))[])` |
| `0x0efe6a8b` | `deposit(address,uint256,uint256)` |
| `0x2217b211` | `registerIdentities(uint256[8],uint256,uint32,uint256[],uint256)` |
| `0x24fccdcf` | `batchClaimWithdraw(uint256[])` |
| `0x30c0766d` | `withdraw((address,bytes),(uint256[2],uint256[2][2],uint256[2],uint256[8]))` |
| `0x397a1b28` | `requestWithdraw(address,uint256)` |
| `0x573ade81` | `repay(address,uint256,uint256,address)` |
| `0x5b709f17` | `swapSyForExactPt(address,uint256,bytes)` |
| `0x5c2bea49` | `withdraw((address,address,address,address,uint256),uint256,uint256,address,address)` |
| `0x617ba037` | `supply(address,uint256,address,uint16)` |
| `0x69328dec` | `withdraw(address,uint256,address)` |
| `0x6a761202` | `execTransaction(address,uint256,bytes,uint8,uint256,uint256,uint256,address,address,bytes)` |
| `0x6fadcf72` | `forward(address,bytes)` |
| `0x71235b34` | `ragequit((uint256[2],uint256[2][2],uint256[2],uint256[4]))` |
| `0x7299aa31` | `reallocate(((address,address,address,address,uint256),uint256)[])` |
| `0x8720316d` | `withdrawCollateral((address,address,address,address,uint256),uint256,address,address)` |
| `0x87bf00f0` | `updateRoot(uint256,string)` |
| `0x8a44121e` | `relay((address,bytes),(uint256[2],uint256[2][2],uint256[2],uint256[8]),uint256)` |
| `0xa415bcad` | `borrow(address,uint256,uint256,uint16,address)` |
| `0xa99aad89` | `supply((address,address,address,address,uint256),uint256,uint256,address,bytes)` |
| `0xac9650d8` | `multicall(bytes[])` |
| `0xb13acedd` | `claimWithdraw(uint256)` |
| `0xb6b55f25` | `deposit(uint256)` |
| `0xd0e30db0` | `deposit()` |
| `0xde0e9a3e` | `unwrap(uint256)` |
| `0xea10fbbe` | `deleteIdentities(uint256[8],bytes,uint256,uint256)` |
| `0xea598cb0` | `wrap(uint256)` |

### Selectors that could not be identified

Present in measured transactions but never matched. Labels in the table come from emitted events:

- `0x03f00196` — flash-loan MEV bot (from `FlashLoan`)
- `0x146ee74d` — third-party aggregator
- `0x1a28e979` — liquidator bot (from `Liquidate`)
- `0x28223a77` — Railgun RelayAdapt entry point; its first inner call is `0xd8ae136a` on the smart wallet, i.e. it wraps the same operation the direct transactions perform
- `0x2e03931e` — EtherFiAdmin oracle-report execution (from `Rebase`)
- `0x2f5066dd` — flash-loan deleverage (from events)
- `0x374f435d` — router (from events)
- `0x594a88cc` — Pendle router action
- `0x60fc8466` — Pendle router action
- `0x642ba7a7` — flash-loan leverage (from events)
- `0x7e809076` — third-party liquidator bot
- `0x99999999` — flash-loan MEV bot (from `FlashLoan`)
- `0xc685f647` — third-party aggregator
- `0xd8ae136a` — Railgun private transfer / unshield (from `Nullified`+`Unshield`)
- `0xeb7499cf` — 9-market reallocation (from events)
- `0xed48907e` — Pendle router action


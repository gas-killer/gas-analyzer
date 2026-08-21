# Protocol survey — GasKiller candidates beyond Morpho

Measured with this repository's analyzer (`cargo run -- t <hash>`, EvmSketch backend) against Ethereum mainnet. Companion to `MORPHO_CANDIDATES.md` (Morpho) and `CALL_BLOCKED_CANDIDATES.md` (transactions that score zero because of external calls).

Every number in the results tables is analyzer output. Where a figure is my own arithmetic it says so.

## Headline: Railgun

**15 direct calls to the Railgun smart wallet, all trace-measured, all clearing both signature floors.** Schnorr savings range **65.57%–80.84%** (median 74.06%). BLS savings range 19.53%–64.86%.

In aggregate: 15,174,097 gas used on chain becomes 3,765,439 under GasKiller with Schnorr — a saving of **11,408,658 gas (75.2%)**.

This is the only protocol surveyed where **BLS also saves**. Across the other six, 0 of ~50 transactions cleared the 250,000 BLS floor.

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

By function (both selectors keccak-verified against the calldata):

- **`transact`** (`0xd8ae136a`, private transfer / unshield) — 10 txs, 65.6–80.8% Schnorr, gas 484,303–1,615,755
- **`shield`** (`0x044a40c3`, `shield(((bytes32,(uint8,address,uint256),uint120),(bytes32[3],bytes32))[])`) — 5 txs, 68.5–73.0% Schnorr, gas 724,515–758,741

On the RelayAdapt side, only 1 of the 3 completed a real replay (`0x2d754e6f…`, surplus −10,281). The other two fell back to heuristic — one of them reverted mid-replay against the smart wallet on all 3 attempts. All three scored zero either way, and their heuristic surpluses (+4,121 and +10,372) are well under the 27,000 floor, so the fallbacks do not change the conclusion.

## All protocols surveyed

Best trace-measured Schnorr saving per protocol, and whether the winning shape is reachable in practice.

| protocol | best measured | BLS | winning shape | how common |
|---|---:|---:|---|---|
| **Railgun** | **80.84%** | **60.30%** | direct call to smart wallet | 374 of 704 (53%) |
| Privacy Pools | 23.39% | 0% | direct call to pool | 2 in 17 days |
| Ether.fi | 18.99% | 0% | EtherFiAdmin oracle report | every ~4 hours |
| Pendle | 3.94% | 0% | one large aggregator tx | marginal |
| World ID | 2.87% | 0% | direct, but verifier call dominates | all txs direct |
| Morpho | 2.60% | 0% | bundler multicall reallocation | marginal |
| Chainlink | 0% | 0% | none — all traffic via forwarder | 0 of 242 direct |

## What predicts a good candidate

Two conditions, both necessary. Every result above is explained by them.

**1. The expensive work must happen in the contract the transaction is sent to.** The analyzer keeps a regular `CALL` as one instruction and re-executes it on replay, so anything inside it is never stripped (`crates/core/src/trace.rs:255`; `crates/core/src/prestate.rs` documents the same constraint for the net form). Railgun direct vs Railgun RelayAdapt is the same operation on the same contract scoring 80% vs 0% on this alone.

**2. The work must actually be computation, not bookkeeping.** Pendle satisfies condition 1 on its one direct-to-market swap and still scores ~2.7% surplus, because a swap is mostly storage writes. Ether.fi's `claimWithdraw` is the clearest case: its 43,868 gas per claim decomposes as ~25,000 of storage writes + 3,768 of logs + ~15,100 of calls, leaving ~0 for computation.

The winners are cryptographic: Railgun (ZK proofs + Poseidon in its own code), Privacy Pools (Poseidon over a merkle tree), Ether.fi (oracle report processing). Ordinary DeFi arithmetic — swap curves, share conversions, interest accrual — is never large enough to dominate a transaction's gas.

## Limitations

- **Opportunistic samples.** Railgun is the largest at 15 direct transactions from one 40,000-block window; the others are 7–20 transactions each. None is a systematic survey and none should be read as a population statistic.
- **Contract addresses came from web search, then were verified on chain.** For each protocol, event topic0 values and function selectors were matched against keccak hashes computed locally (pure-Python keccak, self-tested against the empty-string digest and the ERC-20 `Transfer` topic) before any conclusion was drawn. Unmatched selectors are named as unidentified rather than guessed.
- **Some transactions cannot be measured at all**, independent of rate limits: senders with EIP-7702 delegation designators are rejected by the simulator (`RejectCallerWithCode`), and some transactions reproducibly revert mid-replay. See `MORPHO_CANDIDATES.md`.
- **The prestate/net-form encoder was not exercised.** The CLI's `t` command uses the struct-log encoder. The net form collapses repeated writes to the same slot, which materially changes batched transactions — Ether.fi's `batchClaimWithdraw` has 65 struct-log stores but only 5 net changed slots. Everything measured here is the struct-log path.
- **Savings are the analyzer's cost model**, not on-chain outcomes; nothing was executed through a deployed GasKiller.
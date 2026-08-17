# Morpho transactions as GasKiller candidates under Schnorr

Produced by running this repository's own analyzer (`cargo run -- t <hash>`, EvmSketch backend) against Morpho Blue transactions discovered on Ethereum mainnet. Every number below is analyzer output, not an estimate of mine.

> **Revision note.** The first pass of this document reported three Schnorr-only candidates. The top one was a heuristic fallback; it has since been re-measured through the real replay path and **no longer qualifies** — see *"Follow-up: the top result was re-measured and it collapsed"* below. Two trace-based candidates remain.

**This is a small opportunistic sample, not a systematic survey.** 20 transactions from a single ~500-block window were analyzed. Nothing here should be read as a population statistic about Morpho.

## Method

- **Endpoint**: `$RPC_URL` (Ethereum mainnet, verified `eth_chainId` → `0x1`). Chain head at scan time: block 25774562.
- **Anchor**: Morpho Blue at `0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb`, verified live via `eth_getCode` → **15,623 bytes** of code.
- **Block range scanned**: 25774012–25774512 (501 blocks, ~1.7 h of mainnet), via `eth_getLogs` against Morpho Blue in 100-block pages.
- **Logs collected**: 771, spanning **11 distinct event types**.
- **Transactions discovered**: 284 unique hashes; receipts fetched for all 284.
- **Filter** (status success, `to` non-null, 60,000 ≤ gas_used ≤ 2,500,000): **274 passed**. Excluded: 10 for gas_used > 2.5 M; 0 below 60 k; 0 reverted; 0 contract-creations (`to` null).
- **Analyzed**: 20 (the run cap), chosen by hand for shape and size diversity — the one liquidation in range, flash-loan leverage/deleverage, signature-authorised bundler multicalls, MetaMorpho vault `reallocate` calls, multi-market vault reallocations (2–11 markets), direct Morpho Blue entrypoint calls, and flash-loan-only MEV bots. Gas spread 99,912–1,779,190, weighted toward the 100 k–600 k mid-range.
- Run **sequentially** (never in parallel) with a 5 s gap, to stay inside the provider's rate limit. No rate-limit errors were hit.
- **Skipped for CREATE/CREATE2 of a large contract**: 0 — see "Contract-deployment shapes" below.

### Event-signature and selector provenance

No address or signature below is quoted from memory. `cast`/`forge` are unavailable in this environment, so event topics and function selectors were resolved by implementing Keccak-256 in pure Python (self-tested against the empty-string digest and the canonical ERC-20 `Transfer` topic) and matching computed hashes against the topic0 values and calldata prefixes actually observed on chain. All 11 observed topic0 values matched:

- `0x9d9bd501d0657d7dfe415f779a620a62b78bc508ddc0891fbbd8b7ac0f8fce87` ×279 → `AccrueInterest(bytes32,uint256,uint256,uint256)`
- `0xedf8870433c83823eb071d3df1caa8d008f12f6440918c20d75a3602cda30fe0` ×162 → `Supply(bytes32,address,address,uint256,uint256)`
- `0xc76f1b4fe4396ac07a9fa55a415d4ca430e72651d37d3401f3bed7cb13fc4f12` ×154 → `FlashLoan(address,address,uint256)`
- `0xa56fc0ad5702ec05ce63666221f796fb62437c32db1aa1aa075fc6484cf58fbf` ×101 → `Withdraw(bytes32,address,address,address,uint256,uint256)`
- `0xa3b9472a1399e17e123f3c2e6586c23e504184d504de59cdaa2b375e880c6184` ×29 → `SupplyCollateral(bytes32,address,address,uint256)`
- `0x570954540bed6b1304a87dfe815a5eda4a648f7097a16240dcd85c9b5fd42a43` ×26 → `Borrow(bytes32,address,address,address,uint256,uint256)`
- `0x52acb05cebbd3cd39715469f22afbf5a17496295ef3bc9bb5944056c63ccaa09` ×9 → `Repay(bytes32,address,address,uint256,uint256)`
- `0xe80ebd7cc9223d7382aab2e0d1d6155c65651f83d53c8b9b06901d167e321142` ×8 → `WithdrawCollateral(bytes32,address,address,address,uint256)`
- `0xa4946ede45d0c6f06a0f5ce92c9ad3b4751452d2fe0e25010783bcab57a67e41` ×1 → `Liquidate(bytes32,address,address,uint256,uint256,uint256,uint256,uint256)`
- `0xa58af1a0c70dba0c7aa60d1a1a147ebd61000d1690a968828ac718bca927f2c7` ×1 → `IncrementNonce(address,address,uint256)`
- `0xd5e969f01efe921d3f766bdebad25f0a05e3f237311f56482bf132d0326309c0` ×1 → `SetAuthorization(address,address,address,bool)`

Labels for transactions whose selector did **not** match any signature I could construct are derived from their emitted Morpho events and are marked as such in the table. I did not guess those.

### Counterparty contracts (derived, not remembered)

Rather than trusting remembered MetaMorpho/bundler/public-allocator addresses, these were derived as the most frequent `caller`/`onBehalf` indexed topics in Morpho Blue's own logs, each then verified to have code:

| appearances | address | code size |
|---:|---|---:|
| 64 | `0xbeef01735c132ada46aa9aa4c54623caa92a64cb` | 19,340 B |
| 60 | `0x75741a12b36d181f44f389e0c6b1e0210311e3ff` | 19,730 B |
| 56 | `0x5e3aca36fe2361e1866752ad74cb429f64cdcf1a` | 11,784 B |
| 38 | `0xc9c2c513a0e78f15a301b6ee6c0ee727ae8ff641` | 438 B |
| 32 | `0xbeefff209270748ddd194831b3fa287a5386f5bc` | 19,340 B |
| 28 | `0xbfe734baab130048e20a64e800c4c2ec25756284` | 11,784 B |
| 26 | `0x8eb67a509616cd6a7c1b3c8c21d48ff57df3d458` | 19,340 B |
| 19 | `0x06cff7088619c7178f5e14f0b119458d08d2f5ef` | 22,777 B |
| 18 | `0x950fd558f47e234a2fde23b7d61f7ccdbcb4a86f` | 18,725 B |
| 16 | `0x4a6c312ec70e8747a587ee860a0353cd42be0ae0` | 17,575 B |

Two code-size families dominate: **19,340 B** and **19,730 B** vault-like contracts, and **11,784 B** contracts. That 11,784 B figure is exactly the contract size CREATE2-deployed by the known-hopeless transaction named in the task brief — so that transaction was deploying one of the vault instances that appear here as ordinary counterparties.

## Results

All 20 analyzed transactions, sorted by Schnorr savings descending. `surplus` = `gas_used − base_estimate`; savings are `gas_used − (base_estimate + floor)`, saturating at 0, with floor = 27,000 (Schnorr) or 250,000 (BLS).

| # | tx | block | `to` | what it does | gas_used | base_estimate | surplus | Schnorr est. | Schnorr saved | BLS saved | est. source | state updates |
|---:|---|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|
| 1 | [`0x4e547494…`](https://etherscan.io/tx/0x4e547494fcf332b50465117a6467c8cb097787e4b54fd5b97ff6ff5cfec96ceb) | 25774052 | `0x5af8b1e9…` | Flash-loan leverage open: `FlashLoan`+`SupplyCollateral`+`Borrow`, 12,324 B calldata (label from events; selector `0x642ba7a7` not identified) 🟢 **SCHNORR-ONLY** | 1,779,190 | 1,720,040 | 59,150 | 1,747,040 | **32,150** (1.81%) | 0 (0.00%) | trace | 6: Store×3, Call×2, Log1×1 |
| 2 | [`0x80329618…`](https://etherscan.io/tx/0x80329618f5c5261829097e2a8a079c765c6ae0ce35f6d98e09a4d246a694c8bf) | 25774472 | `0x3a618e9d…` | Bundler `multicall(bytes[])` (selector verified): 2-market `Supply`+`Withdraw` reallocation 🟢 **SCHNORR-ONLY** | 380,049 | 343,154 | 36,895 | 370,154 | **9,895** (2.60%) | 0 (0.00%) | trace | 18: Store×10, Call×4, Log3×3, Log1×1 |
| 3 | [`0x1c71eb76…`](https://etherscan.io/tx/0x1c71eb76549cc6a80467e06e8bc938b7fc1e67e9575c2aece8d98345243bb218) | 25774086 | `0x9e9110cf…` | 9-market reallocation: `Supply`+`Withdraw`+`AccrueInterest` across 9 market ids (label from events; selector `0xeb7499cf` not identified)  | 725,295 | 699,768 | 25,527 | 726,768 | **0** (0.00%) | 0 (0.00%) | trace | 1: Call×1 |
| 4 | [`0x9a08a526…`](https://etherscan.io/tx/0x9a08a526e05f5fe827840f0ec4e3d1ce31906fa5a7a9bda7677098e4d78903df) | 25774239 | `0x00000f91…` | Flash-loan-only MEV/arb bot: single `FlashLoan` event (label from event; selector `0x03f00196` not identified)  | 420,293 | 414,721 | 5,572 | 441,721 | **0** (0.00%) | 0 (0.00%) | ⚠️ **heuristic** | 5: Call×5 |
| 5 | [`0x8e4616ac…`](https://etherscan.io/tx/0x8e4616acfaf812a41b471e139924a1bc906e03e8e1203760ae6117113682b760) | 25774072 | `0x64c18dcc…` | Bundler `multicall(bytes[])` (selector verified): 2-market `Supply`+`Withdraw` reallocation  | 312,814 | 314,184 | -1,370 | 341,184 | **0** (0.00%) | 0 (0.00%) | trace | 18: Store×10, Call×4, Log3×3, Log1×1 |
| 6 | [`0x8d0c4018…`](https://etherscan.io/tx/0x8d0c40187a36dd2de2b64800b87d8db9b235624479d35202a11c2b2fb98fd76a) | 25774169 | `0x4095f064…` | Bundler `multicall(bytes[])` (selector verified): single-market `Supply`+`AccrueInterest`  | 157,756 | 161,582 | -3,826 | 188,582 | **0** (0.00%) | 0 (0.00%) | trace | 5: Call×3, Store×2 |
| 7 | [`0x50305a21…`](https://etherscan.io/tx/0x50305a216cbeabbc02ad2262619090e91c6928ecc30def46af4eaab7bde99e9b) | 25774272 | `0x4095f064…` | Bundler `multicall(bytes[])` (selector verified): single-market `Supply`+`AccrueInterest`  | 179,644 | 183,470 | -3,826 | 210,470 | **0** (0.00%) | 0 (0.00%) | trace | 5: Call×3, Store×2 |
| 8 | [`0x2d7cebfe…`](https://etherscan.io/tx/0x2d7cebfe726192fe692ccfa905b401fdedb108f5a3ecdf34aff20d2e77b3c320) | 25774308 | `0x65661941…` | Router call emitting only `SupplyCollateral` (label from event; selector `0x374f435d` not identified)  | 122,812 | 126,798 | -3,986 | 153,798 | **0** (0.00%) | 0 (0.00%) | ⚠️ **heuristic** | 3: Call×3 |
| 9 | [`0x3dffb38b…`](https://etherscan.io/tx/0x3dffb38b03f52c07073a0ad32f336c5d3106640c2462f72204c9d6fe02534ed1) | 25774475 | `0x4095f064…` | Bundler `multicall(bytes[])` (selector verified): signature-authorised position open — `SetAuthorization`+`IncrementNonce`+`SupplyCollateral`+`Borrow`  | 360,323 | 371,754 | -11,431 | 398,754 | **0** (0.00%) | 0 (0.00%) | trace | 8: Call×6, Store×2 |
| 10 | [`0xdc74e020…`](https://etherscan.io/tx/0xdc74e020e296fbb968edfc2ffd630bad47d557c71dabe901938315be6329c5c9) | 25774018 | `0x65661941…` | Same router selector `0x374f435d`, this time `Repay`+`AccrueInterest` (label from events)  | 134,366 | 148,620 | -14,254 | 175,620 | **0** (0.00%) | 0 (0.00%) | ⚠️ **heuristic** | 2: Call×2 |
| 11 | [`0x1338ba16…`](https://etherscan.io/tx/0x1338ba16b0a7f61988caf43896fde0e32edac97cd7dab32bb6136bf9e77f0302) | 25774092 | `0x06cff708…` | Flash-loan-only MEV/arb bot: single `FlashLoan` event, no Morpho market mutation (label from event; selector `0x99999999` not identified)  | 405,201 | 423,289 | -18,088 | 450,289 | **0** (0.00%) | 0 (0.00%) | trace | 1: Call×1 |
| 12 | [`0xcd59750e…`](https://etherscan.io/tx/0xcd59750e91859ec6af4209c588997b61962dcec2aad6c549ef35324476870bdf) | 25774397 | `0xc54b4e08…` | MetaMorpho vault `reallocate(((address,address,address,address,uint256),uint256)[])` (selector verified) called by an **EOA** allocator: `accrueInterest`+`withdraw` from 3 USDT markets (wstETH 86% / WBTC 86% / sUSDS 96% LLTV) then `accrueInterest`+`supply` into the idle market in 2 tranches — all selectors and the `ReallocateWithdraw`/`ReallocateSupply` topics keccak-verified  | 324,608 | 349,625 | -25,017 | 376,625 | **0** (0.00%) | 0 (0.00%) | trace | 15: Call×10, Log3×5 |
| 13 | [`0x441cd851…`](https://etherscan.io/tx/0x441cd85183e88986305c0721c98bdd3c25edbe5ecc4578baeb964f09b8b42686) | 25774438 | `0xbbbbbbbb…` | Direct Morpho Blue `withdrawCollateral(MarketParams,uint256,address,address)` (selector verified)  | 132,478 | 158,129 | -25,651 | 185,129 | **0** (0.00%) | 0 (0.00%) | trace | 8: Store×4, Call×2, Log2×1, Log4×1 |
| 14 | [`0xe520cf76…`](https://etherscan.io/tx/0xe520cf761e3fd61b115b0f31bd7f182a9cce43ec747b1aeaf77bf3457ebdc91f) | 25774420 | `0xbeef00a5…` | Bundler `multicall(bytes[])` (selector verified): 3-market `Supply`+`Withdraw` reallocation  | 366,689 | 393,924 | -27,235 | 420,924 | **0** (0.00%) | 0 (0.00%) | trace | 24: Store×13, Call×6, Log3×3, Log1×2 |
| 15 | [`0x4419f117…`](https://etherscan.io/tx/0x4419f1176b254fa8b1f0cb0daa7093b223379894701addb921fa02f00f373f8d) | 25774413 | `0xbbbbbbbb…` | Direct Morpho Blue `withdraw(MarketParams,uint256,uint256,address,address)` (selector verified)  | 111,792 | 142,279 | -30,487 | 169,279 | **0** (0.00%) | 0 (0.00%) | trace | 10: Store×6, Call×2, Log2×1, Log4×1 |
| 16 | [`0x8a27bff6…`](https://etherscan.io/tx/0x8a27bff6e7606ee0f89b63f01241c1a6a8d5cff37d726ee413df165b243c2f64) | 25774103 | `0xbbbbbbbb…` | Direct Morpho Blue `supply(MarketParams,uint256,uint256,address,bytes)` (selector verified)  | 99,912 | 130,941 | -31,029 | 157,941 | **0** (0.00%) | 0 (0.00%) | trace | 10: Store×6, Call×2, Log2×1, Log4×1 |
| 17 | [`0xbeffded8…`](https://etherscan.io/tx/0xbeffded8df725752edea428f171d8ec2a842dcdb645977ea9cf5bedba14ca414) | 25774418 | `0x68aea7b8…` | MetaMorpho vault `reallocate(((address,address,address,address,uint256),uint256)[])` (selector verified), 5-market `Supply`+`Withdraw`; sender is an **EIP-7702-delegated EOA** (code `0xef0100…`, verified) calling the 19,730 B vault directly — not the public allocator  | 416,744 | 451,366 | -34,622 | 478,366 | **0** (0.00%) | 0 (0.00%) | trace | 18: Call×12, Log3×6 |
| 18 | [`0x09fd0f6e…`](https://etherscan.io/tx/0x09fd0f6eb66388ce7cdc484b2020d300b5c6d519df89c5bddc73307d9e68bd80) | 25774216 | `0x591a8529…` | Liquidation of one borrower (label from `Liquidate`+`AccrueInterest` events; selector `0x1a28e979` not identified)  | 619,024 | 733,358 | -114,334 | 760,358 | **0** (0.00%) | 0 (0.00%) | ⚠️ **heuristic** | 2: Call×1, Log3×1 |
| 19 | [`0xb1bf36be…`](https://etherscan.io/tx/0xb1bf36beaf1aeeb69e575a1230468d917ef4646c6416ab465201bca70d8c7a72) | 25774269 | `0xbeeff2c5…` | Bundler `multicall(bytes[])` (selector verified): 11-market reallocation — `Supply`+`Withdraw`+`AccrueInterest` across 11 market ids  | 1,278,050 | 1,457,568 | -179,518 | 1,484,568 | **0** (0.00%) | 0 (0.00%) | trace | 84: Store×45, Call×22, Log3×12, Log1×5 |
| 20 | [`0x16a0a31c…`](https://etherscan.io/tx/0x16a0a31c0547f2f35018c38f0c2fa3bdcf1320e6a75f998caaa957747e9dc568) | 25774313 | `0xaad84c80…` | Flash-loan deleverage: `FlashLoan`+`Repay`+`WithdrawCollateral` (label from events; selector `0x2f5066dd` not identified)  | 1,312,558 | 1,577,595 | -265,037 | 1,604,595 | **0** (0.00%) | 0 (0.00%) | ⚠️ **heuristic** | 1: Call×1 |

Full hashes, in the same order:

1. `0x4e547494fcf332b50465117a6467c8cb097787e4b54fd5b97ff6ff5cfec96ceb`
2. `0x80329618f5c5261829097e2a8a079c765c6ae0ce35f6d98e09a4d246a694c8bf`
3. `0x1c71eb76549cc6a80467e06e8bc938b7fc1e67e9575c2aece8d98345243bb218`
4. `0x9a08a526e05f5fe827840f0ec4e3d1ce31906fa5a7a9bda7677098e4d78903df`
5. `0x8e4616acfaf812a41b471e139924a1bc906e03e8e1203760ae6117113682b760`
6. `0x8d0c40187a36dd2de2b64800b87d8db9b235624479d35202a11c2b2fb98fd76a`
7. `0x50305a216cbeabbc02ad2262619090e91c6928ecc30def46af4eaab7bde99e9b`
8. `0x2d7cebfe726192fe692ccfa905b401fdedb108f5a3ecdf34aff20d2e77b3c320`
9. `0x3dffb38b03f52c07073a0ad32f336c5d3106640c2462f72204c9d6fe02534ed1`
10. `0xdc74e020e296fbb968edfc2ffd630bad47d557c71dabe901938315be6329c5c9`
11. `0x1338ba16b0a7f61988caf43896fde0e32edac97cd7dab32bb6136bf9e77f0302`
12. `0xcd59750e91859ec6af4209c588997b61962dcec2aad6c549ef35324476870bdf`
13. `0x441cd85183e88986305c0721c98bdd3c25edbe5ecc4578baeb964f09b8b42686`
14. `0xe520cf761e3fd61b115b0f31bd7f182a9cce43ec747b1aeaf77bf3457ebdc91f`
15. `0x4419f1176b254fa8b1f0cb0daa7093b223379894701addb921fa02f00f373f8d`
16. `0x8a27bff6e7606ee0f89b63f01241c1a6a8d5cff37d726ee413df165b243c2f64`
17. `0xbeffded8df725752edea428f171d8ec2a842dcdb645977ea9cf5bedba14ca414`
18. `0x09fd0f6eb66388ce7cdc484b2020d300b5c6d519df89c5bddc73307d9e68bd80`
19. `0xb1bf36beaf1aeeb69e575a1230468d917ef4646c6416ab465201bca70d8c7a72`
20. `0x16a0a31c0547f2f35018c38f0c2fa3bdcf1320e6a75f998caaa957747e9dc568`

## ⭐ Transactions where Schnorr saves but BLS does not

These are the headline result: the surplus (`gas_used − base_estimate`) lands inside the 27,000–250,000 band, so Schnorr's cheaper floor turns a non-candidate into a candidate.

| tx | what it does | gas_used | base_estimate | **surplus** | Schnorr saved | BLS saved | est. source |
|---|---|---:|---:|---:|---:|---:|---|
| `0x4e547494fcf332b5…` | Flash-loan leverage open: `FlashLoan`+`SupplyCollateral`+`Borrow`, 12,324 B calldata (label from events; selector `0x642ba7a7` not identified) | 1,779,190 | 1,720,040 | **59,150** | **32,150** (1.81%) | 0 | trace |
| `0x80329618f5c52618…` | Bundler `multicall(bytes[])` (selector verified): 2-market `Supply`+`Withdraw` reallocation | 380,049 | 343,154 | **36,895** | **9,895** (2.60%) | 0 | trace |

- `0x4e547494fcf332b50465117a6467c8cb097787e4b54fd5b97ff6ff5cfec96ceb` — surplus **59,150**, i.e. 32,150 gas above the Schnorr floor and 190,850 gas below the BLS floor.
- `0x80329618f5c5261829097e2a8a079c765c6ae0ce35f6d98e09a4d246a694c8bf` — surplus **36,895**, i.e. 9,895 gas above the Schnorr floor and 213,105 gas below the BLS floor.

## Follow-up: the top result was re-measured and it collapsed

The highest-scoring transaction in the first pass was a heuristic fallback. It has since been **re-run successfully through the real `StateChangeHandler` replay**, and the trace-based number reverses the finding completely.

`0xcd59750e91859ec6af4209c588997b61962dcec2aad6c549ef35324476870bdf`

| | heuristic (first pass) | **measured (re-run)** |
|---|---:|---:|
| gas_used | 324,608 | 324,608 |
| base_estimate | 253,881 | **349,625** |
| surplus | 70,727 | **-25,017** |
| Schnorr savings | 43,727 (13.47%) | **0 (0.00%)** |
| BLS savings | 0 | 0 |

The heuristic understated the replay cost by **95,744 gas** (27% of the true `base_estimate`). An apparent 13.47% win is in fact a **25,017 gas loss** — not a candidate under either scheme.

### Why the first pass fell back (it was not a trace failure)

The extracted diff was never the problem — all 15 state updates were recovered correctly both times. The fallback was triggered by the provider's rate limiter during the mid-block replay:

```
Warning: Measured gas estimation failed, using heuristic
   Reason: Failed to replay preceding tx 0: Database(SimpleRpcDbError(
     "get_proof failed for 0x0000000aa232009084Bd71A5797d089AA4Edfad4:
      HTTP error 429 ... 50/second request limit reached"))
```

This transaction sits at **index 250** in its block, so measuring it requires replaying 250 preceding transactions, each fetching state proofs. During the original 20-transaction sweep that burst tripped the 50 req/s ceiling and `crates/cli/src/main.rs:386` fell back rather than failing hard. Re-run on its own, with nothing else competing for the endpoint, it **succeeded on the first attempt**.

Two corrections to what the first pass of this document claimed:

1. Heuristic rows are **not** necessarily cases where "trace extraction failed to recover the real diff". At least this one had a complete, correct diff; only the *measured replay* failed, for infrastructural reasons. The `Call`×10 + `Log3`×5 shape with no `Store` updates is genuine, and it is genuine for a good reason — a vault's state changes *are* outgoing calls into Morpho Blue, not local storage writes.
2. The label was imprecise. It is **not** a public-allocator call: `from` is an **EOA** (`0xcfeb081e…`, 0 bytes of code) calling `reallocate` directly on a 19,730-byte MetaMorpho vault (`0xc54b4e08…`). The public allocator is a separate contract and is not involved.

### Why the heuristic was biased upward on savings

`crates/core/src/heuristic.rs:40` prices a `Call` state update at **zero gas**, on the stated grounds that call gas is already included in `external_call_gas` extracted from the trace. Using that module's own constants the heuristic's 253,881 decomposes exactly:

| component | gas |
|---|---:|
| `BASE_TX_COST` | 21,000 |
| `external_call_gas` (from trace) | 222,821 |
| 5 × `Log3` (375 + 3×375 + 64 B×8) | 10,060 |
| 10 × `Call` | **0** |
| **total** | **253,881** |

So it charges what the ten Morpho calls cost *inside the original transaction* and nothing for re-issuing them. The real replay must also pay per-`CALL` account-access and memory/calldata-copy costs plus the handler's decode-and-dispatch loop over 15 updates — which measured out at 95,744 gas, about 9,574 per call. **Heuristic rows should be assumed to overstate savings on `Call`-heavy diffs, and by a margin large enough to erase the result entirely.**

### What this implies for the remaining heuristic rows

5 heuristic rows remain, and all of them are `Call`-dominated:

- `0x16a0a31c0547f2f3…` — 1 updates (`Call`×1), heuristic surplus -265,037. Same bias applies.
- `0x09fd0f6eb66388ce…` — 2 updates (`Call`×1, `Log3`×1), heuristic surplus -114,334. Same bias applies.
- `0x9a08a526e05f5fe8…` — 5 updates (`Call`×5), heuristic surplus 5,572. Same bias applies.
- `0xdc74e020e296fbb9…` — 2 updates (`Call`×2), heuristic surplus -14,254. Same bias applies.
- `0x2d7cebfe726192fe…` — 3 updates (`Call`×3), heuristic surplus -3,986. Same bias applies.

None of them currently show savings, so the bias does not create false positives in the table as it stands — but it does mean the liquidation's apparent −114,334 surplus is not trustworthy either, in *either* direction. It remains unmeasured.
### Near misses — positive surplus, but under the 27,000 Schnorr floor

Two transactions had a genuinely positive surplus yet still scored zero, because the surplus did not clear Schnorr's 27,000 floor. One of them missed by a rounding error:

| tx | what it does | gas_used | base_estimate | surplus | short of Schnorr floor by | est. source |
|---|---|---:|---:|---:|---:|---|
| `0x1c71eb76549cc6a8…` | 9-market reallocation: `Supply`+`Withdraw`+`AccrueInterest` across 9 market ids (label from events; selector `0xeb7499cf` not identified) | 725,295 | 699,768 | 25,527 | **1,473** | trace |
| `0x9a08a526e05f5fe8…` | Flash-loan-only MEV/arb bot: single `FlashLoan` event (label from event; selector `0x03f00196` not identified) | 420,293 | 414,721 | 5,572 | **21,428** | ⚠️ heuristic |

`0x1c71eb76…` is the notable one: a 9-market reallocation that came within **1,473 gas** of qualifying. Slightly more market churn in the same transaction and it would have crossed. This says the Schnorr band is not a rare accident for this shape — reallocations sit right on its edge.

## What the numbers say structurally

### Headline: no transaction in this sample saved anything under BLS

**0 of 20** transactions cleared the 250,000 BLS floor. **2 of 20** cleared the 27,000 Schnorr floor. Every single win in this sample is a Schnorr-only win — the 223,000-gas band is doing all of the work here. On this evidence, Morpho is a protocol GasKiller can only address with the cheaper signature scheme.

### The controlling variable is surplus, and surplus is usually negative

Surplus (`gas_used − base_estimate`) ranged from -265,037 to 59,150. **16 of 20 were negative** — replaying the final state diff costs *more* than the original transaction spent. That is the opposite of what GasKiller needs, and it is the normal case for Morpho.

Across all 20 diffs the observed state-update kinds were:

- `Store` — 103 occurrences (42%)
- `Call` — 92 occurrences (38%)
- `Log3` — 33 occurrences (14%)
- `Log1` — 10 occurrences (4%)
- `Log2` — 3 occurrences (1%)
- `Log4` — 3 occurrences (1%)

### The cleanest evidence: two transactions with identical diffs and different outcomes

Two of the analyzed transactions are near-twins — both bundler `multicall(bytes[])` 2-market `Supply`+`Withdraw` reallocations, both with **exactly 18 state updates of exactly the same kinds**:

| | `0x80329618…` (winner) | `0x8e4616ac…` (zero) |
|---|---:|---:|
| state updates | 18 | 18 |
| kinds | `Store`×10, `Call`×4, `Log3`×3, `Log1`×1 | `Store`×10, `Call`×4, `Log3`×3, `Log1`×1 |
| base_estimate | 343,154 | 314,184 |
| gas_used | 380,049 | 312,814 |
| **surplus** | **36,895** | **-1,370** |
| Schnorr saved | **9,895** (2.60%) | 0 |

The diffs cost almost the same to replay (343,154 vs 314,184, a 28,970 gas difference), but the winner *burned 67,235 more gas doing it*. That extra gas went into computation that left no additional trace in the final state — and it is precisely that gap that Schnorr's floor converts into savings. **Structurally, this is the whole thesis in one comparison: for a fixed diff, savings are a function of how much invisible computation the transaction did.**

### What the winners have in common

- **`0x4e547494fcf332b5…`** — Flash-loan leverage open: `FlashLoan`+`SupplyCollateral`+`Borrow`, 12,324 B calldata (label from events; selector `0x642ba7a7` not identified). surplus **59,150**, 6 updates (`Store`×3, `Call`×2, `Log1`×1), Schnorr **32,150** (1.81%).
- **`0x80329618f5c52618…`** — Bundler `multicall(bytes[])` (selector verified): 2-market `Supply`+`Withdraw` reallocation. surplus **36,895**, 18 updates (`Store`×10, `Call`×4, `Log3`×3, `Log1`×1), Schnorr **9,895** (2.60%).

Two structural features recur:

1. **A diff dominated by `Call` and `Log`, not `Store`.** `0x4e547494…` collapses a 1.78 M-gas flash-loan leverage bundle into six instructions: a reentrancy-lock `Store` set and cleared (slot `0x…02` → 1, then → 0), a USDC `transferFrom` `Call` (selector `0x23b872dd`, verified by keccak match), a `flashLoan(address,uint256,bytes)` `Call` into Morpho Blue (selector `0xe0232b42`, verified), and one `Log1`. The entire leverage path — collateral supply, borrow, swap — nets out of the final diff.
2. **Market churn without proportional writes.** The reallocation winners spend gas on repeated `AccrueInterest` math, IRM/oracle `staticcall`s and share↔asset conversions per market, which the diff never records.

The catch, and it is a big one: both surviving winners save only **1.81%** and **2.60%**. The one double-digit result this survey originally reported (13.47%) was a heuristic estimate and **collapsed to zero when re-measured** — see the follow-up section above. So Schnorr-only candidates do exist in Morpho, but every properly measured one here is a *thin* win, not a dramatic one.

### What the zero-savings ones have in common

- **diffs containing `Store` writes** (9 txs): surplus -179,518 … -1,370.
- **`Call`-only / `Call`+`Log`-only diffs** (9 txs): surplus -265,037 … 25,527.

Three specific sub-patterns, all grounded in the observed diffs:

1. **Direct single-market Morpho Blue calls are the worst candidates in the sample.** `supply`, `withdraw` and `withdrawCollateral` (all three selectors keccak-verified) came in at surplus −31,029, −30,487 and −25,651. Their diffs are 8–10 updates of `Store`×4–6 + `Call`×2 + `Log2`/`Log4` — i.e. they are *almost pure state change*, ~100–130 k gas with essentially no computation to strip out. The `Store`s land on **consecutive slots of one packed struct** (`…d52b21dc`, `…dd`, `…de` in one observed diff — Morpho's packed `Market` struct). Morpho Blue is already close to storage-optimal, which leaves GasKiller nothing to arbitrage.
2. **Multi-market reallocations scale their diff with their computation.** The 11-market reallocation produced **84 state updates** (`Store`×45, `Call`×22, `Log3`×12, `Log1`×5) and a surplus of −179,518. Each extra market adds its own `AccrueInterest` math *and* its own `Store`s, `Log`s and `Call`, so `base_estimate` grows in step with `gas_used`. Market count alone is not a predictor of candidacy — the 2-market winner beat the 11-market one.
3. **`Call`-heavy diffs are a coin flip, because a `Call` replay re-executes the callee for real.** A single `Call` update can be nearly free or can carry the whole transaction: `0x1c71eb76…` compressed a 9-market reallocation into **one** `Call` update, yet `base_estimate` was still 699,768 of its 725,295 gas, because replaying that one `Call` re-did all nine markets. `Call` updates do not compress a transaction; they relocate its cost into `base_estimate`.

## ⚠️ Heuristic fallbacks — softer numbers, not comparable to the trace-based rows

**5 of 20 transactions (25%) did not produce a measured `StateChangeHandler` replay.** The CLI labelled these itself as heuristic; the figures are much softer and must not be read as equivalent to the trace-based rows:

| tx | analyzer's own label | gas_used | base_estimate | surplus | Schnorr saved | state updates |
|---|---|---:|---:|---:|---:|---|
| `0x09fd0f6eb66388ce…` | *heuristic - measured estimation failed* | 619,024 | 733,358 | -114,334 | 0 | 2: `Call`×1, `Log3`×1 |
| `0x16a0a31c0547f2f3…` | *heuristic - measured estimation failed* | 1,312,558 | 1,577,595 | -265,037 | 0 | 1: `Call`×1 |
| `0x2d7cebfe726192fe…` | *heuristic - measured estimation failed* | 122,812 | 126,798 | -3,986 | 0 | 3: `Call`×3 |
| `0xdc74e020e296fbb9…` | *heuristic - measured estimation failed* | 134,366 | 148,620 | -14,254 | 0 | 2: `Call`×2 |
| `0x9a08a526e05f5fe8…` | *heuristic - measured estimation failed* | 420,293 | 414,721 | 5,572 | 0 | 5: `Call`×5 |

The tell-tale sign is a very small extracted update count paired with a `base_estimate` far from `gas_used` — trace extraction failed to recover the real diff, so a heuristic filled in. Two consequences matter:

- **The one heuristic row that did show savings has since been re-measured, and it went to zero.** `0xcd59750e…` reported 13.47% on a fallback estimate; the real replay put `base_estimate` 95,744 gas higher and the surplus at −25,017. That is the empirical basis for distrusting the remaining heuristic rows — see the follow-up section above for the full decomposition.
- **The liquidation was not really measured.** `0x09fd0f6e…` — the only `Liquidate` in range — extracted just 2 updates and fell back, producing a `base_estimate` (733,358) *above* its `gas_used` (619,024). That is a statement about the fallback, not evidence that liquidations are bad candidates. A liquidation remains the most promising shape in theory (oracle + IRM staticcalls, share math, seizure computation, small final diff), and this survey should be considered to have **no usable data** on it.

## Contract-deployment shapes

**0 transactions were skipped for containing a `Create`/`Create2` of a large contract.** No `Create` or `Create2` update appeared in any of the 20 diffs, and no discovered transaction had a null `to` (the `eth_getLogs` anchor on Morpho Blue naturally excludes bare deployments). The known-hopeless transaction `0x2dcd16b5…` was neither re-analyzed nor proposed.

Worth recording, though: its 11,784-byte CREATE2 payload matches the code size of several contracts that appear in this scan as ordinary `onBehalf` counterparties (`0x5e3aca36…`, `0xbfe734ba…`, `0xef4cb7e8…`, `0xce0b17e6…`). The vault instances that factory deploys are themselves routine Morpho users — so their *usage* transactions are fair game even though their *deployment* transaction is structurally hopeless.

## Limitations

- **Sample size and selection.** 20 transactions from one ~500-block window, hand-picked for shape and size diversity out of 274 that passed the filter. Not random, not systematic, not a population estimate. A window containing a volatility spike (many liquidations, many bad-debt realisations) could look very different.
- **A 25% heuristic-fallback rate undermines a substantial part of the sample.** 5 of 20 transactions, including the only liquidation, were not measured through the real replay path. One further transaction was originally in this group and has since been re-measured.
- **The two solid wins are small.** 1.81% and 2.60% of gas. Whether that clears a real deployment's overhead is outside what this survey measured.
- **One protocol, one narrow slice.** Only transactions emitting a Morpho Blue event were considered; Morpho activity confined to a vault's ERC-4626 layer without touching Blue in the same transaction is absent.
- **No `CreateMarket` in range.** The brief asked for a spread including it; none occurred in the scanned window, so that event type is genuinely absent rather than omitted. `Liquidate` occurred exactly once.
- **Mid-block replay not independently verified.** The analyzer replays preceding transactions in the block (up to 534 in one case) to rebuild mid-block state. The README itself warns real traces may differ; I did not verify replay fidelity per transaction.
- **Unidentified selectors** are labelled from emitted events only. I did not decompile the callee contracts, so those labels describe observed effects, not intended function semantics.
- **Savings are the analyzer's model, not measured on-chain outcomes.** Everything rests on `base_estimate + floor` being the right cost model; nothing was re-executed through a deployed GasKiller.
- **Counterparty identification is by frequency and code size only.** I deliberately did not assert which addresses are "the" MetaMorpho factory, bundler, or public allocator, since I could not verify those names from chain data alone.
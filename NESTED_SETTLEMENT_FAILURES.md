# Nested settlement: failing transactions

Tested 2026-10-07 against branch `nested-settlement` at `889de30`, with
`cargo run -- t <tx> --owned <addrs>`. The goal was to retry transactions that scored 0%
because of external calls.

## What worked

| tx | what | owned | before | nested (Schnorr) |
|---|---|---|---:|---:|
| [`0x15082298…`](https://etherscan.io/tx/0x150822981204592e4cfa340ba2e63e607a1c6ded490b988f9a8bd37c1f2b46d0) | Privacy Pools relayed withdrawal | pool, verifier | 0% | **46.14%** |
| [`0xad4ac41d…`](https://etherscan.io/tx/0xad4ac41d7ad3ba9792d7c426631dba0d46a31e271f5105dbb6aa6df349c891a5) | Privacy Pools relayed withdrawal | pool, verifier | 0% | **43.37%** |
| [`0x03ebad9a…`](https://etherscan.io/tx/0x03ebad9a10bc3dc5ad36613de80975b7ee8061d7fa74367f1a9aa04e77cc1524) | Privacy Pools relayed withdrawal | pool, verifier | 0% | **43.36%** |
| [`0x4d8f00ee…`](https://etherscan.io/tx/0x4d8f00ee277c67f95049a43dfe604418d0a408fed40a6473bd5b154045c2e2e2) | Privacy Pools relayed withdrawal | pool, verifier | 0% | **32.31%** |
| [`0x53e6375e…`](https://etherscan.io/tx/0x53e6375e0156f40d6917e8d48d26f0af6e1fd197d54d0042b96138dfc449660f) | Privacy Pools Entrypoint deposit | pool | 0% | 4.28% |
| [`0x2d754e6f…`](https://etherscan.io/tx/0x2d754e6f8a34058e5d07596e627cbc70e0c704279e9f745a4c0baef80389cca7) | Railgun RelayAdapt | smart wallet | 0% | **54.64%** |

## What failed

Every analyzer failure below aborts the whole report with
`RevertingContext(index, target, revertData, callargs)` (`0x493f09c4`). That error means one
replayed `CALL` reverted. The decoded fields are given for each transaction.

### 1. Parent depends on what the child did: Privacy Pools USDT relay

- **tx** [`0x2766c992…`](https://etherscan.io/tx/0x2766c992f22f5aec9bdfc1f16e394d4c4ed6b996bd002f64235b5234e9269cd2)
- **owned:** pool `0xe859c0bd…`, verifier `0x022891f9…`
- **fails at:** frame 0 (Entrypoint `0x6818809e…`), op 1. This is `USDT.transfer(0xf4f78d98…, 1,006.03 USDT)`, the relayer fee. USDT reverts with no data.
- **why:** in the real transaction, the pool (frame 1) sends the withdrawn USDT to the Entrypoint, and the Entrypoint then pays the fee from it. Frame 0 is priced from the state before the transaction, so the Entrypoint holds no USDT yet.
- **why the ETH relays pass:** the same pattern pays the fee in ETH there, and the replay doesn't fail on it.

### 2. Child depends on what the parent did: deposit through PPRouter

- **tx** [`0x1a18f555…`](https://etherscan.io/tx/0x1a18f5556a44d6c405964019ff6a1ea6020d155777303407240269ecd65ad95a)
- **contracts:** PPRouter `0x13a0b86b…` (root) → Entrypoint `0xca1e0722…` → PoolVault `0x0eb42804…`. The Entrypoint and PoolVault were owned.
- **fails at:** frame 1 (Entrypoint), op 0. This is `ppUSDT.transferFrom(PPRouter → Entrypoint, 10.0195)`. The revert is `ERC20InsufficientAllowance(0xca1e0722…, 0, 10.0195e18)`.
- **why:** the root sets that allowance (`ppUSDT.approve`) earlier in the same transaction. Frame 1 is priced from the state before the transaction, so the allowance is 0.

**Cases 1 and 2 have one root cause.** Each frame is priced in isolation from the state before the transaction. Any frame that reads state another frame wrote earlier in the transaction reverts. That includes balances, allowances, and anything else written by the parent before a call or by the child before it returns.

**Suggested fix:** price frames in execution order, on a state that carries every effect up to the point each frame starts. When the parent resumes after a nested call, it should see the child's effects.

### 3. Gas price check: Railgun RelayAdapt, large

- **tx** [`0x7c731150…`](https://etherscan.io/tx/0x7c731150234add278ecae3ee9b6bcd35ee50435cc69bc47cce1a008e54c1f2ce)
- **owned:** smart wallet `0xfa7093cd…`
- **fails at:** frame 0, op 1, the `CALL` to the smart wallet (`transact`). It reverts with `"RailgunSmartWallet: Gas price too low"`.
- **why:** Railgun checks `tx.gasprice` against a minimum signed into the transaction. The replay runs with a lower gas price.
- **The smaller RelayAdapt tx works.** [`0x2d754e6f…`](https://etherscan.io/tx/0x2d754e6f8a34058e5d07596e627cbc70e0c704279e9f745a4c0baef80389cca7) passes, probably because its signed minimum is lower.
- **Plain mode hides this one.** Without `--owned`, the same transaction prints a result, but it is the heuristic fallback. The survey recorded it as `heur`. Nested mode has no fallback, so it errors out.
- **Suggested fix:** replay with the original transaction's gas price.

### 4. Flash-loan callback: Morpho deleverage

- **tx** [`0x16a0a31c…`](https://etherscan.io/tx/0x16a0a31c0547f2f35018c38f0c2fa3bdcf1320e6a75f998caaa957747e9dc568)
- **owned:** Morpho Blue `0xbbbbbbbb…`
- **fails at:** frame 0, op 0. This is `MorphoBlue.flashLoan(USDC, …)`, which reverts with `0x7e5f4b61`. I have not decoded that error.
- **structure:** Morpho calls back into the root (`onMorphoFlashLoan`), and that callback does about 1.5M gas of the work.
- **Plain mode** also falls back to the heuristic here.
- **Lower priority:** this needs Morpho Blue and the deleverage contract to share an owner, which isn't realistic.

## Ran, but nothing was nested

**Chainlink** ([`0xff646682…`](https://etherscan.io/tx/0xff6466828843a8e795e4b6ae1b29644a148141dd48784b4be99c58b0ad3be268),
[`0xa676c243…`](https://etherscan.io/tx/0xa676c24374af6324558937b595e3a94fca0fb817823fd22a24ea0f783ebffc6c))

- **owned:** the aggregator, plus the downstream contracts in the large transaction.
- **result:** the aggregator is reported as `called directly, but cheaper as a plain CALL (or it reverted, or uses transient storage or selfdestruct)`, so both transactions stay at 0%.
- **The message folds four different reasons into one**, so I can't tell which applies.
- **Request:** report the specific reason, and the computed benefit next to the overhead, for each contract that wasn't nested.

## Not tested: RPC rate limit

World ID [`0xa447c2d3…`](https://etherscan.io/tx/0xa447c2d3d0786a32f8b23c0f571e714e91d4d812b575d7bee27864c7c3e8c556)
and Null [`0x36b6116b…`](https://etherscan.io/tx/0x36b6116b621b2cfaa9f082f1f4295c4aca95435a105ed9f2eccce42232c555db)
both failed on `HTTP 429` (QuickNode's 50 req/s limit) while replaying the earlier transactions in the block.

- **This is our RPC plan's limit, not an analyzer bug.**
- **Nested mode makes the limit easier to hit,** because it replays the earlier transactions in the block for every frame.
- **Possible improvements:** cache the replayed state across frames, or retry on 429.

## Added after the full run (2026-10-08)

The full batch over 127 call-blocked transactions (`NESTED_SETTLEMENT_RESULTS.md`) found more
cases of the same isolation problem as cases 1 and 2. It also found two new failure types.

### More "priced in isolation" reverts

**Ondo instant mint** ([`0x10fdab16…`](https://etherscan.io/tx/0x10fdab165e),
[`0x089be390…`](https://etherscan.io/tx/0x089be390aa))
- **fails at:** op 1, `USDY.burn`, with `"ERC20: burn amount exceeds balance"`.
- **This is a regression.** The old analyzer measured both transactions (0%, under the floor). Now they produce nothing.

**Grove** ([`0x0ce6843c…`](https://etherscan.io/tx/0x0ce6843c42))
- **fails at:** op 1, a `CALL` to `MainnetController`, with `"ERC20: transfer amount exceeds balance"`.

**Morpho Bundler3** ([`0xdc74e020…`](https://etherscan.io/tx/0xdc74e020e2))
- **fails at:** op 0, a `CALL` to `GeneralAdapter1`, with `"ERC20: transfer amount exceeds allowance"`.
- The old analyzer also failed on this one; the survey used its heuristic fallback (`heur`).

### Doppler `create`: the token factory's CREATE fails

Seven `Airlock.create` transactions fail.
- **fails at:** op 0, the `CALL` to the token factory (`0xb5d97103`), which reverts with
  `0x30116425`. That is probably Solady's `DeploymentFailed()`, raised when a CREATE/CREATE2 fails.
- **Not diagnosed further.**
- The survey had these as `heur` too, so the old analyzer never measured them either.

### Performance: earlier transactions in the block are replayed once per frame

`estimate_state_changes_gas_with_preceding` builds a fresh `CacheDB` and replays every earlier
transaction in the block on each call.
- The `--owned` path calls it once per frame, for the root-only baseline and again for the nested tree.
- For a transaction late in a busy block, that is thousands of `eth_getProof` /
  `eth_getStorageAt` calls repeated 3–5 times, all fired concurrently.
- **Effect on our run:** it hit our RPC's 50 req/s limit constantly (HTTP 429), and one
  transaction took over 10 minutes.

**What I did, locally only (not pushed):** cached the `CacheDB.cache` after the first replay
and reused it for every later frame of the same transaction. Results were identical, and the
Privacy Pools relay went from minutes to 27 seconds.

**Suggestions:**
- replay once per transaction and clone the state for each frame;
- bound the concurrency of the prefetch `JoinSet`, or retry on 429.

Five transactions still timed out at 15 minutes even with the cache (Centrifuge, ENS, Grove,
Morpho and Securitize, one each).

## Reproduce

```bash
git checkout nested-settlement && cargo build --release
RPC_URL=<archive node with debug_traceTransaction> \
  ./target/release/gas-analyzer-cli --debug t <tx> --owned <addr1>,<addr2>
```

`--debug` prints the full `RevertingContext` payload. Without it, only `pricing frame N (addr)` is shown.

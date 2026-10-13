# @gas-killer/analyzer-wasm

Gas Killer's gas analyzer compiled to WebAssembly. Give it a transaction's Geth trace and it returns the state updates Gas Killer would apply, ABI-encoded, plus an estimate of the gas the transaction would use with Gas Killer.

```sh
npm install @gas-killer/analyzer-wasm
```

## Usage

The package is built for the web target. Initialize it once before calling anything:

```js
import init, { analyze_trace_bytes, analyze_prestate } from "@gas-killer/analyzer-wasm"

await init() // in Node: initSync({ module: fs.readFileSync(".../gas_killer_wasm_bg.wasm") })
```

### `analyze_trace(trace, estimator, caller, block?, origin?, value?, prestateDiff?, prestate?, gasPrice?, timestamp?)`

Analyzes the `result` of `debug_traceTransaction` with the default struct-log tracer (`{ enableMemory: true }`), passed as a JSON string.

- `estimator`: address of the gas estimator contract.
- `caller`: the transaction's sender.
- `block`: block number, as a `bigint`.
- `origin`: the transaction's target contract, for detecting re-entrant callbacks.
- `value`: the transaction's `msg.value` in wei, as a decimal or `0x` hex string. Pass it for transactions that send ETH: without it, a payload that forwards that ETH in a `CALL` can't be simulated and falls back to the heuristic.
- `prestateDiff`: the transaction's `prestateTracer` diff, as a JSON string (see `analyze_prestate`). It needs `origin`. Without it the simulation starts from an empty state, so:
  - every write is priced as a fresh 22,100-gas one, where changing a set slot costs 5,000 and rewriting its value 2,200;
  - every `CALL` that sends ETH pays the 25,000-gas charge for creating its recipient, which on chain only a new account pays.

  The diff supplies each written slot's value before the transaction and the called accounts that already existed. A recipient whose call runs code in the trace is treated as existing even without the diff, which leaves it out when its balance ends unchanged.
- `prestate`: the transaction's `prestateTracer` output in its default mode (`{ tracer: "prestateTracer" }`, no `diffMode`), as a JSON string. It has the code, balance, nonce and touched slots of every account the transaction touched. With it, the program runs at `origin` itself over that state, so each contract it calls runs its own code and is measured in the settlement's context. It needs `origin`.
- `gasPrice`: the transaction's effective gas price in wei, as for `value`, for callees that check `tx.gasprice`.
- `timestamp`: the block's timestamp, as a `bigint`, for callees that check `block.timestamp`.

```js
const result = analyze_trace(JSON.stringify(trace), estimator, receipt.from, BigInt(receipt.blockNumber), receipt.to)
// { gas_estimate, is_heuristic, encoded_updates, state_update_count, skipped_opcodes, reentered, native_call_gas, callees_executed, replay_error }
```

With `prestate`, the callees run their own code and `callees_executed` is `true`. A callee can still revert in the settlement's context, for instance a swap whose callback into the target checks state its entry point set up. It can also revert because an input is missing: without `block` and `timestamp` both run as 0. The estimate then falls back to the empty state, and `replay_error` says why the replay failed.

Against the empty state, the contracts the transaction calls have no code, and the state holds only what `prestateDiff` supplies. The simulation charges each external `CALL` the program replays for the opcode itself, and what its callee spent comes from the trace instead. That trace gas is reported as `native_call_gas`, and it's part of `gas_estimate` unless `callees_executed`. A callee can cost more or less in the settlement than in the trace, since the target's reads and earlier calls aren't replayed.

`gas_estimate` covers applying the state updates. Settling also verifies the operators' aggregate signature, which costs up to `signature_verification_gas` (50,000 gas for Schnorr). Add the two for the transaction's total cost with Gas Killer.

### `analyze_trace_bytes(response, estimator, caller, block?, origin?, value?, prestateDiff?, prestate?, gasPrice?, timestamp?)`

Takes the same arguments as `analyze_trace`, except that it accepts the whole JSON-RPC response body as a `Uint8Array`. Use it for large traces:

- The trace is never decoded into a JavaScript string, so it isn't held twice and isn't subject to the string length limit.
- Memory snapshots are skipped for steps that don't need them, so the analysis uses about 1.4× the response's size in wasm memory.

```js
const response = new Uint8Array(await (await fetch(rpcUrl, { method: "POST", body })).arrayBuffer())
try {
  const result = analyze_trace_bytes(response, estimator, receipt.from, BigInt(receipt.blockNumber), receipt.to)
} catch (e) {
  // e.name is "RpcError" when the node returned an error, or "TraceTooLargeError" when the error
  // was the provider's response size limit
}
```

### `analyze_prestate(diff, callFrame, consumer, estimator, caller, block?, value?)`

Analyzes the same transaction from two much smaller traces:

- `diff`: `debug_traceTransaction` with `{ tracer: "prestateTracer", tracerConfig: { diffMode: true } }`;
- `callFrame`: `debug_traceTransaction` with `{ tracer: "callTracer", tracerConfig: { withLog: true } }`.

`consumer` is the transaction's target contract. It only works for calls that change only that contract's storage and make no external `CALL`, `CREATE` or `SELFDESTRUCT`. Writes are priced against their original values from `diff`:

```js
const out = analyze_prestate(JSON.stringify(diff), JSON.stringify(frame), receipt.to, estimator, receipt.from, block)
if (out.eligible) use(out.result) // same shape as analyze_trace's result
else console.log(out.reason) // fall back to analyze_trace
```

### `analyze_frame_tree_bytes(response, root, owned, caller, block?, value?, prestateDiff?, prestate?, gasPrice?, timestamp?)`

Prices the transaction as a nested settlement, as if its target `root` and the contracts in `owned` had integrated the SDK. `response` is the struct-log trace's JSON-RPC response, as for `analyze_trace_bytes`, and it throws the same named errors.

A `CALL` from a frame into an owned contract becomes that contract's own frame wherever nesting is cheaper than running it natively. The result compares this tree with settling the whole transaction through `root` alone:

```js
const tree = analyze_frame_tree_bytes(response, receipt.to, [vault, ledger], receipt.from, block, tx.value.toString())
// {
//   root_only_gas, nested_gas,                          // the two settlements' totals
//   root_only_native_call_gas, nested_native_call_gas,  // trace gas of the callees each replays natively
//   frame_overhead_gas,                                 // the fixed charge per nested frame
//   signature_verification_gas,                         // paid once per tree, in neither total
//   frames: [{ target, caller, op_count, gas }],        // root first, in execution order
//   not_nested: [{ address, reason }],                  // owned contracts that got no frame, and why
//   skipped_opcodes,
// }
```

With `prestate`, both settlements run over the transaction's starting state, as for `analyze_trace`. Each frame's target keeps its storage and balance, and the contracts outside the tree run their own code. If either settlement fails that way, both fall back to the empty state, so `callees_executed` describes the pair, and `replay_error` names the settlement that failed and why. In the empty state:

- A natively replayed `CALL` runs into an address with no code. The simulation charges the opcode, and what its callee spent comes from the trace.
- The state holds only what `prestateDiff` supplies, as for `analyze_trace`. Without it every write is priced as a write to an empty slot and every ETH transfer as one to a new account, so both totals run high.

There's no heuristic fallback: a simulation failure throws. `analyze_frame_tree(trace, …)` takes the trace as a JSON string instead.

### Also exported

- `estimate_gas_heuristic(trace, origin?)`: a faster, rougher estimate, with no EVM simulation.
- `encode_trace(trace)`: the encoded state updates only.

Errors are thrown as JavaScript `Error`s.

## License

[Peer Production License](LICENSE).

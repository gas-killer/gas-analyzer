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

### `analyze_trace(trace, estimator, caller, block?, origin?, value?)`

Analyzes the `result` of `debug_traceTransaction` with the default struct-log tracer (`{ enableMemory: true }`), passed as a JSON string.

- `estimator`: address of the gas estimator contract.
- `caller`: the transaction's sender.
- `block`: block number, as a `bigint`.
- `origin`: the transaction's target contract, for detecting re-entrant callbacks.
- `value`: the transaction's `msg.value` in wei, as a decimal or `0x` hex string. Pass it for transactions that send ETH: without it, a payload that forwards that ETH in a `CALL` can't be simulated and falls back to the heuristic.

```js
const result = analyze_trace(JSON.stringify(trace), estimator, receipt.from, BigInt(receipt.blockNumber), receipt.to)
// { gas_estimate, is_heuristic, encoded_updates, state_update_count, skipped_opcodes, reentered, native_call_gas }
```

The estimate is measured against an empty state, where the contracts the transaction calls have no code. Each external `CALL` the program replays is charged its gas from the trace instead, reported as `native_call_gas` and included in `gas_estimate`.

`gas_estimate` covers applying the state updates. Settling also verifies the operators' aggregate signature, which costs up to `signature_verification_gas` (50,000 gas for Schnorr). Add the two for the transaction's total cost with Gas Killer.

### `analyze_trace_bytes(response, estimator, caller, block?, origin?, value?)`

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

`consumer` is the transaction's target contract. It only works for calls that change only that contract's storage and make no external `CALL`, `CREATE` or `SELFDESTRUCT`:

```js
const out = analyze_prestate(JSON.stringify(diff), JSON.stringify(frame), receipt.to, estimator, receipt.from, block)
if (out.eligible) use(out.result) // same shape as analyze_trace's result
else console.log(out.reason) // fall back to analyze_trace
```

### `analyze_frame_tree_bytes(response, root, owned, caller, block?, value?)`

Prices the transaction as a nested settlement, as if its target `root` and the contracts in `owned` had integrated the SDK. `response` is the struct-log trace's JSON-RPC response, as for `analyze_trace_bytes`, and it throws the same named errors.

A `CALL` from a frame into an owned contract becomes that contract's own frame wherever nesting is cheaper than running it natively. The result compares this tree with settling the whole transaction through `root` alone:

```js
const tree = analyze_frame_tree_bytes(response, receipt.to, [vault, ledger], receipt.from, block, tx.value.toString())
// {
//   root_only_gas, nested_gas,                          // the two settlements' totals
//   root_only_native_call_gas, nested_native_call_gas,  // trace gas of the CALLs each replays natively
//   frame_overhead_gas,                                 // the fixed charge per nested frame
//   signature_verification_gas,                         // paid once per tree, in neither total
//   frames: [{ target, caller, op_count, gas }],        // root first, in execution order
//   not_nested: [{ address, reason }],                  // owned contracts that got no frame, and why
//   skipped_opcodes,
// }
```

Both totals are measured against an empty state, so:

- A natively replayed `CALL` runs into an address with no code. Its gas comes from the trace instead.
- Every storage write is priced as a write to an empty slot. Against real state many are cheaper, so `nested_gas` tends to run high and the savings shown are conservative.

There's no heuristic fallback: a simulation failure throws. `analyze_frame_tree(trace, …)` takes the trace as a JSON string instead.

### Also exported

- `estimate_gas_heuristic(trace, origin?)`: a faster, rougher estimate, with no EVM simulation.
- `encode_trace(trace)`: the encoded state updates only.

Errors are thrown as JavaScript `Error`s.

## License

[Peer Production License](LICENSE).

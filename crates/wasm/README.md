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

### `analyze_trace(trace, estimator, caller, block?, origin?)`

Analyzes the `result` of `debug_traceTransaction` with the default struct-log tracer (`{ enableMemory: true }`), passed as a JSON string.

- `estimator`: address of the gas estimator contract.
- `caller`: the transaction's sender.
- `block`: block number, as a `bigint`.
- `origin`: the transaction's target contract, for detecting re-entrant callbacks.

```js
const result = analyze_trace(JSON.stringify(trace), estimator, receipt.from, BigInt(receipt.blockNumber), receipt.to)
// { gas_estimate, is_heuristic, encoded_updates, state_update_count, skipped_opcodes, reentered }
```

### `analyze_trace_bytes(response, estimator, caller, block?, origin?)`

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

### `analyze_prestate(diff, callFrame, consumer, estimator, caller, block?)`

Analyzes the same transaction from two much smaller traces:

- `diff`: `debug_traceTransaction` with `{ tracer: "prestateTracer", tracerConfig: { diffMode: true } }`;
- `callFrame`: `debug_traceTransaction` with `{ tracer: "callTracer", tracerConfig: { withLog: true } }`.

`consumer` is the transaction's target contract. It only works for calls that change only that contract's storage and make no external `CALL`, `CREATE` or `SELFDESTRUCT`:

```js
const out = analyze_prestate(JSON.stringify(diff), JSON.stringify(frame), receipt.to, estimator, receipt.from, block)
if (out.eligible) use(out.result) // same shape as analyze_trace's result
else console.log(out.reason) // fall back to analyze_trace
```

### Also exported

- `estimate_gas_heuristic(trace, origin?)`: a faster, rougher estimate, with no EVM simulation.
- `encode_trace(trace)`: the encoded state updates only.

Errors are thrown as JavaScript `Error`s.

## License

[Peer Production License](LICENSE).

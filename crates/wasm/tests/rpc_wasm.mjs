#!/usr/bin/env node
// RPC integration test for the WASM module.
//
// Run with:
//   RPC_URL=https://... node tests/rpc_wasm.mjs
//
// Prerequisites:
//   wasm-pack build --target web
//
// Uses the same test transactions as the gas-killer-analyzer repo.

import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const pkgDir = join(__dirname, "..", "pkg");

const RPC_URL = process.env.RPC_URL;
if (!RPC_URL) {
  console.error("Usage: RPC_URL=https://... node tests/rpc_wasm.mjs");
  process.exit(1);
}

// Same TX hashes and addresses used in gas-killer-analyzer (crates/core/src/constants.rs)
const ESTIMATOR_ADDRESS = "0xd682Fe2ee8bdd59fdcCc5a4962FD98c20Ef47290";
const CALLER_ADDRESS = "0x0000000000000000000000000000000000000001";
const SIMPLE_STORAGE_SET_TX = "0xccd4b5a1d020bfc69fb44452f942cdef29996fc6d822f127d9a5a6108e95c3f9";
const SIMPLE_STORAGE_DEPOSIT_TX = "0xa787da2025d8e9943cb175559aa91ab38cff62dde3fd09b6da117a38c4ccd431";

// Load and initialize the WASM module
const wasmJs = await import(join(pkgDir, "gas_killer_wasm.js"));
const wasmBytes = await readFile(join(pkgDir, "gas_killer_wasm_bg.wasm"));
await wasmJs.default(wasmBytes);

async function fetchTrace(txHash) {
  console.log(`Fetching trace for ${txHash}...`);
  const resp = await fetch(RPC_URL, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({
      jsonrpc: "2.0",
      method: "debug_traceTransaction",
      params: [txHash, { enableMemory: true }],
      id: 1,
    }),
  });

  const json = await resp.json();
  if (json.error) {
    throw new Error(`RPC error for ${txHash}: ${JSON.stringify(json.error)}`);
  }
  const trace = JSON.stringify(json.result);
  console.log(`  ${trace.length} bytes\n`);
  return trace;
}

// Fetch each unique trace once up front
console.log("Fetching traces...\n");
const traces = {
  [SIMPLE_STORAGE_SET_TX]: await fetchTrace(SIMPLE_STORAGE_SET_TX),
  [SIMPLE_STORAGE_DEPOSIT_TX]: await fetchTrace(SIMPLE_STORAGE_DEPOSIT_TX),
};

let failures = 0;

// Test 1: analyze_trace with SimpleStorage.set()
try {
  const result = wasmJs.analyze_trace(traces[SIMPLE_STORAGE_SET_TX], ESTIMATOR_ADDRESS, CALLER_ADDRESS);
  console.log("analyze_trace (SimpleStorage.set):");
  console.log(`  gas_estimate:      ${result.gas_estimate}`);
  console.log(`  is_heuristic:      ${result.is_heuristic}`);
  console.log(`  state_update_count: ${result.state_update_count}`);
  console.log(`  skipped_opcodes:   ${JSON.stringify(result.skipped_opcodes)}`);

  if (result.gas_estimate <= 0) throw new Error("gas_estimate should be positive");
  if (result.state_update_count <= 0) throw new Error("state_update_count should be positive");
  if (!result.encoded_updates.startsWith("0x")) throw new Error("encoded_updates should be hex-prefixed");
  console.log("  PASS\n");
} catch (e) {
  console.error(`  FAIL: ${e.message}\n`);
  failures++;
}

// Test 2: analyze_trace with SimpleStorage.deposit() (SSTOREs + LOGs)
try {
  const result = wasmJs.analyze_trace(traces[SIMPLE_STORAGE_DEPOSIT_TX], ESTIMATOR_ADDRESS, CALLER_ADDRESS);
  console.log("analyze_trace (SimpleStorage.deposit):");
  console.log(`  gas_estimate:      ${result.gas_estimate}`);
  console.log(`  is_heuristic:      ${result.is_heuristic}`);
  console.log(`  state_update_count: ${result.state_update_count}`);

  if (result.gas_estimate <= 0) throw new Error("gas_estimate should be positive");
  if (result.state_update_count <= 1) throw new Error("deposit should produce multiple state updates");
  console.log("  PASS\n");
} catch (e) {
  console.error(`  FAIL: ${e.message}\n`);
  failures++;
}

// Test 3: encode_trace
try {
  const result = wasmJs.encode_trace(traces[SIMPLE_STORAGE_SET_TX]);
  console.log("encode_trace (SimpleStorage.set):");
  console.log(`  state_update_count: ${result.state_update_count}`);
  console.log(`  encoded_len:       ${result.encoded_updates.length} chars`);

  if (result.state_update_count <= 0) throw new Error("state_update_count should be positive");
  if (!result.encoded_updates.startsWith("0x")) throw new Error("encoded_updates should be hex-prefixed");
  console.log("  PASS\n");
} catch (e) {
  console.error(`  FAIL: ${e.message}\n`);
  failures++;
}

// Test 4: estimate_gas_heuristic
try {
  const result = wasmJs.estimate_gas_heuristic(traces[SIMPLE_STORAGE_SET_TX]);
  console.log("estimate_gas_heuristic (SimpleStorage.set):");
  console.log(`  gas_estimate:      ${result.gas_estimate}`);
  console.log(`  is_heuristic:      ${result.is_heuristic}`);

  if (result.gas_estimate < 21000) throw new Error("gas should be at least BASE_TX_COST");
  if (result.is_heuristic !== true) throw new Error("should always be heuristic");
  console.log("  PASS\n");
} catch (e) {
  console.error(`  FAIL: ${e.message}\n`);
  failures++;
}

// Test 5: all 3 paths agree on state_update_count
try {
  const trace = traces[SIMPLE_STORAGE_SET_TX];
  const analyze = wasmJs.analyze_trace(trace, ESTIMATOR_ADDRESS, CALLER_ADDRESS);
  const encode = wasmJs.encode_trace(trace);
  const heuristic = wasmJs.estimate_gas_heuristic(trace);

  console.log("consistency check:");
  if (analyze.state_update_count !== encode.state_update_count) {
    throw new Error(`analyze (${analyze.state_update_count}) != encode (${encode.state_update_count})`);
  }
  if (analyze.state_update_count !== heuristic.state_update_count) {
    throw new Error(`analyze (${analyze.state_update_count}) != heuristic (${heuristic.state_update_count})`);
  }
  console.log(`  all paths agree: ${analyze.state_update_count} state updates`);
  console.log("  PASS\n");
} catch (e) {
  console.error(`  FAIL: ${e.message}\n`);
  failures++;
}

if (failures > 0) {
  console.error(`${failures} test(s) failed`);
  process.exit(1);
} else {
  console.log("All WASM RPC integration tests passed!");
}

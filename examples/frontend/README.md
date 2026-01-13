# Frontend Example - WebAssembly Usage

This directory contains an example demonstrating how to use the `gas-analyzer-rs` WASM module in a frontend application.

## Prerequisites

Install `wasm-pack`:
```bash
cargo install wasm-pack
```

## Building the WASM Module

### Quick Build (using script)
```bash
# From the project root
./scripts/build-wasm.sh
```

### Manual Build
```bash
wasm-pack build --target web --out-dir pkg --no-default-features
```

**Note**: If you encounter compilation errors with dependencies like `foundry-evm-traces` or `opcode-tracer`, you may need to create a WASM-specific feature flag to exclude incompatible dependencies. See troubleshooting below.

This will create a `pkg/` directory with:
- `gas_analyzer_rs.js` - JavaScript bindings
- `gas_analyzer_rs_bg.wasm` - The compiled WebAssembly binary
- TypeScript definitions (if enabled)

## Setup

1. Build the WASM module (see above)
2. Copy the `pkg/` directory to this `examples/frontend/` directory:
   ```bash
   cp -r ../../pkg .
   ```

## Running the Example

**Important**: Due to CORS restrictions, you cannot open the HTML file directly with `file://`. You must serve it over HTTP.

### Option 1: Python HTTP Server
```bash
# From the project root
python3 -m http.server 8000
# Then open http://localhost:8000/examples/frontend/frontend-example.html
```

### Option 2: Node.js HTTP Server
```bash
# Install http-server globally
npm install -g http-server

# From the project root
http-server -p 8000
# Then open http://localhost:8000/examples/frontend/frontend-example.html
```

### Option 3: VS Code Live Server
If you're using VS Code, install the "Live Server" extension and right-click on `frontend-example.html` → "Open with Live Server"

## Usage

1. Enter an RPC URL (e.g., `https://eth-sepolia.g.alchemy.com/v2/YOUR_KEY`)
2. Enter a transaction hash
3. Click "Analyze Transaction"

The example will:
- Fetch the transaction trace from the RPC endpoint
- Analyze it using the WASM module
- Display state updates, breakdown, and encoded ABI

## Using in Your Own Project

### Basic Usage

```javascript
import init, { analyze_trace } from './pkg/gas_analyzer_rs.js';

// Initialize the WASM module (call once)
await init();

// Fetch trace from RPC (you need to implement this)
const trace = await fetchTraceFromRPC(rpcUrl, txHash);

// Analyze the trace
const analysisJson = analyze_trace(JSON.stringify(trace));
const analysis = JSON.parse(analysisJson);

console.log(`Found ${analysis.state_update_count} state updates`);
console.log(`SSTOREs: ${analysis.state_update_breakdown.stores}`);
console.log(`CALLs: ${analysis.state_update_breakdown.calls}`);
```

### Available Functions

#### Main Function (Recommended)

- **`analyze_trace(trace_json: string) -> string`**
  - **Main function for frontend use** - analyzes a Geth trace and returns comprehensive results
  - Takes: Geth trace JSON string
  - Returns: JSON with `TraceAnalysis` containing:
    - `state_update_count`: Total number of state updates
    - `state_update_breakdown`: Counts by type (stores, calls, log0-4)
    - `skipped_opcodes`: List of skipped opcodes (CREATE, CREATE2, SELFDESTRUCT)
    - `encoded_abi`: Hex-encoded state updates ready for state handler contract
    - `estimated_gas`: Heuristic gas estimate (rough approximation)

#### Lower-Level Functions

- `compute_state_updates_from_trace_json(trace_json: string) -> string`
  - Computes state updates from a Geth trace JSON string
  - Returns JSON with `{ state_updates: [...], skipped_opcodes: [...] }`

- `encode_state_updates_to_abi(state_updates_json: string) -> string`
  - Encodes state updates to ABI format
  - Returns hex string (0x-prefixed)

- `parse_trace_memory(memory: string[]) -> string`
  - Parses trace memory from hex string array
  - Returns hex string (0x-prefixed)

### Target Options

When building with `wasm-pack`, you can specify different targets:

- `--target web` - For use with ES modules in browsers (recommended for most cases)
- `--target bundler` - For use with webpack/rollup/vite
- `--target nodejs` - For use in Node.js
- `--target no-modules` - For use without a bundler (legacy)

## Limitations

⚠️ **Note**: Functions that require Anvil (like `GasKiller` for gas estimation) are **not available in WASM** because Anvil is a native binary. Only pure computation functions are exposed.

⚠️ **CORS Note**: When fetching traces from RPC endpoints in the browser, you may encounter CORS issues. Solutions:
- Use a CORS proxy
- Use an RPC provider that supports CORS (some do)
- Make RPC calls from your backend and proxy them to the frontend
- Use a browser extension that disables CORS (development only)

## Troubleshooting

### CORS Errors
If you see CORS errors, make sure you're serving the files over HTTP, not opening them directly with `file://`.

### RPC Errors
Some RPC providers may block browser requests. If you encounter CORS issues with the RPC endpoint:
- Use a CORS proxy
- Make RPC calls from your backend and proxy them
- Use an RPC provider that supports CORS

### Dependency Compilation Errors

If you get errors about dependencies not compiling to WASM (e.g., `foundry-evm-traces`), you have two options:

1. **Exclude problematic dependencies** by creating a WASM feature flag in `Cargo.toml`:
   ```toml
   [features]
   default = ["foundry-evm-traces", "opcode-tracer"]
   wasm = []  # Exclude native dependencies
   ```
   Then build with: `wasm-pack build --target web --out-dir pkg --no-default-features --features wasm`

2. **Use alternative implementations** for WASM that don't require those dependencies

### Size Optimization

The WASM binary can be large. To reduce size:
- Use `wasm-opt` after building: `wasm-opt -Os pkg/gas_analyzer_rs_bg.wasm -o pkg/gas_analyzer_rs_bg.wasm`
- LTO and size optimizations are already configured in `Cargo.toml` under `[profile.release]`

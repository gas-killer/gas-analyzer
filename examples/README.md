# Examples

This directory contains example code demonstrating how to use `gas-analyzer-rs` in different contexts.

## Directory Structure

- **[`frontend/`](./frontend/)** - WebAssembly frontend example
  - Demonstrates using the WASM module in a browser
  - Includes HTML and JavaScript example files
  - See [`frontend/README.md`](./frontend/README.md) for setup instructions

- **[`tx-extractor/`](./tx-extractor/)** - Rust transaction extractor example
  - Demonstrates using the library as a Rust dependency
  - Shows how to extract state updates from transactions
  - See [`tx-extractor/TX_EXTRACTOR_README.md`](./tx-extractor/TX_EXTRACTOR_README.md) for usage

## Quick Start

### Frontend Example
```bash
# Build WASM
./scripts/build-wasm.sh

# Copy pkg/ to examples/frontend/
cp -r pkg examples/frontend/

# Serve and open
cd examples/frontend
python3 -m http.server 8000
# Open http://localhost:8000/frontend-example.html
```

### Transaction Extractor Example
```bash
# Run the example
RPC_URL=https://your-rpc cargo run --example tx_extractor_example 0xTRANSACTION_HASH
```

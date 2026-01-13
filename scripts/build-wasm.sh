#!/bin/bash
# Build script for WebAssembly

set -e

echo "Building gas-analyzer-rs for WebAssembly..."

# Check if wasm-pack is installed
if ! command -v wasm-pack &> /dev/null; then
    echo "Error: wasm-pack is not installed"
    echo "Install it with: cargo install wasm-pack"
    exit 1
fi

# Build for web target
# Use --no-default-features to exclude foundry-evm-traces, opcode-tracer, and tokio
# The WASM module has its own implementation that doesn't need these
wasm-pack build --target web --out-dir pkg --no-default-features

# Update package.json with scoped name and GitHub Packages configuration
cd pkg
npm pkg set name="@breadcoop/gas-analyzer-rs"
npm pkg set publishConfig.registry="https://npm.pkg.github.com"
npm pkg set publishConfig."@breadcoop:registry"="https://npm.pkg.github.com"
cd ..

echo "Build complete! Output is in ./pkg/"
echo ""
echo "To use in your frontend:"
echo "  import init, { compute_state_updates_from_trace_json } from './pkg/gas_analyzer_rs.js';"
echo "  await init();"

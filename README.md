# Gas Analyzer

Compute state update instructions for gas killer application and estimate gas savings

## Implementation Notes
- Uses transaction tracing API (not trace call) since trace call can't produce execution traces
- Executes transactions in forked Anvil (non-forked can't generate Geth traces)
- Note: Real blockchain traces may differ due to other transactions in block
- Ignores transactions that 
   - are below the gas limit
   - do not call a smart contract
   - create a smart contract

## Setup
1. Clone the repository
2. Copy the example environment file:
   ```bash
   cp .env.example .env
   ```
3. Fill in the required environment variables in `.env`:

## Tests
```bash
cargo test
```


## CLI (unstable)
The CLI currently supports analyzing single transactions and complete blocks (using the respective hashes) or transaction requests (provided as json files).

For a transaction: 
```bash
cargo run -- t aecc4a9d20d48a84989bca3ffaf1001c8965d86d90ba688020deb958ddf9ed12
```
For a block: 

```bash
cargo run -- b 0x386725b93d39849e06d42c52b6ed492d98459f12db1f6c124ab483f5e7a64375
cargo run -- b latest
```

For a transaction request:
```bash
cargo run -- r path/to/file.json
```

The analysis report is written to the `OUTPUT_FILE`.

## WebAssembly Build

This library can be compiled to WebAssembly for use in frontend applications.

### Quick Start

1. Install `wasm-pack`: `cargo install wasm-pack`
2. Build: `./scripts/build-wasm.sh`
3. See [`examples/frontend/README.md`](./examples/frontend/README.md) for detailed usage instructions and examples

### Publishing to GitHub Packages

The WASM package is automatically built on pushes to `main` and can be published manually via GitHub Actions workflow. See [`.github/workflows/publish-wasm.yml`](./.github/workflows/publish-wasm.yml) for details.

The published package is available as `@breadcoop/gas-analyzer-rs` on GitHub Packages.

### Documentation

For complete documentation on:
- Building and using the WASM module
- Available functions and API
- Troubleshooting
- Integration examples

See the [Frontend Example README](./examples/frontend/README.md).
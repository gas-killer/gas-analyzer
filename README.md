# Gas Analyzer

Compute state update instructions for gas killer application and estimate gas savings

## Implementation Notes
- Default mode uses EvmSketch for Anvil-free transaction simulation
- Legacy Anvil mode available via `--anvil` flag for precise gas estimation
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
The CLI supports analyzing single transactions and transaction requests. By default, it uses EvmSketch (Anvil-free) for analysis.

### Analyze a transaction
```bash
cargo run -- t aecc4a9d20d48a84989bca3ffaf1001c8965d86d90ba688020deb958ddf9ed12
```

### Analyze a transaction request
```bash
cargo run -- r path/to/file.json
```

### Legacy Anvil Mode

To use the legacy Anvil-based implementation (requires running Anvil, provides precise gas estimates):

```bash
# Build with anvil feature
cargo build --features anvil

# Run with --anvil flag
cargo run --features anvil -- --anvil t aecc4a9d20d48a84989bca3ffaf1001c8965d86d90ba688020deb958ddf9ed12
```

### Block Analysis (Anvil only)

Block analysis requires Anvil for generating detailed reports:

```bash
cargo run --features anvil -- b 0x386725b93d39849e06d42c52b6ed492d98459f12db1f6c124ab483f5e7a64375
cargo run --features anvil -- b latest
```

The analysis report is written to the `OUTPUT_FILE`.

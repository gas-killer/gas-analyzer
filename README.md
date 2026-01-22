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
The CLI supports analyzing single transactions and transaction requests. By default, it uses EvmSketch for analysis.

> **Note:** Examples are mainnet transactions. Ensure `.env` has a mainnet RPC set or update to transactions on your target network.


### Analyze a transaction
```bash
cargo run -- t 0x9add9d0f26bc6d867c1d6d41dda6287d9721a377cea42440250884f76d2a0fa7
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
cargo run --features anvil -- --anvil t 0x9add9d0f26bc6d867c1d6d41dda6287d9721a377cea42440250884f76d2a0fa7
```

### Block Analysis (Anvil only)

Block analysis requires Anvil for generating detailed reports:

```bash
cargo run --features anvil -- b 0x386725b93d39849e06d42c52b6ed492d98459f12db1f6c124ab483f5e7a64375
cargo run --features anvil -- b latest
```

The analysis report is written to the `OUTPUT_FILE`.

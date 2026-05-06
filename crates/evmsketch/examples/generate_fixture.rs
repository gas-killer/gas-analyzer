//! Fetches a Sepolia trace from a live node and writes it to
//! `benches/fixtures/sepolia_trace.json` so the `trace_parsing` bench
//! can run offline.
//!
//! Usage:
//!   RPC_URL=<sepolia-node> cargo run -p gas-analyzer-evmsketch --example generate_fixture

use alloy::primitives::FixedBytes;
use alloy::providers::ProviderBuilder;
use anyhow::Result;

const SEPOLIA_TX_HASH: &str = "0x680e2abfbccaf6246b4bda0989fc55dee169d0f6aef2ca4c63a17c6a8a39d6cb";

const FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/benches/fixtures/sepolia_trace.json"
);

fn main() -> Result<()> {
    let rpc_url = std::env::var("RPC_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .expect("RPC_URL env var is required (Sepolia node with debug_traceTransaction support)");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async {
        let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
        let hash: FixedBytes<32> = SEPOLIA_TX_HASH.parse().expect("invalid tx hash constant");

        eprintln!("Fetching trace for {SEPOLIA_TX_HASH} ...");
        let trace = gas_analyzer_rpc::get_tx_trace(&provider, hash).await?;

        let json = serde_json::to_string_pretty(&trace)?;

        std::fs::create_dir_all(std::path::Path::new(FIXTURE_PATH).parent().unwrap())?;
        std::fs::write(FIXTURE_PATH, &json)?;

        let struct_log_count = trace.struct_logs.len();
        eprintln!("Written {struct_log_count} struct-log entries to {FIXTURE_PATH}");
        Ok::<_, anyhow::Error>(())
    })
}

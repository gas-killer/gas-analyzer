//! Phase-by-phase latency profiler for the EvmSketch path.
//!
//! Investigates issue #119: where does time go between fork-setup
//! (`build()`) and gas estimation? Run against a real RPC with:
//!
//! ```bash
//! RPC_URL=... cargo run --release --example bench_phases -p gas-analyzer-evmsketch -- \
//!     0x9add9d0f26bc6d867c1d6d41dda6287d9721a377cea42440250884f76d2a0fa7
//! ```
//!
//! Each `EvmSketchExecutorBuilder::build()` is run REPEATS times (cold per
//! call — fresh `RootProvider`, no connection pooling) so we get a sense of
//! variance versus a single sample. The downstream phases (trace, compute,
//! estimate) run once each since they are the same shape every call.

use std::time::Instant;

use alloy::providers::ProviderBuilder;
use alloy_eips::BlockNumberOrTag;
use alloy_provider::Provider;
use anyhow::{Result, anyhow};
use gas_analyzer_core::compute_state_updates;
use gas_analyzer_evmsketch::GasKillerEvmSketchDefault;
use gas_analyzer_rpc::get_tx_trace;
use url::Url;

const DEFAULT_TX: &str = "0x9add9d0f26bc6d867c1d6d41dda6287d9721a377cea42440250884f76d2a0fa7";
const REPEATS: usize = 5;

// `SimpleRpcDb` uses `block_in_place` inside its `DatabaseRef` impls, which
// only works on the multi-thread runtime. The CLI uses `rt-multi-thread`
// for the same reason — keep this in sync.
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let _ = dotenv::dotenv();
    let rpc_url: Url = std::env::var("RPC_URL")
        .map_err(|_| anyhow!("RPC_URL must be set"))?
        .parse()?;

    let tx_arg = std::env::args().nth(1).unwrap_or_else(|| DEFAULT_TX.to_string());
    let tx_hash: [u8; 32] = hex_to_bytes32(&tx_arg)?;

    let provider = ProviderBuilder::new().connect_http(rpc_url.clone());

    // ---------- Phase 0: receipt + tx fetch (CLI front-matter) ----------
    let t0 = Instant::now();
    let receipt = provider
        .get_transaction_receipt(tx_hash.into())
        .await?
        .ok_or_else(|| anyhow!("no receipt"))?;
    let receipt_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let block_number = receipt.block_number.ok_or_else(|| anyhow!("no block number"))?;
    let to_address = receipt.to.ok_or_else(|| anyhow!("tx has no 'to' (skipping)"))?;
    let from_address = receipt.from;

    println!("--- target ---");
    println!("  tx        {tx_arg}");
    println!("  block     {block_number}");
    println!();
    println!("--- pre-pipeline (CLI does these once) ---");
    println!("  eth_getTransactionReceipt   {receipt_ms:>8.1} ms");
    println!();

    // ---------- Phase 1: EvmSketchExecutorBuilder::build() ----------
    // This is the headline. Repeat to see variance. Each call constructs a
    // fresh RootProvider, so there is no connection reuse across iterations
    // — we are measuring true cold latency, which is the validator's
    // worst case.
    println!("--- build() (the bottleneck) — {REPEATS}× cold ---");
    let mut build_samples = Vec::with_capacity(REPEATS);
    for i in 0..REPEATS {
        let t0 = Instant::now();
        let _gk = GasKillerEvmSketchDefault::builder(rpc_url.clone())
            .at_block(BlockNumberOrTag::Number(block_number))
            .build()
            .await?;
        let dt = t0.elapsed().as_secs_f64() * 1000.0;
        build_samples.push(dt);
        println!("  iter {i}                       {dt:>8.1} ms");
    }
    let (min, med, max) = stats(&build_samples);
    println!("  -> min/median/max            {min:>5.1} / {med:>5.1} / {max:>5.1} ms");
    println!();

    // Time `eth_chainId` alone — that round-trip is one of the three RPC
    // calls inside our `build()`, and unlike the two `eth_getBlockByNumber`
    // calls it is trivially cacheable across requests. We subtract it from
    // the build() median below to estimate the floor after caching.
    println!("--- eth_chainId alone (cacheable) — {REPEATS}× ---");
    let mut chainid_samples = Vec::with_capacity(REPEATS);
    for i in 0..REPEATS {
        let t0 = Instant::now();
        let _ = provider.get_chain_id().await?;
        let dt = t0.elapsed().as_secs_f64() * 1000.0;
        chainid_samples.push(dt);
        println!("  iter {i}                       {dt:>8.1} ms");
    }
    let (min, med, max) = stats(&chainid_samples);
    println!("  -> min/median/max            {min:>5.1} / {med:>5.1} / {max:>5.1} ms");
    println!();

    // Time a single `eth_getBlockByNumber(N)` against a fresh provider —
    // this is the floor for build(): the *one* round-trip we actually need
    // (header anchor). The current builder does *two* (the second fetches
    // block N-1 only to seed `BasicRpcDb`, which our crate replaces with
    // `SimpleRpcDb` and never reads from). See sketch_builder.rs:172-196
    // and lib.rs:191/322/369. The gap between this and `build()` above is
    // the dead weight.
    println!("--- single eth_getBlockByNumber(N) (theoretical lean floor) — {REPEATS}× cold ---");
    let mut lean_samples = Vec::with_capacity(REPEATS);
    for i in 0..REPEATS {
        let cold_provider = ProviderBuilder::new().connect_http(rpc_url.clone());
        let t0 = Instant::now();
        let _block = cold_provider
            .get_block(BlockNumberOrTag::Number(block_number).into())
            .await?
            .ok_or_else(|| anyhow!("block {block_number} not found"))?;
        let dt = t0.elapsed().as_secs_f64() * 1000.0;
        lean_samples.push(dt);
        println!("  iter {i}                       {dt:>8.1} ms");
    }
    let (min, med, max) = stats(&lean_samples);
    println!("  -> min/median/max            {min:>5.1} / {med:>5.1} / {max:>5.1} ms");
    println!();

    // ---------- Phase 2-4: the rest of the pipeline (one shot) ----------
    let gk = GasKillerEvmSketchDefault::builder(rpc_url.clone())
        .at_block(BlockNumberOrTag::Number(block_number))
        .build()
        .await?;

    let t0 = Instant::now();
    let trace = get_tx_trace(&provider, tx_hash.into()).await?;
    let trace_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t0 = Instant::now();
    let (state_updates, _skipped, _call_gas) = compute_state_updates(trace)?;
    let compute_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let t0 = Instant::now();
    let _gas = gk.estimate_state_changes_gas(to_address, from_address, &state_updates)?;
    let estimate_ms = t0.elapsed().as_secs_f64() * 1000.0;

    println!("--- downstream phases (one shot) ---");
    println!("  debug_traceTransaction      {trace_ms:>8.1} ms   (RPC, heavy)");
    println!("  compute_state_updates       {compute_ms:>8.1} ms   (CPU)");
    println!("  estimate_state_changes_gas  {estimate_ms:>8.1} ms   (revm + per-slot RPC)");
    println!("  state_updates count         {:>8}", state_updates.len());
    println!();

    // ---------- Summary ----------
    let build_med = stats(&build_samples).1;
    let chainid_med = stats(&chainid_samples).1;
    let lean_med = stats(&lean_samples).1;
    let total = build_med + trace_ms + compute_ms + estimate_ms;
    println!("--- summary ---");
    println!("  build()                     {build_med:>8.1} ms ({:>5.1}%)", pct(build_med, total));
    println!("  debug_traceTransaction      {trace_ms:>8.1} ms ({:>5.1}%)", pct(trace_ms, total));
    println!("  compute_state_updates       {compute_ms:>8.1} ms ({:>5.1}%)", pct(compute_ms, total));
    println!("  estimate_state_changes_gas  {estimate_ms:>8.1} ms ({:>5.1}%)", pct(estimate_ms, total));
    println!("  ----                        --------");
    println!("  total                       {total:>8.1} ms");
    println!();
    println!();
    println!("--- optimization potential ---");
    println!("  current build() (cold)              {build_med:>8.1} ms");
    println!("  - chain_id (cacheable across reqs)  {chainid_med:>8.1} ms");
    println!("  - dead-weight get_block(N-1)        {:>8.1} ms (≈ single get_block above)", lean_med);
    println!("  = lean theoretical floor            {:>8.1} ms", build_med - chainid_med - lean_med);
    println!();
    println!("  even at full elimination: trace dominates ({:.0}× larger than build)", trace_ms / build_med);

    Ok(())
}

fn hex_to_bytes32(s: &str) -> Result<[u8; 32]> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = alloy::hex::decode(s).map_err(|e| anyhow!("bad hex: {e}"))?;
    if bytes.len() != 32 {
        return Err(anyhow!("expected 32 bytes, got {}", bytes.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn stats(xs: &[f64]) -> (f64, f64, f64) {
    let mut s = xs.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = s[s.len() / 2];
    (s[0], med, s[s.len() - 1])
}

fn pct(a: f64, total: f64) -> f64 {
    if total == 0.0 { 0.0 } else { 100.0 * a / total }
}

//! `h <request.json>` — extract a tracked function that may read historical state.

use alloy::primitives::{Address, Bytes, U256};
use alloy::rpc::types::eth::TransactionRequest;
use alloy_provider::{Provider, ProviderBuilder};
use anyhow::{Context, Result, anyhow};
use colored::Colorize;
use gas_analyzer_core::{
    HistoricalRead, HistoryReadKind, SimProfile, TURETZKY_UPPER_GAS_LIMIT_BLS,
    TURETZKY_UPPER_GAS_LIMIT_SCHNORR,
};
use gas_analyzer_evmsketch::{
    EvmSketchExecutorCache, StateEncoding, call_to_encoded_state_updates_with_history,
};
use serde::Deserialize;
use url::Url;

/// The request file. Field names follow `eth_call` conventions.
///
/// ```json
/// {
///   "to": "0x…",            // the tracked contract (required)
///   "data": "0x…",          // calldata for the tracked function (required)
///   "from": "0x…",          // caller; defaults to the zero address
///   "value": "0x0",         // msg.value; defaults to 0
///   "gas": 3000000,         // tx gas limit under the chain profile; defaults to the block gas limit
///   "block": 12345678,      // execution block; defaults to latest
///   "encoding": "prestate-net", // "prestate-net" (default), "canonical" or "legacy"
///   "profile": "chain"      // "chain" (default) or "unbounded"
/// }
/// ```
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryRequestFile {
    to: Address,
    data: Bytes,
    #[serde(default)]
    from: Option<Address>,
    #[serde(default)]
    value: Option<U256>,
    #[serde(default)]
    gas: Option<u64>,
    #[serde(default)]
    block: Option<u64>,
    #[serde(default)]
    encoding: Option<String>,
    #[serde(default)]
    profile: Option<String>,
}

fn parse_encoding(s: Option<&str>) -> Result<StateEncoding> {
    match s.unwrap_or("prestate-net") {
        "prestate-net" | "prestate_net" | "net" => Ok(StateEncoding::PrestateNet),
        "canonical" => Ok(StateEncoding::Canonical),
        "legacy" => Ok(StateEncoding::Legacy),
        other => Err(anyhow!(
            "unknown encoding {other:?}: use prestate-net, canonical or legacy"
        )),
    }
}

fn parse_profile(s: Option<&str>) -> Result<SimProfile> {
    match s.unwrap_or("chain") {
        "chain" => Ok(SimProfile::Chain),
        "unbounded" => Ok(SimProfile::Unbounded),
        other => Err(anyhow!("unknown profile {other:?}: use chain or unbounded")),
    }
}

fn kind_name(kind: u8) -> &'static str {
    match kind {
        k if k == HistoryReadKind::Storage as u8 => "storageAt",
        k if k == HistoryReadKind::Balance as u8 => "balanceAt",
        k if k == HistoryReadKind::CodeHash as u8 => "codeHashAt",
        k if k == HistoryReadKind::BlockHash as u8 => "blockHashAt",
        k if k == HistoryReadKind::BlockTimestamp as u8 => "blockTimestampAt",
        k if k == HistoryReadKind::Call as u8 => "callAt",
        _ => "unknown",
    }
}

fn describe(read: &HistoricalRead) -> String {
    let what = match read.kind {
        k if k == HistoryReadKind::Storage as u8 => format!("{}[{}]", read.account, read.slot),
        k if k == HistoryReadKind::Balance as u8 || k == HistoryReadKind::CodeHash as u8 => {
            read.account.to_string()
        }
        k if k == HistoryReadKind::Call as u8 => format!(
            "{} {} ({})",
            read.account,
            read.input,
            if read.success { "ok" } else { "failed" }
        ),
        _ => String::new(),
    };
    format!("{what} -> {}", read.output)
}

pub async fn run_history_request(rpc_url: &Url, path: &str) -> Result<()> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
    let req: HistoryRequestFile =
        serde_json::from_str(&raw).with_context(|| format!("{path} is not a valid request"))?;
    let encoding = parse_encoding(req.encoding.as_deref())?;
    let profile = parse_profile(req.profile.as_deref())?;

    let block = match req.block {
        Some(b) => b,
        None => ProviderBuilder::new()
            .connect_http(rpc_url.clone())
            .get_block_number()
            .await
            .context("failed to fetch the latest block number")?,
    };

    let mut tx = TransactionRequest::default()
        .from(req.from.unwrap_or_default())
        .to(req.to)
        .input(req.data.into())
        .value(req.value.unwrap_or_default());
    if let Some(gas) = req.gas {
        tx = tx.gas_limit(gas);
    }

    println!("{}", "=== Historical-state extraction ===".green().bold());
    println!("Contract: {}  Block: {block}", req.to);
    println!("Encoding: {encoding:?}  Profile: {profile:?}");

    let out = call_to_encoded_state_updates_with_history(
        &EvmSketchExecutorCache::new(1),
        rpc_url.as_str(),
        tx,
        block,
        encoding,
        profile,
    )
    .await?;
    let enc = &out.encoded;

    println!("Extraction: {}", enc.extraction.as_str());
    println!(
        "State updates: {}  Payload: {} bytes",
        enc.update_count,
        enc.storage_updates.len()
    );
    println!(
        "Gas to apply on-chain (before signature floor): {}",
        enc.gas_estimate
    );
    println!(
        "  with Schnorr verification: {}",
        enc.gas_estimate + TURETZKY_UPPER_GAS_LIMIT_SCHNORR
    );
    println!(
        "  with BLS verification:     {}",
        enc.gas_estimate + TURETZKY_UPPER_GAS_LIMIT_BLS
    );
    if !enc.skipped_opcodes.is_empty() {
        let mut ops: Vec<_> = enc.skipped_opcodes.iter().cloned().collect();
        ops.sort();
        println!("{}: {}", "Skipped opcodes".yellow(), ops.join(", "));
    }

    println!(
        "\n{} {}  (commitment {})",
        "History reads:".bold(),
        out.reads.len(),
        out.reads_commitment
    );
    for (i, r) in out.reads.iter().enumerate() {
        println!(
            "  {:>3}  {:<16} block {:<10} {}",
            i + 1,
            kind_name(r.kind),
            r.blockNumber,
            describe(r)
        );
    }
    println!("\n{}\n{}", "Payload:".bold(), enc.storage_updates);
    Ok(())
}

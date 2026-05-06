//! Records every JSON-RPC request made during the full validator-style
//! analysis flow for one or more transactions. Forwards each request to
//! the real RPC (recording layer is passthrough) and prints:
//!
//!   - Per-method counts
//!   - Duplicate detection (same method + identical params seen ≥2×)
//!   - Phase-by-phase wall-time
//!
//! Use to validate / refute the headline finding of the issue #119
//! investigation, where build() was claimed to be ~3% of per-request cost
//! and `debug_traceTransaction` ~86%.
//!
//! ```bash
//! RPC_URL=... cargo run --release --example trace_rpc -p gas-analyzer-evmsketch -- \
//!     0x9add9d0f26bc6d867c1d6d41dda6287d9721a377cea42440250884f76d2a0fa7 \
//!     0x...another...
//! ```
//!
//! Modes:
//!   - `SKIP_PRECEDING=1`     — skip preceding-tx replay (simulate tx_index 0)
//!   - `USE_BASIC_DB=1`       — use sp1-cc's BasicRpcDb + finalize() (implies SKIP_PRECEDING)
//!   - `BASIC_AT_PREV=1`      — when USE_BASIC_DB is set, anchor a *standalone*
//!                              BasicRpcDb at N-1 with a placeholder state_root
//!                              (revert goes away, but finalize on the at-N sketch
//!                              has nothing to do — useful only for debugging the
//!                              revert)
//!   - `FULL_PROOF_PREP=1`    — when USE_BASIC_DB is set, build a *second* sketch
//!                              anchored at N-1 so its rpc_db AND state_root pair
//!                              correctly. Estimate runs against this sketch's
//!                              BasicRpcDb (succeeds), then finalize() runs against
//!                              the same sketch (does real per-account proof work).
//!                              This is the load-bearing measurement of sp1-cc
//!                              proof-preparation overhead.
//!   - `DEBUG_B=1`            — when USE_BASIC_DB is set, wrap the DB with a logging
//!                              adapter, decode the revert error in detail, dump every
//!                              state read, and (if ETHERSCAN_API_KEY is set) fetch the
//!                              contract source name from Etherscan

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

thread_local! {
    /// Wall-time of the second `EvmSketch::build()` (anchored at N-1) when
    /// FULL_PROOF_PREP is set. Threaded through a thread-local so the report
    /// section at the bottom of `trace_one_tx` can surface it without
    /// reshuffling the existing function-local timing variables.
    static FULL_PROOF_PREP_BUILD_PREV_MS: Cell<f64> = const { Cell::new(0.0) };
}

use alloy::primitives::{Address, B256, U256};
use alloy_eips::BlockNumberOrTag;
use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_provider::Provider;
use alloy_provider::RootProvider;
use alloy_provider::network::AnyNetwork;
use alloy_rpc_client::ClientBuilder;
use alloy_sol_types::{SolError, sol};
use alloy_transport::{TransportError, TransportFut};
use anyhow::{Result, anyhow};
use gas_analyzer_core::compute_state_updates;
use gas_analyzer_core::types::{IStateUpdateTypes, StateUpdate};
use gas_analyzer_evmsketch::simple_rpc_db::SimpleRpcDb;
use gas_analyzer_evmsketch::{EvmSketchExecutor, chain_id_to_genesis_and_spec};
use gas_analyzer_rpc::{get_preceding_transactions, get_tx_trace};
use revm::database::CacheDB;
use revm::database_interface::{DBErrorMarker, DatabaseRef};
use revm::state::{AccountInfo, Bytecode};
use rsp_rpc_db::BasicRpcDb;
use sp1_cc_host_executor::EvmSketch;
use tower::{Layer, Service};
use url::Url;

const DEFAULT_TXS: &[&str] = &[
    // Original bench tx (13 state updates)
    "0x9add9d0f26bc6d867c1d6d41dda6287d9721a377cea42440250884f76d2a0fa7",
];

// ---------- error/event ABI fragments used to decode the revert ----------
//
// `RevertingContext` is the gas-estimator's wrapper around an inner revert
// (see crates/abis/StateChangeHandlerGasEstimator.json — the contract
// re-bubbles inner errors with the index/target/callargs context). Decoding
// it tells us *which* state update reverted, against *which* target.
sol! {
    error RevertingContext(uint256 index, address target, bytes revertData, bytes callargs);
    error EnvironmentMismatch(bytes32 expected, bytes32 actual, string explanation);
    error Error(string message);
    error Panic(uint256 code);
    error UnsupportedStateUpdate(uint8 kind);
}

#[derive(Debug, Clone)]
struct CallRecord {
    method: String,
    /// Truncated params signature (first ~120 chars). Used to detect duplicates.
    params_sig: String,
    elapsed_ms: f64,
    /// Phase name *at record time* — owned, not a shared handle, so updates
    /// to the current-phase pointer don't retroactively rewrite history.
    phase: String,
}

#[derive(Clone, Default)]
struct CallLog {
    records: Arc<Mutex<Vec<CallRecord>>>,
    current_phase: Arc<Mutex<String>>,
}

impl CallLog {
    fn set_phase(&self, name: &str) {
        *self.current_phase.lock().unwrap() = name.to_string();
    }
    fn snapshot(&self) -> Vec<CallRecord> {
        self.records.lock().unwrap().clone()
    }
}

#[derive(Clone)]
struct RecordingLayer {
    log: CallLog,
}

impl<S> Layer<S> for RecordingLayer {
    type Service = RecordingService<S>;
    fn layer(&self, inner: S) -> Self::Service {
        RecordingService { inner, log: self.log.clone() }
    }
}

#[derive(Clone)]
struct RecordingService<S> {
    inner: S,
    log: CallLog,
}

impl<S> Service<RequestPacket> for RecordingService<S>
where
    S: Service<RequestPacket, Response = ResponsePacket, Error = TransportError, Future = TransportFut<'static>>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
{
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: RequestPacket) -> Self::Future {
        let log = self.log.clone();
        let phase_handle = log.current_phase.clone();
        let mut inner = self.inner.clone();

        // Pull (method, params_sig) out of the request before forwarding.
        let descriptors: Vec<(String, String)> = match &req {
            RequestPacket::Single(r) => {
                let params = r.params().map(|p| p.get().to_string()).unwrap_or_default();
                vec![(r.method().to_string(), truncate(&params, 120))]
            }
            RequestPacket::Batch(b) => b
                .iter()
                .map(|r| {
                    let params = r.params().map(|p| p.get().to_string()).unwrap_or_default();
                    (r.method().to_string(), truncate(&params, 120))
                })
                .collect(),
        };

        Box::pin(async move {
            // Crude rate-limit throttle: most public RPCs cap at 50/sec.
            // 30 ms before each call keeps us well under without
            // distorting per-call latency (the throttle isn't recorded).
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;

            let t0 = Instant::now();
            let resp = inner.call(req).await;
            let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;

            // Snapshot the phase NOW — between the call returning and the
            // log push, the phase is still whatever it was when this RPC
            // was issued (we are inside the awaited future of that phase).
            let phase_now = phase_handle.lock().unwrap().clone();

            let mut records = log.records.lock().unwrap();
            // For a batch packet we attribute the full wall-time to each
            // record (overcounts, but batches are not used by alloy's
            // standard provider methods on this path so it's fine).
            for (method, params_sig) in descriptors {
                records.push(CallRecord {
                    method,
                    params_sig,
                    elapsed_ms,
                    phase: phase_now.clone(),
                });
            }

            resp
        })
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…", &s[..max]) }
}

// ============================================================================
// Logging DB wrapper
// ============================================================================
//
// Wraps any DatabaseRef and records every read with the returned value. Used
// in DEBUG_B mode to make the state-read pattern of the failing B-flow
// run visible: which contract gets read first, what code/balance/nonce came
// back, what storage slots were touched in what order, and what the final
// read was *just before* revm reverted.

#[derive(Debug, Clone)]
enum DbReadKind {
    Basic { exists: bool, balance: U256, nonce: u64, code_len: usize, code_hash: B256 },
    Storage { value: U256 },
    BlockHash { hash: B256 },
}

#[derive(Debug, Clone)]
struct DbReadRecord {
    kind: DbReadKind,
    address: Option<Address>,
    slot_or_block: Option<U256>,
    err: Option<String>,
}

#[derive(Clone, Default)]
struct DbReadLog {
    records: Arc<Mutex<Vec<DbReadRecord>>>,
}

impl DbReadLog {
    fn push(&self, r: DbReadRecord) {
        self.records.lock().unwrap().push(r);
    }
    fn snapshot(&self) -> Vec<DbReadRecord> {
        self.records.lock().unwrap().clone()
    }
}

#[derive(Debug)]
struct AnyDbError(String);
impl std::fmt::Display for AnyDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for AnyDbError {}
impl DBErrorMarker for AnyDbError {}

struct LoggingDb<D> {
    inner: D,
    log: DbReadLog,
}

impl<D> LoggingDb<D> {
    fn new(inner: D, log: DbReadLog) -> Self {
        Self { inner, log }
    }
}

impl<D> DatabaseRef for LoggingDb<D>
where
    D: DatabaseRef,
    <D as DatabaseRef>::Error: std::fmt::Debug + std::fmt::Display,
{
    type Error = AnyDbError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let res = self.inner.basic_ref(address);
        match &res {
            Ok(Some(info)) => self.log.push(DbReadRecord {
                kind: DbReadKind::Basic {
                    exists: true,
                    balance: info.balance,
                    nonce: info.nonce,
                    code_len: info.code.as_ref().map(|c| c.len()).unwrap_or(0),
                    code_hash: info.code_hash,
                },
                address: Some(address),
                slot_or_block: None,
                err: None,
            }),
            Ok(None) => self.log.push(DbReadRecord {
                kind: DbReadKind::Basic {
                    exists: false,
                    balance: U256::ZERO,
                    nonce: 0,
                    code_len: 0,
                    code_hash: B256::ZERO,
                },
                address: Some(address),
                slot_or_block: None,
                err: None,
            }),
            Err(e) => self.log.push(DbReadRecord {
                kind: DbReadKind::Basic {
                    exists: false,
                    balance: U256::ZERO,
                    nonce: 0,
                    code_len: 0,
                    code_hash: B256::ZERO,
                },
                address: Some(address),
                slot_or_block: None,
                err: Some(format!("{e}")),
            }),
        }
        res.map_err(|e| AnyDbError(format!("basic_ref({address}): {e}")))
    }

    fn code_by_hash_ref(&self, hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner
            .code_by_hash_ref(hash)
            .map_err(|e| AnyDbError(format!("code_by_hash_ref({hash}): {e}")))
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let res = self.inner.storage_ref(address, index);
        match &res {
            Ok(v) => self.log.push(DbReadRecord {
                kind: DbReadKind::Storage { value: *v },
                address: Some(address),
                slot_or_block: Some(index),
                err: None,
            }),
            Err(e) => self.log.push(DbReadRecord {
                kind: DbReadKind::Storage { value: U256::ZERO },
                address: Some(address),
                slot_or_block: Some(index),
                err: Some(format!("{e}")),
            }),
        }
        res.map_err(|e| AnyDbError(format!("storage_ref({address}, {index}): {e}")))
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        let res = self.inner.block_hash_ref(number);
        match &res {
            Ok(h) => self.log.push(DbReadRecord {
                kind: DbReadKind::BlockHash { hash: *h },
                address: None,
                slot_or_block: Some(U256::from(number)),
                err: None,
            }),
            Err(e) => self.log.push(DbReadRecord {
                kind: DbReadKind::BlockHash { hash: B256::ZERO },
                address: None,
                slot_or_block: Some(U256::from(number)),
                err: Some(format!("{e}")),
            }),
        }
        res.map_err(|e| AnyDbError(format!("block_hash_ref({number}): {e}")))
    }
}

// ============================================================================
// Revert decoder
// ============================================================================

/// Pull the trailing `0x<hex>` substring off the gas-estimator's revert
/// `anyhow!` message (format: `"Gas estimation reverted (gas: N): 0x..."`).
fn extract_revert_hex(msg: &str) -> Option<Vec<u8>> {
    let idx = msg.rfind("0x")?;
    let body = &msg[idx + 2..];
    let end = body
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(body.len());
    let hex_str = &body[..end];
    hex::decode(hex_str).ok()
}

/// Try to identify a 4-byte selector against well-known errors. Returns a
/// human-readable hint when found.
fn identify_selector(bytes: &[u8]) -> String {
    if bytes.len() < 4 {
        return format!("(short payload: {} bytes — not a selector-prefixed error)", bytes.len());
    }
    let sel: [u8; 4] = bytes[..4].try_into().unwrap();
    let known: &[(&str, [u8; 4])] = &[
        ("RevertingContext", RevertingContext::SELECTOR),
        ("EnvironmentMismatch", EnvironmentMismatch::SELECTOR),
        ("Error(string)", Error::SELECTOR),
        ("Panic(uint256)", Panic::SELECTOR),
        ("UnsupportedStateUpdate", UnsupportedStateUpdate::SELECTOR),
    ];
    for (name, k) in known {
        if &sel == k {
            return format!("0x{} ({name})", hex::encode(sel));
        }
    }
    format!("0x{} (unknown — possibly a custom error from the analyzed contract)", hex::encode(sel))
}

/// Recursively decode a revert payload. Logs the chain of nested errors
/// (e.g. RevertingContext.revertData → Error(string) → "...").
fn decode_revert(prefix: &str, bytes: &[u8]) {
    println!("{prefix}selector: {}", identify_selector(bytes));
    if bytes.len() < 4 {
        println!("{prefix}payload: 0x{}", hex::encode(bytes));
        return;
    }
    let sel: [u8; 4] = bytes[..4].try_into().unwrap();

    if sel == RevertingContext::SELECTOR {
        match RevertingContext::abi_decode(bytes) {
            Ok(d) => {
                println!("{prefix}RevertingContext {{");
                println!("{prefix}  index:      {}", d.index);
                println!("{prefix}  target:     {}", d.target);
                println!("{prefix}  callargs:   0x{}", hex::encode(&d.callargs));
                println!("{prefix}  revertData: 0x{} ({} bytes)", hex::encode(&d.revertData), d.revertData.len());
                println!("{prefix}}}");
                println!("{prefix}— inner revertData decode —");
                let inner_prefix = format!("{prefix}    ");
                decode_revert(&inner_prefix, &d.revertData);
            }
            Err(e) => println!("{prefix}(failed to abi_decode RevertingContext: {e})"),
        }
        return;
    }
    if sel == EnvironmentMismatch::SELECTOR {
        match EnvironmentMismatch::abi_decode(bytes) {
            Ok(d) => {
                println!("{prefix}EnvironmentMismatch {{");
                println!("{prefix}  expected:    {}", d.expected);
                println!("{prefix}  actual:      {}", d.actual);
                println!("{prefix}  explanation: {:?}", d.explanation);
                println!("{prefix}}}");
            }
            Err(e) => println!("{prefix}(failed to abi_decode EnvironmentMismatch: {e})"),
        }
        return;
    }
    if sel == Error::SELECTOR {
        match Error::abi_decode(bytes) {
            Ok(d) => println!("{prefix}Error({:?})", d.message),
            Err(e) => println!("{prefix}(failed to abi_decode Error(string): {e})"),
        }
        return;
    }
    if sel == Panic::SELECTOR {
        match Panic::abi_decode(bytes) {
            Ok(d) => println!("{prefix}Panic(0x{:x})", d.code),
            Err(e) => println!("{prefix}(failed to abi_decode Panic: {e})"),
        }
        return;
    }
    println!("{prefix}payload: 0x{} ({} bytes)", hex::encode(bytes), bytes.len());
}

// ============================================================================
// Etherscan helper
// ============================================================================

/// Look up the contract's verified-source name on Etherscan v2.
///
/// Uses ETHERSCAN_API_KEY from env. Returns `None` if the key is missing or
/// the lookup fails — Etherscan is a nice-to-have for human-readable output,
/// not a load-bearing part of the diagnosis.
async fn etherscan_contract_name(chain_id: u64, address: Address) -> Option<String> {
    let api_key = std::env::var("ETHERSCAN_API_KEY").ok()?;
    let url = format!(
        "https://api.etherscan.io/v2/api?chainid={chain_id}&module=contract&action=getsourcecode&address={address}&apikey={api_key}"
    );
    let body = reqwest::get(&url).await.ok()?.text().await.ok()?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let arr = v.get("result")?.as_array()?;
    let entry = arr.first()?;
    let name = entry.get("ContractName")?.as_str()?.to_string();
    if name.is_empty() { None } else { Some(name) }
}

// ============================================================================
// State update summary (for context when diagnosing reverts)
// ============================================================================

fn summarize_state_updates(state_updates: &[StateUpdate]) {
    println!("  state_updates ({}):", state_updates.len());
    for (i, su) in state_updates.iter().enumerate() {
        match su {
            StateUpdate::Store(IStateUpdateTypes::Store { slot, value }) => {
                println!("    [{i}] SSTORE  slot=0x{} value=0x{}", hex::encode(slot), hex::encode(value));
            }
            StateUpdate::Call(IStateUpdateTypes::Call { target, value, callargs }) => {
                let sel: String = callargs.get(0..4).map(|b| format!("0x{}", hex::encode(b))).unwrap_or_else(|| "(empty)".to_string());
                println!(
                    "    [{i}] CALL    target={target} value={value} sel={sel} callargs_len={}",
                    callargs.len()
                );
            }
            other => println!("    [{i}] {other:?}"),
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let _ = dotenv::dotenv();
    let rpc_url: Url = std::env::var("RPC_URL")
        .map_err(|_| anyhow!("RPC_URL must be set"))?
        .parse()?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let tx_hashes: Vec<String> = if args.is_empty() {
        DEFAULT_TXS.iter().map(|s| s.to_string()).collect()
    } else {
        args
    };

    println!("=== RPC trace across full validator-style flow ===\n");

    for tx_hex in &tx_hashes {
        let tx_hash = parse_hash(tx_hex)?;
        // Each tx gets its own log; whether the run completes or aborts
        // mid-phase, the recorded calls are dumped so we can see exactly
        // where the budget went.
        let log = CallLog::default();
        let result = trace_one_tx(&rpc_url, tx_hash, log.clone()).await;
        if let Err(e) = &result {
            println!("[{tx_hex}] aborted partway: {e:#}");
            println!("(partial RPC log follows — useful for understanding what burned budget)\n");
            print_log_summary(&log);
        }
        println!();
    }

    Ok(())
}

async fn trace_one_tx(rpc_url: &Url, tx_hash: B256, log: CallLog) -> Result<()> {
    // Mode flags.
    //
    // SKIP_PRECEDING — empty preceding-tx list (simulates tx_index 0).
    // USE_BASIC_DB   — swap SimpleRpcDb for sp1-cc's BasicRpcDb during
    //                  the estimate phase, then call sketch.finalize().
    //                  Implies SKIP_PRECEDING.
    // BASIC_AT_PREV  — when USE_BASIC_DB, build a *fresh* BasicRpcDb anchored
    //                  at block N-1 (vs the builder's default at N). The
    //                  builder anchors at N which gives post-block state and
    //                  the estimator reverts; N-1 should not.
    // DEBUG_B        — when USE_BASIC_DB, wrap the DB in a logging adapter
    //                  that records every read, decode the revert hex, and
    //                  fetch the contract name from Etherscan.
    let use_basic_db = std::env::var("USE_BASIC_DB").is_ok();
    let basic_at_prev = std::env::var("BASIC_AT_PREV").is_ok();
    let full_proof_prep = std::env::var("FULL_PROOF_PREP").is_ok();
    let debug_b = std::env::var("DEBUG_B").is_ok();
    let skip_preceding = use_basic_db || std::env::var("SKIP_PRECEDING").is_ok();
    // Build one shared, recording-instrumented RpcClient. Two providers
    // are derived from it — one default-network (`get_tx_trace`,
    // `get_preceding_transactions`, receipts use Ethereum-typed RPC
    // methods), one AnyNetwork (sp1-cc's EvmSketch is generic over
    // AnyNetwork). Both share the same transport, so every call lands
    // in the same `CallLog`.
    let client = ClientBuilder::default()
        .layer(RecordingLayer { log: log.clone() })
        .http(rpc_url.clone());
    let provider_eth: RootProvider = RootProvider::new(client.clone());

    println!("--- tx 0x{} ---", alloy::hex::encode(tx_hash));
    println!(
        "  modes: USE_BASIC_DB={use_basic_db} BASIC_AT_PREV={basic_at_prev} FULL_PROOF_PREP={full_proof_prep} DEBUG_B={debug_b} SKIP_PRECEDING={skip_preceding}"
    );

    // Phase 0: receipt + chain_id (front matter)
    log.set_phase("0_front_matter");
    let t0 = Instant::now();
    let chain_id = provider_eth.get_chain_id().await?;
    let receipt = provider_eth
        .get_transaction_receipt(tx_hash)
        .await?
        .ok_or_else(|| anyhow!("no receipt"))?;
    let block_number = receipt.block_number.ok_or_else(|| anyhow!("no block number"))?;
    let tx_index = receipt.transaction_index.ok_or_else(|| anyhow!("no tx index"))?;
    let to_address = receipt.to.ok_or_else(|| anyhow!("tx has no 'to'"))?;
    let from_address = receipt.from;
    let phase_0_ms = t0.elapsed().as_secs_f64() * 1000.0;

    if debug_b {
        println!("  block={block_number} tx_index={tx_index} to={to_address} from={from_address} chain_id={chain_id}");
        if let Some(name) = etherscan_contract_name(chain_id, to_address).await {
            println!("  etherscan ContractName(to): {name:?}");
        } else {
            println!("  etherscan ContractName(to): <unavailable>");
        }
    }

    // Phase 1: build() — manually, using the recording-backed client so
    // its inner RPC calls land in the log. Mirrors
    // `EvmSketchExecutorBuilder::build` (crates/evmsketch/src/lib.rs:152)
    // but skips the eth_chainId probe since we already did it in phase 0.
    log.set_phase("1_build");
    let t0 = Instant::now();
    let (genesis, _spec) = chain_id_to_genesis_and_spec(chain_id)?;
    let sketch: EvmSketch<RootProvider<AnyNetwork>, reth_primitives::EthPrimitives> =
        EvmSketch::builder()
            .at_block(BlockNumberOrTag::Number(block_number))
            .el_rpc_client(client.clone())
            .with_genesis(genesis)
            .build()
            .await
            .map_err(|e| anyhow!("EvmSketch build: {e}"))?;
    let executor = EvmSketchExecutor { sketch, chain_id };
    let phase_1_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // Phase 2: debug_traceTransaction (state-update extraction)
    log.set_phase("2_trace");
    let t0 = Instant::now();
    let trace = get_tx_trace(&provider_eth, tx_hash).await?;
    let phase_2_ms = t0.elapsed().as_secs_f64() * 1000.0;

    // Phase 3: compute_state_updates (CPU)
    log.set_phase("3_compute_cpu");
    let t0 = Instant::now();
    let (state_updates, _skipped, _gas) = compute_state_updates(trace)?;
    let phase_3_ms = t0.elapsed().as_secs_f64() * 1000.0;

    if debug_b {
        summarize_state_updates(&state_updates);
    }

    // Phase 4: get_preceding_transactions
    log.set_phase("4_preceding");
    let t0 = Instant::now();
    let preceding = if skip_preceding {
        Vec::new()
    } else {
        get_preceding_transactions(&provider_eth, block_number, tx_index).await?
    };
    let phase_4_ms = t0.elapsed().as_secs_f64() * 1000.0;

    let sim_env = executor.sim_env();
    let state_block = executor.anchor_block_number().saturating_sub(1);

    // Phase 5a: replay_preceding_transactions (revm + many cold-read RPCs)
    let phase_5a_ms;
    let phase_5b_ms;
    let phase_6_ms; // sp1-cc finalize (only meaningful in USE_BASIC_DB mode)

    if use_basic_db && full_proof_prep {
        // sp1-cc full proof-prep path — the load-bearing measurement of
        // prefetching overhead. We build a *second* sketch anchored at N-1
        // (sketch_prev). Its rpc_db is a BasicRpcDb pinned to N-1 with
        // state_root populated from N-2's header — these are paired
        // consistently by sp1-cc's builder, so finalize() can reconstruct a
        // coherent EthereumState. Simulation environment (timestamps,
        // basefee, coinbase, prevrandao, spec) still comes from the original
        // at-N sketch, so the analyzed tx observes block N's context as it
        // would on chain.
        log.set_phase("5a_replay");
        phase_5a_ms = 0.0;
        let _ = preceding;

        // Phase 1b: second build, anchored at N-1.
        log.set_phase("1b_build_prev");
        let t1b = Instant::now();
        let (genesis_prev, _) = chain_id_to_genesis_and_spec(chain_id)?;
        let sketch_prev: EvmSketch<RootProvider<AnyNetwork>, reth_primitives::EthPrimitives> =
            EvmSketch::builder()
                .at_block(BlockNumberOrTag::Number(state_block))
                .el_rpc_client(client.clone())
                .with_genesis(genesis_prev)
                .build()
                .await
                .map_err(|e| anyhow!("EvmSketch (at N-1) build: {e}"))?;
        let phase_1b_ms = t1b.elapsed().as_secs_f64() * 1000.0;

        // Phase 5b: estimate against sketch_prev's BasicRpcDb (at N-1, pre-N
        // state — correct anchor for tx simulation).
        log.set_phase("5b_estimate");
        let t0 = Instant::now();
        let basic_db = sketch_prev.rpc_db.clone();
        let read_log = DbReadLog::default();
        let estimate_result = if debug_b {
            let mut cache_db = CacheDB::new(LoggingDb::new(basic_db, read_log.clone()));
            gas_analyzer_estimator::estimate_state_changes_gas(
                &mut cache_db,
                to_address,
                from_address,
                &state_updates,
                &sim_env,
            )
        } else {
            let mut cache_db = CacheDB::new(basic_db);
            gas_analyzer_estimator::estimate_state_changes_gas(
                &mut cache_db,
                to_address,
                from_address,
                &state_updates,
                &sim_env,
            )
        };
        phase_5b_ms = t0.elapsed().as_secs_f64() * 1000.0;
        match &estimate_result {
            Ok(gas) => println!("  estimate succeeded: gas={gas}"),
            Err(e) => println!(
                "  (note: estimate returned error in FULL_PROOF_PREP — proceeding to finalize for measurement: {})",
                truncate(&e.to_string(), 120)
            ),
        }

        // Phase 6: sp1-cc finalize on sketch_prev — this is THE proof-prep
        // overhead measurement. It batches one `eth_getProof(addr, all_slots)`
        // per touched account, fetches ancestor headers, and reconstructs
        // EthereumState in CPU.
        log.set_phase("6_finalize");
        let t0 = Instant::now();
        match sketch_prev.finalize().await {
            Ok(_input) => {
                phase_6_ms = t0.elapsed().as_secs_f64() * 1000.0;
                println!("  finalize succeeded — produced an EvmSketchInput");
            }
            Err(e) => {
                phase_6_ms = t0.elapsed().as_secs_f64() * 1000.0;
                // Don't abort the bench — the RPC fetches inside finalize
                // already happened and were timed; only the in-process state
                // reconstruction (`EthereumState::from_proofs`) might fail
                // if the state_root doesn't match. The numbers are still
                // useful.
                println!(
                    "  (finalize returned error in {phase_6_ms:.1}ms — RPC fetches were still timed: {})",
                    truncate(&e.to_string(), 120)
                );
            }
        }

        if debug_b {
            println!();
            println!("  DB reads observed during estimate: {}", read_log.snapshot().len());
        }

        // Reuse phase_1_ms slot to add the second build's cost on top so the
        // headline build time reflects "what FULL_PROOF_PREP actually costs."
        // We tag it visibly in the output below.
        let phase_1_total_ms = phase_1_ms + phase_1b_ms;
        // Stash separately for printing.
        // The simplest way to surface this is to print 1b inline after the
        // main report; we expose it via a closure-captured value.
        FULL_PROOF_PREP_BUILD_PREV_MS.with(|c| c.set(phase_1b_ms));
        let _ = phase_1_total_ms;
    } else if use_basic_db {
        // sp1-cc proof-prep path. Two sub-modes:
        //   - default: use the BasicRpcDb the builder constructed (anchored at N).
        //     This returns post-block state and the estimator reverts.
        //   - BASIC_AT_PREV: build a fresh BasicRpcDb anchored at N-1, which
        //     returns pre-block state — same anchor SimpleRpcDb uses.
        log.set_phase("5a_replay");
        phase_5a_ms = 0.0; // forced skip in USE_BASIC_DB
        let _ = preceding; // silence unused

        // For BASIC_AT_PREV we need a state_root for N-1's *parent* (= N-2).
        // The builder gives us the original BasicRpcDb's state_root, which is
        // the previous block's state_root at construction time (= N-1 for an
        // anchor of N). For an N-1-anchored DB we'd want N-2. Since
        // state_root only matters during finalize() (for the
        // EthereumState::from_proofs reconstruction), and finalize hits the
        // RPC for everything else, this divergence isn't load-bearing for
        // *measuring proof-prep overhead*; it would matter for actually
        // proving. We just supply a placeholder and let finalize() do its
        // thing — if it fails internally that's still useful information.
        let basic_db_at_prev_block = if basic_at_prev {
            Some(BasicRpcDb::<_, AnyNetwork>::new(
                executor.sketch.provider.clone(),
                state_block,
                B256::ZERO, // see comment above
            ))
        } else {
            None
        };

        // Run estimate against whichever DB was selected. We dispatch up-front
        // based on the (basic_at_prev, debug_b) cross product so the type
        // parameter to CacheDB is monomorphized concretely.
        log.set_phase("5b_estimate");
        let t0 = Instant::now();
        let read_log = DbReadLog::default();

        let estimate_err: Option<anyhow::Error> = match (basic_at_prev, debug_b) {
            (true, true) => {
                let inner = basic_db_at_prev_block.as_ref().unwrap().clone();
                let mut cache_db = CacheDB::new(LoggingDb::new(inner, read_log.clone()));
                gas_analyzer_estimator::estimate_state_changes_gas(
                    &mut cache_db,
                    to_address,
                    from_address,
                    &state_updates,
                    &sim_env,
                )
                .err()
            }
            (true, false) => {
                let inner = basic_db_at_prev_block.as_ref().unwrap().clone();
                let mut cache_db = CacheDB::new(inner);
                gas_analyzer_estimator::estimate_state_changes_gas(
                    &mut cache_db,
                    to_address,
                    from_address,
                    &state_updates,
                    &sim_env,
                )
                .err()
            }
            (false, true) => {
                let inner = executor.sketch.rpc_db.clone();
                let mut cache_db = CacheDB::new(LoggingDb::new(inner, read_log.clone()));
                gas_analyzer_estimator::estimate_state_changes_gas(
                    &mut cache_db,
                    to_address,
                    from_address,
                    &state_updates,
                    &sim_env,
                )
                .err()
            }
            (false, false) => {
                let inner = executor.sketch.rpc_db.clone();
                let mut cache_db = CacheDB::new(inner);
                gas_analyzer_estimator::estimate_state_changes_gas(
                    &mut cache_db,
                    to_address,
                    from_address,
                    &state_updates,
                    &sim_env,
                )
                .err()
            }
        };
        phase_5b_ms = t0.elapsed().as_secs_f64() * 1000.0;

        // Diagnostics if the estimate didn't return a number.
        if let Some(e) = &estimate_err {
            println!();
            println!("  ====== ESTIMATE FAILED — DIAGNOSTICS ======");
            println!("  basic_at_prev = {basic_at_prev} (false → BasicRpcDb anchored at N = {block_number}, post-block; true → at N-1 = {state_block}, pre-block)");
            let msg = e.to_string();
            println!("  raw error string:");
            for line in msg.lines() {
                println!("    {line}");
            }
            if let Some(bytes) = extract_revert_hex(&msg) {
                println!("  decoded revert payload ({} bytes):", bytes.len());
                decode_revert("    ", &bytes);
            } else {
                println!("  (no `0x...` revert hex found in error message — likely a halt or DB error, not a revert)");
            }

            // Compare what BasicRpcDb at N gives for the analyzed contract
            // vs SimpleRpcDb at N-1 (pre-block).
            println!();
            println!("  ====== STATE READ COMPARISON: BasicRpcDb@N vs SimpleRpcDb@N-1 ======");
            let simple_db_at_prev = SimpleRpcDb {
                provider: executor.sketch.provider.clone(),
                block_number: state_block,
            };
            let basic_at_n = executor.sketch.rpc_db.clone();
            for addr in [to_address, from_address] {
                println!("  -- account {addr} --");
                let basic_info = basic_at_n.basic_ref(addr).ok().flatten();
                let simple_info = simple_db_at_prev.basic_ref(addr).ok().flatten();
                println!(
                    "    Basic@N    balance={} nonce={} code_hash={} code_len={}",
                    basic_info.as_ref().map(|i| i.balance).unwrap_or_default(),
                    basic_info.as_ref().map(|i| i.nonce).unwrap_or_default(),
                    basic_info.as_ref().map(|i| i.code_hash).unwrap_or_default(),
                    basic_info.as_ref().and_then(|i| i.code.as_ref().map(|c| c.len())).unwrap_or(0),
                );
                println!(
                    "    Simple@N-1 balance={} nonce={} code_hash={} code_len={}",
                    simple_info.as_ref().map(|i| i.balance).unwrap_or_default(),
                    simple_info.as_ref().map(|i| i.nonce).unwrap_or_default(),
                    simple_info.as_ref().map(|i| i.code_hash).unwrap_or_default(),
                    simple_info.as_ref().and_then(|i| i.code.as_ref().map(|c| c.len())).unwrap_or(0),
                );
            }
            // For each SSTORE state update, compare the slot value at N vs N-1.
            println!();
            println!("  -- per-SSTORE slot, value at N (Basic) vs N-1 (Simple) --");
            for (i, su) in state_updates.iter().enumerate() {
                if let StateUpdate::Store(IStateUpdateTypes::Store { slot, value }) = su {
                    let slot_u = U256::from_be_bytes(slot.0);
                    let value_u = U256::from_be_bytes(value.0);
                    // `Store` updates have no target field — by convention the
                    // analyzed contract (`to_address`) is the target.
                    let basic_val = basic_at_n.storage_ref(to_address, slot_u).unwrap_or_default();
                    let simple_val = simple_db_at_prev.storage_ref(to_address, slot_u).unwrap_or_default();
                    let basic_match = basic_val == value_u;
                    let simple_match = simple_val == value_u;
                    println!(
                        "    [{i}] target={to_address} slot=0x{}",
                        hex::encode(slot),
                    );
                    println!(
                        "        update.value=0x{}",
                        hex::encode(value),
                    );
                    println!(
                        "        Basic@N    =0x{basic_val:x} {} (post-state already applied? → estimator's pre-check fails)",
                        if basic_match { "== update.value" } else { "!= update.value" }
                    );
                    println!(
                        "        Simple@N-1 =0x{simple_val:x} {}",
                        if simple_match { "== update.value (this would be wrong — pre-state already final)" } else { "!= update.value (correct: pre-state ≠ post-state)" }
                    );
                }
            }

            // Dump the recorded read log if DEBUG_B was on.
            if debug_b {
                println!();
                println!("  ====== DB READ LOG ({} reads) ======", read_log.snapshot().len());
                for (i, r) in read_log.snapshot().iter().enumerate() {
                    match &r.kind {
                        DbReadKind::Basic { exists, balance, nonce, code_len, code_hash } => {
                            println!(
                                "    [{i:>3}] BASIC   addr={} exists={exists} balance={balance} nonce={nonce} code_len={code_len} code_hash={code_hash}{}",
                                r.address.unwrap_or_default(),
                                r.err.as_ref().map(|e| format!(" ERR={e}")).unwrap_or_default(),
                            );
                        }
                        DbReadKind::Storage { value } => {
                            println!(
                                "    [{i:>3}] STORE   addr={} slot=0x{:x} value=0x{value:x}{}",
                                r.address.unwrap_or_default(),
                                r.slot_or_block.unwrap_or_default(),
                                r.err.as_ref().map(|e| format!(" ERR={e}")).unwrap_or_default(),
                            );
                        }
                        DbReadKind::BlockHash { hash } => {
                            println!(
                                "    [{i:>3}] BLKHSH  number={} hash={hash}{}",
                                r.slot_or_block.unwrap_or_default(),
                                r.err.as_ref().map(|e| format!(" ERR={e}")).unwrap_or_default(),
                            );
                        }
                    }
                }
            }
            println!("  ====== END DIAGNOSTICS ======");
            println!();
        }

        // Phase 6: sp1-cc proof bundling — fetches one batched
        // `eth_getProof(addr, all_slots)` per touched account and N
        // ancestor headers, then reconstructs the EthereumState (CPU).
        // We finalize the *original* sketch (anchored at N) so the
        // measurement is comparable across BASIC_AT_PREV settings.
        log.set_phase("6_finalize");
        let t0 = Instant::now();
        let _input = executor
            .sketch
            .finalize()
            .await
            .map_err(|e| anyhow!("sketch.finalize: {e}"))?;
        phase_6_ms = t0.elapsed().as_secs_f64() * 1000.0;
    } else {
        let simple_db = SimpleRpcDb {
            provider: executor.sketch.provider.clone(),
            block_number: state_block,
        };
        let mut cache_db = CacheDB::new(simple_db);

        log.set_phase("5a_replay");
        let t0 = Instant::now();
        if !preceding.is_empty() {
            gas_analyzer_estimator::replay_preceding_transactions(&mut cache_db, &preceding, &sim_env)?;
        }
        phase_5a_ms = t0.elapsed().as_secs_f64() * 1000.0;

        log.set_phase("5b_estimate");
        let t0 = Instant::now();
        let _gas = gas_analyzer_estimator::estimate_state_changes_gas(
            &mut cache_db,
            to_address,
            from_address,
            &state_updates,
            &sim_env,
        )?;
        phase_5b_ms = t0.elapsed().as_secs_f64() * 1000.0;
        phase_6_ms = 0.0;
    }
    let phase_5_ms = phase_5a_ms + phase_5b_ms;

    // ---------- Reporting ----------
    let total_ms = phase_0_ms + phase_1_ms + phase_2_ms + phase_3_ms + phase_4_ms + phase_5_ms + phase_6_ms;
    println!(
        "  block {block_number}, tx_index {tx_index}, preceding={}, state_updates={}",
        preceding.len(),
        state_updates.len()
    );
    println!();
    let phase_1b_ms = FULL_PROOF_PREP_BUILD_PREV_MS.with(|c| c.get());
    let total_ms = total_ms + phase_1b_ms;
    println!("  phase                  wall-time      share");
    println!("  0_front_matter        {phase_0_ms:>8.1} ms   {:>5.1}%", pct(phase_0_ms, total_ms));
    println!("  1_build               {phase_1_ms:>8.1} ms   {:>5.1}%", pct(phase_1_ms, total_ms));
    if full_proof_prep {
        println!("  1b_build_prev (sp1-cc){phase_1b_ms:>8.1} ms   {:>5.1}%", pct(phase_1b_ms, total_ms));
    }
    println!("  2_trace               {phase_2_ms:>8.1} ms   {:>5.1}%", pct(phase_2_ms, total_ms));
    println!("  3_compute_cpu         {phase_3_ms:>8.1} ms   {:>5.1}%", pct(phase_3_ms, total_ms));
    println!("  4_preceding           {phase_4_ms:>8.1} ms   {:>5.1}%", pct(phase_4_ms, total_ms));
    println!("  5a_replay             {phase_5a_ms:>8.1} ms   {:>5.1}%", pct(phase_5a_ms, total_ms));
    println!("  5b_estimate           {phase_5b_ms:>8.1} ms   {:>5.1}%", pct(phase_5b_ms, total_ms));
    if use_basic_db {
        println!("  6_finalize (sp1-cc)   {phase_6_ms:>8.1} ms   {:>5.1}%", pct(phase_6_ms, total_ms));
    }
    println!("  ----                  -----------");
    println!("  total                 {total_ms:>8.1} ms");
    println!();

    print_log_summary(&log);
    Ok(())
}

fn print_log_summary(log: &CallLog) {
    let records = log.snapshot();
    if records.is_empty() {
        println!("  no RPC calls recorded");
        return;
    }

    // Per-method counts
    let mut by_method: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for r in &records {
        let e = by_method.entry(r.method.clone()).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += r.elapsed_ms;
    }
    println!("  RPC calls: {} total, {} unique methods", records.len(), by_method.len());
    println!("  {:<32} {:>5} {:>9}", "method", "n", "total_ms");
    for (m, (n, ms)) in &by_method {
        println!("  {m:<32} {n:>5} {ms:>9.1}");
    }
    println!();

    // Duplicate detection: same (method, params_sig) seen ≥2×
    let mut by_call: BTreeMap<(String, String), Vec<&CallRecord>> = BTreeMap::new();
    for r in &records {
        by_call
            .entry((r.method.clone(), r.params_sig.clone()))
            .or_default()
            .push(r);
    }
    let dups: Vec<_> = by_call.iter().filter(|(_, v)| v.len() >= 2).collect();
    if dups.is_empty() {
        println!("  no duplicate calls (same method+params seen ≥2×)");
    } else {
        println!("  duplicate calls (same method+params seen ≥2×):");
        for ((method, params), calls) in dups {
            let phases: Vec<&str> = calls.iter().map(|c| c.phase.as_str()).collect();
            let saved_ms: f64 = calls.iter().skip(1).map(|c| c.elapsed_ms).sum();
            println!(
                "    {method} ×{count}  saving≈{saved_ms:>5.1}ms  phases={phases:?}",
                count = calls.len(),
            );
            println!("        params: {params}");
        }
    }
    println!();

    // Phase-by-phase RPC counts
    let mut by_phase: BTreeMap<String, (usize, f64)> = BTreeMap::new();
    for r in &records {
        let e = by_phase.entry(r.phase.clone()).or_insert((0, 0.0));
        e.0 += 1;
        e.1 += r.elapsed_ms;
    }
    println!("  RPC count per phase:");
    for (phase, (n, ms)) in &by_phase {
        println!("    {phase:<20} {n:>3} calls   {ms:>8.1} ms");
    }
}

fn parse_hash(s: &str) -> Result<B256> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = alloy::hex::decode(s).map_err(|e| anyhow!("bad hex: {e}"))?;
    if bytes.len() != 32 {
        return Err(anyhow!("expected 32 bytes, got {}", bytes.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(out.into())
}

fn pct(a: f64, total: f64) -> f64 {
    if total == 0.0 { 0.0 } else { 100.0 * a / total }
}

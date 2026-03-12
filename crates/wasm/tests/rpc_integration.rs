#![cfg(not(target_arch = "wasm32"))]
//! RPC integration tests — exercise the analyzer against real Geth traces.
//!
//! These tests are `#[ignore]`d by default. Run them with:
//!
//!   RPC_URL=https://... cargo test -- --ignored
//!
//! Requirements:
//! - RPC_URL: an Ethereum Sepolia node with `debug_traceTransaction` support
//!
//! Uses the same test transactions as the gas-killer-analyzer repo.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use gas_killer_wasm::{analyze_trace_inner, encode_trace_inner, estimate_gas_heuristic_inner};
mod common;
use common::{test_caller_address, test_estimator_address};

// Same TX hashes and addresses used in gas-killer-analyzer (crates/core/src/constants.rs)
const SIMPLE_STORAGE_SET_TX: &str =
    "0xccd4b5a1d020bfc69fb44452f942cdef29996fc6d822f127d9a5a6108e95c3f9";
const SIMPLE_STORAGE_DEPOSIT_TX: &str =
    "0xa787da2025d8e9943cb175559aa91ab38cff62dde3fd09b6da117a38c4ccd431";
const SIMPLE_STORAGE_CALL_EXTERNAL_TX: &str =
    "0xc48dfdc874d62df779a0a351c05d0d07302f801522fe9a80289d6f6b9a836579";
const DELEGATECALL_CONTRACT_MAIN_RUN_TX: &str =
    "0x71aa01e28adbb015d0d4003fb3e770b4344a00a112704fa2e05014f846532d43";
const ACCESS_CONTROL_MAIN_RUN_TX: &str =
    "0x3fe223c8aabc4e5e6b918d65dd76d7f7bd8e93f6012a0e183ff0a299260b2f60";

/// Per-process trace cache — avoids re-fetching the same TX across tests.
static TRACE_CACHE: LazyLock<Mutex<HashMap<&'static str, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Fetch a Geth trace via `debug_traceTransaction` with memory enabled.
/// Results are cached so repeated calls for the same TX don't hit the RPC.
fn fetch_trace(tx_hash: &'static str) -> String {
    {
        let cache = TRACE_CACHE.lock().unwrap();
        if let Some(trace) = cache.get(tx_hash) {
            return trace.clone();
        }
    }

    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL env var not set");

    let client = reqwest::blocking::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "debug_traceTransaction",
        "params": [tx_hash, { "enableMemory": true }],
        "id": 1
    });

    let resp = client
        .post(&rpc_url)
        .json(&body)
        .send()
        .expect("RPC request failed");

    let json: serde_json::Value = resp.json().expect("Failed to parse RPC response");

    assert!(
        json.get("result").is_some(),
        "RPC response missing 'result' field for tx {}. Error: {:?}",
        tx_hash,
        json.get("error")
    );

    let trace = json["result"].to_string();
    TRACE_CACHE.lock().unwrap().insert(tx_hash, trace.clone());
    trace
}

// ---------------------------------------------------------------------------
// SimpleStorage.set() — single SSTORE
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_simple_storage_set() {
    let trace = fetch_trace(SIMPLE_STORAGE_SET_TX);
    let result = analyze_trace_inner(&trace, &test_estimator_address(), &test_estimator_address(), None).unwrap();

    // Pinned: deterministic Sepolia TX produces exactly 2 state updates
    assert_eq!(result.state_update_count, 2);
    assert!(result.gas_estimate > 0, "gas estimate should be positive");

    eprintln!(
        "simple_storage_set: gas={}, updates={}, heuristic={}",
        result.gas_estimate, result.state_update_count, result.is_heuristic
    );
}

// ---------------------------------------------------------------------------
// SimpleStorage.deposit() — SSTORE + LOG
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_simple_storage_deposit() {
    let trace = fetch_trace(SIMPLE_STORAGE_DEPOSIT_TX);
    let result = analyze_trace_inner(&trace, &test_estimator_address(), &test_estimator_address(), None).unwrap();

    // Pinned: deterministic Sepolia TX produces exactly 2 state updates (SSTORE + LOG)
    assert_eq!(result.state_update_count, 2);
    assert!(result.gas_estimate > 0);

    eprintln!(
        "simple_storage_deposit: gas={}, updates={}, heuristic={}",
        result.gas_estimate, result.state_update_count, result.is_heuristic
    );
}

// ---------------------------------------------------------------------------
// SimpleStorage.callExternal() — has an external CALL
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_simple_storage_call_external() {
    let trace = fetch_trace(SIMPLE_STORAGE_CALL_EXTERNAL_TX);
    let result = analyze_trace_inner(&trace, &test_estimator_address(), &test_estimator_address(), None).unwrap();

    // Pinned: deterministic Sepolia TX produces exactly 1 state update
    assert_eq!(result.state_update_count, 1);
    assert!(result.gas_estimate > 0);

    eprintln!(
        "call_external: gas={}, updates={}, heuristic={}",
        result.gas_estimate, result.state_update_count, result.is_heuristic
    );
}

// ---------------------------------------------------------------------------
// DelegateCall contract — DELEGATECALL pattern
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_delegatecall() {
    let trace = fetch_trace(DELEGATECALL_CONTRACT_MAIN_RUN_TX);
    let result = analyze_trace_inner(&trace, &test_estimator_address(), &test_estimator_address(), None).unwrap();

    // Pinned: deterministic Sepolia TX produces exactly 4 state updates
    assert_eq!(result.state_update_count, 4);
    assert!(result.gas_estimate > 0);

    eprintln!(
        "delegatecall: gas={}, updates={}, heuristic={}",
        result.gas_estimate, result.state_update_count, result.is_heuristic
    );
}

// ---------------------------------------------------------------------------
// AccessControl — more complex contract
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_access_control() {
    let trace = fetch_trace(ACCESS_CONTROL_MAIN_RUN_TX);
    let result = analyze_trace_inner(&trace, &test_estimator_address(), &test_estimator_address(), None).unwrap();

    // Pinned: deterministic Sepolia TX produces exactly 1 state update
    assert_eq!(result.state_update_count, 1);
    assert!(result.gas_estimate > 0);

    eprintln!(
        "access_control: gas={}, updates={}, heuristic={}",
        result.gas_estimate, result.state_update_count, result.is_heuristic
    );
}

// ---------------------------------------------------------------------------
// Cross-function consistency: all 3 paths agree on state_update_count
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_all_paths_agree_on_simple_storage_set() {
    let trace = fetch_trace(SIMPLE_STORAGE_SET_TX);

    let analyze = analyze_trace_inner(&trace, &test_estimator_address(), &test_estimator_address(), None).unwrap();
    let encode = encode_trace_inner(&trace).unwrap();
    let heuristic = estimate_gas_heuristic_inner(&trace).unwrap();

    assert_eq!(analyze.state_update_count, encode.state_update_count);
    assert_eq!(analyze.state_update_count, heuristic.state_update_count);

    eprintln!(
        "all paths agree: {} state updates",
        analyze.state_update_count
    );
}

// ---------------------------------------------------------------------------
// Encoding produces valid hex for a real trace
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_encode_real_trace() {
    let trace = fetch_trace(SIMPLE_STORAGE_DEPOSIT_TX);
    let result = encode_trace_inner(&trace).unwrap();

    assert!(result.encoded_updates.starts_with("0x"));
    assert!(result.encoded_updates.len() > 2);
    assert!(result.state_update_count > 0);

    // Verify the hex decodes cleanly
    let bytes = hex::decode(result.encoded_updates.trim_start_matches("0x")).unwrap();
    assert!(bytes.len() > 64, "ABI encoding should be non-trivial");

    eprintln!(
        "encode: {} updates, {} encoded bytes",
        result.state_update_count,
        bytes.len()
    );
}

// ---------------------------------------------------------------------------
// Heuristic produces reasonable gas for a real trace
// ---------------------------------------------------------------------------

#[test]
#[ignore]
fn test_heuristic_real_trace() {
    let trace = fetch_trace(SIMPLE_STORAGE_SET_TX);
    let result = estimate_gas_heuristic_inner(&trace).unwrap();

    assert!(
        result.gas_estimate >= 21_000,
        "gas should be at least BASE_TX_COST"
    );
    assert!(result.is_heuristic);

    eprintln!("heuristic: gas={}", result.gas_estimate);
}

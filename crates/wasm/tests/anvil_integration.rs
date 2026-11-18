#![cfg(not(target_arch = "wasm32"))]
//! Integration tests: the analyzer on real Geth-format traces from a local anvil (#109).
//!
//! Each test spawns anvil, deploys the test contracts in `contracts/src` from their creation
//! bytecode (`tests/fixtures/*.hex`, regenerate with `forge inspect <Contract> bytecode` in
//! `contracts/`), sends one transaction and analyzes its `debug_traceTransaction` output. No
//! external RPC is involved.
//!
//! `#[ignore]`d so a `cargo test` without foundry skips them rather than fails, as with the
//! evmsketch anvil tests. Run them with:
//!
//!   cargo test -p gas-killer-wasm --test anvil_integration -- --ignored

use std::process::{Child, Command, Stdio};
use std::time::Duration;

use gas_killer_wasm::{analyze_trace_inner, encode_trace_inner, estimate_gas_heuristic_inner};
use serde_json::{Value, json};
mod common;
use common::{test_caller_address, test_estimator_address};

/// anvil's first dev account, unlocked by default.
const DEV: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";

const SIMPLE_STORAGE: &str = include_str!("fixtures/SimpleStorage.hex");
const DELEGATECALL_MAIN: &str = include_str!("fixtures/DelegateCallTestMain.hex");
const ACCESS_CONTROL_MAIN: &str = include_str!("fixtures/AccessControlTestMain.hex");

// Function selectors (`cast sig`).
const SET: &str = "0x60fe47b1"; // set(uint256)
const DEPOSIT: &str = "0xd0e30db0"; // deposit()
const CALL_EXTERNAL: &str = "0xc7117edf"; // callExternalContract()
const RUN: &str = "0xc0406226"; // run()

/// A local anvil on a free port, killed on drop. Steps tracing is on so
/// `debug_traceTransaction` returns struct logs.
struct LocalAnvil {
    child: Child,
    url: String,
    client: reqwest::blocking::Client,
}

impl Drop for LocalAnvil {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl LocalAnvil {
    fn spawn() -> Self {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("pick a free port")
            .port();
        let child = Command::new("anvil")
            .args(["--port", &port.to_string(), "--silent", "--steps-tracing"])
            // Pinned so gas figures don't move with anvil's default hardfork.
            .args(["--hardfork", "osaka"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect(
                "failed to spawn `anvil`: these tests need foundry (https://getfoundry.sh) on \
                 PATH; CI installs it via foundry-toolchain",
            );
        let anvil = LocalAnvil {
            child,
            url: format!("http://127.0.0.1:{port}"),
            client: reqwest::blocking::Client::new(),
        };
        for _ in 0..100 {
            if anvil.try_rpc("eth_chainId", json!([])).is_ok() {
                return anvil;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("anvil did not become ready within 5s");
    }

    fn try_rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": 1 });
        let resp: Value = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .and_then(|r| r.json())
            .map_err(|e| e.to_string())?;
        match resp.get("result") {
            Some(result) => Ok(result.clone()),
            None => Err(format!("{method} failed: {:?}", resp.get("error"))),
        }
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        self.try_rpc(method, params)
            .unwrap_or_else(|e| panic!("{e}"))
    }

    /// Send a transaction from the dev account and return its successful receipt.
    fn send(&self, to: Option<&str>, data: &str, value: u64) -> Value {
        let mut tx = json!({
            "from": DEV,
            "data": data,
            "value": format!("{value:#x}"),
            "gas": "0x1c9c380",
        });
        if let Some(to) = to {
            tx["to"] = json!(to);
        }
        let hash = self.rpc("eth_sendTransaction", json!([tx]));
        // anvil automines, but the receipt can lag the send under parallel load.
        let mut receipt = Value::Null;
        for _ in 0..100 {
            receipt = self.rpc("eth_getTransactionReceipt", json!([hash]));
            if !receipt.is_null() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(receipt["status"], "0x1", "transaction reverted: {receipt}");
        receipt
    }

    fn deploy(&self, creation_hex: &str) -> String {
        let receipt = self.send(None, creation_hex.trim(), 0);
        receipt["contractAddress"]
            .as_str()
            .unwrap_or_else(|| panic!("no contract address in {receipt}"))
            .to_owned()
    }

    /// Deploy `creation_hex`, call it with `data` (and `value` wei), and return the call's
    /// struct-log trace as JSON, the way a client hands it to the analyzer.
    fn trace_of(creation_hex: &str, data: &str, value: u64) -> String {
        let anvil = Self::spawn();
        let contract = anvil.deploy(creation_hex);
        let receipt = anvil.send(Some(&contract), data, value);
        let trace = anvil.rpc(
            "debug_traceTransaction",
            json!([receipt["transactionHash"], { "enableMemory": true }]),
        );
        assert!(
            !trace["structLogs"]
                .as_array()
                .is_none_or(|steps| steps.is_empty()),
            "anvil returned no struct logs"
        );
        trace.to_string()
    }
}

fn set_trace() -> String {
    LocalAnvil::trace_of(SIMPLE_STORAGE, &format!("{SET}{:064x}", 42), 0)
}

fn deposit_trace() -> String {
    LocalAnvil::trace_of(SIMPLE_STORAGE, DEPOSIT, 1_000)
}

fn call_external_trace() -> String {
    LocalAnvil::trace_of(SIMPLE_STORAGE, CALL_EXTERNAL, 0)
}

// ---------------------------------------------------------------------------
// SimpleStorage.set() — single SSTORE
// ---------------------------------------------------------------------------

#[test]
#[ignore = "spawns anvil"]
fn test_simple_storage_set() {
    let trace = set_trace();
    let result = analyze_trace_inner(
        &trace,
        &test_estimator_address(),
        &test_caller_address(),
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    // SSTORE + LOG1
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
#[ignore = "spawns anvil"]
fn test_simple_storage_deposit() {
    let trace = deposit_trace();
    let result = analyze_trace_inner(
        &trace,
        &test_estimator_address(),
        &test_caller_address(),
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    // SSTORE + LOG2
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
#[ignore = "spawns anvil"]
fn test_simple_storage_call_external() {
    let trace = call_external_trace();
    let result = analyze_trace_inner(
        &trace,
        &test_estimator_address(),
        &test_caller_address(),
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    // The CALL to DummyExternal; its write happens inside the call
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
#[ignore = "spawns anvil"]
fn test_delegatecall() {
    let trace = LocalAnvil::trace_of(DELEGATECALL_MAIN, RUN, 0);
    let result = analyze_trace_inner(
        &trace,
        &test_estimator_address(),
        &test_caller_address(),
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    // 3 SSTOREs in main's context (two through DELEGATECALL) + the CALL to A
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
#[ignore = "spawns anvil"]
fn test_access_control() {
    let trace = LocalAnvil::trace_of(ACCESS_CONTROL_MAIN, RUN, 0);
    let result = analyze_trace_inner(
        &trace,
        &test_estimator_address(),
        &test_caller_address(),
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();

    // The CALL to AccessControlA
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
#[ignore = "spawns anvil"]
fn test_all_paths_agree_on_simple_storage_set() {
    let trace = set_trace();

    let analyze = analyze_trace_inner(
        &trace,
        &test_estimator_address(),
        &test_caller_address(),
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    let encode = encode_trace_inner(&trace).unwrap();
    let heuristic = estimate_gas_heuristic_inner(&trace, None).unwrap();

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
#[ignore = "spawns anvil"]
fn test_encode_real_trace() {
    let trace = deposit_trace();
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
#[ignore = "spawns anvil"]
fn test_heuristic_real_trace() {
    let trace = set_trace();
    let result = estimate_gas_heuristic_inner(&trace, None).unwrap();

    assert!(
        result.gas_estimate >= 21_000,
        "gas should be at least BASE_TX_COST"
    );
    assert!(result.is_heuristic);

    eprintln!("heuristic: gas={}", result.gas_estimate);
}

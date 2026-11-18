#![cfg(not(target_arch = "wasm32"))]
//! Integration tests: the analyzer on struct-log traces from a local anvil (#109).
//!
//! One anvil per test process deploys the test contracts in `contracts/src`, sends one
//! transaction per scenario, records each `debug_traceTransaction` result and exits; the tests
//! then analyze those traces. No external RPC is involved.
//!
//! `SimpleStorage` is deployed from the bytecode in `gas_analyzer_core::SimpleStorage`; the other
//! contracts from `tests/fixtures/*.hex` (regenerate with `forge inspect <Contract> bytecode` in
//! `contracts/`).
//!
//! `#[ignore]`d so a plain `cargo test` on a machine without foundry skips them rather than
//! fails, as with the evmsketch anvil tests; CI installs foundry and runs them with:
//!
//!   cargo test -p gas-killer-wasm --test anvil_integration -- --ignored

use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::LazyLock;
use std::time::Duration;

use gas_killer_wasm::{
    AnalyzeTraceResult, analyze_trace_inner, encode_trace_inner, estimate_gas_heuristic_inner,
};
use serde_json::{Value, json};
mod common;
use common::{test_caller_address, test_estimator_address};

/// anvil's first dev account, unlocked by default.
const DEV: &str = "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266";
/// Under Osaka's per-transaction cap (EIP-7825: 2^24), and ample for these contracts.
const TX_GAS: &str = "0xf42400"; // 16,000,000

const DELEGATECALL_MAIN: &str = include_str!("fixtures/DelegateCallTestMain.hex");
const ACCESS_CONTROL_MAIN: &str = include_str!("fixtures/AccessControlTestMain.hex");

// Function selectors (`cast sig`).
const SET: &str = "0x60fe47b1"; // set(uint256)
const DEPOSIT: &str = "0xd0e30db0"; // deposit()
const CALL_EXTERNAL: &str = "0xc7117edf"; // callExternalContract()
const RUN: &str = "0xc0406226"; // run()

/// The trace of each scenario, recorded once per test process.
struct Traces {
    set: String,
    deposit: String,
    call_external: String,
    delegatecall: String,
    access_control: String,
}

static TRACES: LazyLock<Traces> = LazyLock::new(|| {
    let anvil = LocalAnvil::spawn();
    let simple_storage = anvil.deploy(&gas_analyzer_core::SimpleStorage::BYTECODE.to_string());
    let delegatecall_main = anvil.deploy(DELEGATECALL_MAIN);
    let access_control_main = anvil.deploy(ACCESS_CONTROL_MAIN);
    Traces {
        set: anvil.trace_of_call(&simple_storage, &format!("{SET}{:064x}", 42), 0),
        deposit: anvil.trace_of_call(&simple_storage, DEPOSIT, 1_000),
        call_external: anvil.trace_of_call(&simple_storage, CALL_EXTERNAL, 0),
        delegatecall: anvil.trace_of_call(&delegatecall_main, RUN, 0),
        access_control: anvil.trace_of_call(&access_control_main, RUN, 0),
    }
    // `anvil` is dropped, and killed, here.
});

/// A local anvil, killed on drop. Steps tracing is on so `debug_traceTransaction` returns
/// struct logs.
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
        // `--port 0` lets the OS pick a free port; anvil prints the one it bound.
        let mut child = Command::new("anvil")
            .args(["--port", "0", "--steps-tracing"])
            // Pinned so gas figures don't move with anvil's default hardfork.
            .args(["--hardfork", "osaka"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect(
                "failed to spawn `anvil`: these tests need foundry (https://getfoundry.sh) on \
                 PATH; CI installs it via foundry-toolchain",
            );
        let mut stdout = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        let mut url = None;
        for line in stdout.by_ref() {
            let Ok(line) = line else { break };
            if let Some(addr) = line.trim().strip_prefix("Listening on ") {
                url = Some(format!("http://{addr}"));
                break;
            }
        }
        let Some(url) = url else {
            // stdout closed without the ready line: anvil exited. Say why.
            let status = child.wait().expect("wait for anvil");
            let mut stderr = String::new();
            if let Some(mut err) = child.stderr.take() {
                let _ = err.read_to_string(&mut stderr);
            }
            panic!(
                "anvil exited before it was ready ({status}): {}",
                stderr.trim()
            );
        };
        // Keep draining anvil's output: a closed pipe would fail its writes, a full one block them.
        std::thread::spawn(move || stdout.for_each(drop));
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || BufReader::new(stderr).lines().for_each(drop));
        }
        LocalAnvil {
            child,
            url,
            client: reqwest::blocking::Client::new(),
        }
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        let body = json!({ "jsonrpc": "2.0", "method": method, "params": params, "id": 1 });
        let resp: Value = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .and_then(|r| r.json())
            .unwrap_or_else(|e| panic!("{method}: {e}"));
        match resp.get("result") {
            Some(result) => result.clone(),
            None => panic!("{method} failed: {:?}", resp.get("error")),
        }
    }

    /// Send a transaction from the dev account and return its successful receipt.
    fn send(&self, to: Option<&str>, data: &str, value: u64) -> Value {
        let mut tx = json!({
            "from": DEV,
            "data": data,
            "value": format!("{value:#x}"),
            "gas": TX_GAS,
        });
        if let Some(to) = to {
            tx["to"] = json!(to);
        }
        let hash = self.rpc("eth_sendTransaction", json!([tx]));
        // anvil automines, but the receipt can lag the send.
        let mut receipt = Value::Null;
        for _ in 0..100 {
            receipt = self.rpc("eth_getTransactionReceipt", json!([hash]));
            if !receipt.is_null() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!receipt.is_null(), "no receipt for {hash} after 2s");
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

    /// Call `contract` with `data` (and `value` wei) and return the call's struct-log trace as
    /// JSON, the way a client hands it to the analyzer.
    fn trace_of_call(&self, contract: &str, data: &str, value: u64) -> String {
        let receipt = self.send(Some(contract), data, value);
        let trace = self.rpc(
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

fn analyze(trace: &str) -> AnalyzeTraceResult {
    analyze_trace_inner(
        trace,
        &test_estimator_address(),
        &test_caller_address(),
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// SimpleStorage.set() — single SSTORE
// ---------------------------------------------------------------------------

#[test]
#[ignore = "spawns anvil"]
fn test_simple_storage_set() {
    let result = analyze(&TRACES.set);

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
    let result = analyze(&TRACES.deposit);

    // SSTORE + LOG2
    assert_eq!(result.state_update_count, 2);
    assert!(result.gas_estimate > 0);

    eprintln!(
        "simple_storage_deposit: gas={}, updates={}, heuristic={}",
        result.gas_estimate, result.state_update_count, result.is_heuristic
    );
}

// ---------------------------------------------------------------------------
// SimpleStorage.callExternalContract() — has an external CALL
// ---------------------------------------------------------------------------

#[test]
#[ignore = "spawns anvil"]
fn test_simple_storage_call_external() {
    let result = analyze(&TRACES.call_external);

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
    let result = analyze(&TRACES.delegatecall);

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
    let result = analyze(&TRACES.access_control);

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
    let trace = &TRACES.set;

    let analyze = analyze(trace);
    let encode = encode_trace_inner(trace).unwrap();
    let heuristic = estimate_gas_heuristic_inner(trace, None).unwrap();

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
    let result = encode_trace_inner(&TRACES.deposit).unwrap();

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
    let result = estimate_gas_heuristic_inner(&TRACES.set, None).unwrap();

    assert!(
        result.gas_estimate >= 21_000,
        "gas should be at least BASE_TX_COST"
    );
    assert!(result.is_heuristic);

    eprintln!("heuristic: gas={}", result.gas_estimate);
}

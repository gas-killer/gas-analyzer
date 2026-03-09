use alloy_primitives::{Address, B256};
use alloy_rpc_types::trace::geth::DefaultFrame;
use gas_analyzer_core::{
    StateUpdate, compute_state_updates, encode_state_updates_to_abi,
    estimate_gas_from_state_updates,
};
use gas_analyzer_estimator::{SimEnv, estimate_state_changes_gas};
use revm::database::{CacheDB, EmptyDB};
use serde::Serialize;
use std::collections::HashSet;
use wasm_bindgen::prelude::*;

/// Initialize panic hook for better error messages in browser console.
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
pub struct AnalyzeTraceResult {
    pub encoded_updates: String,
    pub gas_estimate: u64,
    pub is_heuristic: bool,
    pub state_update_count: usize,
    pub skipped_opcodes: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct EncodeTraceResult {
    pub encoded_updates: String,
    pub state_update_count: usize,
    pub skipped_opcodes: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct EstimateGasResult {
    pub gas_estimate: u64,
    pub is_heuristic: bool,
    pub state_update_count: usize,
    pub skipped_opcodes: Vec<String>,
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn parse_and_compute(trace_json: &str) -> Result<(Vec<StateUpdate>, HashSet<String>, u64), String> {
    let trace: DefaultFrame =
        serde_json::from_str(trace_json).map_err(|e| format!("Failed to parse trace: {}", e))?;
    let (updates, skipped, call_gas) = compute_state_updates(trace)
        .map_err(|e| format!("Failed to compute state updates: {}", e))?;
    Ok((updates, skipped.into_iter().collect(), call_gas))
}

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsError> {
    let serializer = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
    value
        .serialize(&serializer)
        .map_err(|e| JsError::new(&e.to_string()))
}

// ---------------------------------------------------------------------------
// Inner functions (testable without wasm-bindgen)
// ---------------------------------------------------------------------------

pub fn analyze_trace_inner(
    trace_json: &str,
    estimator_address: &str,
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
) -> Result<AnalyzeTraceResult, String> {
    let (state_updates, skipped_opcodes, call_gas_total) = parse_and_compute(trace_json)?;

    let encoded = encode_state_updates_to_abi(&state_updates);

    let addr: Address = estimator_address
        .parse()
        .map_err(|e| format!("Invalid estimator address: {}", e))?;

    let caller: Address = caller_address
        .parse()
        .map_err(|e| format!("Invalid caller address: {}", e))?;

    let mut cache_db = CacheDB::new(EmptyDB::default());
    let sim_env = SimEnv {
        number: estimate_state_changes_block_number.unwrap_or(0),
        timestamp: 0,
        gas_limit: 30_000_000,
        coinbase: Address::ZERO,
        prevrandao: B256::ZERO,
        gas_price: 0,
    };

    let (gas_estimate, is_heuristic) =
        match estimate_state_changes_gas(&mut cache_db, addr, caller, &state_updates, &sim_env) {
            Ok(gas) => (gas, false),
            Err(_) => (
                estimate_gas_from_state_updates(&state_updates, call_gas_total),
                true,
            ),
        };

    let mut skipped = skipped_opcodes.into_iter().collect::<Vec<_>>();
    skipped.sort();

    Ok(AnalyzeTraceResult {
        encoded_updates: format!("0x{}", hex::encode(&encoded)),
        gas_estimate,
        is_heuristic,
        state_update_count: state_updates.len(),
        skipped_opcodes: skipped,
    })
}

pub fn estimate_gas_heuristic_inner(trace_json: &str) -> Result<EstimateGasResult, String> {
    let (state_updates, skipped_opcodes, call_gas_total) = parse_and_compute(trace_json)?;

    let gas = estimate_gas_from_state_updates(&state_updates, call_gas_total);

    let mut skipped = skipped_opcodes.into_iter().collect::<Vec<_>>();
    skipped.sort();

    Ok(EstimateGasResult {
        gas_estimate: gas,
        is_heuristic: true,
        state_update_count: state_updates.len(),
        skipped_opcodes: skipped,
    })
}

pub fn encode_trace_inner(trace_json: &str) -> Result<EncodeTraceResult, String> {
    let (state_updates, skipped_opcodes, _) = parse_and_compute(trace_json)?;

    let encoded = encode_state_updates_to_abi(&state_updates);
    let mut skipped = skipped_opcodes.into_iter().collect::<Vec<_>>();
    skipped.sort();

    Ok(EncodeTraceResult {
        encoded_updates: format!("0x{}", hex::encode(&encoded)),
        state_update_count: state_updates.len(),
        skipped_opcodes: skipped,
    })
}

// ---------------------------------------------------------------------------
// wasm-bindgen exports
// ---------------------------------------------------------------------------

/// Analyze a Geth trace: parse state updates, ABI-encode them, and estimate gas.
///
/// `trace_json` is the JSON body of a debug_traceTransaction/debug_traceCall response
/// (the `result` field, not the full JSON-RPC envelope).
///
/// `estimator_address` is the hex address where the gas estimator contract will be
/// deployed in the empty CacheDB, e.g. `"0x1234..."`.
///
/// `caller_address` is the hex address of the original transaction sender, used as
/// `tx.origin` during gas simulation.
///
/// Returns a JS object with: `encoded_updates`, `gas_estimate`, `is_heuristic`,
/// `state_update_count`, `skipped_opcodes`.
#[wasm_bindgen]
pub fn analyze_trace(
    trace_json: &str,
    estimator_address: &str,
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
) -> Result<JsValue, JsError> {
    let result = analyze_trace_inner(
        trace_json,
        estimator_address,
        caller_address,
        estimate_state_changes_block_number,
    )
    .map_err(|e| JsError::new(&e))?;
    to_js(&result)
}

/// Heuristic-only gas estimation (no revm, faster, less accurate).
#[wasm_bindgen]
pub fn estimate_gas_heuristic(trace_json: &str) -> Result<JsValue, JsError> {
    let result = estimate_gas_heuristic_inner(trace_json).map_err(|e| JsError::new(&e))?;
    to_js(&result)
}

/// Encode state updates only (no gas estimation).
#[wasm_bindgen]
pub fn encode_trace(trace_json: &str) -> Result<JsValue, JsError> {
    let result = encode_trace_inner(trace_json).map_err(|e| JsError::new(&e))?;
    to_js(&result)
}

// ---------------------------------------------------------------------------
// TypeScript type definitions
// ---------------------------------------------------------------------------

#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &str = r#"
export interface AnalyzeTraceResult {
    encoded_updates: string;
    gas_estimate: number;
    is_heuristic: boolean;
    state_update_count: number;
    skipped_opcodes: string[];
}

export interface EncodeTraceResult {
    encoded_updates: string;
    state_update_count: number;
    skipped_opcodes: string[];
}

export interface EstimateGasResult {
    gas_estimate: number;
    is_heuristic: boolean;
    state_update_count: number;
    skipped_opcodes: string[];
}
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ===== Fixture builders =====

    /// Build a DefaultFrame JSON string from struct log entries.
    fn make_trace(struct_logs: Vec<serde_json::Value>) -> String {
        serde_json::json!({
            "failed": false,
            "gas": 100000,
            "returnValue": "0x",
            "structLogs": struct_logs,
        })
        .to_string()
    }

    /// SSTORE structlog. Geth stack is bottom-to-top: [value, slot].
    /// After reverse in compute_state_updates: stack[0]=slot, stack[1]=value.
    fn make_sstore_log(slot: &str, value: &str, gas: u64) -> serde_json::Value {
        serde_json::json!({
            "pc": 100,
            "op": "SSTORE",
            "gas": gas,
            "gasCost": 5000,
            "depth": 1,
            "stack": [
                format!("0x{:0>64}", value.trim_start_matches("0x")),
                format!("0x{:0>64}", slot.trim_start_matches("0x")),
            ],
            "memory": [],
        })
    }

    /// LOG1 structlog. Geth stack bottom-to-top: [topic1, length, offset].
    /// After reverse: stack[0]=offset, stack[1]=length, stack[2]=topic1.
    fn make_log1_log(data_hex: &str, topic1: &str, gas: u64) -> serde_json::Value {
        let data_bytes = hex::decode(data_hex.trim_start_matches("0x")).unwrap();
        let data_len = data_bytes.len();
        // Place data at memory offset 0
        let mem_word_count = data_len.div_ceil(32).max(1);
        let mut memory_hex = hex::encode(&data_bytes);
        // Pad to 32-byte words (Geth memory is returned as 32-byte hex chunks without 0x)
        let target_len = mem_word_count * 64;
        while memory_hex.len() < target_len {
            memory_hex.push('0');
        }
        let memory: Vec<String> = memory_hex
            .as_bytes()
            .chunks(64)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();

        serde_json::json!({
            "pc": 300,
            "op": "LOG1",
            "gas": gas,
            "gasCost": 375,
            "depth": 1,
            "stack": [
                format!("0x{:0>64}", topic1.trim_start_matches("0x")),
                format!("0x{:0>64}", format!("{:x}", data_len)),
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ],
            "memory": memory,
        })
    }

    /// CALL structlog at depth 1 with memory for args.
    /// Geth stack bottom-to-top: [retLen, retOff, argsLen, argsOff, value, addr, gas].
    /// After reverse: [gas, addr, value, argsOff, argsLen, retOff, retLen].
    fn make_call_log(
        target: &str,
        call_value: &str,
        args_hex: &str,
        gas: u64,
    ) -> serde_json::Value {
        let args_bytes = hex::decode(args_hex.trim_start_matches("0x")).unwrap_or_default();
        let args_len = args_bytes.len();
        let mem_word_count = args_len.div_ceil(32).max(1);
        let mut memory_hex = hex::encode(&args_bytes);
        let target_len = mem_word_count * 64;
        while memory_hex.len() < target_len {
            memory_hex.push('0');
        }
        let memory: Vec<String> = memory_hex
            .as_bytes()
            .chunks(64)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();

        serde_json::json!({
            "pc": 200,
            "op": "CALL",
            "gas": gas,
            "gasCost": 100,
            "depth": 1,
            "stack": [
                "0x0000000000000000000000000000000000000000000000000000000000000020",
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                format!("0x{:0>64}", format!("{:x}", args_len)),
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                format!("0x{:0>64}", call_value.trim_start_matches("0x")),
                format!("0x{:0>64}", target.trim_start_matches("0x")),
                format!("0x{:0>64}", format!("{:x}", gas)),
            ],
            "memory": memory,
        })
    }

    /// Build a CALL trace with the depth transitions needed for gas tracking:
    /// CALL at depth 1 → entry at depth 2 → return to depth 1.
    fn make_call_trace_with_depth(target: &str, args_hex: &str) -> String {
        let call = make_call_log(target, "0", args_hex, 50000);
        let subcall_entry = serde_json::json!({
            "pc": 0, "op": "STOP", "gas": 49000, "gasCost": 0, "depth": 2,
            "stack": [], "memory": [],
        });
        let return_to_depth1 = serde_json::json!({
            "pc": 201, "op": "POP", "gas": 48000, "gasCost": 2, "depth": 1,
            "stack": [
                "0x0000000000000000000000000000000000000000000000000000000000000001",
            ],
            "memory": [],
        });
        make_trace(vec![call, subcall_entry, return_to_depth1])
    }

    fn test_estimator_address() -> String {
        "0xd682Fe2ee8bdd59fdcCc5a4962FD98c20Ef47290".to_string()
    }

    fn test_caller_address() -> String {
        "0x0000000000000000000000000000000000000001".to_string()
    }

    // ===== Parsing & deserialization tests =====

    #[test]
    fn test_parse_valid_empty_trace() {
        let trace = make_trace(vec![]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 0);
    }

    #[test]
    fn test_parse_invalid_json() {
        let result = encode_trace_inner("not json at all");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to parse trace"));
    }

    #[test]
    fn test_parse_wrong_json_shape() {
        let result = encode_trace_inner(r#"{"foo": "bar"}"#);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to parse trace"));
    }

    #[test]
    fn test_parse_empty_string() {
        let result = encode_trace_inner("");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to parse trace"));
    }

    #[test]
    fn test_parse_valid_address() {
        let trace = make_trace(vec![]);
        let result = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_invalid_address() {
        let trace = make_trace(vec![]);
        let result = analyze_trace_inner(&trace, "not-an-address", &test_caller_address(), None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid estimator address"));
    }

    #[test]
    fn test_parse_address_too_short() {
        let trace = make_trace(vec![]);
        let result = analyze_trace_inner(&trace, "0x1234", &test_caller_address(), None);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid estimator address"));
    }

    // ===== State update extraction tests =====

    #[test]
    fn test_single_sstore() {
        let trace = make_trace(vec![make_sstore_log("1", "2a", 90000)]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 1);
        assert!(result.skipped_opcodes.is_empty());
    }

    #[test]
    fn test_single_log1() {
        let data_hex = "00000000000000000000000000000000000000000000000000000000000000ff";
        let topic = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        let trace = make_trace(vec![make_log1_log(data_hex, topic, 80000)]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 1);
    }

    #[test]
    fn test_multiple_mixed_ops() {
        let data_hex = "00000000000000000000000000000000000000000000000000000000000000ff";
        let topic = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        let trace = make_trace(vec![
            make_sstore_log("1", "2a", 90000),
            make_log1_log(data_hex, topic, 80000),
            make_sstore_log("2", "3b", 70000),
        ]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 3);
    }

    #[test]
    fn test_create_opcode_ignored() {
        // CREATE is not in the matches!(CALL|SSTORE|LOG*) filter in compute_state_updates,
        // so it's silently ignored — not extracted and not added to skipped_opcodes.
        let create_log = serde_json::json!({
            "pc": 50, "op": "CREATE", "gas": 85000, "gasCost": 32000, "depth": 1,
            "stack": [
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ],
            "memory": [],
        });
        let trace = make_trace(vec![create_log]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 0);
    }

    #[test]
    fn test_depth_gt_1_only_produces_no_updates() {
        let deep_sstore = serde_json::json!({
            "pc": 100, "op": "SSTORE", "gas": 90000, "gasCost": 5000, "depth": 2,
            "stack": [
                "0x0000000000000000000000000000000000000000000000000000000000000001",
                "0x0000000000000000000000000000000000000000000000000000000000000002",
            ],
            "memory": [],
        });
        let trace = make_trace(vec![deep_sstore]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 0);
    }

    // ===== Encoding tests =====

    #[test]
    fn test_encode_single_store_produces_hex() {
        let trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let result = encode_trace_inner(&trace).unwrap();
        assert!(result.encoded_updates.starts_with("0x"));
        assert!(result.encoded_updates.len() > 2);
    }

    #[test]
    fn test_encode_empty_trace_produces_valid_output() {
        let trace = make_trace(vec![]);
        let result = encode_trace_inner(&trace).unwrap();
        assert!(result.encoded_updates.starts_with("0x"));
    }

    // ===== revm/EmptyDB gas estimation tests =====

    #[test]
    fn test_revm_sstore_only_succeeds() {
        let trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let result = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
        )
        .unwrap();
        assert!(!result.is_heuristic);
        assert!(result.gas_estimate > 0);
    }

    #[test]
    fn test_revm_multiple_sstores() {
        let trace = make_trace(vec![
            make_sstore_log("1", "aa", 90000),
            make_sstore_log("2", "bb", 85000),
            make_sstore_log("3", "cc", 80000),
        ]);
        let result = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
        )
        .unwrap();
        assert!(!result.is_heuristic);
        // Gas for 3 stores should be meaningfully more than for 1
        let single_trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let single_result = analyze_trace_inner(
            &single_trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
        )
        .unwrap();
        assert!(result.gas_estimate > single_result.gas_estimate);
    }

    #[test]
    fn test_revm_call_to_empty_address_succeeds() {
        // In the EVM, a CALL to an address with no code succeeds (like sending to an EOA).
        // The StateChangeHandlerGasEstimator's CALL to an empty address returns success,
        // so revm doesn't revert and is_heuristic stays false.
        let trace =
            make_call_trace_with_depth("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "aabbccdd");
        let result = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
        )
        .unwrap();
        assert!(!result.is_heuristic);
        assert!(result.gas_estimate > 0);
    }

    #[test]
    fn test_revm_gas_estimate_is_reasonable() {
        let trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let result = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
        )
        .unwrap();
        // Should be between 21k (base) and 500k for a single SSTORE
        assert!(
            result.gas_estimate > 21_000 && result.gas_estimate < 500_000,
            "gas was {}",
            result.gas_estimate
        );
    }

    // ===== Heuristic gas estimation tests =====

    #[test]
    fn test_heuristic_empty_trace() {
        let trace = make_trace(vec![]);
        let result = estimate_gas_heuristic_inner(&trace).unwrap();
        assert_eq!(result.gas_estimate, 21_000);
        assert!(result.is_heuristic);
    }

    #[test]
    fn test_heuristic_single_sstore() {
        let trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let result = estimate_gas_heuristic_inner(&trace).unwrap();
        // BASE_TX_COST(21000) + WARM_SSTORE_COST(5000) = 26000
        assert_eq!(result.gas_estimate, 26_000);
    }

    #[test]
    fn test_heuristic_log1_with_32_bytes_data() {
        let data_hex = "00000000000000000000000000000000000000000000000000000000000000ff";
        let topic = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        let trace = make_trace(vec![make_log1_log(data_hex, topic, 80000)]);
        let result = estimate_gas_heuristic_inner(&trace).unwrap();
        // BASE_TX_COST(21000) + LOG_BASE_COST(375) + LOG_TOPIC_COST(375) + 32*LOG_DATA_COST_PER_BYTE(8) = 22006
        assert_eq!(result.gas_estimate, 22_006);
    }

    #[test]
    fn test_heuristic_always_is_heuristic() {
        let trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let result = estimate_gas_heuristic_inner(&trace).unwrap();
        assert!(result.is_heuristic);
    }

    #[test]
    fn test_heuristic_call_includes_gas() {
        let trace =
            make_call_trace_with_depth("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "aabbccdd");
        let result = estimate_gas_heuristic_inner(&trace).unwrap();
        assert!(
            result.gas_estimate > 21_000,
            "gas was {}",
            result.gas_estimate
        );
    }

    // Response shape is now guaranteed at compile time by the typed structs.
    // AnalyzeTraceResult has 5 fields, EncodeTraceResult has 3, EstimateGasResult has 3.

    // ===== Revm vs heuristic comparison test =====

    #[test]
    fn test_revm_and_heuristic_both_produce_positive_gas() {
        let trace_json = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let analyze = analyze_trace_inner(
            &trace_json,
            &test_estimator_address(),
            &test_caller_address(),
            None,
        )
        .unwrap();
        let heuristic = estimate_gas_heuristic_inner(&trace_json).unwrap();

        assert!(analyze.gas_estimate > 0);
        assert!(heuristic.gas_estimate > 0);
        assert!(!analyze.is_heuristic);
        assert!(heuristic.is_heuristic);
    }

    // ===== Roundtrip encoding test =====

    #[test]
    fn test_encode_decode_roundtrip() {
        // Encode a trace with 2 SSTOREs, then verify the ABI structure directly.
        let trace = make_trace(vec![
            make_sstore_log("1", "aa", 90000),
            make_sstore_log("2", "bb", 85000),
        ]);
        let result = encode_trace_inner(&trace).unwrap();
        let encoded_bytes = hex::decode(result.encoded_updates.trim_start_matches("0x")).unwrap();

        // Should be non-trivial size for 2 SSTOREs
        assert!(
            encoded_bytes.len() > 64,
            "encoding too short: {} bytes",
            encoded_bytes.len()
        );

        // First 32 bytes: offset to types array = 0x40 (64)
        let types_offset = u64::from_be_bytes(encoded_bytes[24..32].try_into().unwrap()) as usize;
        assert_eq!(types_offset, 64);

        // At types_offset: count = 2
        let types_count = u64::from_be_bytes(
            encoded_bytes[types_offset + 24..types_offset + 32]
                .try_into()
                .unwrap(),
        );
        assert_eq!(types_count, 2, "should have 2 type entries");

        // types[0] and types[1] should both be 0 (StateUpdateType::STORE)
        let type0 = encoded_bytes[types_offset + 32 + 31]; // last byte of first 32-byte word
        let type1 = encoded_bytes[types_offset + 64 + 31]; // last byte of second 32-byte word
        assert_eq!(type0, 0, "type[0] should be STORE(0)");
        assert_eq!(type1, 0, "type[1] should be STORE(0)");
    }

    // ===== Failed trace test =====

    #[test]
    fn test_failed_trace_still_extracts_updates() {
        // compute_state_updates processes structLogs regardless of the `failed` field.
        // Document this behavior: a reverted tx's trace still yields state updates
        // (the caller decides whether to use them).
        let trace = serde_json::json!({
            "failed": true,
            "gas": 100000,
            "returnValue": "0x",
            "structLogs": [
                {
                    "pc": 100, "op": "SSTORE", "gas": 90000, "gasCost": 5000, "depth": 1,
                    "stack": [
                        "0x00000000000000000000000000000000000000000000000000000000000000ff",
                        "0x0000000000000000000000000000000000000000000000000000000000000001",
                    ],
                    "memory": [],
                }
            ],
        })
        .to_string();
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 1);
    }

    // ===== LOG0 test =====

    #[test]
    fn test_single_log0() {
        // LOG0 has no topics. Stack (bottom-to-top): [length, offset].
        // After reverse: [offset, length].
        let data_hex = "deadbeef";
        let data_bytes = hex::decode(data_hex).unwrap();
        let data_len = data_bytes.len();
        let mut memory_hex = hex::encode(&data_bytes);
        while memory_hex.len() < 64 {
            memory_hex.push('0');
        }
        let log0 = serde_json::json!({
            "pc": 300, "op": "LOG0", "gas": 80000, "gasCost": 375, "depth": 1,
            "stack": [
                format!("0x{:0>64}", format!("{:x}", data_len)),
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ],
            "memory": [memory_hex],
        });
        let trace = make_trace(vec![log0]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 1);
    }

    #[test]
    fn test_heuristic_log0() {
        let data_hex = "deadbeef";
        let data_bytes = hex::decode(data_hex).unwrap();
        let data_len = data_bytes.len();
        let mut memory_hex = hex::encode(&data_bytes);
        while memory_hex.len() < 64 {
            memory_hex.push('0');
        }
        let log0 = serde_json::json!({
            "pc": 300, "op": "LOG0", "gas": 80000, "gasCost": 375, "depth": 1,
            "stack": [
                format!("0x{:0>64}", format!("{:x}", data_len)),
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ],
            "memory": [memory_hex],
        });
        let trace = make_trace(vec![log0]);
        let result = estimate_gas_heuristic_inner(&trace).unwrap();
        // BASE_TX_COST(21000) + LOG_BASE_COST(375) + 4*LOG_DATA_COST_PER_BYTE(8) = 21407
        assert_eq!(result.gas_estimate, 21_407);
    }

    // ===== Isolation test =====

    #[test]
    fn test_sequential_calls_dont_leak_state() {
        let trace1 = make_trace(vec![make_sstore_log("1", "aa", 90000)]);
        let trace2 = make_trace(vec![
            make_sstore_log("1", "bb", 90000),
            make_sstore_log("2", "cc", 85000),
        ]);
        let addr = test_estimator_address();
        let caller = test_caller_address();
        let result1 = analyze_trace_inner(&trace1, &addr, &caller, None).unwrap();
        let result2 = analyze_trace_inner(&trace2, &addr, &caller, None).unwrap();
        // Each call uses a fresh CacheDB, so results should differ
        assert_eq!(result1.state_update_count, 1);
        assert_eq!(result2.state_update_count, 2);
    }
}

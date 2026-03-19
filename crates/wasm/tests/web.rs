use wasm_bindgen_test::*;

use gas_killer_wasm::{analyze_trace, encode_trace, estimate_gas_heuristic};

mod common;
use common::{test_caller_address, test_estimator_address, valid_sstore_trace};

// ---------------------------------------------------------------------------
// WASM boundary smoke tests
// ---------------------------------------------------------------------------

#[wasm_bindgen_test]
fn test_wasm_analyze_trace_returns_jsvalue() {
    let result = analyze_trace(
        &valid_sstore_trace(),
        &test_estimator_address(),
        &test_caller_address(),
        None,
    );
    assert!(
        result.is_ok(),
        "analyze_trace should succeed: {:?}",
        result.err()
    );
    let val = result.unwrap();
    assert!(!val.is_undefined());
    assert!(!val.is_null());
}

#[wasm_bindgen_test]
fn test_wasm_analyze_trace_invalid_json_returns_error() {
    let result = analyze_trace(
        "bad json",
        &test_estimator_address(),
        &test_caller_address(),
        None,
    );
    assert!(result.is_err());
}

#[wasm_bindgen_test]
fn test_wasm_analyze_trace_invalid_address_returns_error() {
    let result = analyze_trace(
        &valid_sstore_trace(),
        "not-an-address",
        &test_caller_address(),
        None,
    );
    assert!(result.is_err());
}

#[wasm_bindgen_test]
fn test_wasm_estimate_gas_heuristic_returns_jsvalue() {
    let result = estimate_gas_heuristic(&valid_sstore_trace());
    assert!(
        result.is_ok(),
        "estimate_gas_heuristic should succeed: {:?}",
        result.err()
    );
    let val = result.unwrap();
    assert!(!val.is_undefined());
}

#[wasm_bindgen_test]
fn test_wasm_encode_trace_returns_jsvalue() {
    let result = encode_trace(&valid_sstore_trace());
    assert!(
        result.is_ok(),
        "encode_trace should succeed: {:?}",
        result.err()
    );
    let val = result.unwrap();
    assert!(!val.is_undefined());
}

#[wasm_bindgen_test]
fn test_wasm_estimate_gas_heuristic_invalid_json_returns_error() {
    let result = estimate_gas_heuristic("bad json");
    assert!(result.is_err());
}

#[wasm_bindgen_test]
fn test_wasm_encode_trace_invalid_json_returns_error() {
    let result = encode_trace("bad json");
    assert!(result.is_err());
}

#[wasm_bindgen_test]
fn test_wasm_analyze_trace_response_fields() {
    let result = analyze_trace(
        &valid_sstore_trace(),
        &test_estimator_address(),
        &test_caller_address(),
        None,
    )
    .unwrap();
    let json: serde_json::Value = serde_wasm_bindgen::from_value(result).unwrap();
    let obj = json.as_object().unwrap();

    assert!(
        obj.contains_key("encoded_updates"),
        "missing encoded_updates"
    );
    assert!(obj.contains_key("gas_estimate"), "missing gas_estimate");
    assert!(obj.contains_key("is_heuristic"), "missing is_heuristic");
    assert!(
        obj.contains_key("state_update_count"),
        "missing state_update_count"
    );
    assert!(
        obj.contains_key("skipped_opcodes"),
        "missing skipped_opcodes"
    );

    // Verify types
    assert!(obj["encoded_updates"].as_str().unwrap().starts_with("0x"));
    assert!(obj["gas_estimate"].as_u64().unwrap() > 0);
    assert_eq!(obj["state_update_count"], 1);
}

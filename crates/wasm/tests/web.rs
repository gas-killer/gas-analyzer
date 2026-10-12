use wasm_bindgen_test::*;

use gas_killer_wasm::{analyze_trace, analyze_trace_bytes, encode_trace, estimate_gas_heuristic};

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
        None,
    );
    assert!(result.is_err());
}

#[wasm_bindgen_test]
fn test_wasm_estimate_gas_heuristic_returns_jsvalue() {
    let result = estimate_gas_heuristic(&valid_sstore_trace(), None);
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
    let result = estimate_gas_heuristic("bad json", None);
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

#[wasm_bindgen_test]
fn test_wasm_analyze_trace_bytes_unwraps_the_envelope() {
    let response = format!(
        r#"{{"jsonrpc":"2.0","id":1,"result":{}}}"#,
        valid_sstore_trace()
    );
    let result = analyze_trace_bytes(
        response.as_bytes(),
        &test_estimator_address(),
        &test_caller_address(),
        None,
        None,
    )
    .unwrap();
    let json: serde_json::Value = serde_wasm_bindgen::from_value(result).unwrap();
    assert_eq!(json["state_update_count"], 1);
}

#[wasm_bindgen_test]
fn test_wasm_analyze_trace_bytes_names_rpc_errors() {
    let name = |message: &str| {
        let response =
            serde_json::json!({ "id": 1, "error": { "code": -32000, "message": message } });
        let err = analyze_trace_bytes(
            response.to_string().as_bytes(),
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
        )
        .unwrap_err();
        let err: js_sys::Error = err.into();
        (String::from(err.name()), String::from(err.message()))
    };
    assert_eq!(
        name("execution timeout"),
        ("RpcError".into(), "execution timeout".into())
    );
    assert_eq!(name("response too large").0, "TraceTooLargeError");
}

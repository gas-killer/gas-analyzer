//! Shared test fixtures for integration tests.

#![allow(dead_code)]

pub fn make_trace(struct_logs: Vec<serde_json::Value>) -> String {
    serde_json::json!({
        "failed": false,
        "gas": 100000,
        "returnValue": "0x",
        "structLogs": struct_logs,
    })
    .to_string()
}

pub fn make_sstore_log(slot: &str, value: &str, gas: u64) -> serde_json::Value {
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

pub fn test_estimator_address() -> String {
    "0xd682Fe2ee8bdd59fdcCc5a4962FD98c20Ef47290".to_string()
}

pub fn valid_sstore_trace() -> String {
    make_trace(vec![make_sstore_log("1", "ff", 90000)])
}

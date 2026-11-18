#![cfg(not(target_arch = "wasm32"))]
//! The analyzer on struct-log traces recorded from a real Geth node.
//!
//! The anvil integration tests cover the contracts; these pin the parser to Geth's own output
//! (its `storage` and `refund` fields, memory and stack encoding), which anvil only emulates.
//! They need no network, so they run by default.
//!
//! Fixtures are the `result` of `debug_traceTransaction(hash, {"enableMemory": true})` from
//! Geth v1.17.8 on mainnet:
//! - `geth_weth_deposit.json`: WETH9 `deposit()`,
//!   0x0cff7e0d83c020ed0e316d49558fb4ffe4d410bb23e49556051daae29e5e18c5 (block 26438455)
//! - `geth_weth_transfer.json`: WETH9 `transfer(address,uint256)`,
//!   0x523b4ce4dd6f2a59c6d728529e33fd16cb659b3f5012b43f7c375a18ace3a596 (block 26438459)

use gas_killer_wasm::{analyze_trace_inner, encode_trace_inner, estimate_gas_heuristic_inner};
mod common;
use common::{test_caller_address, test_estimator_address};

const WETH_DEPOSIT: &str = include_str!("fixtures/geth_weth_deposit.json");
const WETH_TRANSFER: &str = include_str!("fixtures/geth_weth_transfer.json");

fn state_update_counts(trace: &str) -> (usize, usize, usize) {
    let analyze = analyze_trace_inner(
        trace,
        &test_estimator_address(),
        &test_caller_address(),
        None,
        None,
        None,
        None,
        None,
    )
    .unwrap();
    assert!(!analyze.is_heuristic, "a Geth trace should be measured");
    assert!(analyze.gas_estimate > 0);
    let encode = encode_trace_inner(trace).unwrap();
    let heuristic = estimate_gas_heuristic_inner(trace, None).unwrap();
    (
        analyze.state_update_count,
        encode.state_update_count,
        heuristic.state_update_count,
    )
}

#[test]
fn geth_weth_deposit() {
    // The balance SSTORE and the Deposit LOG2.
    assert_eq!(state_update_counts(WETH_DEPOSIT), (2, 2, 2));
}

#[test]
fn geth_weth_transfer() {
    // Both balance SSTOREs and the Transfer LOG3.
    assert_eq!(state_update_counts(WETH_TRANSFER), (3, 3, 3));
}

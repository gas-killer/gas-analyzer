//! Core module containing shared types, encoding, and constants.
//!
//! This crate provides the foundational types and utilities used
//! by both native and WASM implementations. It contains only pure
//! computation - no async, no I/O, no RPC calls.

pub mod constants;
pub mod encoding;
pub mod heuristic;
pub mod prestate;
pub mod sim_profile;
pub mod trace;
pub mod types;

// Re-export commonly used items
pub use encoding::{
    SignatureType, TURETZKY_UPPER_GAS_LIMIT_BLS, TURETZKY_UPPER_GAS_LIMIT_SCHNORR,
    encode_state_updates_to_abi, encode_state_updates_to_sol,
};
pub use heuristic::{
    BASE_TX_COST, CALLDATA_NONZERO_BYTE_COST, CALLDATA_ZERO_BYTE_COST, LOG_BASE_COST,
    LOG_DATA_COST_PER_BYTE, LOG_TOPIC_COST, MAX_REFUND_QUOTIENT, TraceOperations, calldata_gas,
    estimate_gas_from_operations, estimate_gas_from_state_updates,
    extract_operation_counts_from_trace,
};
pub use prestate::{
    PrestateEligibility, build_state_updates_from_prestate, classify_prestate_eligibility,
};
pub use sim_profile::{
    SimProfile, UNBOUNDED_BLOCK_GAS_LIMIT, UNBOUNDED_COLD_SSTORE_COST,
    UNBOUNDED_PAYLOAD_GAS_BUDGET, UNBOUNDED_TX_GAS_LIMIT, UnboundedCost, UnboundedCostViolation,
    estimate_applied_payload_gas, validate_unbounded_cost,
};
pub use trace::{
    TraceExtract, compute_state_updates, compute_state_updates_canonical, copy_memory,
    parse_trace_memory,
};
pub use types::{
    DummyExternal, IStateUpdateTypes, Opcode, RevertingContext, SimpleStorage, StateUpdate,
    StateUpdateType, StateUpdates,
};

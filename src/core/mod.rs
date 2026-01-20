//! Core module containing shared types, encoding, and constants.
//!
//! This module provides the foundational types and utilities used
//! by both Anvil and EvmSketch implementations.

pub mod constants;
pub mod encoding;
pub mod heuristic;
pub mod types;

// Re-export commonly used items
pub use encoding::{
    TURETZKY_UPPER_GAS_LIMIT, decode_state_updates_tuple, encode_state_updates_to_abi,
    encode_state_updates_to_sol,
};
pub use heuristic::{
    BASE_TX_COST, COLD_SSTORE_COST, LOG_BASE_COST, LOG_DATA_COST_PER_BYTE, LOG_TOPIC_COST,
    TraceOperations, estimate_gas_from_operations, estimate_gas_from_state_updates,
    extract_operation_counts_from_trace,
};
pub use types::{
    DummyExternal, IStateUpdateTypes, Opcode, RevertingContext, SimpleStorage, StateUpdate,
    StateUpdateType, StateUpdates,
};

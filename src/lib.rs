//! Gas Analyzer - Transaction analysis and gas estimation library
//!
//! This crate provides tools for analyzing Ethereum transactions and
//! estimating gas costs for state updates. It supports two implementations:
//!
//! - **Anvil** (`--features anvil`): Legacy implementation using Foundry's Anvil
//! - **EvmSketch** (`--features evmsketch`, default): Anvil-free implementation using sp1-contract-call
//!
//! # Feature Flags
//!
//! - `anvil`: Enable the Anvil-based implementation (requires Foundry)
//! - `evmsketch`: Enable the EvmSketch-based implementation (default, no external deps)
//!
//! # WASM-compatible crates
//!
//! The core computation is split into WASM-safe crates:
//! - `gas-analyzer-core`: Trace parsing, encoding, heuristics
//! - `gas-analyzer-estimator`: revm-based gas estimation

// Core module - thin re-export of gas_analyzer_core
pub mod core;

// RPC functions - async, requires alloy-provider
pub mod rpc;

// Feature-gated implementations
#[cfg(feature = "anvil")]
pub mod anvil;

#[cfg(feature = "evmsketch")]
pub mod evmsketch;

// Re-export core types (always available, backward compat)
pub use gas_analyzer_core::{
    // Types
    IStateUpdateTypes,
    Opcode,
    RevertingContext,
    SimpleStorage,
    StateUpdate,
    StateUpdateType,
    StateUpdates,
    // Encoding
    TURETZKY_UPPER_GAS_LIMIT,
    compute_state_updates,
    decode_state_updates_tuple,
    encode_state_updates_to_abi,
    encode_state_updates_to_sol,
    // Heuristics
    estimate_gas_from_state_updates,
};

// Re-export RPC functions at top level (backward compat)
pub use rpc::{compute_state_updates_from_tx, get_tx_trace};

// Re-export Anvil types and functions
#[cfg(feature = "anvil")]
pub use anvil::{
    // GasKiller
    GasKiller,
    GasKillerDefault,
    // Reports
    GasKillerReport,
    ReportDetails,
    StateUpdateReport,
    // Transaction extractor
    TxStateExtractor,
    // Public API
    call_to_encoded_state_updates_with_gas_estimate,
    gas_estimate_block,
    gas_estimate_tx,
    gaskiller_reporter,
    get_report,
    get_trace_from_call,
    invokes_smart_contract,
    tx_extractor_from_rpc_url,
};

// Re-export EvmSketch types and functions
#[cfg(feature = "evmsketch")]
pub use evmsketch::{
    // Executor
    DefaultEvmSketchExecutor,
    EvmSketchExecutor,
    EvmSketchExecutorBuilder,
    // GasKiller
    GasKillerEvmSketch,
    GasKillerEvmSketchBuilder,
    GasKillerEvmSketchDefault,
    // Public API
    call_to_encoded_state_updates_with_evmsketch,
};

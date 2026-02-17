//! Core module - thin re-export of gas_analyzer_core crate.
//!
//! This preserves backward compatibility so that `gas_analyzer_rs::core::*`
//! paths continue to work.

pub use gas_analyzer_core::*;

// Re-export async RPC functions that were originally in this module,
// to maintain backward compatibility for `gas_analyzer_rs::core::*` paths.
pub use crate::rpc::{compute_state_updates_from_tx, get_trace_from_call, get_tx_trace};

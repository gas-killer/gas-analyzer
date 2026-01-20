//! Heuristic gas estimation utilities.
//!
//! This module provides shared functions for estimating gas costs heuristically
//! when exact measurement is not possible. Used by both Anvil and EvmSketch implementations.

use crate::core::StateUpdate;

/// Heuristic gas costs for different operations
pub const BASE_TX_COST: u64 = 21_000;
pub const COLD_SSTORE_COST: u64 = 20_000;
pub const LOG_BASE_COST: u64 = 375;
pub const LOG_TOPIC_COST: u64 = 375;
pub const LOG_DATA_COST_PER_BYTE: u64 = 8;

/// Operations and gas data extracted from a trace
#[derive(Debug, Default)]
pub struct TraceOperations {
    pub sstore_count: u64,
    pub log_counts: [u64; 5],   // LOG0-LOG4
    pub external_call_gas: u64, // Total gas used by external calls (extracted from trace)
}

/// Estimate gas from state updates using heuristic costs.
///
/// This provides a rough estimate based on known gas costs for each operation type.
///
/// # Arguments
/// * `state_updates` - The state updates to estimate gas for
/// * `external_call_gas` - Actual gas used by external calls (cannot be optimized)
///
/// # Returns
/// Estimated gas cost
pub fn estimate_gas_from_state_updates(
    state_updates: &[StateUpdate],
    external_call_gas: u64,
) -> u64 {
    let mut gas = BASE_TX_COST;

    // Add actual gas used by external calls (cannot be optimized)
    gas += external_call_gas;

    for update in state_updates {
        gas += match update {
            StateUpdate::Store(_) => COLD_SSTORE_COST,
            // CALL gas is already included in external_call_gas from the trace
            StateUpdate::Call(_) => 0,
            StateUpdate::Log0(log) => {
                LOG_BASE_COST + log.data.len() as u64 * LOG_DATA_COST_PER_BYTE
            }
            StateUpdate::Log1(log) => {
                LOG_BASE_COST + LOG_TOPIC_COST + log.data.len() as u64 * LOG_DATA_COST_PER_BYTE
            }
            StateUpdate::Log2(log) => {
                LOG_BASE_COST + LOG_TOPIC_COST * 2 + log.data.len() as u64 * LOG_DATA_COST_PER_BYTE
            }
            StateUpdate::Log3(log) => {
                LOG_BASE_COST + LOG_TOPIC_COST * 3 + log.data.len() as u64 * LOG_DATA_COST_PER_BYTE
            }
            StateUpdate::Log4(log) => {
                LOG_BASE_COST + LOG_TOPIC_COST * 4 + log.data.len() as u64 * LOG_DATA_COST_PER_BYTE
            }
        };
    }

    gas
}

/// Estimate gas from trace operations using heuristic costs.
///
/// This is used when we extract operations directly from a trace without
/// creating StateUpdate objects (e.g., for fallback estimation).
///
/// # Arguments
/// * `operations` - The operations and gas data extracted from a trace
///
/// # Returns
/// Estimated gas cost
pub fn estimate_gas_from_operations(operations: &TraceOperations) -> u64 {
    let mut gas = BASE_TX_COST;

    // Add SSTORE costs (cold SSTORE)
    gas += operations.sstore_count * COLD_SSTORE_COST;

    // Add LOG costs
    // LOG0: base cost only (we don't have data length in operations)
    gas += operations.log_counts[0] * LOG_BASE_COST;
    // LOG1-LOG4: base + topics
    gas += operations.log_counts[1] * (LOG_BASE_COST + LOG_TOPIC_COST);
    gas += operations.log_counts[2] * (LOG_BASE_COST + LOG_TOPIC_COST * 2);
    gas += operations.log_counts[3] * (LOG_BASE_COST + LOG_TOPIC_COST * 3);
    gas += operations.log_counts[4] * (LOG_BASE_COST + LOG_TOPIC_COST * 4);

    // Add actual gas used by external calls (extracted from trace)
    gas += operations.external_call_gas;

    gas
}

/// Extract operations and gas usage from a Geth trace (DefaultFrame).
///
/// This counts all operations regardless of depth and extracts actual gas used
/// by external calls, useful for fallback estimation.
///
/// # Arguments
/// * `trace` - The Geth trace to extract operations from
///
/// # Returns
/// Operations and gas data extracted from the trace
pub fn extract_operation_counts_from_trace(
    trace: &alloy_rpc_types::trace::geth::DefaultFrame,
) -> TraceOperations {
    let mut operations = TraceOperations::default();

    // Track gas usage for external calls
    // In Geth traces, struct_log.gas is the remaining gas AFTER the opcode executes
    // When we see a CALL at depth 1:
    //   - gas = gas remaining AFTER CALL opcode executes (before sub-call)
    //   - gasCost = cost of CALL opcode itself
    // When depth increases to 2, we've entered the external call
    // When depth decreases back to 1, we've exited the external call
    // Gas used by sub-call = gas_after_call_opcode - gas_after_subcall_returns
    let mut gas_after_call_opcode: Option<u64> = None;
    let mut previous_depth = 0;
    let mut in_external_call = false;

    for struct_log in &trace.struct_logs {
        let op = &*struct_log.op;
        let depth = struct_log.depth;

        // Track depth changes to detect entering/exiting external calls
        if depth == 1 && previous_depth == 2 {
            // We've exited an external call (depth went from 2 to 1)
            if in_external_call {
                if let Some(gas_after_opcode) = gas_after_call_opcode {
                    // Gas remaining after the sub-call returns
                    // struct_log.gas is u64 (remaining gas after opcode executes)
                    let gas_after_subcall = struct_log.gas;

                    // Gas used by the sub-call = gas after CALL opcode - gas after sub-call returns
                    let gas_used = gas_after_opcode.saturating_sub(gas_after_subcall);
                    operations.external_call_gas += gas_used;
                }
                in_external_call = false;
                gas_after_call_opcode = None;
            }
        } else if depth == 2 && previous_depth == 1 {
            // We've entered an external call (depth went from 1 to 2)
            in_external_call = true;
        }

        // Track CALL opcodes at depth 1 (external calls from the main contract)
        if op == "CALL" && depth == 1 {
            // Note the gas remaining AFTER the CALL opcode executes (before sub-call)
            // This is the gas available for the sub-call
            // struct_log.gas is u64 (remaining gas after opcode executes)
            gas_after_call_opcode = Some(struct_log.gas);
        }

        // Count operations
        match op {
            "SSTORE" => {
                operations.sstore_count += 1;
            }
            "LOG0" => operations.log_counts[0] += 1,
            "LOG1" => operations.log_counts[1] += 1,
            "LOG2" => operations.log_counts[2] += 1,
            "LOG3" => operations.log_counts[3] += 1,
            "LOG4" => operations.log_counts[4] += 1,
            _ => {}
        }

        previous_depth = depth;
    }

    // Handle case where we're still in an external call at the end
    // (shouldn't happen in a valid trace, but handle gracefully)
    if in_external_call {
        if let Some(gas_after_opcode) = gas_after_call_opcode {
            // Use the last gas value as gas_after
            if let Some(last_log) = trace.struct_logs.last() {
                // Gas remaining after the sub-call returns
                // struct_log.gas is u64 (remaining gas after opcode executes)
                let gas_after_subcall = last_log.gas;
                let gas_used = gas_after_opcode.saturating_sub(gas_after_subcall);
                operations.external_call_gas += gas_used;
            }
        }
    }

    operations
}

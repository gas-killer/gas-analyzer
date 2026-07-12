//! Heuristic gas estimation utilities.
//!
//! This module provides shared functions for estimating gas costs heuristically
//! when exact measurement is not possible. Used by both Anvil and EvmSketch implementations.
//!
//! # Known discrepancies
//!
//! The heuristic compares against post-refund receipt `gasUsed`, so every bias
//! below pushes the estimate *up* and reported savings *down* (validated
//! empirically on mainnet tx `0x19daad7a…a16795`, an MEV arb with 128 depth-1
//! calls: gross call gas 12.15M vs net gasUsed 10.56M):
//!
//! - **Gas refunds** (fixed): `external_call_gas` is measured from struct-log
//!   gas deltas, which are pre-refund. EIP-3529 refunds (SSTORE clears) are now
//!   netted out via the trace's final refund counter, capped at 1/5 of the
//!   gross estimate ([`MAX_REFUND_QUOTIENT`]).
//! - **Re-entrant callbacks**: a depth-1 CALL whose callee re-enters the
//!   origin contract (e.g. Uniswap V3 `swap` → `uniswapV3SwapCallback`) is
//!   counted entirely as unoptimizable external gas, but part of that gas is
//!   the origin contract's own logic reached via the callback. The replay only
//!   works because the estimator proxy DELEGATECALLs the original bytecode.
//! - **Flat [`WARM_SSTORE_COST`]**: depth-1 SSTOREs are charged a flat 5,000
//!   regardless of cold/new-slot (22,100), warm-repeat (100), and repeated
//!   writes to the same slot are not deduplicated.
//! - **Calldata unmodeled**: [`BASE_TX_COST`] has no calldata term. Replaying
//!   the state updates ships them as calldata (tens of KB on call-heavy txs,
//!   hundreds of thousands of gas) which no term accounts for.
//! - **[`extract_operation_counts_from_trace`] double-counts**: it counts
//!   SSTORE/LOG at *all* depths and also adds `external_call_gas`, which
//!   already includes the gas of nested SSTOREs/LOGs.
//!   [`estimate_gas_from_state_updates`] does not have this problem (it only
//!   sees depth-1 updates).

use std::collections::HashMap;

use crate::types::StateUpdate;

/// Heuristic gas costs for different operations
pub const BASE_TX_COST: u64 = 21_000;
pub const WARM_SSTORE_COST: u64 = 5_000;
/// EIP-3529: refunds are capped at `gas_used / MAX_REFUND_QUOTIENT`.
pub const MAX_REFUND_QUOTIENT: u64 = 5;
pub const LOG_BASE_COST: u64 = 375;
pub const LOG_TOPIC_COST: u64 = 375;
pub const LOG_DATA_COST_PER_BYTE: u64 = 8;
/// EIP-3860: 2 gas per 32-byte word of initcode
pub const CREATE_BASE_COST: u64 = 32_000;
pub const INITCODE_WORD_COST: u64 = 2;
/// keccak256 word cost used by CREATE2 to hash initcode
pub const KECCAK_WORD_COST: u64 = 6;

/// Operations and gas data extracted from a trace
#[derive(Debug, Default)]
pub struct TraceOperations {
    pub sstore_count: u64,
    pub log_counts: [u64; 5],   // LOG0-LOG4
    pub external_call_gas: u64, // Total gas used by external calls (extracted from trace)
    /// Final EIP-3529 refund counter of the traced execution (pre-cap).
    pub refund_counter: u64,
}

/// Net EIP-3529 refunds out of a gross (pre-refund) gas estimate.
///
/// Struct-log gas deltas are pre-refund while receipt `gasUsed` is
/// post-refund; without this, refund-heavy txs (SSTORE clears) overshoot by up
/// to 20% and their savings clamp to zero. The cap is applied against the
/// gross *estimate* rather than the original tx's `gasUsed` — the two track
/// each other closely since the replay re-executes the same operations.
fn apply_refund(gross: u64, refund_counter: u64) -> u64 {
    gross - refund_counter.min(gross / MAX_REFUND_QUOTIENT)
}

/// Estimate gas from state updates using heuristic costs.
///
/// This provides a rough estimate based on known gas costs for each operation type.
///
/// # Arguments
/// * `state_updates` - The state updates to estimate gas for
/// * `external_call_gas` - Actual gas used by external calls (cannot be optimized)
/// * `refund_counter` - Final EIP-3529 refund counter from the trace (pre-cap);
///   pass 0 when unavailable to reproduce the old (overshooting) behavior
///
/// # Returns
/// Estimated gas cost
pub fn estimate_gas_from_state_updates(
    state_updates: &[StateUpdate],
    external_call_gas: u64,
    refund_counter: u64,
) -> u64 {
    let mut gas = BASE_TX_COST;

    // Add actual gas used by external calls (cannot be optimized)
    gas += external_call_gas;

    for update in state_updates {
        gas += match update {
            StateUpdate::Store(_) => WARM_SSTORE_COST,
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
            StateUpdate::Create(c) => {
                let words = (c.initcode.len() as u64).div_ceil(32);
                CREATE_BASE_COST + words * INITCODE_WORD_COST
            }
            StateUpdate::Create2(c) => {
                let words = (c.initcode.len() as u64).div_ceil(32);
                CREATE_BASE_COST + words * INITCODE_WORD_COST + words * KECCAK_WORD_COST
            }
        };
    }

    apply_refund(gas, refund_counter)
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
    gas += operations.sstore_count * WARM_SSTORE_COST;

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

    apply_refund(gas, operations.refund_counter)
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
    // The refund counter is transaction-global and cumulative; its value at
    // the final step is the tx's total pre-cap refund. Geth omits the field
    // when it is zero.
    let mut operations = TraceOperations {
        refund_counter: trace
            .struct_logs
            .last()
            .and_then(|log| log.refund_counter)
            .unwrap_or(0),
        ..Default::default()
    };

    // Track gas usage for external calls
    // In Geth traces, struct_log.gas is the remaining gas AFTER the opcode executes
    // When we see a CALL at depth 1:
    //   - gas = gas remaining AFTER CALL opcode executes (before sub-call)
    //   - gasCost = cost of CALL opcode itself
    // When depth increases to 2, we've entered the external call
    // When depth decreases back to 1, we've exited the external call
    // Gas used by sub-call = gas_after_call_opcode - gas_after_subcall_returns
    //
    // Important: We track CALL gas at depth 1 (outer calls include inner calls).
    // For DELEGATECALL/CALLCODE: we don't track their gas, but we DO track
    // CALL gas at depth 2 when within a DELEGATECALL context (since those
    // external calls can't be optimized and aren't included in DELEGATECALL gas).
    let mut gas_after_call_opcode: Option<u64> = None;
    let mut previous_depth = 0;
    let mut in_external_call = false;
    // Track what type of call brought us to each depth
    let mut call_type_at_depth: HashMap<u64, &str> = HashMap::new();
    let mut current_call_type: Option<&str> = None;

    for struct_log in &trace.struct_logs {
        let op = &*struct_log.op;
        let depth = struct_log.depth;

        // Track depth changes to detect entering/exiting external calls
        if depth == 1 && previous_depth == 2 {
            // We've exited an external call (depth went from 2 to 1)
            if in_external_call {
                if let Some(gas_after_opcode) = gas_after_call_opcode {
                    let call_type_at_depth_2 = call_type_at_depth.get(&2);

                    // Account gas for:
                    // 1. CALL at depth 1 (outer calls include inner calls, so track total gas)
                    // 2. CALL at depth 2 within DELEGATECALL (these external calls can't be optimized)
                    if call_type_at_depth_2 == Some(&"CALL")
                        || call_type_at_depth_2 == Some(&"DELEGATECALL")
                    {
                        // Gas remaining after the sub-call returns
                        // struct_log.gas is u64 (remaining gas after opcode executes)
                        let gas_after_subcall = struct_log.gas;

                        // Gas used by the sub-call = gas after CALL opcode - gas after sub-call returns
                        let gas_used = gas_after_opcode.saturating_sub(gas_after_subcall);
                        operations.external_call_gas += gas_used;
                    }
                }
                in_external_call = false;
                gas_after_call_opcode = None;
                call_type_at_depth.remove(&2);
                current_call_type = None;
            }
        } else if depth == 2 && previous_depth == 1 {
            // We've entered an external call (depth went from 1 to 2)
            // Record what call type brought us here
            if let Some(call_type) = current_call_type {
                call_type_at_depth.insert(2, call_type);
            }
            in_external_call = true;
        }

        // Track CALL opcodes at depth 1 (external calls from the main contract)
        // These outer calls include inner calls, so we track their total gas
        if op == "CALL" && depth == 1 {
            // Note the gas remaining AFTER the CALL opcode executes (before sub-call)
            // This is the gas available for the sub-call
            // struct_log.gas is u64 (remaining gas after opcode executes)
            gas_after_call_opcode = Some(struct_log.gas);
            current_call_type = Some("CALL");
        }

        // Track DELEGATECALL/CALLCODE at depth 1 (but don't track their gas)
        // We need to know when we're in a DELEGATECALL context to track CALLs within it
        if (op == "DELEGATECALL" || op == "CALLCODE") && depth == 1 {
            current_call_type = Some("DELEGATECALL");
            // Don't set gas_after_call_opcode - we don't track DELEGATECALL gas itself
        }

        // Track CALL at depth 2 only if we're in a DELEGATECALL context
        // Regular CALLs at depth 2 are already included in the outer CALL gas
        if op == "CALL" && depth == 2 && call_type_at_depth.get(&2) == Some(&"DELEGATECALL") {
            // This is a CALL within a DELEGATECALL - track its gas
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
    if in_external_call
        && let Some(gas_after_opcode) = gas_after_call_opcode
        && let Some(last_log) = trace.struct_logs.last()
    {
        let call_type_at_depth_2 = call_type_at_depth.get(&2);
        // Account gas for CALL at depth 1 or CALL at depth 2 within DELEGATECALL
        if call_type_at_depth_2 == Some(&"CALL") || call_type_at_depth_2 == Some(&"DELEGATECALL") {
            // Gas remaining after the sub-call returns
            // struct_log.gas is u64 (remaining gas after opcode executes)
            let gas_after_subcall = last_log.gas;
            let gas_used = gas_after_opcode.saturating_sub(gas_after_subcall);
            operations.external_call_gas += gas_used;
        }
    }

    operations
}

#[cfg(test)]
mod refund_tests {
    use super::*;

    #[test]
    fn refund_below_cap_subtracts_fully() {
        // gross = 21_000 base + 100_000 call gas = 121_000; cap = 24_200
        assert_eq!(
            estimate_gas_from_state_updates(&[], 100_000, 10_000),
            111_000
        );
    }

    #[test]
    fn refund_capped_at_fifth_of_gross() {
        // gross = 121_000; refund 1M clamps to 121_000 / 5 = 24_200
        assert_eq!(
            estimate_gas_from_state_updates(&[], 100_000, 1_000_000),
            96_800
        );
    }

    #[test]
    fn zero_refund_preserves_old_behavior() {
        assert_eq!(estimate_gas_from_state_updates(&[], 100_000, 0), 121_000);
    }

    #[test]
    fn operations_estimate_nets_refund() {
        let ops = TraceOperations {
            sstore_count: 2,
            external_call_gas: 9_000,
            refund_counter: 5_000,
            ..Default::default()
        };
        // gross = 21_000 + 2*5_000 + 9_000 = 40_000; cap = 8_000, so the
        // 5_000 refund applies in full
        assert_eq!(estimate_gas_from_operations(&ops), 35_000);
    }
}

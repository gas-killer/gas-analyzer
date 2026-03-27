//! Shared Geth trace processing functionality.
//!
//! This module provides functions for extracting state updates from
//! Geth-format transaction traces (`DefaultFrame`). Contains only
//! pure computation functions - no async, no I/O, no RPC calls.

use std::collections::{HashMap, HashSet};

use alloy_primitives::Address;
use alloy_rpc_types::trace::geth::{DefaultFrame, StructLog};
use anyhow::{Result, bail};

use crate::types::{IStateUpdateTypes, Opcode, StateUpdate};

// ============================================================================
// Memory Utilities
// ============================================================================

/// Copy memory with bounds checking, zero-padding if needed.
pub fn copy_memory(memory: &[u8], offset: usize, length: usize) -> Vec<u8> {
    if memory.len() >= offset + length {
        memory[offset..offset + length].to_vec()
    } else {
        let mut result = memory.to_vec();
        result.resize(offset + length, 0);
        result[offset..offset + length].to_vec()
    }
}

/// Parse trace memory from Geth format (hex strings) to bytes.
pub fn parse_trace_memory(memory: Vec<String>) -> Vec<u8> {
    memory
        .join("")
        .chars()
        .collect::<Vec<char>>()
        .chunks(2)
        .map(|c| c.iter().collect::<String>())
        .map(|s| u8::from_str_radix(&s, 16).expect("invalid hex"))
        .collect::<Vec<u8>>()
}

// ============================================================================
// State Update Extraction
// ============================================================================

/// Extract a state update from a Geth StructLog entry.
///
/// Returns `Ok(Some(opcode))` if the opcode should be skipped (CREATE, etc.),
/// `Ok(None)` if successfully processed or not a state-changing opcode,
/// or an error if something unexpected happened.
pub fn append_state_update_from_struct_log(
    state_updates: &mut Vec<StateUpdate>,
    struct_log: StructLog,
) -> Result<Option<Opcode>> {
    let mut stack = struct_log.stack.expect("stack is empty");
    stack.reverse();

    let memory = match struct_log.memory {
        Some(memory) => parse_trace_memory(memory),
        None => match struct_log.op.as_ref() {
            "CALL" | "LOG0" | "LOG1" | "LOG2" | "LOG3" | "LOG4" if struct_log.depth == 1 => {
                bail!("There is no memory for {:?} in depth 1", struct_log.op)
            }
            _ => return Ok(None),
        },
    };

    match struct_log.op.as_ref() {
        "CREATE" | "CREATE2" | "SELFDESTRUCT" | "TSTORE" => {
            return Ok(Some(struct_log.op.to_string()));
        }
        "DELEGATECALL" | "CALLCODE" => {
            bail!(
                "Calling opcode {:?}, this shouldn't even happen!",
                struct_log.op
            );
        }
        "SSTORE" => {
            let slot = stack[0];
            let value = stack[1];
            state_updates.push(StateUpdate::Store(IStateUpdateTypes::Store {
                slot: slot.into(),
                value: value.into(),
            }));
        }
        "CALL" => {
            let args_offset: usize = stack[3].try_into().expect("invalid args offset");
            let args_length: usize = stack[4].try_into().expect("invalid args length");
            let args = copy_memory(&memory, args_offset, args_length);
            state_updates.push(StateUpdate::Call(IStateUpdateTypes::Call {
                target: Address::from_word(stack[1].into()),
                value: stack[2],
                callargs: args.into(),
            }));
        }
        "LOG0" => {
            let data_offset: usize = stack[0].try_into().expect("invalid data offset");
            let data_length: usize = stack[1].try_into().expect("invalid data length");
            let data = copy_memory(&memory, data_offset, data_length);
            state_updates.push(StateUpdate::Log0(IStateUpdateTypes::Log0 {
                data: data.into(),
            }));
        }
        "LOG1" => {
            let data_offset: usize = stack[0].try_into().expect("invalid data offset");
            let data_length: usize = stack[1].try_into().expect("invalid data length");
            let data = copy_memory(&memory, data_offset, data_length);
            state_updates.push(StateUpdate::Log1(IStateUpdateTypes::Log1 {
                data: data.into(),
                topic1: stack[2].into(),
            }));
        }
        "LOG2" => {
            let data_offset: usize = stack[0].try_into().expect("invalid data offset");
            let data_length: usize = stack[1].try_into().expect("invalid data length");
            let data = copy_memory(&memory, data_offset, data_length);
            state_updates.push(StateUpdate::Log2(IStateUpdateTypes::Log2 {
                data: data.into(),
                topic1: stack[2].into(),
                topic2: stack[3].into(),
            }));
        }
        "LOG3" => {
            let data_offset: usize = stack[0].try_into().expect("invalid data offset");
            let data_length: usize = stack[1].try_into().expect("invalid data length");
            let data = copy_memory(&memory, data_offset, data_length);
            state_updates.push(StateUpdate::Log3(IStateUpdateTypes::Log3 {
                data: data.into(),
                topic1: stack[2].into(),
                topic2: stack[3].into(),
                topic3: stack[4].into(),
            }));
        }
        "LOG4" => {
            let data_offset: usize = stack[0].try_into().expect("invalid data offset");
            let data_length: usize = stack[1].try_into().expect("invalid data length");
            let data = copy_memory(&memory, data_offset, data_length);
            state_updates.push(StateUpdate::Log4(IStateUpdateTypes::Log4 {
                data: data.into(),
                topic1: stack[2].into(),
                topic2: stack[3].into(),
                topic3: stack[4].into(),
                topic4: stack[5].into(),
            }));
        }
        _ => {}
    }
    Ok(None)
}

// ============================================================================
// Trace Processing
// ============================================================================

/// Compute state updates from a Geth DefaultFrame trace.
///
/// This extracts SSTORE, CALL, and LOG operations from an existing transaction's trace,
/// handling DELEGATECALL and CALLCODE depth tracking correctly.
///
/// Returns: (state_updates, skipped_opcodes, call_gas_total)
/// - `call_gas_total` is the total gas cost of all CALL operations in state_updates
pub fn compute_state_updates(
    trace: DefaultFrame,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>, u64)> {
    let mut state_updates: Vec<StateUpdate> = Vec::new();
    let mut target_depth = 1u64;
    let mut skipped_opcodes = HashSet::new();
    // Stack of (depth, call_index) for CALLs we're inside. Call index is 1-based for display.
    let mut call_stack: Vec<(u64, usize)> = Vec::new();
    // Track what type of call brought us to each depth (for filtering nested CALLs)
    // "CALL" = regular CALL, "DELEGATECALL" = DELEGATECALL/CALLCODE
    let mut call_type_at_depth: HashMap<u64, &str> = HashMap::new();
    // Track gas for each CALL we extract: map from call_index to gas_after_call_opcode
    let mut call_gas_tracking: HashMap<usize, u64> = HashMap::new();
    let mut total_call_gas = 0u64;

    for struct_log in trace.struct_logs {
        let depth = struct_log.depth;
        let op = struct_log.op.as_ref().to_string();

        // Whenever stepping up (leaving a CALL/CALLCODE/DELEGATECALL) reset the target depth
        // and pop call stack for any CALLs we've exited.
        if depth < target_depth {
            while let Some(&(d, idx)) = call_stack.last() {
                if d >= depth {
                    // We're exiting this CALL. Calculate its gas cost.
                    if let Some(gas_after_opcode) = call_gas_tracking.remove(&idx) {
                        // Gas remaining after the CALL returns
                        let gas_after_call = struct_log.gas;
                        // Gas used by the CALL = gas after CALL opcode - gas after CALL returns
                        let gas_used = gas_after_opcode.saturating_sub(gas_after_call);
                        total_call_gas += gas_used;
                    }
                    call_stack.pop();
                } else {
                    break;
                }
            }
            call_type_at_depth.remove(&target_depth);
            target_depth = depth;
        }

        if depth == target_depth {
            if op == "DELEGATECALL" || op == "CALLCODE" {
                target_depth = depth + 1;
                call_type_at_depth.insert(depth + 1, "DELEGATECALL");
            } else if matches!(
                op.as_str(),
                "CALL" | "SSTORE" | "LOG0" | "LOG1" | "LOG2" | "LOG3" | "LOG4"
            ) {
                // Filter out all state-changing operations (CALL, SSTORE, LOG*) that are nested within any CALL
                // (they'll be executed as part of the outer CALL, so we can't optimize them)
                // Keep operations at depth 1 (top-level) and operations directly within DELEGATECALL (not nested within a CALL)
                if !call_stack.is_empty() {
                    // We're nested within a CALL - filter it out
                    // Nested operations will be executed as part of the parent CALL, so we can't optimize them separately.
                    continue;
                }

                // Now add the state update (if not filtered)
                // Read gas before moving struct_log
                let gas_after_opcode = struct_log.gas;
                if let Some(skipped) =
                    append_state_update_from_struct_log(&mut state_updates, struct_log)?
                {
                    skipped_opcodes.insert(skipped);
                } else {
                    // We added a state update.
                    if op == "CALL" {
                        let call_index_1based = state_updates.len();
                        call_stack.push((depth, call_index_1based));
                        // Track the gas remaining after the CALL opcode executes
                        // This will be used to calculate gas used when the CALL exits
                        call_gas_tracking.insert(call_index_1based, gas_after_opcode);
                        // Increase target_depth to track when we exit this CALL
                        // This allows us to detect when the CALL returns and pop from call_stack
                        target_depth = depth + 1;
                    }
                }
            }
        }
    }

    // Panic if there are any remaining CALLs that didn't exit (shouldn't happen)
    if !call_gas_tracking.is_empty() {
        let call_indices: Vec<_> = call_gas_tracking.keys().copied().collect();
        panic!(
            "Found {} remaining CALL(s) that didn't exit properly. Call indices: {:?}",
            call_gas_tracking.len(),
            call_indices
        );
    }

    Ok((state_updates, skipped_opcodes, total_call_gas))
}

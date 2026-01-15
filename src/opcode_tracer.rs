//! Opcode Tracer Integration Module
//!
//! This module integrates the opcode-tracer crate from sp1-contract-call
//! and provides utilities to compare state updates from both the Geth trace
//! approach and the opcode tracer approach.

use alloy::primitives::Address;
use anyhow::{Result, bail};
use std::collections::HashSet;

use crate::sol_types::{IStateUpdateTypes, StateUpdate};
use crate::structs::Opcode;

// Re-export the opcode-tracer crate types for external use
pub use opcode_tracer::{
    CallTraceArena, CallTraceNode, CallTraceStep, OpcodeExecution, StorageChange, TraceConfig,
    TraceResult, trace_call, trace_function,
};

/// Copy memory with bounds checking.
fn copy_memory(memory: &[u8], offset: usize, length: usize) -> Vec<u8> {
    if memory.len() >= offset + length {
        memory[offset..offset + length].to_vec()
    } else {
        let mut result = memory.to_vec();
        result.resize(offset + length, 0);
        result[offset..offset + length].to_vec()
    }
}

// ============================================================================
// EvmSketch-based state update extraction (uses sp1-cc CallTraceArena)
// ============================================================================

/// Compute state updates from a sp1-cc CallTraceArena.
///
/// This processes the trace in the same way as `compute_state_updates`,
/// extracting SSTORE, CALL, and LOG operations as state updates.
///
/// Returns: (state_updates, skipped_opcodes, external_call_gas)
/// - external_call_gas: Total gas used by external CALLs (to be added to heuristic estimate)
pub fn compute_state_updates_from_call_trace(
    trace: &sp1_cc_client_executor::CallTraceArena,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>, u64)> {
    let mut state_updates: Vec<StateUpdate> = Vec::new();
    let mut target_depth: usize = 1;
    let mut skipped_opcodes = HashSet::new();
    let mut external_call_gas: u64 = 0;

    // Collect all steps with their depths from all call frames
    // We need to process in execution order across all nodes
    let nodes = trace.nodes();

    for node in nodes {
        // Call depth: 0-indexed in trace, we use 1-indexed (1 = top level)
        let depth = node.trace.depth as usize + 1;

        // Track gas used by external calls (depth 1 = direct calls from the entry point)
        // These are calls we can't optimize, so we add their gas to the estimate
        if depth == 2 {
            // depth 2 in our 1-indexed system = direct external calls from depth 1
            external_call_gas += node.trace.gas_used;
        }

        for step in &node.trace.steps {
            // Whenever stepping up (leaving a CALL/CALLCODE/DELEGATECALL) reset the target depth
            if depth < target_depth {
                target_depth = depth;
            } else if depth == target_depth {
                let op_name = step.op.as_str();

                // If we're going to step into a new execution context, increase the target depth
                if op_name == "DELEGATECALL" || op_name == "CALLCODE" {
                    target_depth = depth + 1;
                } else if let Some(opcode) =
                    append_state_update_from_call_trace_step(&mut state_updates, step, depth)?
                {
                    skipped_opcodes.insert(opcode);
                }
            }
        }
    }

    Ok((state_updates, skipped_opcodes, external_call_gas))
}

/// Append a state update from a CallTraceStep.
fn append_state_update_from_call_trace_step(
    state_updates: &mut Vec<StateUpdate>,
    step: &sp1_cc_client_executor::CallTraceStep,
    depth: usize,
) -> Result<Option<Opcode>> {
    let op_name = step.op.as_str();

    match op_name {
        "CREATE" | "CREATE2" | "SELFDESTRUCT" => {
            return Ok(Some(op_name.to_string()));
        }
        "DELEGATECALL" | "CALLCODE" => {
            bail!("Calling opcode {:?}, this shouldn't even happen!", op_name);
        }
        _ => {}
    }

    // Extract stack (reversed so index 0 is top of stack)
    let stack: Vec<alloy::primitives::U256> = step
        .stack
        .as_ref()
        .map(|s| s.iter().rev().copied().collect())
        .unwrap_or_default();

    // Extract memory
    let memory: Vec<u8> = step
        .memory
        .as_ref()
        .map(|m| m.as_bytes().to_vec())
        .unwrap_or_default();

    match op_name {
        "SSTORE" => {
            if stack.len() >= 2 {
                state_updates.push(StateUpdate::Store(IStateUpdateTypes::Store {
                    slot: stack[0].into(),
                    value: stack[1].into(),
                }));
            }
        }
        "CALL" => {
            if stack.len() >= 5 && depth == 1 {
                let args_offset: usize = stack[3].try_into().unwrap_or(0);
                let args_length: usize = stack[4].try_into().unwrap_or(0);
                let args = copy_memory(&memory, args_offset, args_length);
                state_updates.push(StateUpdate::Call(IStateUpdateTypes::Call {
                    target: Address::from_word(stack[1].into()),
                    value: stack[2],
                    callargs: args.into(),
                }));
            }
        }
        "LOG0" => {
            if stack.len() >= 2 && depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log0(IStateUpdateTypes::Log0 {
                    data: data.into(),
                }));
            }
        }
        "LOG1" => {
            if stack.len() >= 3 && depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log1(IStateUpdateTypes::Log1 {
                    data: data.into(),
                    topic1: stack[2].into(),
                }));
            }
        }
        "LOG2" => {
            if stack.len() >= 4 && depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log2(IStateUpdateTypes::Log2 {
                    data: data.into(),
                    topic1: stack[2].into(),
                    topic2: stack[3].into(),
                }));
            }
        }
        "LOG3" => {
            if stack.len() >= 5 && depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log3(IStateUpdateTypes::Log3 {
                    data: data.into(),
                    topic1: stack[2].into(),
                    topic2: stack[3].into(),
                    topic3: stack[4].into(),
                }));
            }
        }
        "LOG4" => {
            if stack.len() >= 6 && depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log4(IStateUpdateTypes::Log4 {
                    data: data.into(),
                    topic1: stack[2].into(),
                    topic2: stack[3].into(),
                    topic3: stack[4].into(),
                    topic4: stack[5].into(),
                }));
            }
        }
        _ => {}
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_module_compiles() {
        // This test verifies the module compiles correctly.
        // More comprehensive tests require mocking CallTraceArena.
    }
}

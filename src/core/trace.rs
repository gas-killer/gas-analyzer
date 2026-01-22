//! Shared Geth trace processing functionality.
//!
//! This module provides functions for extracting state updates from
//! Geth-format transaction traces (`DefaultFrame`). Used by both
//! Anvil and EvmSketch implementations.

use std::collections::HashSet;

use alloy::primitives::{Address, FixedBytes};
use alloy::rpc::types::trace::geth::{
    DefaultFrame, GethDebugTracingOptions, GethDefaultTracingOptions, GethTrace, StructLog,
};
use alloy_provider::Provider;
use alloy_provider::ext::DebugApi;
use anyhow::{Result, anyhow, bail};

use super::types::{IStateUpdateTypes, Opcode, StateUpdate};

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
        "CREATE" | "CREATE2" | "SELFDESTRUCT" => {
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
/// Returns: (state_updates, skipped_opcodes)
pub fn compute_state_updates(trace: DefaultFrame) -> Result<(Vec<StateUpdate>, HashSet<Opcode>)> {
    let mut state_updates: Vec<StateUpdate> = Vec::new();
    let mut target_depth = 1;
    let mut skipped_opcodes = HashSet::new();

    for struct_log in trace.struct_logs {
        // Whenever stepping up (leaving a CALL/CALLCODE/DELEGATECALL) reset the target depth
        if struct_log.depth < target_depth {
            target_depth = struct_log.depth;
        } else if struct_log.depth == target_depth {
            // If we're going to step into a new execution context, increase the target depth
            // else, try to add the state update
            if &*struct_log.op == "DELEGATECALL" || &*struct_log.op == "CALLCODE" {
                target_depth = struct_log.depth + 1;
            } else if let Some(opcode) =
                append_state_update_from_struct_log(&mut state_updates, struct_log)?
            {
                skipped_opcodes.insert(opcode);
            }
        }
    }
    Ok((state_updates, skipped_opcodes))
}

// ============================================================================
// RPC Trace Fetching
// ============================================================================

/// Get transaction trace from a provider using debug_traceTransaction.
///
/// This fetches the actual historical trace from an already-executed transaction,
/// ensuring we get the exact values that were stored during the original execution.
pub async fn get_tx_trace<P: Provider + DebugApi>(
    provider: &P,
    tx_hash: FixedBytes<32>,
) -> Result<DefaultFrame> {
    let tx_receipt = provider
        .get_transaction_receipt(tx_hash)
        .await?
        .ok_or_else(|| anyhow!("could not get receipt for tx {}", tx_hash))?;

    if !tx_receipt.status() {
        bail!("transaction failed");
    }

    let options = GethDebugTracingOptions {
        config: GethDefaultTracingOptions {
            enable_memory: Some(true),
            ..Default::default()
        },
        ..Default::default()
    };

    let GethTrace::Default(trace) = provider.debug_trace_transaction(tx_hash, options).await?
    else {
        return Err(anyhow!("Expected default trace"));
    };
    Ok(trace)
}

/// Compute state updates from an existing transaction using its actual trace.
///
/// This is a convenience function that combines `get_tx_trace` and `compute_state_updates`.
///
/// Returns: (state_updates, skipped_opcodes)
pub async fn compute_state_updates_from_tx<P: Provider + DebugApi>(
    provider: &P,
    tx_hash: FixedBytes<32>,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>)> {
    let trace = get_tx_trace(provider, tx_hash).await?;
    compute_state_updates(trace)
}

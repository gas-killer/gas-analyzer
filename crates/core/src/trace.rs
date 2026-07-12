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
    let end = offset.saturating_add(length);
    if memory.len() >= end {
        memory[offset..end].to_vec()
    } else {
        let mut result = vec![0u8; length];
        if offset < memory.len() {
            let copy_len = (memory.len() - offset).min(length);
            result[..copy_len].copy_from_slice(&memory[offset..offset + copy_len]);
        }
        result
    }
}

/// Parse trace memory from Geth format (hex strings) to bytes.
///
/// Accepts entries with or without an `0x` prefix — Anvil began emitting prefixed
/// memory words after revm-inspectors v0.38.1 (Foundry v1.7.0), while geth/erigon
/// and older Anvil emit bare hex.
pub fn parse_trace_memory(memory: Vec<String>) -> Vec<u8> {
    let total_bytes: usize = memory
        .iter()
        .map(|s| s.strip_prefix("0x").unwrap_or(s).len() / 2)
        .sum();
    let mut result = Vec::with_capacity(total_bytes);
    for s in &memory {
        let s = s.strip_prefix("0x").unwrap_or(s);
        let start = result.len();
        result.resize(start + s.len() / 2, 0);
        hex::decode_to_slice(s, &mut result[start..]).expect("invalid hex");
    }
    result
}

// ============================================================================
// State Update Extraction
// ============================================================================

/// Extract a state update from a Geth StructLog entry.
///
/// Returns `Ok(Some(opcode))` if the opcode is unsupported and was skipped (SELFDESTRUCT, TSTORE),
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
        "SELFDESTRUCT" | "TSTORE" => {
            return Ok(Some(struct_log.op.to_string()));
        }
        "CREATE" => {
            // Stack: [value, offset, size] (top = index 0 after reverse)
            let value = stack[0];
            let offset: usize = stack[1].try_into().expect("invalid CREATE offset");
            let length: usize = stack[2].try_into().expect("invalid CREATE length");
            let initcode = copy_memory(&memory, offset, length);
            state_updates.push(StateUpdate::Create(IStateUpdateTypes::Create {
                value,
                initcode: initcode.into(),
            }));
        }
        "CREATE2" => {
            // Stack: [value, offset, size, salt] (top = index 0 after reverse)
            let value = stack[0];
            let offset: usize = stack[1].try_into().expect("invalid CREATE2 offset");
            let length: usize = stack[2].try_into().expect("invalid CREATE2 length");
            let salt = stack[3];
            let initcode = copy_memory(&memory, offset, length);
            state_updates.push(StateUpdate::Create2(IStateUpdateTypes::Create2 {
                salt: salt.into(),
                value,
                initcode: initcode.into(),
            }));
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
/// Everything extracted from a struct-log trace in a single pass.
#[derive(Debug, Default)]
pub struct TraceExtract {
    pub state_updates: Vec<StateUpdate>,
    pub skipped_opcodes: HashSet<Opcode>,
    /// Total gas consumed by the extracted depth-1 CALLs (gross, pre-refund).
    pub call_gas_total: u64,
    /// Final (pre-cap) EIP-3529 refund counter of the traced execution, for
    /// netting refunds out of heuristic estimates.
    pub refund_counter: u64,
}

/// Compute state updates from a Geth DefaultFrame trace (see [`TraceExtract`]).
#[tracing::instrument(name = "gas.trace_parse", skip_all, fields(state_update_count = tracing::field::Empty))]
pub fn compute_state_updates(trace: DefaultFrame) -> Result<TraceExtract> {
    // Transaction-global cumulative counter; the final step holds the tx
    // total. Geth omits the field when it is zero.
    let refund_counter = trace
        .struct_logs
        .last()
        .and_then(|log| log.refund_counter)
        .unwrap_or(0);
    let mut state_updates: Vec<StateUpdate> = Vec::with_capacity(trace.struct_logs.len() / 4);
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
                "CALL"
                    | "SSTORE"
                    | "LOG0"
                    | "LOG1"
                    | "LOG2"
                    | "LOG3"
                    | "LOG4"
                    | "CREATE"
                    | "CREATE2"
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

    tracing::Span::current().record("state_update_count", state_updates.len());
    Ok(TraceExtract {
        state_updates,
        skipped_opcodes,
        call_gas_total: total_call_gas,
        refund_counter,
    })
}

#[cfg(test)]
mod tests {
    use alloy_primitives::U256;

    use super::*;

    fn make_struct_log(op: &str, stack: Vec<U256>, memory_words: Vec<&str>) -> StructLog {
        StructLog {
            pc: 0,
            op: op.to_string().into(),
            gas: 100_000,
            gas_cost: 0,
            depth: 1,
            error: None,
            stack: Some(stack),
            return_data: None,
            memory: Some(memory_words.iter().map(|s| s.to_string()).collect()),
            memory_size: None,
            storage: None,
            refund_counter: None,
        }
    }

    #[test]
    fn create_extracts_initcode_from_memory() {
        // Initcode: 0x6080604052 (5 bytes) placed at memory offset 0.
        // Memory word: 5 bytes + 27 zero bytes = 32 bytes total.
        let memory_word = "6080604052000000000000000000000000000000000000000000000000000000";

        // Stack before CREATE (bottom→top): size=5, offset=0, value=0x3e8
        // After stack.reverse(): stack[0]=value=0x3e8, stack[1]=offset=0, stack[2]=size=5
        let stack = vec![
            U256::from(5u64),     // size (bottom, becomes stack[2] after reverse)
            U256::from(0u64),     // offset (becomes stack[1])
            U256::from(1_000u64), // value (top, becomes stack[0])
        ];

        let mut updates = Vec::new();
        let result = append_state_update_from_struct_log(
            &mut updates,
            make_struct_log("CREATE", stack, vec![memory_word]),
        );

        assert!(result.is_ok());
        assert!(result.unwrap().is_none(), "CREATE should not be skipped");
        assert_eq!(updates.len(), 1);

        let StateUpdate::Create(c) = &updates[0] else {
            panic!("expected Create, got {:?}", updates[0]);
        };
        assert_eq!(&c.initcode[..], &[0x60, 0x80, 0x60, 0x40, 0x52]);
        assert_eq!(c.value, U256::from(1_000u64), "endowment value extracted");
    }

    #[test]
    fn create2_extracts_salt_and_initcode_from_memory() {
        let memory_word = "6080604052000000000000000000000000000000000000000000000000000000";

        // Stack before CREATE2 (bottom→top): salt, size=5, offset=0, value=0x3e8
        // After stack.reverse(): stack[0]=value, stack[1]=offset=0, stack[2]=size=5, stack[3]=salt
        let salt_val = U256::from(0xdeadbeef_u64);
        let stack = vec![
            salt_val,             // salt (bottom, becomes stack[3] after reverse)
            U256::from(5u64),     // size (becomes stack[2])
            U256::from(0u64),     // offset (becomes stack[1])
            U256::from(1_000u64), // value (top, becomes stack[0])
        ];

        let mut updates = Vec::new();
        let result = append_state_update_from_struct_log(
            &mut updates,
            make_struct_log("CREATE2", stack, vec![memory_word]),
        );

        assert!(result.is_ok());
        assert!(result.unwrap().is_none(), "CREATE2 should not be skipped");
        assert_eq!(updates.len(), 1);

        let StateUpdate::Create2(c) = &updates[0] else {
            panic!("expected Create2, got {:?}", updates[0]);
        };
        assert_eq!(&c.initcode[..], &[0x60, 0x80, 0x60, 0x40, 0x52]);
        assert_eq!(c.value, U256::from(1_000u64), "endowment value extracted");
        assert_eq!(c.salt, alloy_primitives::B256::from(salt_val));
    }

    #[test]
    fn compute_state_updates_surfaces_final_refund_counter() {
        // The counter is cumulative; only the final step's value matters.
        let mut mid = make_struct_log("PUSH1", vec![], vec![]);
        mid.refund_counter = Some(4_800);
        let mut last = make_struct_log("STOP", vec![], vec![]);
        last.refund_counter = Some(40_000);

        let trace = DefaultFrame {
            gas: 0,
            failed: false,
            return_value: Default::default(),
            struct_logs: vec![mid, last],
        };
        let extract = compute_state_updates(trace).unwrap();
        assert!(extract.state_updates.is_empty());
        assert_eq!(extract.refund_counter, 40_000);
    }

    #[test]
    fn missing_refund_counter_reads_as_zero() {
        // Geth omits the field when the counter is zero.
        let trace = DefaultFrame {
            gas: 0,
            failed: false,
            return_value: Default::default(),
            struct_logs: vec![make_struct_log("STOP", vec![], vec![])],
        };
        let extract = compute_state_updates(trace).unwrap();
        assert_eq!(extract.refund_counter, 0);
    }

    #[test]
    fn parse_trace_memory_handles_both_prefixed_and_bare_hex() {
        let bare = vec![
            "00000000000000000000000000000000000000000000000000000000000000ff".to_string(),
            "1100000000000000000000000000000000000000000000000000000000000000".to_string(),
        ];
        let prefixed = vec![
            "0x00000000000000000000000000000000000000000000000000000000000000ff".to_string(),
            "0x1100000000000000000000000000000000000000000000000000000000000000".to_string(),
        ];
        let mixed = vec![
            "0x00000000000000000000000000000000000000000000000000000000000000ff".to_string(),
            "1100000000000000000000000000000000000000000000000000000000000000".to_string(),
        ];

        let expected = parse_trace_memory(bare);
        assert_eq!(expected.len(), 64);
        assert_eq!(expected[31], 0xff);
        assert_eq!(expected[32], 0x11);

        assert_eq!(parse_trace_memory(prefixed), expected);
        assert_eq!(parse_trace_memory(mixed), expected);
    }
}

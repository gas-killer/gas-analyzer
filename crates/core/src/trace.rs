//! Shared Geth trace processing functionality.
//!
//! This module provides functions for extracting state updates from
//! Geth-format transaction traces (`DefaultFrame`). Contains only
//! pure computation functions - no async, no I/O, no RPC calls.

use std::collections::{BTreeMap, HashMap, HashSet};

use alloy_primitives::{Address, B256};
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
/// Returns: (state_updates, skipped_opcodes, call_gas_total)
/// - `call_gas_total` is the total gas cost of all CALL operations in state_updates
#[tracing::instrument(name = "gas.trace_parse", skip_all, fields(state_update_count = tracing::field::Empty))]
pub fn compute_state_updates(
    trace: DefaultFrame,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>, u64)> {
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
    Ok((state_updates, skipped_opcodes, total_call_gas))
}

// ============================================================================
// Canonical-checkpoint extraction
// ============================================================================

/// An execution frame observed while walking the trace.
struct CanonFrame {
    /// Depth of the code executing *inside* this frame (root = 1).
    depth: u64,
    /// Whose storage `SSTORE`s in this frame write to. `None` for frames whose
    /// storage can never be the target (CREATE initcode — the target already
    /// has code, so a fresh deployment cannot alias it).
    storage_ctx: Option<Address>,
    /// Whether state-changing ops in this frame are emitted as updates. True for
    /// the root frame and for DELEGATECALL/CALLCODE frames entered from an
    /// emitting frame (their writes hit the target's storage directly); false
    /// inside any CALL/STATICCALL/CREATE frame — those replay natively on-chain
    /// as part of the emitted parent update.
    emitting: bool,
    /// Writes to the *target's* storage made inside this frame (and merged from
    /// successfully-completed child frames). Committed into the parent only if
    /// this frame completes successfully; discarded on revert — exactly
    /// mirroring EVM journaling.
    journal: BTreeMap<B256, B256>,
    /// Emitted updates buffered in this frame, in order. Appended to the parent
    /// on success, dropped on revert (so a reverted emitting frame's writes/logs
    /// never reach the signed program). The root frame's buffer *is* the program.
    out: Vec<StateUpdate>,
    /// Replay-side view of the target's storage as of this frame's emissions —
    /// only meaningful in emitting frames. Used to suppress re-emitting a slot
    /// whose canonical value a prior slice already set (same-value writes).
    emitted_view: BTreeMap<B256, B256>,
    /// For frames entered via an *emitted* CALL update: the gas remaining right
    /// after the CALL opcode, used to account the call's gas on exit.
    emitted_call_gas: Option<u64>,
}

/// A frame-creating opcode was just executed; whether a frame actually opens is
/// only known from the next log's depth (calls to EOAs/precompiles run inline).
struct PendingFrame {
    parent_depth: u64,
    storage_ctx: Option<Address>,
    emitting: bool,
    /// Snapshot of the parent's replay-side view, inherited by an emitting child.
    emitted_view: BTreeMap<B256, B256>,
    emitted_call_gas: Option<u64>,
}

/// The target's canonical storage image visible right now = every open frame's
/// journal merged bottom-up (deeper frames override shallower ones). At an
/// emitting boundary every open frame is emitting and target-scoped, so this is
/// exactly what native execution would have in the target's storage.
fn canonical_visible_image(frames: &[CanonFrame]) -> BTreeMap<B256, B256> {
    let mut image = BTreeMap::new();
    for f in frames {
        for (slot, value) in &f.journal {
            image.insert(*slot, *value);
        }
    }
    image
}

/// Compute state updates with **canonical state checkpointing**.
///
/// Same extraction as [`compute_state_updates`] for `CALL`/`LOG*`/`CREATE*`
/// updates, but instead of replaying the target contract's `SSTORE` journal
/// verbatim, the target's storage image is tracked across *all* frames
/// (revert-aware, exactly like EVM journaling) and emitted as **state slices**:
///
/// - immediately before every emitted `CALL`/`CREATE`/`CREATE2` update — so the
///   external code that runs during that update (including re-entrant calls
///   back into the target) observes **exactly the storage the target had at
///   that moment in native execution**, and
/// - once at the end — so the signed update program fully determines the
///   target's final storage, even if a re-entrant call's on-chain replay were
///   to diverge from the simulation (the final slice re-asserts the canonical
///   value of every touched slot; when replay matched, those are cheap
///   same-value writes).
///
/// Within a slice, slots are emitted in ascending order and consecutive writes
/// to the same slot between two boundaries collapse to the boundary value —
/// intermediate values are unobservable on-chain (no external code runs between
/// boundaries), so this is both cheaper and byte-deterministic across
/// independent provers.
///
/// Writes made *inside* emitted CALL frames (e.g. by a re-entrant call back
/// into the target) are never emitted at their journal position — they replay
/// natively on-chain during that CALL — but they *do* enter the canonical
/// image, so the next slice (or the final one) re-asserts them.
///
/// Returns the same tuple as [`compute_state_updates`].
#[tracing::instrument(name = "gas.trace_parse_canonical", skip_all, fields(state_update_count = tracing::field::Empty))]
pub fn compute_state_updates_canonical(
    trace: DefaultFrame,
    target: Address,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>, u64)> {
    let mut skipped_opcodes = HashSet::new();
    let mut total_call_gas = 0u64;

    let mut frames: Vec<CanonFrame> = vec![CanonFrame {
        depth: 1,
        storage_ctx: Some(target),
        emitting: true,
        journal: BTreeMap::new(),
        out: Vec::new(),
        emitted_view: BTreeMap::new(),
        emitted_call_gas: None,
    }];
    let mut pending: Option<PendingFrame> = None;

    // Emit a canonical state slice into the current (emitting) frame: for every
    // slot in the target's currently-visible image whose value differs from what
    // this frame has already emitted, push a Store and advance the emitted view.
    // Must be called only when the current frame is emitting.
    fn flush_slice(frames: &mut [CanonFrame]) {
        let visible = canonical_visible_image(frames);
        let cur = frames.last_mut().expect("root frame must remain");
        for (slot, value) in visible {
            if cur.emitted_view.get(&slot) != Some(&value) {
                cur.out
                    .push(StateUpdate::Store(IStateUpdateTypes::Store { slot, value }));
                cur.emitted_view.insert(slot, value);
            }
        }
    }

    for struct_log in trace.struct_logs {
        let depth = struct_log.depth;
        let op = struct_log.op.as_ref().to_string();

        // Resolve a pending frame from the previous log's frame-creating op.
        if let Some(p) = pending.take() {
            if depth == p.parent_depth + 1 {
                frames.push(CanonFrame {
                    depth,
                    storage_ctx: p.storage_ctx,
                    emitting: p.emitting,
                    journal: BTreeMap::new(),
                    out: Vec::new(),
                    emitted_view: p.emitted_view,
                    emitted_call_gas: p.emitted_call_gas,
                });
            } else {
                // No frame was entered (EOA / precompile / failed call): the
                // call completed inline. Account its gas now if it was emitted.
                if let Some(gas_after_opcode) = p.emitted_call_gas {
                    total_call_gas += gas_after_opcode.saturating_sub(struct_log.gas);
                }
            }
        }

        // Pop frames we have stepped out of. The resume log (this one) carries
        // the child's success flag on top of the parent's stack.
        while frames.last().map(|f| f.depth).unwrap_or(1) > depth {
            let frame = frames.pop().expect("frame stack underflow");
            if frame.depth != depth + 1 {
                bail!(
                    "trace depth jumped from {} to {} without resume logs — cannot attribute frame outcomes",
                    frame.depth,
                    depth
                );
            }
            let stack = struct_log
                .stack
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("resume log at depth {depth} has no stack"))?;
            let success = stack
                .last()
                .map(|v| !v.is_zero())
                .ok_or_else(|| anyhow::anyhow!("resume log at depth {depth} has empty stack"))?;
            if let Some(gas_after_opcode) = frame.emitted_call_gas {
                total_call_gas += gas_after_opcode.saturating_sub(struct_log.gas);
            }
            if success {
                let child_emitting = frame.emitting;
                let child_view = frame.emitted_view;
                let parent = frames.last_mut().expect("root frame must remain");
                // Commit the child's target writes and buffered emissions.
                for (slot, value) in frame.journal {
                    parent.journal.insert(slot, value);
                }
                parent.out.extend(frame.out);
                // An emitting child ran while the parent was suspended, so its
                // emitted view is a superset of the parent's — adopt it.
                if child_emitting {
                    parent.emitted_view = child_view;
                }
            }
            // On failure everything (journal, out, view) is dropped — the EVM
            // rolled the whole sub-frame back.
        }

        let idx = frames.len() - 1;
        let cur_emitting = frames[idx].emitting;
        let cur_ctx = frames[idx].storage_ctx;
        let op_errored = struct_log.error.is_some();

        match op.as_str() {
            "SSTORE" => {
                if cur_ctx == Some(target) && !op_errored {
                    let mut stack = struct_log
                        .stack
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("SSTORE log has no stack"))?;
                    stack.reverse();
                    let slot: B256 = stack[0].into();
                    let value: B256 = stack[1].into();
                    frames[idx].journal.insert(slot, value);
                }
            }
            "TSTORE" | "SELFDESTRUCT" => {
                if cur_emitting {
                    skipped_opcodes.insert(op.clone());
                }
            }
            "DELEGATECALL" | "CALLCODE" => {
                if !op_errored {
                    // Runs with the target's storage: inherits ctx and emitting.
                    let emitted_view = if cur_emitting {
                        frames[idx].emitted_view.clone()
                    } else {
                        BTreeMap::new()
                    };
                    pending = Some(PendingFrame {
                        parent_depth: depth,
                        storage_ctx: cur_ctx,
                        emitting: cur_emitting,
                        emitted_view,
                        emitted_call_gas: None,
                    });
                }
            }
            "STATICCALL" => {
                if !op_errored {
                    // Read-only: never writes, never emitted. Track only for depth.
                    let stack = struct_log
                        .stack
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("STATICCALL log has no stack"))?;
                    let callee = stack
                        .get(stack.len().wrapping_sub(2))
                        .map(|v| Address::from_word((*v).into()));
                    pending = Some(PendingFrame {
                        parent_depth: depth,
                        storage_ctx: callee,
                        emitting: false,
                        emitted_view: BTreeMap::new(),
                        emitted_call_gas: None,
                    });
                }
            }
            "CALL" => {
                let stack = struct_log
                    .stack
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("CALL log has no stack"))?;
                let callee = stack
                    .get(stack.len().wrapping_sub(2))
                    .map(|v| Address::from_word((*v).into()));
                let gas_after_opcode = struct_log.gas;
                let mut emitted_call_gas = None;
                if cur_emitting && !op_errored {
                    // Boundary: external code is about to observe the target's
                    // storage — bring it to the canonical image first, then emit
                    // the CALL so on-chain the same external code runs against it.
                    flush_slice(&mut frames);
                    if append_state_update_from_struct_log(&mut frames[idx].out, struct_log)?
                        .is_some()
                    {
                        unreachable!("CALL is never a skipped opcode");
                    }
                    emitted_call_gas = Some(gas_after_opcode);
                }
                if !op_errored {
                    pending = Some(PendingFrame {
                        parent_depth: depth,
                        storage_ctx: callee,
                        emitting: false,
                        emitted_view: BTreeMap::new(),
                        emitted_call_gas,
                    });
                }
            }
            "CREATE" | "CREATE2" => {
                if cur_emitting && !op_errored {
                    // Initcode can call back into the target: boundary here too.
                    flush_slice(&mut frames);
                    if append_state_update_from_struct_log(&mut frames[idx].out, struct_log)?
                        .is_some()
                    {
                        unreachable!("CREATE/CREATE2 are never skipped opcodes");
                    }
                }
                if !op_errored {
                    pending = Some(PendingFrame {
                        parent_depth: depth,
                        // A fresh deployment can never alias the target (the
                        // target already has code), so its writes are never ours.
                        storage_ctx: None,
                        emitting: false,
                        emitted_view: BTreeMap::new(),
                        emitted_call_gas: None,
                    });
                }
            }
            "LOG0" | "LOG1" | "LOG2" | "LOG3" | "LOG4" if cur_emitting && !op_errored => {
                let skipped =
                    append_state_update_from_struct_log(&mut frames[idx].out, struct_log)?;
                debug_assert!(skipped.is_none(), "LOG* is never a skipped opcode");
            }
            _ => {}
        }
    }

    if frames.len() != 1 {
        bail!(
            "trace ended with {} unclosed frame(s) — malformed trace",
            frames.len() - 1
        );
    }

    // Final slice: the signed program must fully determine the target's end
    // state, even if a re-entrant call's on-chain replay diverged.
    flush_slice(&mut frames);

    let root = frames.pop().expect("root frame present");
    tracing::Span::current().record("state_update_count", root.out.len());
    Ok((root.out, skipped_opcodes, total_call_gas))
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

    // ========================================================================
    // Canonical-checkpoint encoder
    // ========================================================================

    const TARGET_ADDR: Address = Address::new([0x77; 20]);

    fn addr_word(a: Address) -> U256 {
        U256::from_be_slice(a.as_slice())
    }

    fn slot(n: u64) -> B256 {
        B256::from(U256::from(n))
    }

    fn val(n: u64) -> B256 {
        B256::from(U256::from(n))
    }

    /// Build a StructLog. `top_first` is the stack with the TOP element first
    /// (how you'd read the opcode's args); it is stored bottom-to-top as geth
    /// emits it. `mem` is memory words (32-byte hex, no 0x). `error` marks the
    /// op as reverted-in-place.
    fn log(
        op: &str,
        depth: u64,
        gas: u64,
        top_first: &[U256],
        mem: &[&str],
        error: Option<&str>,
    ) -> StructLog {
        let mut stack: Vec<U256> = top_first.to_vec();
        stack.reverse(); // store bottom-to-top
        StructLog {
            pc: 0,
            op: op.to_string().into(),
            gas,
            gas_cost: 0,
            depth,
            error: error.map(|e| e.to_string()),
            stack: Some(stack),
            return_data: None,
            memory: Some(mem.iter().map(|s| s.to_string()).collect()),
            memory_size: None,
            storage: None,
            refund_counter: None,
        }
    }

    fn sstore(depth: u64, s: B256, v: B256) -> StructLog {
        log("SSTORE", depth, 100_000, &[s.into(), v.into()], &[], None)
    }

    /// A CALL with empty callargs (argsOffset=argsLength=0).
    fn call(depth: u64, gas: u64, callee: Address) -> StructLog {
        log(
            "CALL",
            depth,
            gas,
            &[
                U256::from(gas), // gas
                addr_word(callee),
                U256::ZERO, // value
                U256::ZERO, // argsOffset
                U256::ZERO, // argsLength
                U256::ZERO, // retOffset
                U256::ZERO, // retLength
            ],
            &[],
            None,
        )
    }

    fn delegatecall(depth: u64, gas: u64, callee: Address, error: Option<&str>) -> StructLog {
        log(
            "DELEGATECALL",
            depth,
            gas,
            &[
                U256::from(gas),
                addr_word(callee),
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
            ],
            &[],
            error,
        )
    }

    /// A resume log at `depth`: the caller's next step after a sub-call returned,
    /// carrying the sub-call's success flag (1/0) on top of the stack.
    fn resume(depth: u64, gas: u64, success: bool) -> StructLog {
        log(
            "JUMPDEST",
            depth,
            gas,
            &[U256::from(success as u64)],
            &[],
            None,
        )
    }

    fn stores(updates: &[StateUpdate]) -> Vec<(B256, B256)> {
        updates
            .iter()
            .filter_map(|u| match u {
                StateUpdate::Store(s) => Some((s.slot, s.value)),
                _ => None,
            })
            .collect()
    }

    fn run(logs: Vec<StructLog>) -> Vec<StateUpdate> {
        let trace = DefaultFrame {
            failed: false,
            gas: 0,
            return_value: Default::default(),
            struct_logs: logs,
        };
        compute_state_updates_canonical(trace, TARGET_ADDR)
            .expect("canonical extraction")
            .0
    }

    #[test]
    fn canonical_final_slice_sorts_and_collapses() {
        // A=1, A=2, B=3 with no external calls → one final slice, sorted, A collapsed.
        let updates = run(vec![
            sstore(1, slot(0xAA), val(1)),
            sstore(1, slot(0xAA), val(2)),
            sstore(1, slot(0xBB), val(3)),
        ]);
        assert_eq!(
            stores(&updates),
            vec![(slot(0xAA), val(2)), (slot(0xBB), val(3))],
            "collapsed to boundary values, ascending slot order"
        );
    }

    #[test]
    fn canonical_slice_before_call_shows_pre_call_state() {
        // A=1; CALL(eoa); A=2. The external call must observe A=1 (the native
        // value at the call), and the final value A=2 lands after.
        let eoa = Address::new([0x0E; 20]);
        let updates = run(vec![
            sstore(1, slot(0xAA), val(1)),
            call(1, 50_000, eoa),
            resume(1, 40_000, true), // EOA call returns inline (same depth), success
            sstore(1, slot(0xAA), val(2)),
        ]);
        // Expect: Store(A,1), Call, Store(A,2) — the call sees A=1, ends at A=2.
        assert!(
            matches!(updates[0], StateUpdate::Store(ref s) if s.slot == slot(0xAA) && s.value == val(1))
        );
        assert!(
            matches!(updates[1], StateUpdate::Call(_)),
            "call emitted after the pre-call slice"
        );
        assert!(
            matches!(updates[2], StateUpdate::Store(ref s) if s.slot == slot(0xAA) && s.value == val(2))
        );
        assert_eq!(updates.len(), 3);
    }

    #[test]
    fn reentrant_write_is_reasserted_in_final_slice_not_duplicated_before() {
        // CALL(x); inside, x re-enters TARGET and writes A=5; then the call ends.
        // The write is NOT emitted at its nested position (it replays natively
        // during the CALL), but the final slice re-asserts A=5 so the committed
        // state is canonical even if the re-entrant replay diverged.
        let x = Address::new([0x11; 20]);
        let updates = run(vec![
            call(1, 80_000, x),            // depth1: TARGET calls x  (emitted)
            call(2, 60_000, TARGET_ADDR),  // depth2: x re-enters TARGET (not emitted)
            sstore(3, slot(0xAA), val(5)), // depth3: TARGET writes A=5 (journaled)
            resume(2, 55_000, true),       // TARGET→x returns, success
            resume(1, 50_000, true),       // x→TARGET returns, success
        ]);
        // First update is the CALL (nothing to flush before it); then the final
        // canonical re-assertion of A=5.
        assert!(
            matches!(updates[0], StateUpdate::Call(_)),
            "the external CALL replays natively"
        );
        assert_eq!(
            stores(&updates),
            vec![(slot(0xAA), val(5))],
            "exactly one re-assertion of the re-entrant write, in the final slice"
        );
        assert_eq!(updates.len(), 2);
    }

    #[test]
    fn reentrant_read_between_two_calls_sees_canonical_image() {
        // CALL(y) [y re-enters TARGET, writes A=5]; then CALL(x) [x re-enters and
        // reads A]. Before CALL(x) the encoder asserts A=5 with an explicit Store
        // so x's re-entrant read is canonical — WITHOUT trusting CALL(y)'s replay
        // to have produced it. This is the point of the design: canonical state
        // is switched in *before* each external call, so an external read is
        // correct even if a prior call's on-chain replay diverged from sim. The
        // extra Store is a same-value write when replay matched (~cheap).
        let y = Address::new([0x22; 20]);
        let x = Address::new([0x11; 20]);
        let updates = run(vec![
            call(1, 90_000, y),            // CALL(y) emitted
            call(2, 70_000, TARGET_ADDR),  // y re-enters TARGET
            sstore(3, slot(0xAA), val(5)), // TARGET writes A=5 (journaled)
            resume(2, 65_000, true),
            resume(1, 60_000, true),
            call(1, 55_000, x), // CALL(x): pre-call slice asserts A=5 first
            resume(1, 50_000, true),
        ]);
        // Program: Call(y), Store(A,5) [pre-CALL(x) canonical slice], Call(x).
        let kinds: Vec<&str> = updates
            .iter()
            .map(|u| match u {
                StateUpdate::Call(_) => "call",
                StateUpdate::Store(_) => "store",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["call", "store", "call"],
            "A=5 is asserted before CALL(x) so its re-entrant read is canonical"
        );
        assert_eq!(stores(&updates), vec![(slot(0xAA), val(5))]);
    }

    #[test]
    fn reverted_delegatecall_writes_are_discarded() {
        // TARGET DELEGATECALLs a library that writes A=9 into TARGET's storage,
        // then the delegatecall REVERTS. The EVM rolls the write back, so it must
        // not appear in the program. (The legacy encoder emits it — this is the
        // journaling fix.)
        let lib = Address::new([0x33; 20]);
        let updates = run(vec![
            delegatecall(1, 80_000, lib, None), // emitting delegatecall
            sstore(2, slot(0xAA), val(9)),      // writes TARGET storage
            resume(1, 40_000, false),           // delegatecall FAILED → rollback
        ]);
        assert!(
            updates.is_empty(),
            "reverted delegatecall writes are dropped, got {updates:?}"
        );
    }

    #[test]
    fn committed_delegatecall_writes_are_emitted() {
        // Same, but the delegatecall succeeds → its write to TARGET storage is
        // canonical and appears in the final slice.
        let lib = Address::new([0x33; 20]);
        let updates = run(vec![
            delegatecall(1, 80_000, lib, None),
            sstore(2, slot(0xAA), val(9)),
            resume(1, 40_000, true), // success
        ]);
        assert_eq!(stores(&updates), vec![(slot(0xAA), val(9))]);
    }

    #[test]
    fn errored_call_is_not_emitted() {
        // A CALL opcode that fails in place (e.g. insufficient gas forwarded) is
        // not a state change and must not enter the program.
        let x = Address::new([0x11; 20]);
        let mut errored = call(1, 10, x);
        errored.error = Some("out of gas".to_string());
        let updates = run(vec![sstore(1, slot(0xAA), val(1)), errored]);
        assert_eq!(stores(&updates), vec![(slot(0xAA), val(1))]);
        assert!(
            !updates.iter().any(|u| matches!(u, StateUpdate::Call(_))),
            "no call emitted"
        );
    }

    #[test]
    fn canonical_total_call_gas_matches_legacy_for_delegatecall_trace() {
        // TARGET delegatecalls a library, which (still in TARGET's context)
        // makes an emitted CALL to y. The delegatecall itself is never emitted
        // and never charged in either encoder — only the CALL is — so both
        // encoders must agree on total_call_gas despite Canonical hoisting
        // delegatecall frames instead of emitting them.
        let lib = Address::new([0x33; 20]);
        let y = Address::new([0x44; 20]);
        let logs = vec![
            delegatecall(1, 80_000, lib, None),
            call(2, 70_000, y),
            resume(2, 65_000, true),
            resume(1, 60_000, true),
        ];
        let canonical_trace = DefaultFrame {
            failed: false,
            gas: 0,
            return_value: Default::default(),
            struct_logs: logs.clone(),
        };
        let (_, _, canon_gas) =
            compute_state_updates_canonical(canonical_trace, TARGET_ADDR).expect("canonical");
        let legacy_trace = DefaultFrame {
            failed: false,
            gas: 0,
            return_value: Default::default(),
            struct_logs: logs,
        };
        let (_, _, legacy_gas) = compute_state_updates(legacy_trace).expect("legacy");
        assert_eq!(
            canon_gas, legacy_gas,
            "total_call_gas must agree between encoders on a delegatecall-heavy trace"
        );
    }

    #[test]
    fn write_by_unrelated_callee_is_not_attributed_to_target() {
        // Inside CALL(x), x writes ITS OWN storage (storage_ctx = x ≠ TARGET).
        // That must never enter TARGET's canonical image.
        let x = Address::new([0x11; 20]);
        let updates = run(vec![
            sstore(1, slot(0xAA), val(1)),   // TARGET writes A=1
            call(1, 80_000, x),              // CALL(x)
            sstore(2, slot(0xAA), val(999)), // x writes ITS slot A — not TARGET's
            resume(1, 50_000, true),
        ]);
        // Only TARGET's own A=1 is canonical; x's write is x's business.
        assert_eq!(stores(&updates), vec![(slot(0xAA), val(1))]);
    }
}

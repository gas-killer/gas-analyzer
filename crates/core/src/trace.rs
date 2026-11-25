//! Shared Geth trace processing functionality.
//!
//! This module provides functions for extracting state updates from
//! Geth-format transaction traces (`DefaultFrame`). Contains only
//! pure computation functions - no async, no I/O, no RPC calls.

use std::collections::{HashMap, HashSet};

use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::trace::geth::{DefaultFrame, StructLog};
use anyhow::{Result, bail};

use crate::observe::{
    Await, ExitData, PendingRead, charge_stateless_read, code_check, dedupe_checks, is_precompile,
    stack_top, static_check,
};
use crate::stateless::StatelessReads;
use crate::taint::Usage;
use crate::types::{CALL_GAS_UNCAPPED, IStateUpdateTypes, Opcode, StateUpdate};

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

/// The most a `CALL` opcode costs before its callee starts: a cold account (2,600), a value
/// transfer (9,000) and a new account (25,000).
const CALL_OPCODE_MAX_COST: u64 = 36_600;

/// The EVM's cap on the gas a call passes on: all but one 64th of what is left (EIP-150).
const fn all_but_one_64th(gas: u64) -> u64 {
    gas - gas / 64
}

/// EIP-7825's per-transaction gas cap, which every settlement transaction is under.
const EIP7825_TX_GAS_CAP: u64 = 1 << 24;

/// The most gas a callee can get in any settlement transaction: EIP-7825's cap, less the
/// transaction's base cost and solidity-sdk's 40,000 allowance for the `CALL` itself. Generous,
/// since the program's earlier ops and the signature check spend gas too; a cap above it can
/// never settle.
pub const SETTLEMENT_CALL_GAS_BUDGET: u64 =
    all_but_one_64th(EIP7825_TX_GAS_CAP - crate::heuristic::BASE_TX_COST - 40_000);

/// solidity-sdk v0.2.0's `gasLimit` for a traced `CALL`, from its gas operand and the gas left
/// when it ran (`gas_at_call`).
///
/// - A cap the call could actually impose, below what the EVM would pass on anyway, is kept as
///   is. That covers `transfer` and `send`, whose operand is 0 with value (the EVM adds the
///   2,300 stipend) and 2,300 without, and explicit `{gas: n}` caps.
/// - An operand at or above that, as `call(gas(), ...)` passes, means "everything left", and so
///   does a cap above [`SETTLEMENT_CALL_GAS_BUDGET`], which an over-limit trace can carry but no
///   settlement can meet. Both become [`CALL_GAS_UNCAPPED`]: always settleable, though the settler
///   then picks the callee's gas.
///
/// Sizing an uncapped call's need, so it is capped too, needs the callee simulated cold and warm.
pub fn traced_call_gas_limit(operand: U256, gas_at_call: u64) -> U256 {
    let passed_on = all_but_one_64th(gas_at_call.saturating_sub(CALL_OPCODE_MAX_COST));
    match u64::try_from(operand) {
        Ok(cap) if cap < passed_on && cap <= SETTLEMENT_CALL_GAS_BUDGET => U256::from(cap),
        _ => CALL_GAS_UNCAPPED,
    }
}

/// Opcodes whose memory the extractors read: [`append_state_update_from_struct_log`]'s ops, plus
/// what the observation pins read ([`crate::observe`]: a `STATICCALL`'s calldata and the data a
/// frame returns or reverts with). It must cover every memory read, since the wasm crate's lean
/// trace parser drops memory for every other opcode.
pub fn reads_memory(op: &str) -> bool {
    matches!(
        op,
        "CREATE"
            | "CREATE2"
            | "CALL"
            | "LOG0"
            | "LOG1"
            | "LOG2"
            | "LOG3"
            | "LOG4"
            | "STATICCALL"
            | "RETURN"
            | "REVERT"
    )
}

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
            state_updates.push(StateUpdate::Create(IStateUpdateTypes::Create::unpinned(
                value,
                initcode.into(),
            )));
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
                resultHash: B256::ZERO,
                gasLimit: traced_call_gas_limit(stack[0], struct_log.gas),
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
    /// The part of `call_gas_total` spent inside the callees, without the `CALL` opcodes' own
    /// cost: what a simulation that runs those `CALL`s into code-less accounts misses. As walked:
    /// a caller that pins stateless reads by code takes their gas off `call_gas_total` only (see
    /// `observe::resolve_code_sizes`), so this keeps it. Only pricers that check every read by
    /// running it use this.
    pub callee_gas_total: u64,
    /// Total gas consumed by the extracted CREATE/CREATE2s (gross, pre-refund): the opcode, its
    /// initcode's execution and the code deposit.
    pub create_gas_total: u64,
    /// Total gas actually charged for the extracted SSTOREs (from struct-log
    /// `gasCost`), reflecting cold/warm/dirty pricing per write.
    pub sstore_gas_total: u64,
    /// Final (pre-cap) EIP-3529 refund counter of the traced execution, for
    /// netting refunds out of heuristic estimates.
    pub refund_counter: u64,
    /// A frame at depth >= 2 issued a CALL-family opcode targeting the origin
    /// contract — a callee called back into it (e.g. a Uniswap V3 swap
    /// callback). The extracted depth-1 call gas then includes the origin
    /// contract's own callback logic, so heuristic estimates over this
    /// extraction are qualified, not corrected. Always `false` when no origin
    /// address was supplied.
    pub reentered: bool,
}

/// What a `CALL`'s callee spent of `gas_used`, the call's whole cost in the trace: its gas
/// before the `CALL` less its gas on resuming.
///
/// `gas_cost` is Geth's `gasCost` for the `CALL`, the opcode's own cost plus the gas forwarded,
/// and `entry_gas` the gas at the callee's first step, the gas forwarded plus any stipend. Their
/// difference is the opcode's cost less the stipend, which `gas_used` also carries; a tracer whose
/// `gasCost` leaves out the forwarded gas makes it zero, and the result `gas_used`. `None` is a
/// call that opened no frame (an account without code, a precompile, a failed call).
pub(crate) fn callee_gas(gas_used: u64, gas_cost: u64, entry_gas: Option<u64>) -> u64 {
    entry_gas.map_or(0, |entry| {
        gas_used.saturating_sub(gas_cost.saturating_sub(entry))
    })
}

/// An extracted `CALL`'s struct log, and its callee's first step.
struct CallGas {
    gas: u64,
    gas_cost: u64,
    entry_gas: Option<u64>,
}

/// Compute state updates from a Geth DefaultFrame trace (see [`TraceExtract`]).
///
/// `origin` is the address of the traced transaction's target contract (the
/// code executing at depth 1). When supplied, calls from deeper frames back
/// into it set [`TraceExtract::reentered`]; pass `None` to skip detection.
///
/// Alongside the writes it records what the consumer observed and the program's effects depend
/// on: call results, `CREATE` addresses and read checks, in trace order.
#[tracing::instrument(name = "gas.trace_parse", skip_all, fields(state_update_count = tracing::field::Empty))]
pub fn compute_state_updates(trace: DefaultFrame, origin: Option<Address>) -> Result<TraceExtract> {
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
    let mut call_gas_tracking: HashMap<usize, CallGas> = HashMap::new();
    // An extracted CALL whose callee's first step, if it has one, is the next log.
    let mut awaiting_entry: Option<(u64, usize)> = None;
    // An extracted CREATE/CREATE2's (depth, gas at the opcode), until execution resumes at its depth.
    let mut pending_create: Option<(u64, u64)> = None;
    let mut total_call_gas = 0u64;
    let mut callee_gas_total = 0u64;
    let mut create_gas_total = 0u64;
    let mut sstore_gas_total = 0u64;
    let mut reentered = false;
    // What each frame returned, the ops whose pins wait on the trace resuming after them (with
    // the depth they ran at and whether they opened a frame), and a read whose result is on the
    // next log's stack.
    let mut exits = ExitData::default();
    let mut awaiting: Vec<(u64, Await, bool)> = Vec::new();
    let mut pending_read: Option<(u64, PendingRead)> = None;
    // Which observations the program's effects depend on; the rest stay unpinned. Of the reads
    // that are, those whose callee ran stateless code are pinned by that code.
    let usage = Usage::of(&trace.struct_logs);
    let stateless = StatelessReads::of_used(&trace.struct_logs, &usage);

    for (step, struct_log) in trace.struct_logs.into_iter().enumerate() {
        let depth = struct_log.depth;
        let op = struct_log.op.as_ref().to_string();

        exits.step(&struct_log)?;
        if let Some((read_depth, read)) = pending_read.take()
            && depth == read_depth
            && let Some(check) = stack_top(&struct_log, 0).and_then(|result| read.check(result))
        {
            state_updates.push(check);
        }
        if let Some((await_depth, _, entered)) = awaiting.last_mut()
            && depth == *await_depth + 1
        {
            *entered = true;
        }
        while let Some(&(await_depth, pin, entered)) = awaiting.last()
            && depth <= await_depth
        {
            awaiting.pop();
            if depth == await_depth
                && let Some(top) = stack_top(&struct_log, 0)
            {
                let returndata = if entered {
                    exits.take(depth + 1)
                } else {
                    Default::default()
                };
                pin.resolve(&mut state_updates, top, &returndata)?;
            }
        }

        if let Some((call_depth, idx)) = awaiting_entry.take()
            && depth == call_depth + 1
            && let Some(call) = call_gas_tracking.get_mut(&idx)
        {
            call.entry_gas = Some(struct_log.gas);
        }

        // The resume log carries the deployed address (zero on failure) on top of the stack.
        if let Some((create_depth, gas_at_opcode)) = pending_create
            && depth <= create_depth
        {
            pending_create = None;
            let deployed = (depth == create_depth)
                .then(|| struct_log.stack.as_ref().and_then(|s| s.last()))
                .flatten();
            if deployed.is_none_or(|address| address.is_zero()) {
                bail!(crate::nested::failed_create(create_depth));
            }
            create_gas_total += gas_at_opcode.saturating_sub(struct_log.gas);
        }

        // Re-entry detection: any CALL-family opcode below depth 1 whose
        // target (second stack item from the top) is the origin contract.
        if !reentered
            && depth >= 2
            && matches!(
                op.as_str(),
                "CALL" | "STATICCALL" | "DELEGATECALL" | "CALLCODE"
            )
            && let (Some(origin), Some(stack)) = (origin, struct_log.stack.as_ref())
            && stack.len() >= 2
            && Address::from_word(stack[stack.len() - 2].into()) == origin
        {
            reentered = true;
        }

        // Whenever stepping up (leaving a CALL/CALLCODE/DELEGATECALL) reset the target depth
        // and pop call stack for any CALLs we've exited.
        if depth < target_depth {
            while let Some(&(d, idx)) = call_stack.last() {
                if d >= depth {
                    // We're exiting this CALL. Calculate its gas cost.
                    if let Some(call) = call_gas_tracking.remove(&idx) {
                        let gas_used = call.gas.saturating_sub(struct_log.gas);
                        total_call_gas += gas_used;
                        charge_stateless_read(&mut state_updates, idx - 1, gas_used);
                        callee_gas_total += callee_gas(gas_used, call.gas_cost, call.entry_gas);
                    }
                    call_stack.pop();
                } else {
                    break;
                }
            }
            call_type_at_depth.remove(&target_depth);
            target_depth = depth;
        }

        // Reads an emitting frame makes and its effects depend on, pinned where they happen.
        if matches!(
            op.as_str(),
            "STATICCALL" | "BALANCE" | "SELFBALANCE" | "EXTCODEHASH" | "EXTCODESIZE"
        ) && depth == target_depth
            && call_stack.is_empty()
            && struct_log.error.is_none()
            && usage.used(step)
        {
            if op == "STATICCALL" {
                if let Some(check) = static_check(&struct_log)? {
                    state_updates.push(check);
                    let index = state_updates.len() - 1;
                    let call_index_1based = state_updates.len();
                    // A read of stateless code may be pinned by that code instead: its markers
                    // follow the check until a caller resolves one or the other.
                    let code = stateless.code_of(step);
                    if let Some(accounts) = code {
                        state_updates.extend(accounts.iter().map(|&account| code_check(account)));
                    }
                    let stateless = code.is_some();
                    awaiting.push((depth, Await::Static { index, stateless }, false));
                    // Settlement runs the read again: its gas counts like a CALL's, tracked the
                    // same way until the trace returns from it. Counted for a stateless read too,
                    // which may fall back to that.
                    call_stack.push((depth, call_index_1based));
                    call_gas_tracking.insert(
                        call_index_1based,
                        CallGas {
                            gas: struct_log.gas,
                            gas_cost: struct_log.gas_cost,
                            entry_gas: None,
                        },
                    );
                    awaiting_entry = Some((depth, call_index_1based));
                    target_depth = depth + 1;
                }
            } else if let Some(read) = PendingRead::of(&struct_log, origin) {
                pending_read = Some((depth, read));
            }
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
                let gas_cost = struct_log.gas_cost;
                let update_count_before = state_updates.len();
                if let Some(skipped) =
                    append_state_update_from_struct_log(&mut state_updates, struct_log)?
                {
                    skipped_opcodes.insert(skipped);
                } else if state_updates.len() > update_count_before {
                    // We added a state update.
                    if op == "SSTORE" {
                        // struct-log gasCost is the actual charge for this
                        // write (cold/warm/dirty per EIP-2929/2200).
                        sstore_gas_total += gas_cost;
                    }
                    if op == "CREATE" || op == "CREATE2" {
                        pending_create = Some((depth, gas_after_opcode));
                    }
                    if op == "CREATE" && usage.used(step) {
                        let index = state_updates.len() - 1;
                        awaiting.push((depth, Await::Create { index }, false));
                    }
                    if op == "CALL"
                        && let Some(StateUpdate::Call(call)) = state_updates.last()
                        && !is_precompile(call.target)
                    {
                        let index = state_updates.len() - 1;
                        let value_sent = !call.value.is_zero();
                        let used = usage.used(step);
                        awaiting.push((
                            depth,
                            Await::Call {
                                index,
                                value_sent,
                                used,
                            },
                            false,
                        ));
                    }
                    if op == "CALL" {
                        let call_index_1based = state_updates.len();
                        call_stack.push((depth, call_index_1based));
                        call_gas_tracking.insert(
                            call_index_1based,
                            CallGas {
                                gas: gas_after_opcode,
                                gas_cost,
                                entry_gas: None,
                            },
                        );
                        awaiting_entry = Some((depth, call_index_1based));
                        // Increase target_depth to track when we exit this CALL
                        // This allows us to detect when the CALL returns and pop from call_stack
                        target_depth = depth + 1;
                    }
                }
            }
        }
    }

    // A trace that ends on the create aborted at the opcode itself (out of gas, static context).
    if let Some((depth, _)) = pending_create {
        bail!(crate::nested::failed_create(depth));
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

    dedupe_checks(&mut state_updates);
    tracing::Span::current().record("state_update_count", state_updates.len());
    Ok(TraceExtract {
        state_updates,
        skipped_opcodes,
        call_gas_total: total_call_gas,
        callee_gas_total,
        create_gas_total,
        sstore_gas_total,
        refund_counter,
        reentered,
    })
}

// ============================================================================
// Canonical-checkpoint extraction
// ============================================================================

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
    let tree = crate::nested::compute_frame_tree_canonical(
        trace,
        target,
        &|_| false,
        &crate::nested::NESTING_COST_MODEL_V1,
    )?;
    let root = tree
        .frames
        .into_iter()
        .next()
        .expect("a frame tree always has its root frame");
    tracing::Span::current().record("state_update_count", root.updates.len());
    Ok((root.updates, tree.skipped_opcodes, tree.call_gas_total))
}

// ============================================================================
// Call Outcome
// ============================================================================

/// A simulated call the EVM failed at the top level, by a revert or an exceptional halt. On chain
/// such a call changes nothing, so it has no program to sign, and simulating it again at the same
/// block fails the same way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvmFailure {
    reason: String,
}

impl EvmFailure {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl std::fmt::Display for EvmFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the call failed in the EVM: {}", self.reason)
    }
}

impl std::error::Error for EvmFailure {}

/// Rejects a struct-log trace whose top-level call failed.
///
/// The struct log records every write as it executes, whether or not the call later reverts, and
/// the extractors read writes straight off it; only `failed` says the EVM rolled them all back.
pub fn ensure_call_succeeded(trace: &DefaultFrame) -> std::result::Result<(), EvmFailure> {
    if trace.failed {
        return Err(EvmFailure::new(format!(
            "reverted or halted, returning {}",
            trace.return_value
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{B256, U256};

    use super::*;
    use crate::encoding::encode_state_updates_to_abi;
    use crate::observe::{resolve_code_sizes, unresolved_code_accounts};

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

    // ── Observation pins (solidity-sdk v0.2.0) ──────────────────────────

    const ORIGIN: Address = Address::new([0x77; 20]);
    const CALLEE: Address = Address::new([0xcc; 20]);

    /// A struct log at `depth` whose stack is `top_first`, top item first.
    fn at(op: &str, depth: u64, top_first: &[U256]) -> StructLog {
        let mut stack = top_first.to_vec();
        stack.reverse();
        let mut l = make_struct_log(op, stack, vec![]);
        l.depth = depth;
        l
    }

    fn call_to(depth: u64, callee: Address) -> StructLog {
        at(
            "CALL",
            depth,
            &[
                U256::from(50_000u64),
                addr_word(callee),
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
            ],
        )
    }

    fn returning(op: &str, depth: u64, data: u8) -> StructLog {
        let mut l = at(op, depth, &[U256::ZERO, U256::from(1u64)]);
        l.memory = Some(vec![format!("{:02x}{}", data, "00".repeat(31))]);
        l
    }

    fn journal(logs: Vec<StructLog>) -> Vec<StateUpdate> {
        let trace = DefaultFrame {
            gas: 0,
            failed: false,
            return_value: Default::default(),
            struct_logs: logs,
        };
        compute_state_updates(trace, Some(ORIGIN))
            .unwrap()
            .state_updates
    }

    #[test]
    fn journal_pins_a_calls_result() {
        let updates = journal(vec![
            call_to(1, CALLEE),
            returning("RETURN", 2, 0x2a),
            at("JUMPDEST", 1, &[U256::from(1u64)]),
        ]);
        let StateUpdate::Call(call) = &updates[0] else {
            panic!("a CALL");
        };
        assert_eq!(call.resultHash, crate::observe::result_hash(true, &[0x2a]));
    }

    #[test]
    fn journal_pins_a_caught_revert() {
        let updates = journal(vec![
            call_to(1, CALLEE),
            returning("REVERT", 2, 0x01),
            at("JUMPDEST", 1, &[U256::ZERO]),
        ]);
        let StateUpdate::Call(call) = &updates[0] else {
            panic!("a CALL");
        };
        assert_eq!(call.resultHash, crate::observe::result_hash(false, &[0x01]));
    }

    #[test]
    fn journal_checks_reads_in_trace_order() {
        let updates = journal(vec![
            at("SSTORE", 1, &[U256::from(1u64), U256::from(10u64)]),
            at(
                "STATICCALL",
                1,
                &[
                    U256::from(50_000u64),
                    addr_word(CALLEE),
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                    U256::ZERO,
                ],
            ),
            // The callee reads storage, so settlement must run the read again.
            at("SLOAD", 2, &[U256::ZERO]),
            returning("RETURN", 2, 0x07),
            at("JUMPDEST", 1, &[U256::from(1u64)]),
            at("SELFBALANCE", 1, &[]),
            at("JUMPDEST", 1, &[U256::from(9u64)]),
            at("SSTORE", 1, &[U256::from(2u64), U256::from(20u64)]),
        ]);
        assert_eq!(updates.len(), 4, "{updates:?}");
        assert!(matches!(&updates[0], StateUpdate::Store(_)));
        let StateUpdate::StaticCallCheck(check) = &updates[1] else {
            panic!("a STATICCALL_CHECK, got {:?}", updates[1]);
        };
        assert_eq!(check.target, CALLEE);
        assert_eq!(check.resultHash, crate::observe::result_hash(true, &[0x07]));
        assert!(
            matches!(&updates[2], StateUpdate::BalanceCheck(b) if b.account == ORIGIN && b.balance == U256::from(9u64))
        );
        assert!(matches!(&updates[3], StateUpdate::Store(_)));
    }

    fn staticcall_to(depth: u64, callee: Address) -> StructLog {
        at(
            "STATICCALL",
            depth,
            &[
                U256::from(50_000u64),
                addr_word(callee),
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
            ],
        )
    }

    /// The root writes slot 1, reads `callee` and branches on what it returned (so the program
    /// depends on the read), then writes slot 2. The read is struct log 1.
    fn branching_on_a_read(callee_logs: Vec<StructLog>) -> Vec<StructLog> {
        let mut logs = vec![
            at("SSTORE", 1, &[U256::from(1u64), U256::from(10u64)]),
            staticcall_to(1, CALLEE),
        ];
        logs.extend(callee_logs);
        logs.extend([
            at("ISZERO", 1, &[U256::from(1u64)]),
            at("JUMPI", 1, &[U256::from(9u64), U256::ZERO]),
            at("SSTORE", 1, &[U256::from(2u64), U256::from(20u64)]),
        ]);
        logs
    }

    fn describe(updates: &[StateUpdate]) -> Vec<String> {
        updates
            .iter()
            .map(|u| match u {
                StateUpdate::StaticCallCheck(c) => format!("STATICCALL {}", c.target),
                StateUpdate::CodehashCheck(c) => format!("CODEHASH {} {}", c.account, c.codehash),
                StateUpdate::Store(s) => format!("STORE {}", s.slot),
                other => format!("{other:?}"),
            })
            .collect()
    }

    fn trace_of(logs: Vec<StructLog>) -> DefaultFrame {
        DefaultFrame {
            gas: 0,
            failed: false,
            return_value: Default::default(),
            struct_logs: logs,
        }
    }

    /// Both walks' programs, which must agree op for op: as walked, and as a caller resolves
    /// them with `code` (or, given none, as one that can't fetch code falls back).
    fn both_programs(
        logs: Vec<StructLog>,
        code: Option<&HashMap<Address, alloy_primitives::Bytes>>,
    ) -> Vec<String> {
        let legacy = journal(logs.clone());
        let tree = crate::nested::compute_frame_tree_canonical(
            trace_of(logs),
            ORIGIN,
            &|_| false,
            &crate::nested::NESTING_COST_MODEL_V1,
        )
        .expect("frame tree");
        let tree = tree.frames[0].updates.clone();
        assert_eq!(describe(&tree), describe(&legacy), "the walks differ");
        let mut program = legacy;
        match code {
            Some(code) => {
                resolve_code_sizes(&mut program, code).unwrap();
            }
            None => crate::observe::unpin_stateless_reads(&mut program).unwrap(),
        }
        let mut tree = tree;
        match code {
            Some(code) => {
                resolve_code_sizes(&mut tree, code).unwrap();
            }
            None => crate::observe::unpin_stateless_reads(&mut tree).unwrap(),
        }
        assert_eq!(
            encode_state_updates_to_abi(&tree),
            encode_state_updates_to_abi(&program),
            "the walks encode differently"
        );
        describe(&program)
    }

    const INNER: Address = Address::new([0xdd; 20]);
    const SHA256: Address =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);
    const CALLEE_CODE: &[u8] = &[0x60, 0x01];
    const INNER_CODE: &[u8] = &[0x60, 0x02];

    fn code() -> HashMap<Address, alloy_primitives::Bytes> {
        HashMap::from([
            (CALLEE, alloy_primitives::Bytes::from_static(CALLEE_CODE)),
            (INNER, alloy_primitives::Bytes::from_static(INNER_CODE)),
        ])
    }

    fn stores_around(middle: &[String]) -> Vec<String> {
        let mut out = vec![format!("STORE {}", B256::from(U256::from(1)))];
        out.extend_from_slice(middle);
        out.push(format!("STORE {}", B256::from(U256::from(2))));
        out
    }

    fn code_pin(account: Address, code: &[u8]) -> String {
        format!("CODEHASH {account} {}", alloy_primitives::keccak256(code))
    }

    /// A verifier that only hashes, calls a precompile and returns: pinned by its code, where
    /// the read happened, in both walks; checked by running it for a caller without code.
    #[test]
    fn a_stateless_read_is_pinned_by_its_code() {
        let logs = branching_on_a_read(vec![
            at("CALLDATALOAD", 2, &[U256::ZERO]),
            at(
                "MULMOD",
                2,
                &[U256::from(3u64), U256::from(2u64), U256::from(7u64)],
            ),
            staticcall_to(2, SHA256),
            at("ISZERO", 2, &[U256::from(1u64)]),
            returning("RETURN", 2, 0x01),
        ]);
        assert_eq!(
            unresolved_code_accounts(&journal(logs.clone())),
            [CALLEE],
            "its code hash waits for the code"
        );
        assert_eq!(
            both_programs(logs.clone(), Some(&code())),
            stores_around(&[code_pin(CALLEE, CALLEE_CODE)])
        );
        assert_eq!(
            both_programs(logs, None),
            stores_around(&[format!("STATICCALL {CALLEE}")])
        );
    }

    /// The read's gas is in the program's call gas, as settlement running it again; pinning it by
    /// its code hands that gas back.
    #[test]
    fn pinning_a_read_by_its_code_returns_its_gas() {
        let mut logs = branching_on_a_read(vec![
            at("CALLDATALOAD", 2, &[U256::ZERO]),
            returning("RETURN", 2, 0x01),
        ]);
        let call = logs
            .iter()
            .position(|l| l.op.as_ref() == "STATICCALL")
            .expect("the read");
        for (i, log) in logs.iter_mut().enumerate() {
            log.gas = if i <= call { 100_000 } else { 70_000 };
        }

        let extract = compute_state_updates(trace_of(logs.clone()), Some(ORIGIN)).unwrap();
        assert_eq!(extract.call_gas_total, 30_000);
        let mut flat = extract.state_updates;
        assert_eq!(resolve_code_sizes(&mut flat, &code()).unwrap(), 30_000);

        let tree = crate::nested::compute_frame_tree_canonical(
            trace_of(logs.clone()),
            ORIGIN,
            &|_| false,
            &crate::nested::NESTING_COST_MODEL_V1,
        )
        .expect("frame tree");
        assert_eq!(tree.call_gas_total, 30_000);
        let mut root = tree.frames[0].updates.clone();
        assert_eq!(resolve_code_sizes(&mut root, &code()).unwrap(), 30_000);
        assert_eq!(
            encode_state_updates_to_abi(&flat),
            encode_state_updates_to_abi(&root),
            "both walks resolve to the same program"
        );

        // Falling back keeps the read, and its gas.
        let mut checked = compute_state_updates(trace_of(logs), Some(ORIGIN))
            .unwrap()
            .state_updates;
        crate::observe::unpin_stateless_reads(&mut checked).unwrap();
        assert_eq!(resolve_code_sizes(&mut checked, &code()).unwrap(), 0);
    }

    #[test]
    fn a_read_of_storage_is_checked_by_running_it() {
        let logs = branching_on_a_read(vec![
            at("SLOAD", 2, &[U256::ZERO]),
            returning("RETURN", 2, 0x01),
        ]);
        assert_eq!(
            both_programs(logs, Some(&code())),
            stores_around(&[format!("STATICCALL {CALLEE}")])
        );
    }

    /// A stateless callee that reads another stateless contract pins both contracts' code.
    #[test]
    fn nested_stateless_code_is_pinned_too() {
        let logs = branching_on_a_read(vec![
            staticcall_to(2, INNER),
            at("ADD", 3, &[U256::from(1u64), U256::from(2u64)]),
            returning("RETURN", 3, 0x05),
            at("ISZERO", 2, &[U256::from(1u64)]),
            returning("RETURN", 2, 0x01),
        ]);
        assert_eq!(
            both_programs(logs.clone(), Some(&code())),
            stores_around(&[code_pin(CALLEE, CALLEE_CODE), code_pin(INNER, INNER_CODE)])
        );
        // Without the inner contract's code, the whole read falls back.
        let only_callee =
            HashMap::from([(CALLEE, alloy_primitives::Bytes::from_static(CALLEE_CODE))]);
        assert_eq!(
            both_programs(logs, Some(&only_callee)),
            stores_around(&[format!("STATICCALL {CALLEE}")])
        );
    }

    #[test]
    fn a_nested_storage_read_keeps_the_whole_read_checked() {
        let logs = branching_on_a_read(vec![
            staticcall_to(2, INNER),
            at("SLOAD", 3, &[U256::ZERO]),
            returning("RETURN", 3, 0x05),
            at("ISZERO", 2, &[U256::from(1u64)]),
            returning("RETURN", 2, 0x01),
        ]);
        assert_eq!(
            both_programs(logs, Some(&code())),
            stores_around(&[format!("STATICCALL {CALLEE}")])
        );
    }

    /// Code the trace deployed isn't at the state it started on, so a read of it is checked by
    /// running it, even with its code at hand.
    #[test]
    fn a_read_of_code_deployed_in_the_trace_is_checked_by_running_it() {
        let mut logs = vec![
            at("CREATE", 1, &[U256::ZERO, U256::ZERO, U256::ZERO]),
            at("STOP", 2, &[]),
            at("JUMPDEST", 1, &[addr_word(CALLEE)]),
        ];
        logs.extend(branching_on_a_read(vec![returning("RETURN", 2, 0x01)]));
        assert!(StatelessReads::of(&logs).code_of(4).is_none());
        let program = describe(&journal(logs));
        assert!(
            program.contains(&format!("STATICCALL {CALLEE}"))
                && !program.iter().any(|op| op.starts_with("CODEHASH")),
            "{program:?}"
        );
    }

    /// A stateless read that failed without data reverted with nothing itself (`follow` rules
    /// out the EVM's own failures), which its code pins; settlement would reject the failure if
    /// it ran the read again, so a caller without code can't fall back.
    #[test]
    fn a_stateless_read_that_failed_without_data_is_pinned_by_its_code() {
        let mut logs = branching_on_a_read(vec![at("REVERT", 2, &[U256::ZERO, U256::ZERO])]);
        logs[3] = at("ISZERO", 1, &[U256::ZERO]);
        assert!(
            StatelessReads::of(&logs).code_of(1).is_some(),
            "the callee is stateless"
        );
        assert_eq!(
            both_programs(logs.clone(), Some(&code())),
            stores_around(&[code_pin(CALLEE, CALLEE_CODE)])
        );
        let err = crate::observe::unpin_stateless_reads(&mut journal(logs)).unwrap_err();
        assert!(
            err.to_string().contains("without pinning its code"),
            "{err}"
        );
    }

    /// A stateful callee's empty failure is still rejected by the walk.
    #[test]
    fn a_stateful_read_that_failed_without_data_is_rejected() {
        let mut logs = branching_on_a_read(vec![
            at("SLOAD", 2, &[U256::ZERO]),
            at("REVERT", 2, &[U256::ZERO, U256::ZERO]),
        ]);
        logs[4] = at("ISZERO", 1, &[U256::ZERO]);
        assert!(compute_state_updates(trace_of(logs), Some(ORIGIN)).is_err());
    }

    #[test]
    fn journal_pins_a_creates_address() {
        let deployed = Address::repeat_byte(0xde);
        let updates = journal(vec![
            at("CREATE", 1, &[U256::ZERO, U256::ZERO, U256::ZERO]),
            at("STOP", 2, &[]),
            at("JUMPDEST", 1, &[addr_word(deployed)]),
        ]);
        let StateUpdate::Create(create) = &updates[0] else {
            panic!("a CREATE");
        };
        assert_eq!(create.expected, deployed);
    }

    #[test]
    fn journal_leaves_precompile_calls_unpinned() {
        let updates = journal(vec![
            call_to(1, Address::with_last_byte(4)),
            at("JUMPDEST", 1, &[U256::from(1u64)]),
        ]);
        let StateUpdate::Call(call) = &updates[0] else {
            panic!("a CALL");
        };
        assert_eq!(call.resultHash, B256::ZERO);
    }

    #[test]
    fn call_gas_limit_keeps_caps_the_call_imposed() {
        let gas = 1_000_000;
        // `transfer` with value, `send` without, an explicit cap.
        for cap in [0u64, 2_300, 50_000] {
            assert_eq!(traced_call_gas_limit(U256::from(cap), gas), U256::from(cap));
        }
    }

    #[test]
    fn call_gas_limit_leaves_everything_left_uncapped() {
        let gas = 1_000_000;
        // `call(gas(), ...)` passes the gas left, which the EVM trims to 63/64.
        assert_eq!(
            traced_call_gas_limit(U256::from(gas), gas),
            CALL_GAS_UNCAPPED
        );
        assert_eq!(
            traced_call_gas_limit(U256::from(all_but_one_64th(gas)), gas),
            CALL_GAS_UNCAPPED
        );
        assert_eq!(traced_call_gas_limit(U256::MAX, gas), CALL_GAS_UNCAPPED);
    }

    /// An over-limit trace can impose a cap no settlement transaction can meet.
    #[test]
    fn call_gas_limit_never_exceeds_the_settlement_budget() {
        let gas = 1u64 << 30;
        let over = SETTLEMENT_CALL_GAS_BUDGET + 1;
        assert_eq!(
            traced_call_gas_limit(U256::from(over), gas),
            CALL_GAS_UNCAPPED
        );
        assert_eq!(
            traced_call_gas_limit(U256::from(SETTLEMENT_CALL_GAS_BUDGET), gas),
            U256::from(SETTLEMENT_CALL_GAS_BUDGET)
        );
    }

    #[test]
    fn call_decodes_its_gas_operand() {
        // Stack bottom→top: retSize, retOffset, argsSize, argsOffset, value, address, gas.
        let stack = vec![
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::from(1u64),
            U256::from(0xabcdu64),
            U256::ZERO,
        ];
        let mut updates = Vec::new();
        append_state_update_from_struct_log(&mut updates, make_struct_log("CALL", stack, vec![]))
            .unwrap();
        let StateUpdate::Call(call) = &updates[0] else {
            panic!("expected Call, got {:?}", updates[0]);
        };
        assert_eq!(
            call.gasLimit,
            U256::ZERO,
            "a transfer with value keeps its 0"
        );
        assert_eq!(call.resultHash, B256::ZERO);
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
        let extract = compute_state_updates(trace, None).unwrap();
        assert!(extract.state_updates.is_empty());
        assert_eq!(extract.refund_counter, 40_000);
    }

    #[test]
    fn sstore_gas_total_sums_actual_charges() {
        // Stack bottom→top is [value, slot]; append reverses it.
        let mut cold = make_struct_log("SSTORE", vec![U256::from(0xff), U256::from(1)], vec![]);
        cold.gas_cost = 22_100;
        let mut warm = make_struct_log("SSTORE", vec![U256::from(0xee), U256::from(2)], vec![]);
        warm.gas_cost = 100;

        let trace = DefaultFrame {
            gas: 0,
            failed: false,
            return_value: Default::default(),
            struct_logs: vec![cold, warm],
        };
        let extract = compute_state_updates(trace, None).unwrap();
        assert_eq!(extract.state_updates.len(), 2);
        assert_eq!(extract.sstore_gas_total, 22_200);
    }

    #[test]
    fn reentry_flagged_when_deep_frame_calls_origin() {
        let origin = Address::repeat_byte(0xaa);
        // STATICCALL at depth 3 back into the origin. Stack bottom→top ends
        // with [.., address, gas]: address is second from the top.
        let mut callback = make_struct_log(
            "STATICCALL",
            vec![
                U256::ZERO,                             // retSize
                U256::ZERO,                             // retOffset
                U256::ZERO,                             // argsSize
                U256::ZERO,                             // argsOffset
                U256::from_be_slice(origin.as_slice()), // address
                U256::from(100_000u64),                 // gas (top)
            ],
            vec![],
        );
        callback.depth = 3;

        let trace = DefaultFrame {
            gas: 0,
            failed: false,
            return_value: Default::default(),
            struct_logs: vec![callback],
        };
        let extract = compute_state_updates(trace, Some(origin)).unwrap();
        assert!(extract.reentered);
    }

    #[test]
    fn reentry_not_flagged_for_other_targets_or_missing_origin() {
        let origin = Address::repeat_byte(0xaa);
        let other = Address::repeat_byte(0xbb);
        let mut call = make_struct_log(
            "STATICCALL",
            vec![
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                U256::from_be_slice(other.as_slice()),
                U256::from(100_000u64),
            ],
            vec![],
        );
        call.depth = 2;

        let mk = |log: StructLog| DefaultFrame {
            gas: 0,
            failed: false,
            return_value: Default::default(),
            struct_logs: vec![log],
        };

        // Different target: not re-entry.
        let extract = compute_state_updates(mk(call.clone()), Some(origin)).unwrap();
        assert!(!extract.reentered);

        // No origin supplied: detection off even for a matching target.
        let mut self_call = call.clone();
        self_call.stack.as_mut().unwrap()[4] = U256::from_be_slice(origin.as_slice());
        let extract = compute_state_updates(mk(self_call), None).unwrap();
        assert!(!extract.reentered);
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
        let extract = compute_state_updates(trace, None).unwrap();
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
    fn a_failed_top_level_call_is_rejected() {
        let trace = |failed| DefaultFrame {
            failed,
            gas: 0,
            return_value: alloy_primitives::Bytes::from_static(&[0xab]),
            struct_logs: vec![sstore(1, slot(0xAA), val(1))],
        };
        let error = ensure_call_succeeded(&trace(true)).expect_err("the call reverted");
        assert!(error.to_string().contains("0xab"), "{error}");
        assert_eq!(ensure_call_succeeded(&trace(false)), Ok(()));
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

    /// The legacy and the frame-tree walk, as (call gas, callee gas).
    fn call_and_callee_gas(logs: Vec<StructLog>) -> [(u64, u64); 2] {
        let trace = || DefaultFrame {
            failed: false,
            gas: 0,
            return_value: Default::default(),
            struct_logs: logs.clone(),
        };
        let legacy = compute_state_updates(trace(), None).expect("legacy");
        let tree = crate::nested::compute_frame_tree_canonical(
            trace(),
            TARGET_ADDR,
            &|_| false,
            &crate::nested::NESTING_COST_MODEL_V1,
        )
        .expect("frame tree");
        [
            (legacy.call_gas_total, legacy.callee_gas_total),
            (tree.call_gas_total, tree.callee_gas_total),
        ]
    }

    /// Geth's `gasCost` for a `CALL` is the opcode's cost plus the gas it forwards.
    fn call_costing(depth: u64, gas: u64, callee: Address, gas_cost: u64) -> StructLog {
        let mut call = call(depth, gas, callee);
        call.gas_cost = gas_cost;
        call
    }

    #[test]
    fn callee_gas_leaves_out_the_call_opcodes_own_cost() {
        let y = Address::new([0x44; 20]);
        // 2,600 for the cold account plus 900,000 forwarded; the callee spends 30,000 of it.
        let logs = vec![
            call_costing(1, 1_000_000, y, 902_600),
            log("JUMPDEST", 2, 900_000, &[], &[], None),
            log("STOP", 2, 870_000, &[], &[], None),
            resume(1, 1_000_000 - 902_600 + 870_000, true),
        ];
        for gas in call_and_callee_gas(logs) {
            assert_eq!(gas, (32_600, 30_000));
        }
    }

    /// Settlement runs a checked read again, so both walks count its gas like a `CALL`'s.
    #[test]
    fn a_checked_static_read_counts_like_a_call() {
        let y = Address::new([0x44; 20]);
        let mut read = log(
            "STATICCALL",
            1,
            1_000_000,
            &[
                U256::from(900_000u64),
                addr_word(y),
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
                U256::ZERO,
            ],
            &[],
            None,
        );
        read.gas_cost = 902_600;
        let logs = vec![
            read,
            log("JUMPDEST", 2, 900_000, &[], &[], None),
            // A storage read: the callee isn't stateless, so settlement runs it again.
            log("SLOAD", 2, 890_000, &[U256::ZERO], &[], None),
            log("STOP", 2, 870_000, &[], &[], None),
            resume(1, 1_000_000 - 902_600 + 870_000, true),
            // The read's result is stored, so the program depends on it.
            log("PUSH1", 1, 960_000, &[U256::from(1u64)], &[], None),
            log(
                "SSTORE",
                1,
                959_000,
                &[U256::from(7u64), U256::from(1u64)],
                &[],
                None,
            ),
        ];
        for gas in call_and_callee_gas(logs.clone()) {
            assert_eq!(gas, (32_600, 30_000));
        }

        // Without the storage read the callee is stateless, but the read may still fall back
        // to being run (a caller without its code), so its gas is counted the same.
        let mut stateless = logs;
        stateless.remove(2);
        for gas in call_and_callee_gas(stateless) {
            assert_eq!(gas, (32_600, 30_000));
        }
    }

    #[test]
    fn a_call_that_opens_no_frame_spends_nothing_in_its_callee() {
        let eoa = Address::new([0x44; 20]);
        let logs = vec![
            call_costing(1, 1_000_000, eoa, 902_600),
            resume(1, 997_400, true),
        ];
        for gas in call_and_callee_gas(logs) {
            assert_eq!(gas, (2_600, 0));
        }
    }

    /// The stipend reaches the callee on top of the forwarded gas, and what it leaves of the
    /// stipend comes back to the caller.
    #[test]
    fn callee_gas_counts_the_stipend_the_callee_spends() {
        let y = Address::new([0x44; 20]);
        // 9,000 for the value transfer plus 50,000 forwarded; the callee enters with the 2,300
        // stipend on top and spends 1,000.
        let logs = vec![
            call_costing(1, 1_000_000, y, 59_000),
            log("JUMPDEST", 2, 52_300, &[], &[], None),
            log("STOP", 2, 51_300, &[], &[], None),
            resume(1, 1_000_000 - 59_000 + 51_300, true),
        ];
        for gas in call_and_callee_gas(logs) {
            assert_eq!(gas, (7_700, 1_000));
        }
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
        let legacy_gas = compute_state_updates(legacy_trace, None)
            .expect("legacy")
            .call_gas_total;
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

    /// A CREATE2 of empty initcode (offset=size=salt=value=0).
    fn create2(depth: u64, gas: u64) -> StructLog {
        log("CREATE2", depth, gas, &[U256::ZERO; 4], &[], None)
    }

    /// A resume log after a create, carrying the deployed address (zero on failure).
    fn create_resume(depth: u64, gas: u64, deployed: Option<Address>) -> StructLog {
        let top = deployed.map_or(U256::ZERO, addr_word);
        log("JUMPDEST", depth, gas, &[top], &[], None)
    }

    fn legacy(logs: Vec<StructLog>) -> Result<TraceExtract> {
        compute_state_updates(
            DefaultFrame {
                failed: false,
                gas: 0,
                return_value: Default::default(),
                struct_logs: logs,
            },
            None,
        )
    }

    const DEPLOYED: Address = Address::new([0x33; 20]);

    #[test]
    fn legacy_create_gas_covers_initcode_execution() {
        let extract = legacy(vec![
            create2(1, 1_000_000),
            sstore(2, slot(1), val(1)),
            create_resume(1, 940_000, Some(DEPLOYED)),
        ])
        .unwrap();
        assert_eq!(extract.state_updates.len(), 1);
        assert_eq!(extract.create_gas_total, 60_000);
        assert_eq!(extract.call_gas_total, 0);
    }

    #[test]
    fn legacy_create_without_a_frame_is_counted() {
        let extract = legacy(vec![
            create2(1, 1_000_000),
            create_resume(1, 968_000, Some(DEPLOYED)),
        ])
        .unwrap();
        assert_eq!(extract.create_gas_total, 32_000);
    }

    #[test]
    fn legacy_failed_create_is_rejected() {
        let with_frame = legacy(vec![
            create2(1, 1_000_000),
            log("REVERT", 2, 990_000, &[U256::ZERO; 2], &[], None),
            create_resume(1, 980_000, None),
        ]);
        let without_frame = legacy(vec![create2(1, 1_000_000), create_resume(1, 968_000, None)]);
        let trace_ends_on_create = legacy(vec![create2(1, 1_000_000)]);
        for result in [with_frame, without_frame, trace_ends_on_create] {
            let err = result.expect_err("a failed create must not be emitted");
            assert!(err.to_string().contains("CREATE/CREATE2 at depth 1 failed"));
        }
    }

    #[test]
    fn legacy_failed_create_inside_a_call_is_not_emitted() {
        let extract = legacy(vec![
            call(1, 1_000_000, Address::new([0x11; 20])),
            create2(2, 990_000),
            create_resume(2, 980_000, None),
            resume(1, 970_000, true),
        ])
        .unwrap();
        assert_eq!(extract.state_updates.len(), 1);
        assert_eq!(extract.create_gas_total, 0);
    }
}

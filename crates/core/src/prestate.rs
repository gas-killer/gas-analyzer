//! The prestate **net form** of a state-update program.
//!
//! `compute_state_updates` (see [`crate::trace`]) builds the program from a full struct-log trace,
//! which is `O(execution steps)` and times out on heavy-compute tracked functions even when the
//! resulting diff is tiny. This module builds an equivalent-effect program from two far cheaper
//! tracers — `prestateTracer` in `diffMode` (net storage diff, `O(changed slots)`) and `callTracer`
//! with logs (the call tree + events) — for the calls where doing so is sound.
//!
//! **The net form is a different program, not a cheaper route to the same one.** It carries one store
//! per *changed* slot, so repeated writes to a slot collapse into one and a slot written back to its
//! original value produces none — the struct-log encoders emit those, having no pre-state to compare
//! against. Applying either program reaches the same end state and emits the same events, but they are
//! not byte-identical, and the encoded bytes are what the task digest commits to. So this is a
//! distinct signed encoding a deployment selects fleet-wide, never an opportunistic substitution
//! inside another one.
//!
//! A net storage diff cannot distinguish the consumer's top-level writes from writes induced by a
//! sub-`CALL` (which `compute_state_updates` represents as a CALL op whose internals re-execute on
//! replay). So [`classify_prestate_eligibility`] reproduces that model: the consumer's frame is
//! "target depth", `DELEGATECALL`/`CALLCODE` are transparent, and a net form exists only when the
//! consumer touches no other account's storage and makes no regular `CALL`, `CREATE`, or
//! `SELFDESTRUCT`. It reads only the call tree and the diff, both fixed by the call and the block, so
//! every prover reaches the same verdict; the calls it rejects are encoded from the struct-log trace.
//!
//! These are pure functions over the tracer outputs — no async, no I/O — to keep them in `core`.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256};
use alloy_rpc_types::trace::geth::{CallFrame, CallLogFrame, DefaultFrame, DiffMode};

use crate::observe::is_precompile;
use crate::types::{IStateUpdateTypes, StateUpdate};

/// Whether a call has a net form, or must be encoded from its struct-log trace instead.
#[derive(Debug, Clone)]
pub enum PrestateEligibility {
    /// The net form is sound for this call: build it with [`build_state_updates_from_prestate`].
    Eligible,
    /// The net form cannot represent this call; encode it from the struct-log trace. Carries a
    /// human-readable reason for logging.
    Fallback(String),
}

/// Decide whether a call has a net form, given its `callTracer` frame and `prestateTracer` diff.
/// Mirrors `compute_state_updates`'s replay-script model: STORE/LOG/CALL live at "target depth" (the
/// `consumer` frame), `DELEGATECALL`/`CALLCODE` are transparent. There is no net form when the
/// consumer:
///   * changes a non-consumer account's storage (only CALL replay reproduces that), or
///   * makes a regular `CALL` at target depth (its internals re-execute; a net diff can't separate
///     those from top-level writes), or
///   * does `CREATE`/`CREATE2` — the struct-log encoders extract a `Create`/`Create2` op carrying the
///     initcode to re-execute on replay, which a net diff has no way to reconstruct — or
///     `SELFDESTRUCT`, which no `StateUpdate` variant represents at all (the canonical encoder
///     surfaces it through `skipped_opcodes`).
///
/// `STATICCALL` is read-only and ignored.
///
/// Two structural preconditions also rule out the net form. `frame` must be the consumer's own frame —
/// the diff is filtered by `consumer`, so a mismatched pair would silently drop every store. And the
/// top-level call must have succeeded: a reverted call leaves an empty net diff, whereas the struct-log
/// extractors still emit the rolled-back writes, so the two would disagree on more than representation.
pub fn classify_prestate_eligibility(
    frame: &CallFrame,
    diff: &DiffMode,
    consumer: Address,
) -> PrestateEligibility {
    if let Some(to) = frame.to
        && to != consumer
    {
        return PrestateEligibility::Fallback(format!(
            "root frame targets {to}, not consumer {consumer}"
        ));
    }
    if let Some(err) = frame_failure(frame) {
        return PrestateEligibility::Fallback(format!("top-level call reverted: {err}"));
    }
    let mut accounts: BTreeSet<Address> = BTreeSet::new();
    accounts.extend(diff.pre.keys().copied());
    accounts.extend(diff.post.keys().copied());
    for a in accounts {
        if a != consumer && account_storage_changed(diff, a) {
            return PrestateEligibility::Fallback(format!(
                "non-consumer account {a} changed storage (needs CALL replay)"
            ));
        }
    }
    if let Some(reason) = scan_target_depth(frame) {
        return PrestateEligibility::Fallback(reason);
    }
    PrestateEligibility::Eligible
}

/// Walk the target-depth context (root + `DELEGATECALL`/`CALLCODE` descendants) for a frame type that
/// rules out the net form. Returns the first disqualifying reason, if any.
fn scan_target_depth(frame: &CallFrame) -> Option<String> {
    for c in &frame.calls {
        match c.typ.as_str() {
            "CALL" => {
                return Some(format!(
                    "regular CALL to {:?} at target depth (needs CALL replay)",
                    c.to
                ));
            }
            "CREATE" | "CREATE2" => {
                return Some(format!(
                    "{} at target depth (contract creation is not representable)",
                    c.typ
                ));
            }
            "SELFDESTRUCT" => {
                return Some("SELFDESTRUCT at target depth (not representable)".into());
            }
            "DELEGATECALL" | "CALLCODE" => {
                if let Some(r) = scan_target_depth(c) {
                    return Some(r);
                }
            }
            "STATICCALL" => {} // read-only — cannot change state, safely ignored
            other => return Some(format!("unhandled call frame type {other:?}")),
        }
    }
    None
}

/// Why a call the net form could encode still needs the struct-log encoder: its target-depth
/// context made a `STATICCALL` to a contract, whose result the program must pin. `None` if it
/// made none (a read of a precompile pins nothing).
///
/// `frame` is the call's `callTracer` frame. The reads the call frame can't show (`BALANCE`,
/// `SELFBALANCE`, `EXTCODEHASH`, `EXTCODESIZE`) are found with [`OBSERVATION_TRACER`], or, where
/// an endpoint won't run it, [`opcode_observation`].
pub fn static_observation(frame: &CallFrame) -> Option<String> {
    static_read_at_target_depth(frame)
        .map(|to| format!("STATICCALL to {to} at target depth (needs a STATICCALL_CHECK)"))
}

/// A `debug_traceCall` JavaScript tracer that returns the first `BALANCE`, `SELFBALANCE`,
/// `EXTCODEHASH` or `EXTCODESIZE` the target-depth context ran, or `null`: the scan
/// [`opcode_observation`] makes, done on the node so the response stays a few bytes however long
/// the call runs.
///
/// The target-depth context is depth 1 and the `DELEGATECALL`/`CALLCODE` frames it opens, at any
/// depth, but nothing a `CALL`, `STATICCALL` or `CREATE` opens.
pub const OBSERVATION_TRACER: &str = r#"{
  emitting: [false, true],
  opened: null,
  found: null,
  step: function(log, db) {
    if (this.found !== null) return;
    var depth = log.getDepth();
    if (this.opened !== null) {
      var parent = this.opened[0];
      if (depth === parent + 1) {
        this.emitting.length = depth;
        this.emitting.push(this.emitting[parent] && this.opened[1]);
      }
      this.opened = null;
    }
    while (this.emitting.length < depth + 1) this.emitting.push(false);
    this.emitting.length = depth + 1;
    if (!this.emitting[depth]) return;
    var op = log.op.toString();
    if (op === "DELEGATECALL" || op === "CALLCODE") this.opened = [depth, true];
    else if (op === "CALL" || op === "STATICCALL" || op === "CREATE" || op === "CREATE2") this.opened = [depth, false];
    else if (op === "BALANCE" || op === "SELFBALANCE" || op === "EXTCODEHASH" || op === "EXTCODESIZE") this.found = op;
  },
  fault: function(log, db) {},
  result: function(ctx, db) { return this.found; }
}"#;

/// How many steps of a call [`opcode_observation`] reads at most: past this, the call may have
/// observed something, so it goes to the struct-log encoder.
pub const OBSERVATION_SCAN_STEP_LIMIT: u64 = 500_000;

/// Why a call needs the struct-log encoder, from a struct-log trace of it with stack and memory
/// off and at most `limit` steps: its target-depth context ran a `BALANCE`, `SELFBALANCE`,
/// `EXTCODEHASH` or `EXTCODESIZE`, or the trace stopped at `limit` before it could tell. `None` if
/// it observed nothing. [`OBSERVATION_TRACER`] makes the same scan on the node.
pub fn opcode_observation(opcodes: &DefaultFrame, limit: u64) -> Option<String> {
    // The target-depth context, indexed by depth.
    let mut emitting = vec![false, true];
    let mut opened: Option<(usize, bool)> = None;
    for log in &opcodes.struct_logs {
        let depth = log.depth as usize;
        if let Some((parent, delegated)) = opened.take()
            && depth == parent + 1
        {
            emitting.truncate(depth);
            emitting.push(emitting[parent] && delegated);
        }
        emitting.resize(depth + 1, false);
        emitting.truncate(depth + 1);
        if !emitting[depth] {
            continue;
        }
        match log.op.as_ref() {
            "DELEGATECALL" | "CALLCODE" => opened = Some((depth, true)),
            "CALL" | "STATICCALL" | "CREATE" | "CREATE2" => opened = Some((depth, false)),
            op @ ("BALANCE" | "SELFBALANCE" | "EXTCODEHASH" | "EXTCODESIZE") => {
                return Some(format!("{op} at target depth (needs a check op)"));
            }
            _ => {}
        }
    }
    (opcodes.struct_logs.len() as u64 >= limit)
        .then(|| format!("the call ran past the {limit}-step observation scan"))
}

/// The target of the first `STATICCALL` the target-depth context makes to a contract.
fn static_read_at_target_depth(frame: &CallFrame) -> Option<Address> {
    frame.calls.iter().find_map(|c| match c.typ.as_str() {
        "STATICCALL" => c.to.filter(|to| !is_precompile(*to)),
        "DELEGATECALL" | "CALLCODE" => static_read_at_target_depth(c),
        _ => None,
    })
}

/// The failure reason a `callTracer` frame carries, or `None` if it completed successfully.
///
/// Clients disagree on which field they populate — geth-family tracers set `error` and add
/// `revertReason` when the reason decodes, while others report only the latter — so a frame is
/// treated as failed if either is present.
pub fn frame_failure(frame: &CallFrame) -> Option<&str> {
    frame.error.as_deref().or(frame.revert_reason.as_deref())
}

/// True iff `addr`'s storage actually changed between pre and post (ignores balance/nonce-only deltas).
fn account_storage_changed(diff: &DiffMode, addr: Address) -> bool {
    let empty = BTreeMap::new();
    let pre = diff.pre.get(&addr).map(|a| &a.storage).unwrap_or(&empty);
    let post = diff.post.get(&addr).map(|a| &a.storage).unwrap_or(&empty);
    let mut keys: BTreeSet<B256> = BTreeSet::new();
    keys.extend(pre.keys().copied());
    keys.extend(post.keys().copied());
    keys.into_iter().any(|k| {
        pre.get(&k).copied().unwrap_or(B256::ZERO) != post.get(&k).copied().unwrap_or(B256::ZERO)
    })
}

/// Build the net-form `Vec<StateUpdate>` for an eligible call from its prestate `diff` (consumer
/// storage → STORE) and `callTracer` `frame` (events → LOGn). Storage is emitted slot-sorted
/// (deterministic) with the net final value per slot; logs in true emission order.
///
/// Only slots whose value actually changed produce a store — this is what makes the net form byte-
/// different from the struct-log encoders, which emit one store per write with no pre-state to
/// compare against. Ordering stores before logs is unobservable here because eligibility rules out
/// any external call that could run between them.
///
/// The caller must have classified the call [`PrestateEligibility::Eligible`] first; this function
/// does not re-check.
pub fn build_state_updates_from_prestate(
    consumer: Address,
    diff: &DiffMode,
    frame: &CallFrame,
) -> Vec<StateUpdate> {
    let mut updates = Vec::new();
    let empty = BTreeMap::new();
    let pre = diff
        .pre
        .get(&consumer)
        .map(|a| &a.storage)
        .unwrap_or(&empty);
    let post = diff
        .post
        .get(&consumer)
        .map(|a| &a.storage)
        .unwrap_or(&empty);

    // Union of touched slots; emit a STORE wherever the value actually changed (covers zeroing).
    let mut slots: BTreeSet<B256> = BTreeSet::new();
    slots.extend(pre.keys().copied());
    slots.extend(post.keys().copied());
    for slot in slots {
        let old = pre.get(&slot).copied().unwrap_or(B256::ZERO);
        let new = post.get(&slot).copied().unwrap_or(B256::ZERO);
        if old != new {
            updates.push(StateUpdate::Store(IStateUpdateTypes::Store {
                slot,
                value: new,
            }));
        }
    }

    for lg in ordered_target_depth_logs(frame) {
        updates.push(log_to_update(lg));
    }
    updates
}

/// Collect the consumer's target-depth logs (root + `DELEGATECALL`/`CALLCODE` descendants; excluding
/// regular `CALL` — those re-emit on replay — and `STATICCALL`) in TRUE EMISSION ORDER, using each
/// log's `position` to interleave a frame's logs with its delegatecall children, then applying the
/// global `index` as an authoritative override when every log carries one.
fn ordered_target_depth_logs(frame: &CallFrame) -> Vec<&CallLogFrame> {
    let mut logs: Vec<&CallLogFrame> = Vec::new();
    collect_target_depth_logs(frame, &mut logs);
    if logs.len() > 1 && logs.iter().all(|l| l.index.is_some()) {
        // Global truth when every log carries it; a stable sort on `Option<u64>` (ascending, `None`
        // first) never worsens the position order established above.
        logs.sort_by_key(|l| l.index);
    }
    logs
}

/// Interleave `frame`'s own logs with its sub-calls by `position`: a log with position `p` was emitted
/// after `p` sub-calls, so it belongs immediately before sub-call `p`. We recurse only into transparent
/// `DELEGATECALL`/`CALLCODE` children; `STATICCALL`/regular-`CALL` children are excluded but still
/// advance the position counter.
///
/// A reverted sub-call's events never made it into the receipt, so failed frames are skipped entirely
/// (see [`frame_failure`]). Geth-family `callTracer` implementations strip logs under a failed frame,
/// but anvil does not, and the diff must not depend on which client answered: emitting a log the EVM
/// discarded would put an event on chain that the traced execution never produced.
fn collect_target_depth_logs<'a>(frame: &'a CallFrame, out: &mut Vec<&'a CallLogFrame>) {
    let mut logs: Vec<&CallLogFrame> = frame.logs.iter().collect();
    logs.sort_by_key(|l| l.position.unwrap_or(0)); // stable → preserves vector order within a position
    let mut li = 0;
    for (call_idx, c) in frame.calls.iter().enumerate() {
        while li < logs.len() && (logs[li].position.unwrap_or(0) as usize) <= call_idx {
            out.push(logs[li]);
            li += 1;
        }
        if (c.typ == "DELEGATECALL" || c.typ == "CALLCODE") && frame_failure(c).is_none() {
            collect_target_depth_logs(c, out);
        }
    }
    while li < logs.len() {
        out.push(logs[li]);
        li += 1;
    }
}

/// Map a call-frame log to the matching LOG0..LOG4 `StateUpdate` by topic count.
fn log_to_update(log: &CallLogFrame) -> StateUpdate {
    let topics = log.topics.clone().unwrap_or_default();
    let data = log.data.clone().unwrap_or_default();
    match topics.len() {
        0 => StateUpdate::Log0(IStateUpdateTypes::Log0 { data }),
        1 => StateUpdate::Log1(IStateUpdateTypes::Log1 {
            data,
            topic1: topics[0],
        }),
        2 => StateUpdate::Log2(IStateUpdateTypes::Log2 {
            data,
            topic1: topics[0],
            topic2: topics[1],
        }),
        3 => StateUpdate::Log3(IStateUpdateTypes::Log3 {
            data,
            topic1: topics[0],
            topic2: topics[1],
            topic3: topics[2],
        }),
        _ => StateUpdate::Log4(IStateUpdateTypes::Log4 {
            data,
            topic1: topics[0],
            topic2: topics[1],
            topic3: topics[2],
            topic4: topics[3],
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Bytes;
    use alloy_rpc_types::trace::geth::AccountState;

    fn b256(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    fn acct(storage: &[(B256, B256)]) -> AccountState {
        AccountState {
            storage: storage.iter().copied().collect(),
            ..Default::default()
        }
    }

    fn frame(typ: &str, logs: Vec<CallLogFrame>, calls: Vec<CallFrame>) -> CallFrame {
        CallFrame {
            typ: typ.to_string(),
            logs,
            calls,
            ..Default::default()
        }
    }

    fn log(topics: &[B256], data: u8) -> CallLogFrame {
        CallLogFrame {
            topics: Some(topics.to_vec()),
            data: Some(Bytes::from(vec![data])),
            ..Default::default()
        }
    }

    fn log_idx(topics: &[B256], data: u8, index: u64) -> CallLogFrame {
        CallLogFrame {
            index: Some(index),
            ..log(topics, data)
        }
    }

    fn log_pos(topics: &[B256], data: u8, position: u64) -> CallLogFrame {
        CallLogFrame {
            position: Some(position),
            ..log(topics, data)
        }
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

    fn log1_topics(updates: &[StateUpdate]) -> Vec<B256> {
        updates
            .iter()
            .filter_map(|u| match u {
                StateUpdate::Log1(l) => Some(l.topic1),
                _ => None,
            })
            .collect()
    }

    fn is_eligible(e: PrestateEligibility) -> bool {
        matches!(e, PrestateEligibility::Eligible)
    }

    const CONSUMER: Address = Address::repeat_byte(0xAA);

    // ---- build_state_updates_from_prestate: storage diffing ------------------------------------

    #[test]
    fn store_emitted_for_changed_slot() {
        let mut diff = DiffMode::default();
        diff.pre.insert(CONSUMER, acct(&[(b256(1), b256(0x11))]));
        diff.post.insert(CONSUMER, acct(&[(b256(1), b256(0x22))]));
        let u = build_state_updates_from_prestate(CONSUMER, &diff, &frame("CALL", vec![], vec![]));
        assert_eq!(stores(&u), vec![(b256(1), b256(0x22))]);
    }

    #[test]
    fn zeroing_emits_store_to_zero_whether_post_holds_zero_or_omits_key() {
        let mut a = DiffMode::default();
        a.pre.insert(CONSUMER, acct(&[(b256(7), b256(0x99))]));
        a.post.insert(CONSUMER, acct(&[(b256(7), B256::ZERO)]));
        let mut b = DiffMode::default();
        b.pre.insert(CONSUMER, acct(&[(b256(7), b256(0x99))]));
        b.post.insert(CONSUMER, acct(&[]));
        for diff in [a, b] {
            let u =
                build_state_updates_from_prestate(CONSUMER, &diff, &frame("CALL", vec![], vec![]));
            assert_eq!(
                stores(&u),
                vec![(b256(7), B256::ZERO)],
                "zeroing must emit STORE(slot, 0)"
            );
        }
    }

    #[test]
    fn net_zero_slot_is_omitted() {
        let mut diff = DiffMode::default();
        diff.pre.insert(CONSUMER, acct(&[(b256(3), b256(0x55))]));
        diff.post.insert(CONSUMER, acct(&[(b256(3), b256(0x55))]));
        let u = build_state_updates_from_prestate(CONSUMER, &diff, &frame("CALL", vec![], vec![]));
        assert!(stores(&u).is_empty(), "pre == post must produce no STORE");
    }

    #[test]
    fn empty_diff_and_empty_frame_produce_empty_updates() {
        let u = build_state_updates_from_prestate(
            CONSUMER,
            &DiffMode::default(),
            &frame("CALL", vec![], vec![]),
        );
        assert!(u.is_empty());
    }

    #[test]
    fn only_consumer_storage_becomes_stores() {
        let other = Address::repeat_byte(0xBB);
        let mut diff = DiffMode::default();
        diff.post.insert(CONSUMER, acct(&[(b256(1), b256(0x11))]));
        diff.post.insert(other, acct(&[(b256(2), b256(0x22))]));
        let u = build_state_updates_from_prestate(CONSUMER, &diff, &frame("CALL", vec![], vec![]));
        assert_eq!(
            stores(&u),
            vec![(b256(1), b256(0x11))],
            "non-consumer storage must be excluded"
        );
    }

    // ---- log collection: target-depth context, ordering ---------------------------------------

    #[test]
    fn topic_count_maps_to_log0_through_log4() {
        let f = frame(
            "CALL",
            vec![
                log(&[], 0),
                log(&[b256(1)], 1),
                log(&[b256(1), b256(2)], 2),
                log(&[b256(1), b256(2), b256(3)], 3),
                log(&[b256(1), b256(2), b256(3), b256(4)], 4),
            ],
            vec![],
        );
        let u: Vec<StateUpdate> = ordered_target_depth_logs(&f)
            .into_iter()
            .map(log_to_update)
            .collect();
        assert!(matches!(u[0], StateUpdate::Log0(_)));
        assert!(matches!(u[1], StateUpdate::Log1(_)));
        assert!(matches!(u[2], StateUpdate::Log2(_)));
        assert!(matches!(u[3], StateUpdate::Log3(_)));
        assert!(matches!(u[4], StateUpdate::Log4(_)));
    }

    #[test]
    fn delegatecall_logs_included_staticcall_and_regular_call_excluded() {
        let f = frame(
            "CALL",
            vec![log(&[b256(0xA1)], 1)], // consumer's own log → included
            vec![
                frame("DELEGATECALL", vec![log(&[b256(0xDE)], 1)], vec![]), // transparent → included
                frame("STATICCALL", vec![log(&[b256(0x5A)], 1)], vec![]),   // read-only → excluded
                frame("CALL", vec![log(&[b256(0xC1)], 1)], vec![]),         // replays → excluded
            ],
        );
        let u: Vec<StateUpdate> = ordered_target_depth_logs(&f)
            .into_iter()
            .map(log_to_update)
            .collect();
        assert_eq!(
            log1_topics(&u),
            vec![b256(0xA1), b256(0xDE)],
            "only consumer + delegatecall logs belong in the diff"
        );
    }

    #[test]
    fn logs_sorted_by_global_index() {
        let f = frame(
            "CALL",
            vec![log_idx(&[b256(0x03)], 1, 3)],
            vec![frame(
                "DELEGATECALL",
                vec![log_idx(&[b256(0x01)], 1, 1)],
                vec![],
            )],
        );
        let u: Vec<StateUpdate> = ordered_target_depth_logs(&f)
            .into_iter()
            .map(log_to_update)
            .collect();
        assert_eq!(
            log1_topics(&u),
            vec![b256(0x01), b256(0x03)],
            "logs in global execution order"
        );
    }

    #[test]
    fn logs_interleaved_by_position_without_global_index() {
        // Consumer emits A, DELEGATECALLs a lib that emits C, then emits B — NO global index, only
        // position. True order is [A, C, B]; naive DFS would give [A, B, C].
        let f = frame(
            "CALL",
            vec![log_pos(&[b256(0xAA)], 1, 0), log_pos(&[b256(0xBB)], 1, 1)],
            vec![frame("DELEGATECALL", vec![log(&[b256(0xCC)], 1)], vec![])],
        );
        let u: Vec<StateUpdate> = ordered_target_depth_logs(&f)
            .into_iter()
            .map(log_to_update)
            .collect();
        assert_eq!(
            log1_topics(&u),
            vec![b256(0xAA), b256(0xCC), b256(0xBB)],
            "position must interleave delegatecall logs in emission order without a global index"
        );
    }

    // ---- classify: prestate-eligibility -------------------------------------------------------

    fn diff_consumer_only() -> DiffMode {
        let mut d = DiffMode::default();
        d.pre.insert(CONSUMER, acct(&[(b256(1), b256(0x00))]));
        d.post.insert(CONSUMER, acct(&[(b256(1), b256(0x01))]));
        d
    }

    #[test]
    fn eligible_pure_self_compute_with_staticcall_and_delegatecall() {
        let f = frame(
            "CALL",
            vec![log(&[b256(1)], 1)],
            vec![
                frame("STATICCALL", vec![], vec![]),
                frame("DELEGATECALL", vec![log(&[b256(2)], 1)], vec![]),
            ],
        );
        assert!(is_eligible(classify_prestate_eligibility(
            &f,
            &diff_consumer_only(),
            CONSUMER
        )));
    }

    #[test]
    fn fallback_on_regular_call() {
        let f = frame("CALL", vec![], vec![frame("CALL", vec![], vec![])]);
        assert!(!is_eligible(classify_prestate_eligibility(
            &f,
            &diff_consumer_only(),
            CONSUMER
        )));
    }

    #[test]
    fn fallback_on_create() {
        let f = frame("CALL", vec![], vec![frame("CREATE", vec![], vec![])]);
        assert!(!is_eligible(classify_prestate_eligibility(
            &f,
            &diff_consumer_only(),
            CONSUMER
        )));
    }

    #[test]
    fn fallback_on_regular_call_nested_in_delegatecall() {
        let f = frame(
            "CALL",
            vec![],
            vec![frame(
                "DELEGATECALL",
                vec![],
                vec![frame("CALL", vec![], vec![])],
            )],
        );
        assert!(!is_eligible(classify_prestate_eligibility(
            &f,
            &diff_consumer_only(),
            CONSUMER
        )));
    }

    #[test]
    fn fallback_on_cross_contract_storage() {
        let other = Address::repeat_byte(0xBB);
        let mut diff = diff_consumer_only();
        diff.pre.insert(other, acct(&[(b256(9), b256(0x00))]));
        diff.post.insert(other, acct(&[(b256(9), b256(0x07))]));
        let f = frame("CALL", vec![], vec![]);
        assert!(!is_eligible(classify_prestate_eligibility(
            &f, &diff, CONSUMER
        )));
    }

    #[test]
    fn fallback_when_top_level_call_reverted() {
        let f = CallFrame {
            error: Some("execution reverted".into()),
            ..frame("CALL", vec![], vec![])
        };
        assert!(
            !is_eligible(classify_prestate_eligibility(
                &f,
                &diff_consumer_only(),
                CONSUMER
            )),
            "a reverted call has an empty net diff but the struct-log paths still emit its writes"
        );
    }

    /// A client that decodes the revert reason but leaves `error` unset must still be recognised as a
    /// failure, or the guard above depends on which node answered.
    #[test]
    fn fallback_when_only_revert_reason_is_populated() {
        let f = CallFrame {
            revert_reason: Some("Custom()".into()),
            ..frame("CALL", vec![], vec![])
        };
        assert!(!is_eligible(classify_prestate_eligibility(
            &f,
            &diff_consumer_only(),
            CONSUMER
        )));
    }

    #[test]
    fn reverted_delegatecall_logs_are_excluded_via_revert_reason_alone() {
        let reverted = CallFrame {
            revert_reason: Some("Custom()".into()),
            ..frame("DELEGATECALL", vec![log(&[b256(0xDE)], 1)], vec![])
        };
        let f = frame("CALL", vec![log(&[b256(0xA1)], 1)], vec![reverted]);
        let u: Vec<StateUpdate> = ordered_target_depth_logs(&f)
            .into_iter()
            .map(log_to_update)
            .collect();
        assert_eq!(
            log1_topics(&u),
            vec![b256(0xA1)],
            "a frame reporting only a revert reason is still a failed frame"
        );
    }

    #[test]
    fn fallback_when_root_frame_targets_another_address() {
        let f = CallFrame {
            to: Some(Address::repeat_byte(0xBB)),
            ..frame("CALL", vec![], vec![])
        };
        assert!(
            !is_eligible(classify_prestate_eligibility(
                &f,
                &diff_consumer_only(),
                CONSUMER
            )),
            "frame and consumer must describe the same call"
        );
    }

    #[test]
    fn reverted_delegatecall_logs_are_excluded() {
        let reverted = CallFrame {
            error: Some("execution reverted".into()),
            ..frame("DELEGATECALL", vec![log(&[b256(0xDE)], 1)], vec![])
        };
        let f = frame("CALL", vec![log(&[b256(0xA1)], 1)], vec![reverted]);
        let u: Vec<StateUpdate> = ordered_target_depth_logs(&f)
            .into_iter()
            .map(log_to_update)
            .collect();
        assert_eq!(
            log1_topics(&u),
            vec![b256(0xA1)],
            "the EVM discarded the reverted frame's events; the diff must not re-emit them"
        );
    }

    /// A reverted `STATICCALL` changes nothing on either path, so it must not cost the call its net
    /// form — the revert guards above are about frames that could have written state, not any failure
    /// anywhere in the tree.
    #[test]
    fn reverted_staticcall_child_stays_eligible() {
        let reverted = CallFrame {
            error: Some("execution reverted".into()),
            ..frame("STATICCALL", vec![], vec![])
        };
        let f = frame("CALL", vec![], vec![reverted]);
        assert!(is_eligible(classify_prestate_eligibility(
            &f,
            &diff_consumer_only(),
            CONSUMER
        )));
    }

    // ---- net form vs the canonical struct-log encoder --------------------------------------------

    /// Pins the two ways the net form's bytes differ from the canonical encoder's for the same call,
    /// so neither encoding can drift into the other unnoticed. Both are correct programs reaching the
    /// same end state; they are not interchangeable, because the digest commits to the bytes.
    ///
    /// Workload (no external calls, so canonical emits a single final slice):
    ///   * `0xAA` 0 → 1 → 2   — written twice, net change
    ///   * `0xBB` 5 → 9 → 5   — written twice, net zero
    ///   * `0xCC` 0 → 7       — written once
    #[test]
    fn net_form_omits_slots_the_canonical_encoder_re_asserts() {
        use crate::trace::compute_state_updates_canonical;
        use alloy_primitives::U256;
        use alloy_rpc_types::trace::geth::{DefaultFrame, StructLog};

        const TARGET: Address = Address::repeat_byte(0x77);
        fn s(n: u64) -> B256 {
            B256::from(U256::from(n))
        }
        // SSTORE struct log; the stack is stored bottom-to-top, so the extractor sees [slot, value].
        fn sstore(slot: B256, value: B256) -> StructLog {
            StructLog {
                op: "SSTORE".into(),
                depth: 1,
                stack: Some(vec![value.into(), slot.into()]),
                memory: Some(Vec::new()),
                ..Default::default()
            }
        }

        let canonical = compute_state_updates_canonical(
            DefaultFrame {
                struct_logs: vec![
                    sstore(s(0xAA), s(1)),
                    sstore(s(0xAA), s(2)),
                    sstore(s(0xBB), s(9)),
                    sstore(s(0xBB), s(5)),
                    sstore(s(0xCC), s(7)),
                ],
                ..Default::default()
            },
            TARGET,
        )
        .expect("canonical extraction")
        .0;

        // The same call as prestateTracer diffMode reports it: net-zero `0xBB` is dropped from both
        // sides, and zero pre-values are omitted too, so only `0xAA` and `0xCC` survive, in `post`.
        // (A slot listed in `pre` alone would mean a zeroing, which is a real change.)
        let mut diff = DiffMode::default();
        diff.pre.insert(TARGET, acct(&[]));
        diff.post
            .insert(TARGET, acct(&[(s(0xAA), s(2)), (s(0xCC), s(7))]));
        let net = build_state_updates_from_prestate(TARGET, &diff, &frame("CALL", vec![], vec![]));

        assert_eq!(
            stores(&canonical),
            vec![(s(0xAA), s(2)), (s(0xBB), s(5)), (s(0xCC), s(7))],
            "canonical collapses repeated writes but has no pre-state, so it re-asserts 0xBB"
        );
        assert_eq!(
            stores(&net),
            vec![(s(0xAA), s(2)), (s(0xCC), s(7))],
            "the net form drops 0xBB entirely — prestateTracer never reports a net-zero slot"
        );
        assert_ne!(
            stores(&canonical),
            stores(&net),
            "the two encodings must stay distinct; collapsing them silently would fork the digest"
        );
    }

    #[test]
    fn balance_only_change_of_other_account_does_not_force_fallback() {
        let other = Address::repeat_byte(0xBB);
        let mut diff = diff_consumer_only();
        diff.post.insert(
            other,
            AccountState {
                ..Default::default()
            },
        ); // no storage delta
        let f = frame("CALL", vec![], vec![]);
        assert!(is_eligible(classify_prestate_eligibility(
            &f, &diff, CONSUMER
        )));
    }
}

#[cfg(test)]
mod observation_tests {
    use super::*;
    use alloy_rpc_types::trace::geth::StructLog;

    fn frame(typ: &str, to: Address, calls: Vec<CallFrame>) -> CallFrame {
        CallFrame {
            typ: typ.to_string(),
            to: Some(to),
            calls,
            ..Default::default()
        }
    }

    fn ops(steps: &[(&str, u64)]) -> DefaultFrame {
        DefaultFrame {
            failed: false,
            gas: 0,
            return_value: Default::default(),
            struct_logs: steps
                .iter()
                .map(|(op, depth)| StructLog {
                    pc: 0,
                    op: op.to_string().into(),
                    gas: 0,
                    gas_cost: 0,
                    depth: *depth,
                    error: None,
                    stack: None,
                    return_data: None,
                    memory: None,
                    memory_size: None,
                    storage: None,
                    refund_counter: None,
                })
                .collect(),
        }
    }

    const ROOT: Address = Address::new([0x77; 20]);
    const OTHER: Address = Address::new([0xcc; 20]);

    #[test]
    fn a_static_read_of_a_contract_needs_a_check() {
        let root = frame("CALL", ROOT, vec![frame("STATICCALL", OTHER, vec![])]);
        assert!(static_observation(&root).is_some());
    }

    #[test]
    fn a_static_read_through_a_delegatecall_needs_a_check() {
        let lib = frame(
            "DELEGATECALL",
            OTHER,
            vec![frame("STATICCALL", OTHER, vec![])],
        );
        let root = frame("CALL", ROOT, vec![lib]);
        assert!(static_observation(&root).is_some());
    }

    #[test]
    fn a_precompile_read_needs_nothing() {
        let root = frame(
            "CALL",
            ROOT,
            vec![frame("STATICCALL", Address::with_last_byte(1), vec![])],
        );
        assert_eq!(static_observation(&root), None);
    }

    #[test]
    fn balance_reads_at_target_depth_need_a_check() {
        assert!(opcode_observation(&ops(&[("PUSH1", 1), ("SELFBALANCE", 1)]), 100).is_some());
        let through_library = ops(&[("DELEGATECALL", 1), ("EXTCODESIZE", 2), ("STOP", 2)]);
        assert!(opcode_observation(&through_library, 100).is_some());
    }

    /// What a static callee reads replays natively; only the call itself is the program's.
    #[test]
    fn reads_inside_another_contract_need_nothing() {
        let inside = ops(&[("STATICCALL", 1), ("BALANCE", 2), ("STOP", 2), ("POP", 1)]);
        assert_eq!(opcode_observation(&inside, 100), None);
    }

    /// A trace cut off at the limit can't show what came after, so it counts as observing.
    #[test]
    fn a_trace_at_the_limit_may_have_observed() {
        let quiet = ops(&[("PUSH1", 1), ("POP", 1), ("STOP", 1)]);
        assert_eq!(opcode_observation(&quiet, 4), None);
        assert!(opcode_observation(&quiet, 3).is_some());
    }
}

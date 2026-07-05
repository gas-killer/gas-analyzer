//! Prestate-based state-update extraction (the cheap fast path).
//!
//! `compute_state_updates` (see [`crate::trace`]) builds the replay script from a full struct-log
//! trace, which is `O(execution steps)` and times out on heavy-compute tracked functions even when the
//! resulting diff is tiny. This module reconstructs the SAME state updates from two far cheaper
//! tracers — `prestateTracer` in `diffMode` (net storage diff, `O(changed slots)`) and `callTracer`
//! with logs (the call tree + events) — but ONLY when doing so is sound.
//!
//! A net storage diff cannot distinguish the consumer's top-level writes from writes induced by a
//! sub-`CALL` (which `compute_state_updates` represents as a CALL op whose internals re-execute on
//! replay). So [`classify_prestate_eligibility`] reproduces that model: the consumer's frame is
//! "target depth", `DELEGATECALL`/`CALLCODE` are transparent, and the fast path is sound only when the
//! call and every transparent frame completed without reverting, the consumer touches no other
//! account's storage, and it makes no regular `CALL`, `CREATE`, or `SELFDESTRUCT`. Callers run the
//! cheap tracers, classify, and either [`build_state_updates_from_prestate`] (eligible) or fall back
//! to the struct-log path (not eligible / tracers unsupported).
//!
//! These are pure functions over the tracer outputs — no async, no I/O — to keep them in `core`.

use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, B256};
use alloy_rpc_types::trace::geth::{CallFrame, CallLogFrame, DiffMode};

use crate::types::{IStateUpdateTypes, StateUpdate};

/// Whether the prestate fast path can soundly reconstruct the diff, or the caller must fall back to the
/// struct-log path.
#[derive(Debug, Clone)]
pub enum PrestateEligibility {
    /// The fast path is sound: call [`build_state_updates_from_prestate`].
    Eligible,
    /// The fast path cannot represent this call; fall back to `compute_state_updates`. Carries a
    /// human-readable reason for logging.
    Fallback(String),
}

/// Decide whether the prestate fast path is sound for a call, given its `callTracer` frame and
/// `prestateTracer` diff. Mirrors `compute_state_updates`'s replay-script model: STORE/LOG/CALL live at
/// "target depth" (the `consumer` frame), `DELEGATECALL`/`CALLCODE` are transparent. We must fall back
/// when:
///   * the call (or a transparent frame at target depth) reverted — `compute_state_updates` is
///     revert-unaware and extracts the rolled-back ops from the struct log, while they are absent
///     from a net diff (and clients disagree on pruning reverted frames' logs), so the two paths
///     would diverge, or
///   * the consumer changes a non-consumer account's storage (only CALL replay reproduces that), or
///   * the consumer makes a regular `CALL` at target depth (its internals re-execute; a net diff
///     can't separate those from top-level writes), or
///   * the consumer does `CREATE`/`CREATE2` (the struct-log path extracts a `Create`/`Create2` op
///     that re-executes the captured initcode on replay; a net diff cannot reconstruct it) or
///     `SELFDESTRUCT` (the struct-log path reports it via `skipped_opcodes`).
///
/// `STATICCALL` is read-only and ignored. Known caveat shared with nothing we can detect here:
/// `TSTORE` leaves no trace in either cheap tracer, so an eligible call using transient storage
/// loses the struct-log path's `TSTORE` skipped-opcode warning — the extracted diff itself is
/// unaffected (transient storage never outlives the transaction).
pub fn classify_prestate_eligibility(
    frame: &CallFrame,
    diff: &DiffMode,
    consumer: Address,
) -> PrestateEligibility {
    if let Some(err) = frame.error.as_deref().or(frame.revert_reason.as_deref()) {
        return PrestateEligibility::Fallback(format!(
            "call reverted/failed ({err:?}); struct-log extraction of rolled-back ops can't be \
             mirrored by a net diff"
        ));
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

/// Walk the target-depth context (root + `DELEGATECALL`/`CALLCODE` descendants) for a frame type or
/// a reverted transparent frame that forces the struct-log fallback. Returns the first disqualifying
/// reason, if any.
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
                if let Some(err) = c.error.as_deref().or(c.revert_reason.as_deref()) {
                    // The child's writes rolled back (absent from the net diff), but the
                    // struct-log path still extracts them — only fallback keeps the paths equal.
                    return Some(format!(
                        "reverted {} at target depth ({err:?}); rolled-back ops need the \
                         struct-log path",
                        c.typ
                    ));
                }
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

/// Build `Vec<StateUpdate>` for an eligible call from its prestate `diff` (consumer storage → STORE)
/// and `callTracer` `frame` (events → LOGn). Storage is emitted slot-sorted (deterministic) with the
/// net final value per slot; logs in true emission order. Mirrors the slot set `compute_state_updates`
/// produces for a no-sub-call function (minus its redundant intermediate writes), so the applied diff
/// reproduces the same final state and events.
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
        logs.sort_by_key(|l| l.index.unwrap()); // global truth; stable, never worsens position order
    }
    logs
}

/// Interleave `frame`'s own logs with its sub-calls by `position`: a log with position `p` was emitted
/// after `p` sub-calls, so it belongs immediately before sub-call `p`. We recurse only into transparent
/// `DELEGATECALL`/`CALLCODE` children; `STATICCALL`/regular-`CALL` children are excluded but still
/// advance the position counter.
fn collect_target_depth_logs<'a>(frame: &'a CallFrame, out: &mut Vec<&'a CallLogFrame>) {
    let mut logs: Vec<&CallLogFrame> = frame.logs.iter().collect();
    logs.sort_by_key(|l| l.position.unwrap_or(0)); // stable → preserves vector order within a position
    let mut li = 0;
    for (call_idx, c) in frame.calls.iter().enumerate() {
        while li < logs.len() && (logs[li].position.unwrap_or(0) as usize) <= call_idx {
            out.push(logs[li]);
            li += 1;
        }
        if c.typ == "DELEGATECALL" || c.typ == "CALLCODE" {
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
    fn fallback_on_reverted_root_frame() {
        // A reverted call rolls everything back: the net diff is empty, but the struct-log path
        // still extracts the pre-revert ops. Some clients (anvil/revm-inspectors) even keep the
        // reverted root frame's logs in callTracer output — without this fallback the fast path
        // would emit events that never happened.
        let mut f = frame("CALL", vec![log(&[b256(1)], 1)], vec![]);
        f.error = Some("execution reverted".into());
        assert!(!is_eligible(classify_prestate_eligibility(
            &f,
            &DiffMode::default(),
            CONSUMER
        )));
    }

    #[test]
    fn fallback_on_revert_reason_only_root_frame() {
        let mut f = frame("CALL", vec![], vec![]);
        f.revert_reason = Some("Custom()".into());
        assert!(!is_eligible(classify_prestate_eligibility(
            &f,
            &DiffMode::default(),
            CONSUMER
        )));
    }

    #[test]
    fn fallback_on_reverted_delegatecall_child() {
        // Caught revert: the delegatecall's writes rolled back (absent from the net diff), but the
        // struct-log path extracts them at target depth — the paths would diverge.
        let mut child = frame("DELEGATECALL", vec![], vec![]);
        child.error = Some("execution reverted".into());
        let f = frame("CALL", vec![], vec![child]);
        assert!(!is_eligible(classify_prestate_eligibility(
            &f,
            &diff_consumer_only(),
            CONSUMER
        )));
    }

    #[test]
    fn reverted_staticcall_child_still_eligible() {
        // A reverted STATICCALL cannot have changed state or emitted logs on either path.
        let mut child = frame("STATICCALL", vec![], vec![]);
        child.error = Some("execution reverted".into());
        let f = frame("CALL", vec![], vec![child]);
        assert!(is_eligible(classify_prestate_eligibility(
            &f,
            &diff_consumer_only(),
            CONSUMER
        )));
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

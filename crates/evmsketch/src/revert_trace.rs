//! Call traces of a failed estimate, for `--debug`.
//!
//! When the estimator's CALL to a target reverts, the estimate fails with
//! `RevertingContext(index, target, revertData, callargs)`. Re-running the estimate under a
//! [`TracingInspector`] and keeping only that CALL's subtree shows where the revert started.

use std::cmp::Reverse;

use alloy::primitives::{Address, Bytes};
use alloy::sol_types::{ContractError, GenericRevertReason, RevertReason};
use colorchoice::ColorChoice;
pub use revm_inspectors::tracing::CallTraceArena;
use revm_inspectors::tracing::types::{CallTraceNode, DecodedCallTrace};
use revm_inspectors::tracing::{
    TraceWriter, TraceWriterConfig, TracingInspector, TracingInspectorConfig,
};

/// The call tracer a failed estimate is re-run under.
///
/// Parity's defaults leave precompile calls out of the trace, but a state update can CALL a
/// precompile and fail there, so they are kept.
pub fn tracer() -> TracingInspector {
    TracingInspector::new(
        TracingInspectorConfig::default_parity()
            .set_exclude_precompile_calls(false)
            .set_record_logs(true),
    )
}

/// The arena node of the estimator's failed CALL to `target` with `callargs`.
///
/// Searches the whole tree below the root, so it does not depend on how the estimator reaches its
/// CALLs. Of the failed matches it takes the shallowest, so a reverting call that repeats itself
/// further down is shown from the outermost call; among those, the last, since the estimator stops
/// at the first failure.
pub fn failed_call(arena: &CallTraceArena, target: Address, callargs: &Bytes) -> Option<usize> {
    arena
        .nodes()
        .iter()
        .skip(1)
        .filter(|node| {
            let trace = &node.trace;
            trace.address == target && trace.data == *callargs && !trace.success
        })
        .max_by_key(|node| (Reverse(node.trace.depth), node.idx))
        .map(|node| node.idx)
}

/// A new arena holding `root` and everything under it, `root` first.
///
/// Nodes keep the arena's order, a depth-first pre-order: each node comes before its children, and
/// a node's subtree is contiguous.
pub fn subtree(arena: &CallTraceArena, root: usize) -> CallTraceArena {
    let nodes = arena.nodes();
    let mut order = Vec::new();
    let mut stack = vec![root];
    while let Some(old) = stack.pop() {
        order.push(old);
        stack.extend(nodes[old].children.iter().rev().copied());
    }
    let mut new_index = vec![usize::MAX; nodes.len()];
    for (new, &old) in order.iter().enumerate() {
        new_index[old] = new;
    }

    let mut out = CallTraceArena::default();
    let out_nodes = out.nodes_mut();
    out_nodes.clear();
    out_nodes.extend(order.iter().map(|&old| {
        let node = &nodes[old];
        CallTraceNode {
            parent: node.parent.filter(|_| old != root).map(|p| new_index[p]),
            children: node.children.iter().map(|&c| new_index[c]).collect(),
            idx: new_index[old],
            ..node.clone()
        }
    }));
    out
}

/// `output` as a revert reason safe to print: `Error(string)` or `Panic(uint256)`, or the output
/// itself when it is plain text (as Vyper reverted before `Error(string)`).
///
/// The reason comes from the contract, so control and bidirectional-override characters are
/// escaped rather than written to the terminal. Output that is text only by accident, such as a
/// custom error's bytes that happen to be valid UTF-8, has control characters (its zero padding)
/// and is left as bytes.
pub fn revert_reason(output: &[u8]) -> Option<String> {
    let reason = match GenericRevertReason::decode(output)? {
        RevertReason::ContractError(ContractError::CustomError(_)) => return None,
        RevertReason::ContractError(error) => error.to_string(),
        RevertReason::RawString(text) if !text.is_empty() && !text.chars().any(unsafe_char) => text,
        RevertReason::RawString(_) => return None,
    };
    Some(escape(&reason))
}

/// Characters that would act on the terminal or reorder the text around them.
fn unsafe_char(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// `text` with every [`unsafe_char`] written as a Rust escape.
fn escape(text: &str) -> String {
    text.chars()
        .map(|c| {
            if unsafe_char(c) {
                c.escape_unicode().to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Label every reverted frame whose output is a revert reason (see [`revert_reason`]) with that
/// reason, so the trace shows it instead of raw bytes.
pub fn decode_revert_reasons(arena: &mut CallTraceArena) {
    for node in arena.nodes_mut() {
        let trace = &mut node.trace;
        if trace.success || trace.output.is_empty() {
            continue;
        }
        if let Some(reason) = revert_reason(&trace.output) {
            trace
                .decoded
                .get_or_insert_with(|| Box::new(DecodedCallTrace::default()))
                .return_data = Some(reason);
        }
    }
}

/// `arena` drawn as a call tree, in color when `colors`.
pub fn render(arena: &CallTraceArena, colors: bool) -> String {
    let choice = if colors {
        ColorChoice::Always
    } else {
        ColorChoice::Never
    };
    let mut writer =
        TraceWriter::with_config(Vec::new(), TraceWriterConfig::new().color_choice(choice));
    writer
        .write_arena(arena)
        .expect("writing to a Vec cannot fail");
    String::from_utf8_lossy(&writer.into_writer()).into_owned()
}

/// The trace of the estimator's failed CALL to `target` with `callargs`, revert reasons decoded.
///
/// `None` if no such CALL failed in `arena`.
pub fn render_failed_call(
    arena: &CallTraceArena,
    target: Address,
    callargs: &Bytes,
    colors: bool,
) -> Option<String> {
    let idx = failed_call(arena, target, callargs)?;
    let mut sub = subtree(arena, idx);
    decode_revert_reasons(&mut sub);
    Some(render(&sub, colors))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{B256, U256, address, b256};
    use alloy::sol_types::{Revert, SolError};
    use gas_analyzer_core::nested::FrameProgram;
    use gas_analyzer_core::{IStateUpdateTypes, StateUpdate};
    use gas_analyzer_estimator::{
        SimEnvOpts, estimate_frame_tree_gas, estimate_state_changes_gas,
        inspect_frame_tree_failure, inspect_state_changes, reverting_context,
    };
    use revm::database::{CacheDB, EmptyDB};
    use revm::primitives::hardfork::SpecId;
    use revm::state::{AccountInfo, Bytecode};

    const ESTIMATOR: Address = address!("0x000000000000000000000000000000000000e570");
    const CALLER: Address = address!("0x00000000000000000000000000000000000ca11e");
    /// Returns without doing anything.
    const OK: Address = address!("0x00000000000000000000000000000000000c0a0a");
    /// Calls `INNER`, then reverts with whatever it returned.
    const OUTER: Address = address!("0x00000000000000000000000000000000000c0b0b");
    /// Reverts with `Error(reason)`.
    const INNER: Address = address!("0x00000000000000000000000000000000000c0c0c");
    /// The BN254 pairing precompile, which fails on input that is not a multiple of 192 bytes.
    const PAIRING: Address = address!("0x0000000000000000000000000000000000000008");

    fn sim_env() -> SimEnvOpts {
        SimEnvOpts {
            number: 100,
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            coinbase: Address::ZERO,
            prevrandao: b256!("0x0000000000000000000000000000000000000000000000000000000000000000"),
            gas_price: 0,
            basefee: 0,
            difficulty: U256::ZERO,
            spec: SpecId::PRAGUE,
            value: U256::ZERO,
            verify_observations: false,
        }
    }

    fn set_code(db: &mut CacheDB<EmptyDB>, at: Address, code: Vec<u8>) {
        let code = Bytecode::new_raw(code.into());
        db.insert_account_info(
            at,
            AccountInfo {
                code_hash: code.hash_slow(),
                code: Some(code),
                ..Default::default()
            },
        );
    }

    /// `OK`, `OUTER` and `INNER` deployed on an empty state, `INNER` reverting with `reason`.
    fn db_with_reason(reason: &str) -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        set_code(&mut db, OK, vec![0x00]);

        // CALL(gas, INNER, 0, 0, 0, 0, 0); RETURNDATACOPY(0, 0, size); REVERT(0, size)
        let mut outer = vec![0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0, 0x60, 0, 0x73];
        outer.extend_from_slice(INNER.as_slice());
        outer.extend_from_slice(&[
            0x5a, 0xf1, 0x50, 0x3d, 0x60, 0, 0x60, 0, 0x3e, 0x3d, 0x60, 0, 0xfd,
        ]);
        set_code(&mut db, OUTER, outer);

        // CODECOPY the payload after the 12-byte prologue to memory 0, then REVERT with it.
        let payload = Revert::from(reason).abi_encode();
        let len = u8::try_from(payload.len()).unwrap();
        let mut inner = vec![0x60, len, 0x60, 12, 0x60, 0, 0x39, 0x60, len, 0x60, 0, 0xfd];
        inner.extend_from_slice(&payload);
        set_code(&mut db, INNER, inner);
        db
    }

    fn db() -> CacheDB<EmptyDB> {
        db_with_reason("nope")
    }

    fn call(target: Address, callargs: &[u8]) -> StateUpdate {
        StateUpdate::Call(IStateUpdateTypes::Call {
            target,
            value: U256::ZERO,
            callargs: Bytes::copy_from_slice(callargs),
            resultHash: alloy::primitives::B256::ZERO,
            gasLimit: alloy::primitives::U256::MAX,
        })
    }

    fn trace_on(mut db: CacheDB<EmptyDB>, updates: &[StateUpdate]) -> CallTraceArena {
        let (result, inspector) =
            inspect_state_changes(&mut db, ESTIMATOR, CALLER, updates, &sim_env(), tracer())
                .unwrap();
        assert!(!result.is_success());
        inspector.into_traces()
    }

    fn trace(updates: &[StateUpdate]) -> CallTraceArena {
        trace_on(db(), updates)
    }

    /// The estimate of `updates` on `db` fails with a `RevertingContext`; the trace of its CALL.
    fn failed_call_trace(db: CacheDB<EmptyDB>, updates: &[StateUpdate]) -> String {
        let err =
            estimate_state_changes_gas(&mut db.clone(), ESTIMATOR, CALLER, updates, &sim_env())
                .unwrap_err();
        let ctx = reverting_context(&err).expect("a RevertingContext");
        render_failed_call(&trace_on(db, updates), ctx.target, &ctx.callargs, false)
            .expect("the failed CALL is in the trace")
    }

    #[test]
    fn traces_the_call_behind_a_reverting_context() {
        let updates = [
            call(OK, &[0x12, 0x34]),
            call(OUTER, &[0xab, 0xcd, 0xef, 0x01]),
        ];
        let err = estimate_state_changes_gas(&mut db(), ESTIMATOR, CALLER, &updates, &sim_env())
            .unwrap_err();
        let ctx = reverting_context(&err).expect("a RevertingContext");
        assert_eq!((ctx.index, ctx.target), (U256::from(1), OUTER));

        let rendered = failed_call_trace(db(), &updates);
        let lines: Vec<&str> = rendered.lines().collect();
        // OUTER at the top with its call data, INNER under it, both reverting with the reason.
        assert!(lines[0].contains(&OUTER.to_checksum(None)), "{rendered}");
        assert!(lines[0].contains("abcdef01"), "{rendered}");
        assert!(rendered.contains(&INNER.to_checksum(None)), "{rendered}");
        assert_eq!(
            rendered.matches("[Revert] revert: nope").count(),
            2,
            "{rendered}"
        );
        // Neither the estimator nor the call that succeeded.
        assert!(
            !rendered.contains(&ESTIMATOR.to_checksum(None)),
            "{rendered}"
        );
        assert!(!rendered.contains(&OK.to_checksum(None)), "{rendered}");
    }

    #[test]
    fn traces_a_failed_call_to_a_precompile() {
        let updates = [call(PAIRING, &[0x01])];
        let rendered = failed_call_trace(db(), &updates);
        assert!(
            rendered
                .lines()
                .next()
                .unwrap()
                .contains(&PAIRING.to_checksum(None)),
            "{rendered}"
        );
        // Kept in the tree: a tracer that excludes precompiles records the node but leaves it out
        // of its parent's children, so the estimator's trace would not show the call.
        let arena = trace(&updates);
        let idx = failed_call(&arena, PAIRING, &Bytes::from_static(&[0x01])).unwrap();
        assert!(arena.nodes()[0].children.contains(&idx));
    }

    #[test]
    fn escapes_terminal_sequences_in_a_revert_reason() {
        let rendered =
            failed_call_trace(db_with_reason("\x1b[2Jgone\u{202e}"), &[call(OUTER, &[])]);
        assert!(!rendered.contains('\x1b'), "{rendered:?}");
        assert!(!rendered.contains('\u{202e}'), "{rendered:?}");
        assert!(
            rendered.contains(r"revert: \u{1b}[2Jgone\u{202e}"),
            "{rendered}"
        );
    }

    #[test]
    fn revert_reasons_are_only_read_from_text() {
        assert_eq!(revert_reason(b"plain text"), Some("plain text".to_owned()));
        assert_eq!(revert_reason(b"bell\x07"), None);
        // A custom error whose bytes are valid UTF-8: selector "abcd", one zero word.
        let mut custom = b"abcd".to_vec();
        custom.extend_from_slice(&[0; 32]);
        assert_eq!(revert_reason(&custom), None);
        assert_eq!(
            revert_reason(&Revert::from("nope").abi_encode()),
            Some("revert: nope".to_owned())
        );
    }

    #[test]
    fn no_trace_for_a_call_that_did_not_fail() {
        let updates = [call(OK, &[0x12, 0x34]), call(OUTER, &[])];
        let arena = trace(&updates);
        assert_eq!(
            failed_call(&arena, OK, &Bytes::from_static(&[0x12, 0x34])),
            None
        );
        // Right target, other call data.
        assert_eq!(
            failed_call(&arena, OUTER, &Bytes::from_static(&[0x99])),
            None
        );
        assert!(failed_call(&arena, OUTER, &Bytes::new()).is_some());
    }

    #[test]
    fn failed_call_takes_the_shallowest_match() {
        let mut arena = trace(&[call(OUTER, &[])]);
        let outer = failed_call(&arena, OUTER, &Bytes::new()).unwrap();
        // Found below the root's children too.
        let inner = failed_call(&arena, INNER, &Bytes::new()).unwrap();
        assert_eq!(arena.nodes()[inner].parent, Some(outer));
        // A deeper failed call to the same target with the same data loses to the outer one.
        arena.nodes_mut()[inner].trace.address = OUTER;
        assert_eq!(failed_call(&arena, OUTER, &Bytes::new()), Some(outer));
    }

    #[test]
    fn subtree_reindexes_in_depth_first_order() {
        // ESTIMATOR -> OUTER -> INNER, and OUTER -> INNER again: grow the tree by hand so OUTER
        // has two children, the first of which has a child of its own.
        let mut arena = trace(&[call(OUTER, &[])]);
        let outer = failed_call(&arena, OUTER, &Bytes::new()).unwrap();
        let inner = arena.nodes()[outer].children[0];
        let nodes = arena.nodes_mut();
        let mut grandchild = nodes[inner].clone();
        grandchild.idx = nodes.len();
        grandchild.parent = Some(inner);
        grandchild.trace.address = OK;
        nodes[inner].children.push(grandchild.idx);
        let mut sibling = nodes[inner].clone();
        sibling.idx = nodes.len() + 1;
        sibling.children.clear();
        sibling.trace.address = CALLER;
        nodes[outer].children.push(sibling.idx);
        nodes.push(grandchild);
        nodes.push(sibling);

        let sub = subtree(&arena, outer);
        let addresses: Vec<Address> = sub.nodes().iter().map(|n| n.trace.address).collect();
        assert_eq!(addresses, [OUTER, INNER, OK, CALLER]);
        let nodes = sub.nodes();
        assert_eq!(
            (nodes[0].idx, nodes[0].parent, &nodes[0].children[..]),
            (0, None, &[1, 3][..])
        );
        assert_eq!(
            (nodes[1].parent, &nodes[1].children[..]),
            (Some(0), &[2][..])
        );
        assert_eq!(nodes[2].parent, Some(1));
        assert_eq!(nodes[3].parent, Some(0));
        assert!(nodes.iter().enumerate().all(|(i, n)| n.idx == i));
    }

    #[test]
    fn traces_the_failed_piece_of_a_frame_tree() {
        let frames = [FrameProgram {
            target: ESTIMATOR,
            caller: Address::ZERO,
            value: U256::ZERO,
            calldata_hash: B256::ZERO,
            transition_index: None,
            updates: vec![call(OK, &[]), call(OUTER, &[0x42])],
            children: vec![],
        }];
        let err = estimate_frame_tree_gas(&mut db(), &frames, CALLER, U256::ZERO, &sim_env())
            .unwrap_err();
        // The frame and piece context sits on top of the revert.
        let ctx = reverting_context(&err).expect("a RevertingContext");
        assert_eq!(ctx.target, OUTER);

        let arena =
            inspect_frame_tree_failure(&mut db(), &frames, CALLER, U256::ZERO, &sim_env(), tracer)
                .expect("a piece failed")
                .into_traces();
        let rendered = render_failed_call(&arena, ctx.target, &ctx.callargs, false)
            .expect("the failed CALL is in the trace");
        assert!(rendered.contains("[Revert] revert: nope"), "{rendered}");
    }

    #[test]
    fn no_failed_piece_in_a_frame_tree_that_runs() {
        let frames = [FrameProgram {
            target: ESTIMATOR,
            caller: Address::ZERO,
            value: U256::ZERO,
            calldata_hash: B256::ZERO,
            transition_index: None,
            updates: vec![call(OK, &[])],
            children: vec![],
        }];
        assert!(
            inspect_frame_tree_failure(&mut db(), &frames, CALLER, U256::ZERO, &sim_env(), tracer)
                .is_none()
        );
    }
}

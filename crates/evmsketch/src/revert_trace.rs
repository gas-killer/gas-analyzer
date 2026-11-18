//! Call traces of a failed estimate, for `--debug`.
//!
//! When the estimator's CALL to a target reverts, the estimate fails with
//! `RevertingContext(index, target, revertData, callargs)`. Re-running the estimate under a
//! [`TracingInspector`] and keeping only that CALL's subtree shows where the revert started.

use alloy::primitives::{Address, Bytes};
use alloy::sol_types::decode_revert_reason;
use colorchoice::ColorChoice;
use revm_inspectors::tracing::types::{CallTraceNode, DecodedCallTrace};
use revm_inspectors::tracing::{CallTraceArena, TraceWriter, TraceWriterConfig};

/// The arena node of the estimator's failed CALL to `target` with `callargs`.
///
/// The estimator makes its CALLs directly, so the node is a child of the root; it stops at the
/// first failure, so the last failed match is the one it reported.
pub fn failed_call(arena: &CallTraceArena, target: Address, callargs: &Bytes) -> Option<usize> {
    let nodes = arena.nodes();
    nodes.first()?.children.iter().rev().copied().find(|&idx| {
        let trace = &nodes[idx].trace;
        trace.address == target && trace.data == *callargs && !trace.success
    })
}

/// A new arena holding `root` and everything under it, `root` first.
pub fn subtree(arena: &CallTraceArena, root: usize) -> CallTraceArena {
    let nodes = arena.nodes();
    // Old indices in depth-first order; `new_index[old]` is the position in that order.
    let mut order = vec![root];
    let mut i = 0;
    while i < order.len() {
        order.extend(nodes[order[i]].children.iter().copied());
        i += 1;
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

/// Label every reverted frame whose output is a revert reason (`Error(string)`, `Panic(uint256)`,
/// or a plain string) with that reason, so the trace shows it instead of raw bytes.
pub fn decode_revert_reasons(arena: &mut CallTraceArena) {
    for node in arena.nodes_mut() {
        let trace = &mut node.trace;
        if trace.success || trace.output.is_empty() {
            continue;
        }
        if let Some(reason) = decode_revert_reason(&trace.output) {
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
    use alloy::primitives::{U256, address, b256};
    use alloy::sol_types::{Revert, SolError};
    use gas_analyzer_core::{IStateUpdateTypes, RevertingContext, StateUpdate};
    use gas_analyzer_estimator::{SimEnvOpts, estimate_state_changes_gas, inspect_state_changes};
    use revm::database::{CacheDB, EmptyDB};
    use revm::primitives::hardfork::SpecId;
    use revm::state::{AccountInfo, Bytecode};
    use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

    const ESTIMATOR: Address = address!("0x000000000000000000000000000000000000e570");
    const CALLER: Address = address!("0x00000000000000000000000000000000000ca11e");
    /// Returns without doing anything.
    const OK: Address = address!("0x00000000000000000000000000000000000c0a0a");
    /// Calls `INNER`, then reverts with whatever it returned.
    const OUTER: Address = address!("0x00000000000000000000000000000000000c0b0b");
    /// Reverts with `Error("nope")`.
    const INNER: Address = address!("0x00000000000000000000000000000000000c0c0c");

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

    /// `OK`, `OUTER` and `INNER` deployed on an empty state.
    fn db() -> CacheDB<EmptyDB> {
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
        let payload = Revert::from("nope").abi_encode();
        let len = u8::try_from(payload.len()).unwrap();
        let mut inner = vec![0x60, len, 0x60, 12, 0x60, 0, 0x39, 0x60, len, 0x60, 0, 0xfd];
        inner.extend_from_slice(&payload);
        set_code(&mut db, INNER, inner);
        db
    }

    fn call(target: Address, callargs: &[u8]) -> StateUpdate {
        StateUpdate::Call(IStateUpdateTypes::Call {
            target,
            value: U256::ZERO,
            callargs: Bytes::copy_from_slice(callargs),
        })
    }

    /// The `RevertingContext` an estimate fails with, as the CLI decodes it.
    fn reverting_context(err: &anyhow::Error) -> RevertingContext {
        let msg = err.to_string();
        let hex = &msg[msg.rfind("0x").unwrap() + 2..];
        RevertingContext::abi_decode(&alloy::hex::decode(hex.trim()).unwrap()).unwrap()
    }

    fn trace(updates: &[StateUpdate]) -> CallTraceArena {
        let inspector = TracingInspector::new(TracingInspectorConfig::default_parity());
        let (result, inspector) =
            inspect_state_changes(&mut db(), ESTIMATOR, CALLER, updates, &sim_env(), inspector)
                .unwrap();
        assert!(!result.is_success());
        inspector.into_traces()
    }

    #[test]
    fn traces_the_call_behind_a_reverting_context() {
        let updates = [
            call(OK, &[0x12, 0x34]),
            call(OUTER, &[0xab, 0xcd, 0xef, 0x01]),
        ];
        let err = estimate_state_changes_gas(&mut db(), ESTIMATOR, CALLER, &updates, &sim_env())
            .unwrap_err();
        let ctx = reverting_context(&err);
        assert_eq!((ctx.index, ctx.target), (U256::from(1), OUTER));

        let rendered = render_failed_call(&trace(&updates), ctx.target, &ctx.callargs, false)
            .expect("the failed CALL is in the trace");
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
    fn subtree_reindexes_from_its_root() {
        let arena = trace(&[call(OUTER, &[])]);
        let idx = failed_call(&arena, OUTER, &Bytes::new()).unwrap();
        let sub = subtree(&arena, idx);
        let nodes = sub.nodes();
        assert_eq!(nodes.len(), 2);
        assert_eq!(
            (nodes[0].idx, nodes[0].parent, &nodes[0].children[..]),
            (0, None, &[1][..])
        );
        assert_eq!((nodes[1].idx, nodes[1].parent), (1, Some(0)));
        assert_eq!(nodes[1].trace.address, INNER);
    }
}

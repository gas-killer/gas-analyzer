//! Offline benchmark contrasting the two extraction costs on the SAME logical workload:
//! a heavy-compute call (many execution steps) that collapses to a small storage diff.
//!
//!   `compute_state_updates`              walks the full struct-log trace — O(execution steps)
//!   `build_state_updates_from_prestate`  walks the prestateTracer diff   — O(changed slots)
//!
//! This measures extraction *cost*, not equivalence: the two encoders emit different programs in
//! general (`gas_analyzer_core::prestate::tests::net_form_omits_slots_the_canonical_encoder_re_asserts`
//! pins where they diverge). The workload below is deliberately chosen so the logical diff is held
//! fixed and only the representation varies, making the wall-time gap the thing being compared — and
//! the reason heavy-compute tracked functions, whose struct-log trace times out the node, become
//! extractable at all. The companion `trace_parsing/compute_state_updates` bench shows the same
//! struct-log cost on a real 141k-step Sepolia trace (~73 ms in CI).
//!
//! Fully synthetic — no RPC, no fixture, no LFS — so it always runs in CI. `TOTAL_STEPS` models the
//! execution length of a heavy on-chain computation; `CHANGED_SLOTS` is the handful of words it
//! actually rewrites (e.g. a 64x64 Game-of-Life board is 16 `uint256` words).

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::trace::geth::{
    AccountState, CallFrame, CallLogFrame, DefaultFrame, DiffMode, StructLog,
};
use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use gas_analyzer_core::{build_state_updates_from_prestate, compute_state_updates};

const CONSUMER: Address = Address::repeat_byte(0xC0);
const TOTAL_STEPS: usize = 50_000; // execution steps of a heavy compute
const CHANGED_SLOTS: usize = 16; // words actually rewritten (a 64x64 GoL board = 16 uint256)

fn slot_u256(i: usize) -> U256 {
    U256::from(i as u64 + 1)
}
fn value_u256(i: usize) -> U256 {
    U256::from(0x1000u64 + i as u64)
}

/// A struct-log trace: `CHANGED_SLOTS` SSTOREs spread through `TOTAL_STEPS` cheap compute ops, plus one
/// LOG2 — exactly what `compute_state_updates` must walk end-to-end.
fn synthetic_trace() -> DefaultFrame {
    let mut struct_logs = Vec::with_capacity(TOTAL_STEPS + 1);
    let stride = (TOTAL_STEPS / CHANGED_SLOTS).max(1);
    let mut written = 0usize;
    for step in 0..TOTAL_STEPS {
        if written < CHANGED_SLOTS && step % stride == 0 {
            // SSTORE: stack is bottom→top = [value, slot]; memory must be `Some` or the SSTORE is
            // skipped (the `None` arm early-returns for non-CALL/LOG ops).
            struct_logs.push(StructLog {
                op: "SSTORE".into(),
                depth: 1,
                stack: Some(vec![value_u256(written), slot_u256(written)]),
                memory: Some(Vec::new()),
                ..Default::default()
            });
            written += 1;
        } else {
            struct_logs.push(StructLog {
                op: "JUMPDEST".into(),
                depth: 1,
                ..Default::default()
            });
        }
    }
    // One LOG2 with empty data (offset 0, length 0) and two topics.
    struct_logs.push(StructLog {
        op: "LOG2".into(),
        depth: 1,
        stack: Some(vec![
            U256::from_be_bytes(B256::repeat_byte(0x22).0), // topic2 (bottom)
            U256::from_be_bytes(B256::repeat_byte(0x11).0), // topic1
            U256::ZERO,                                     // length
            U256::ZERO,                                     // offset (top)
        ]),
        memory: Some(Vec::new()),
        ..Default::default()
    });
    DefaultFrame {
        failed: false,
        gas: 0,
        return_value: Bytes::new(),
        struct_logs,
    }
}

/// The equivalent prestate diff (`CHANGED_SLOTS` changed slots) + call frame (one LOG2) — the same
/// logical result the trace above encodes, but represented as a net diff.
fn synthetic_prestate() -> (DiffMode, CallFrame) {
    let mut pre = AccountState::default();
    let mut post = AccountState::default();
    for i in 0..CHANGED_SLOTS {
        pre.storage.insert(B256::from(slot_u256(i)), B256::ZERO);
        post.storage
            .insert(B256::from(slot_u256(i)), B256::from(value_u256(i)));
    }
    let mut diff = DiffMode::default();
    diff.pre.insert(CONSUMER, pre);
    diff.post.insert(CONSUMER, post);

    let frame = CallFrame {
        typ: "CALL".into(),
        to: Some(CONSUMER),
        logs: vec![CallLogFrame {
            topics: Some(vec![B256::repeat_byte(0x11), B256::repeat_byte(0x22)]),
            data: Some(Bytes::new()),
            ..Default::default()
        }],
        ..Default::default()
    };
    (diff, frame)
}

fn bench_extraction_paths(c: &mut Criterion) {
    let trace = synthetic_trace();
    let (diff, frame) = synthetic_prestate();

    // Fixture guard: the two inputs must describe the same logical diff, or the timings below are
    // comparing different amounts of work rather than two representations of one workload. Equal
    // update counts hold *for this workload* — every slot is written once, in ascending slot order,
    // with a single trailing log — not as a property of the encoders. Asserting vector equality here
    // would pass for the same narrow reason and read as proof of a general equivalence that does not
    // hold; see the `core` test named in the module docs for where they actually diverge.
    let n_struct = compute_state_updates(trace.clone())
        .map(|(u, _, _)| u.len())
        .unwrap_or(0);
    let n_pre = build_state_updates_from_prestate(CONSUMER, &diff, &frame).len();
    eprintln!(
        "prestate_parsing: struct-log encoder -> {n_struct} updates from {} steps; \
         prestate net form -> {n_pre} updates from {CHANGED_SLOTS} changed slots",
        trace.struct_logs.len()
    );
    assert_eq!(
        n_struct, n_pre,
        "the two inputs must model the same logical diff for the timings to be comparable"
    );

    let mut group = c.benchmark_group("prestate_parsing");
    group.bench_function("compute_state_updates_heavy", |b| {
        b.iter_batched(
            || trace.clone(),
            |t| black_box(compute_state_updates(black_box(t))),
            BatchSize::PerIteration,
        )
    });
    group.bench_function("build_from_prestate", |b| {
        b.iter(|| {
            black_box(build_state_updates_from_prestate(
                black_box(CONSUMER),
                black_box(&diff),
                black_box(&frame),
            ))
        })
    });
    group.finish();
}

criterion_group!(benches, bench_extraction_paths);
criterion_main!(benches);

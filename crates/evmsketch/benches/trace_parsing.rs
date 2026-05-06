//! Offline benchmark for the trace → state-updates parsing pipeline.
//!
//! Requires a pre-generated fixture:
//!   make fixture RPC_URL=<sepolia-node>
//!
//! If the fixture is absent the benchmark prints a skip message and exits cleanly.

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use sha2::{Digest, Sha256};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use std::alloc::System;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/benches/fixtures/sepolia_trace.json"
);

// SHA-256 of the committed fixture. If this changes, baseline numbers are no
// longer comparable — regenerate with `make fixture` and update here + bench-baseline.md.
const FIXTURE_SHA256: &str = "9b8ef97f1ae92cbbe49e726bacf75026fa1c382edcb463d81b49e16667740989";

fn bench_trace_parsing(c: &mut Criterion) {
    let trace_json = match std::fs::read_to_string(FIXTURE_PATH) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "Skipping trace_parsing bench: fixture not found.\n\
                 Run `make fixture RPC_URL=<sepolia-node>` to generate it."
            );
            return;
        }
    };

    let actual = format!("{:x}", Sha256::digest(trace_json.as_bytes()));
    if actual != FIXTURE_SHA256 {
        eprintln!(
            "WARNING: fixture checksum mismatch — baseline numbers are not comparable.\n\
             expected: {FIXTURE_SHA256}\n\
             actual:   {actual}\n\
             Update FIXTURE_SHA256 and bench-baseline.md after regenerating the fixture."
        );
    }

    let trace: alloy::rpc::types::trace::geth::DefaultFrame =
        serde_json::from_str(&trace_json).expect("fixture JSON is not a valid DefaultFrame");

    eprintln!(
        "trace_parsing: loaded fixture with {} struct-log entries",
        trace.struct_logs.len()
    );

    // Alloc-count pass: pre-clone inputs before the Region opens so clone
    // allocations are not counted, then measure only compute_state_updates.
    const ALLOC_ITERS: usize = 50;
    let copies: Vec<_> = (0..ALLOC_ITERS).map(|_| trace.clone()).collect();
    let region = Region::new(GLOBAL);
    for t in copies {
        let _ = black_box(gas_analyzer_core::compute_state_updates(t));
    }
    let stats = region.change();
    eprintln!(
        "  compute_state_updates — allocs/iter: {}, bytes/iter: {}",
        stats.allocations / ALLOC_ITERS,
        stats.bytes_allocated / ALLOC_ITERS,
    );

    // Wall-time: clone is in the setup closure, outside criterion's timed region.
    let mut group = c.benchmark_group("trace_parsing");
    group.bench_function("compute_state_updates", |b| {
        b.iter_batched(
            || trace.clone(),
            |t| black_box(gas_analyzer_core::compute_state_updates(black_box(t))),
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

criterion_group!(benches, bench_trace_parsing);
criterion_main!(benches);

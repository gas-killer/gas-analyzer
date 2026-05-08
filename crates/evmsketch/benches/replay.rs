//! Offline benchmark for `replay_preceding_transactions`.
//!
//! Replays the transactions that precede the pinned Sepolia tx against a
//! pre-populated `CacheDB<EmptyDB>`, measuring the CPU + allocation cost of the
//! mid-block state-replay step in isolation — no RPC, no network I/O.
//!
//! Requires pre-generated fixture files (run once, then commit):
//!   make replay-fixture RPC_URL=<sepolia-node>
//!
//! If either fixture is absent the benchmark prints a skip message and exits cleanly.

#[path = "common.rs"]
mod common;

use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use gas_analyzer_estimator::replay_preceding_transactions;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use std::alloc::System;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn bench_replay(c: &mut Criterion) {
    let (preceding_txs, template_db, sim_env) = match common::load_replay_fixtures() {
        Some(v) => v,
        None => return,
    };

    eprintln!(
        "replay: loaded {} preceding txs, {} accounts in pre-block state",
        preceding_txs.len(),
        template_db.cache.accounts.len()
    );

    // Alloc-count pass: two-pass approach to isolate function allocations
    // without holding ALLOC_ITERS DB clones in memory simultaneously.
    const ALLOC_ITERS: usize = 10;

    // Pass 1: clone overhead alone.
    let clone_region = Region::new(GLOBAL);
    for _ in 0..ALLOC_ITERS {
        let _ = black_box(template_db.clone());
    }
    let clone_stats = clone_region.change();

    // Pass 2: clone + function.
    let region = Region::new(GLOBAL);
    for _ in 0..ALLOC_ITERS {
        let mut db = template_db.clone();
        let _ = black_box(replay_preceding_transactions(
            &mut db,
            &preceding_txs,
            &sim_env,
        ));
    }
    let total_stats = region.change();

    eprintln!(
        "  replay_preceding_transactions — allocs/iter: {}, bytes/iter: {}",
        total_stats
            .allocations
            .saturating_sub(clone_stats.allocations)
            / ALLOC_ITERS,
        total_stats
            .bytes_allocated
            .saturating_sub(clone_stats.bytes_allocated)
            / ALLOC_ITERS,
    );

    // Wall-time: fresh clone of the template DB is set up in the BatchSize setup
    // closure, outside criterion's timed region.
    let mut group = c.benchmark_group("replay");
    group.bench_function("replay_preceding_transactions", |b| {
        b.iter_batched(
            || template_db.clone(),
            |mut db| {
                black_box(replay_preceding_transactions(
                    black_box(&mut db),
                    black_box(&preceding_txs),
                    black_box(&sim_env),
                ))
            },
            BatchSize::PerIteration,
        )
    });
    group.finish();
}

criterion_group!(benches, bench_replay);
criterion_main!(benches);

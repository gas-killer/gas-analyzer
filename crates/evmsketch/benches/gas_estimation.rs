//! Offline benchmarks for the gas-estimation pipeline.
//!
//! Two groups:
//!   - `build_calldata`  — ABI-encode state updates into estimator calldata
//!   - `estimate_gas_raw` — full revm execution with CacheDB<EmptyDB>
//!
//! No network access required.

use std::time::Instant;

use alloy::primitives::{Address, B256, Bytes, U256, address};
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use gas_analyzer_core::types::{IStateUpdateTypes, StateUpdate};
use gas_analyzer_estimator::{SimEnvOpts, build_gas_estimation_calldata, estimate_gas_raw};
use revm::database::{CacheDB, EmptyDB};
use revm::primitives::hardfork::SpecId;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use std::alloc::System;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const ESTIMATOR_ADDR: Address = address!("0x000000000000000000000000000000000000E570");
const CALLER_ADDR: Address = address!("0x000000000000000000000000000000000000c411");

fn canned_state_updates() -> Vec<StateUpdate> {
    vec![
        StateUpdate::Store(IStateUpdateTypes::Store {
            slot: B256::from(U256::from(0u64)),
            value: B256::from(U256::from(0x42u64)),
        }),
        StateUpdate::Store(IStateUpdateTypes::Store {
            slot: B256::from(U256::from(1u64)),
            value: B256::from(U256::from(0xff_u64)),
        }),
        StateUpdate::Store(IStateUpdateTypes::Store {
            slot: B256::from(U256::from(2u64)),
            value: B256::from(U256::from(100_u64)),
        }),
        StateUpdate::Log1(IStateUpdateTypes::Log1 {
            data: Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]),
            topic1: B256::from(U256::from(1u64)),
        }),
    ]
}

fn sim_env() -> SimEnvOpts {
    SimEnvOpts {
        number: 22_000_000,
        timestamp: 1_700_000_000,
        gas_limit: 30_000_000,
        coinbase: address!("0x00000000000000000000000000000000c01ba5e0"),
        prevrandao: B256::from(U256::from(0xdeadbeef_u64)),
        gas_price: 1_000_000_000,
        basefee: 10_000_000,
        difficulty: U256::ZERO,
        spec: SpecId::CANCUN,
    }
}

fn bench_build_calldata(c: &mut Criterion) {
    let state_updates = canned_state_updates();

    let mut group = c.benchmark_group("build_calldata");
    group.bench_function("build_gas_estimation_calldata", |b| {
        b.iter_custom(|iters| {
            let region = Region::new(GLOBAL);
            let start = Instant::now();
            for _ in 0..iters {
                let _ =
                    black_box(build_gas_estimation_calldata(black_box(&state_updates)).unwrap());
            }
            let elapsed = start.elapsed();
            let stats = region.change();
            eprintln!(
                "  build_calldata — allocs/iter: {}, bytes/iter: {}",
                stats.allocations / iters as usize,
                stats.bytes_allocated / iters as usize,
            );
            elapsed
        })
    });
    group.finish();
}

fn bench_estimate_gas_raw(c: &mut Criterion) {
    let state_updates = canned_state_updates();
    let calldata = build_gas_estimation_calldata(&state_updates).unwrap();
    let env = sim_env();

    let mut group = c.benchmark_group("estimate_gas_raw");
    group.bench_function("estimate_gas_raw/EmptyDB", |b| {
        b.iter_custom(|iters| {
            let region = Region::new(GLOBAL);
            let start = Instant::now();
            for _ in 0..iters {
                let mut cache_db = CacheDB::new(EmptyDB::default());
                let _ = black_box(estimate_gas_raw(
                    &mut cache_db,
                    ESTIMATOR_ADDR,
                    CALLER_ADDR,
                    black_box(calldata.clone()),
                    black_box(&env),
                ));
            }
            let elapsed = start.elapsed();
            let stats = region.change();
            eprintln!(
                "  estimate_gas_raw — allocs/iter: {}, bytes/iter: {}",
                stats.allocations / iters as usize,
                stats.bytes_allocated / iters as usize,
            );
            elapsed
        })
    });
    group.finish();
}

criterion_group!(benches, bench_build_calldata, bench_estimate_gas_raw);
criterion_main!(benches);

//! Flamegraph profiling for the evmsketch critical paths.
//!
//! Two modes, selected at compile time:
//!
//!   CPU (default) — pprof samples the call stack at 1000 Hz.
//!     SVG per benchmark written to target/criterion/<group>/<bench>/profile/flamegraph.svg.
//!     Activate with --profile-time:
//!       make flamegraph
//!
//!   Heap (--features heap-profile) — dhat tracks every allocation.
//!     Output written to dhat-heap.json in the working directory.
//!     Open at https://nnethercote.github.io/dh_view/dh_view.html
//!       make flamegraph-heap
//!
//! Online/offline:
//!   By default only the offline paths run (trace_parsing, gas_estimation, replay).
//!   Set RPC_URL=<sepolia-node> to also profile the full end-to-end RPC pipeline:
//!       make flamegraph-online RPC_URL=<sepolia-node>

#[path = "common.rs"]
mod common;

use std::time::Duration;

use alloy::primitives::{Address, B256, Bytes, FixedBytes, TxKind, U256, address};
use alloy::providers::ProviderBuilder;
use alloy::rpc::types::eth::{TransactionInput, TransactionRequest};
use alloy_eips::BlockNumberOrTag;
use alloy_provider::Provider;
use alloy_rpc_types::TransactionTrait;
use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use gas_analyzer_core::types::{IStateUpdateTypes, StateUpdate};
use gas_analyzer_estimator::{
    SimEnvOpts, build_gas_estimation_calldata, estimate_gas_raw, replay_preceding_transactions,
};
use pprof::criterion::{Output, PProfProfiler};
use revm::database::{CacheDB, EmptyDB};
use revm::primitives::hardfork::SpecId;

#[cfg(feature = "heap-profile")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

const FIXTURE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/benches/fixtures/sepolia_trace.json"
);

const SEPOLIA_TX_HASH: &str = "0x680e2abfbccaf6246b4bda0989fc55dee169d0f6aef2ca4c63a17c6a8a39d6cb";

const ESTIMATOR_ADDR: Address = address!("0x000000000000000000000000000000000000E570");
const CALLER_ADDR: Address = address!("0x000000000000000000000000000000000000c411");

// Ensures the dhat profiler is initialised exactly once and lives until
// program exit (at which point dhat writes dhat-heap.json).
#[cfg(feature = "heap-profile")]
fn ensure_heap_profiler() {
    use std::sync::OnceLock;
    static PROFILER: OnceLock<dhat::Profiler> = OnceLock::new();
    PROFILER.get_or_init(dhat::Profiler::new_heap);
}

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
        value: U256::ZERO,
    }
}

fn bench_trace_parsing(c: &mut Criterion) {
    #[cfg(feature = "heap-profile")]
    ensure_heap_profiler();

    let trace_json = match std::fs::read_to_string(FIXTURE_PATH) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "Skipping trace_parsing: fixture not found.\n\
                 Run `make fixture RPC_URL=<sepolia-node>` to generate it."
            );
            return;
        }
    };

    let trace: alloy::rpc::types::trace::geth::DefaultFrame =
        match serde_json::from_str(&trace_json) {
            Ok(t) => t,
            Err(_) if trace_json.starts_with("version https://git-lfs.github.com") => {
                eprintln!(
                    "Skipping trace_parsing: fixture is an LFS pointer — run `git lfs pull`."
                );
                return;
            }
            Err(e) => panic!("fixture JSON is not a valid DefaultFrame: {e}"),
        };

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

fn bench_gas_estimation(c: &mut Criterion) {
    #[cfg(feature = "heap-profile")]
    ensure_heap_profiler();

    let state_updates = canned_state_updates();
    let calldata = build_gas_estimation_calldata(&state_updates).unwrap();
    let env = sim_env();

    let mut group = c.benchmark_group("gas_estimation");

    group.bench_function("build_calldata", |b| {
        b.iter(|| black_box(build_gas_estimation_calldata(black_box(&state_updates)).unwrap()))
    });

    group.bench_function("estimate_gas_raw", |b| {
        b.iter_batched(
            || CacheDB::new(EmptyDB::default()),
            |mut db| {
                black_box(estimate_gas_raw(
                    &mut db,
                    ESTIMATOR_ADDR,
                    CALLER_ADDR,
                    black_box(calldata.clone()),
                    black_box(&env),
                ))
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

fn bench_replay(c: &mut Criterion) {
    #[cfg(feature = "heap-profile")]
    ensure_heap_profiler();

    let (preceding_txs, template_db, sim_env) = match common::load_replay_fixtures() {
        Some(v) => v,
        None => {
            eprintln!(
                "Skipping replay: fixtures not found.\n\
                 Run `make replay-fixture RPC_URL=<sepolia-node>` to generate them."
            );
            return;
        }
    };

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

fn bench_end_to_end(c: &mut Criterion) {
    #[cfg(feature = "heap-profile")]
    ensure_heap_profiler();

    let rpc_url = match std::env::var("RPC_URL") {
        Ok(u) if !u.trim().is_empty() => u,
        _ => {
            eprintln!(
                "Skipping end_to_end: RPC_URL not set.\n\
                 Example: make flamegraph-online RPC_URL=<sepolia-node>"
            );
            return;
        }
    };

    let rt = tokio::runtime::Runtime::new().expect("failed to build tokio runtime");

    let (tx_request, block) = rt.block_on(async {
        let provider =
            ProviderBuilder::new().connect_http(rpc_url.parse().expect("invalid RPC_URL"));

        let hash: FixedBytes<32> = SEPOLIA_TX_HASH.parse().expect("invalid tx hash constant");

        let tx = provider
            .get_transaction_by_hash(hash)
            .await
            .expect("RPC error on get_transaction_by_hash")
            .expect("tx not found — check RPC_URL points to Sepolia");

        let receipt = provider
            .get_transaction_receipt(hash)
            .await
            .expect("RPC error on get_transaction_receipt")
            .expect("receipt not found");

        let block_num = receipt.block_number.expect("receipt has no block_number");

        let req = TransactionRequest {
            from: Some(tx.inner.signer()),
            to: tx.inner.to().map(TxKind::Call),
            input: TransactionInput::new(tx.inner.input().clone()),
            value: Some(tx.inner.value()),
            ..Default::default()
        };

        (req, BlockNumberOrTag::Number(block_num))
    });

    eprintln!("end_to_end: pinned to block {block:?}, tx {SEPOLIA_TX_HASH}");

    let mut group = c.benchmark_group("end_to_end");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(120));

    group.bench_function("call_to_encoded_state_updates_with_evmsketch", |b| {
        b.to_async(&rt).iter(|| async {
            // Fresh cache per iteration so each run pays the full build cost.
            let cache = gas_analyzer_evmsketch::EvmSketchExecutorCache::new(1);
            black_box(
                gas_analyzer_evmsketch::call_to_encoded_state_updates_with_evmsketch(
                    &cache,
                    &rpc_url,
                    tx_request.clone(),
                    block,
                )
                .await
                .expect("end-to-end failed"),
            )
        })
    });

    group.finish();
}

fn profiled() -> Criterion {
    // FLAMEGRAPH_PROTO=1 → protobuf output for speedscope.app
    // Default           → SVG flamegraph in target/criterion/
    let output = if std::env::var("FLAMEGRAPH_PROTO").is_ok() {
        Output::Protobuf
    } else {
        Output::Flamegraph(None)
    };
    Criterion::default().with_profiler(PProfProfiler::new(1000, output))
}

criterion_group! {
    name = benches;
    config = profiled();
    targets = bench_trace_parsing, bench_gas_estimation, bench_replay, bench_end_to_end
}
criterion_main!(benches);

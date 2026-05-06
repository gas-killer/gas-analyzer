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

use std::collections::HashMap;

use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use criterion::{BatchSize, Criterion, black_box, criterion_group, criterion_main};
use gas_analyzer_estimator::{PrecedingTx, SimEnvOpts, replay_preceding_transactions};
use revm::context_interface::transaction::AccessList;
use revm::database::{CacheDB, EmptyDB};
use revm::primitives::KECCAK_EMPTY;
use revm::primitives::hardfork::SpecId;
use revm::state::{AccountInfo, Bytecode};
use serde::{Deserialize, Serialize};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use std::alloc::System;

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const PRECEDING_TXS_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/benches/fixtures/preceding_txs.json"
);
const PRE_BLOCK_STATE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/benches/fixtures/pre_block_state.json"
);

#[derive(Serialize, Deserialize)]
struct SimEnvJson {
    number: u64,
    timestamp: u64,
    gas_limit: u64,
    coinbase: Address,
    prevrandao: B256,
    gas_price: String,
    basefee: u64,
}

impl From<SimEnvJson> for SimEnvOpts {
    fn from(e: SimEnvJson) -> Self {
        SimEnvOpts {
            number: e.number,
            timestamp: e.timestamp,
            gas_limit: e.gas_limit,
            coinbase: e.coinbase,
            prevrandao: e.prevrandao,
            gas_price: e.gas_price.parse().expect("gas_price is not a valid u128"),
            basefee: e.basefee,
            difficulty: U256::ZERO,
            spec: SpecId::CANCUN,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct TxJson {
    from: Address,
    to: Option<Address>,
    input: Bytes,
    value: U256,
    gas_limit: u64,
    nonce: u64,
    gas_price: String,
}

#[derive(Serialize, Deserialize)]
struct PrecedingTxsFixture {
    sim_env: SimEnvJson,
    txs: Vec<TxJson>,
}

#[derive(Serialize, Deserialize)]
struct AccountSnap {
    balance: U256,
    nonce: u64,
    code: Bytes,
    storage: HashMap<U256, U256>,
}

fn load_fixtures() -> Option<(Vec<PrecedingTx>, CacheDB<EmptyDB>, SimEnvOpts)> {
    let txs_json = match std::fs::read_to_string(PRECEDING_TXS_PATH) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "Skipping replay bench: fixture not found.\n\
                 Run `make replay-fixture RPC_URL=<sepolia-node>` to generate it."
            );
            return None;
        }
    };
    let state_json = match std::fs::read_to_string(PRE_BLOCK_STATE_PATH) {
        Ok(s) => s,
        Err(_) => {
            eprintln!(
                "Skipping replay bench: pre_block_state.json not found.\n\
                 Run `make replay-fixture RPC_URL=<sepolia-node>` to generate it."
            );
            return None;
        }
    };

    let fixture: PrecedingTxsFixture =
        serde_json::from_str(&txs_json).expect("preceding_txs.json is not valid JSON");
    let state_snap: HashMap<Address, AccountSnap> =
        serde_json::from_str(&state_json).expect("pre_block_state.json is not valid JSON");

    let sim_env: SimEnvOpts = fixture.sim_env.into();

    let preceding_txs: Vec<PrecedingTx> = fixture
        .txs
        .into_iter()
        .map(|t| PrecedingTx {
            from: t.from,
            kind: match t.to {
                Some(addr) => TxKind::Call(addr),
                None => TxKind::Create,
            },
            input: t.input,
            value: t.value,
            gas_limit: t.gas_limit,
            nonce: t.nonce,
            gas_price: t.gas_price.parse().expect("gas_price is not a valid u128"),
            access_list: AccessList::default(),
            authorization_list: vec![],
        })
        .collect();

    // Reconstruct CacheDB<EmptyDB> from the pre-block snapshot.
    let mut cache_db: CacheDB<EmptyDB> = CacheDB::new(EmptyDB::default());
    for (addr, snap) in state_snap {
        let bytecode = if snap.code.is_empty() {
            Bytecode::default()
        } else {
            Bytecode::new_raw(snap.code)
        };
        let info = AccountInfo {
            balance: snap.balance,
            nonce: snap.nonce,
            code: Some(bytecode),
            code_hash: KECCAK_EMPTY,
        };
        cache_db.insert_account_info(addr, info);
        for (slot, value) in snap.storage {
            cache_db
                .insert_account_storage(addr, slot, value)
                .expect("insert_account_storage on EmptyDB should not fail");
        }
    }

    Some((preceding_txs, cache_db, sim_env))
}

fn bench_replay(c: &mut Criterion) {
    let (preceding_txs, template_db, sim_env) = match load_fixtures() {
        Some(v) => v,
        None => return,
    };

    eprintln!(
        "replay: loaded {} preceding txs, {} accounts in pre-block state",
        preceding_txs.len(),
        template_db.cache.accounts.len()
    );

    // Alloc-count pass: run several iterations outside criterion's timed region.
    const ALLOC_ITERS: usize = 10;
    let copies: Vec<CacheDB<EmptyDB>> = (0..ALLOC_ITERS).map(|_| template_db.clone()).collect();
    let region = Region::new(GLOBAL);
    for mut db in copies {
        let _ = black_box(replay_preceding_transactions(
            &mut db,
            &preceding_txs,
            &sim_env,
        ));
    }
    let stats = region.change();
    eprintln!(
        "  replay_preceding_transactions — allocs/iter: {}, bytes/iter: {}",
        stats.allocations / ALLOC_ITERS,
        stats.bytes_allocated / ALLOC_ITERS,
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

//! Generates fixture files for the offline `replay` benchmark.
//!
//! Fetches the preceding transactions for the pinned Sepolia tx, replays
//! them against a `SimpleRpcDb` to discover which accounts and storage slots
//! are touched, then re-fetches the pre-block values for each via async RPC.
//!
//! Writes two fixture files:
//!   benches/fixtures/preceding_txs.json   — sim_env + TxJson structs
//!   benches/fixtures/pre_block_state.json — pre-block account/storage snapshot
//!
//! Usage:
//!   RPC_URL=<sepolia-node> cargo run -p gas-analyzer-evmsketch --example generate_replay_fixture

use std::collections::HashMap;

use alloy::primitives::{Address, B256, Bytes, FixedBytes, TxKind, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy_eips::BlockNumberOrTag;
use anyhow::Result;
use gas_analyzer_estimator::{SimEnvOpts, replay_preceding_transactions};
use gas_analyzer_evmsketch::simple_rpc_db::SimpleRpcDb;
use gas_analyzer_evmsketch::{DefaultEvmSketchExecutor, EvmSketchExecutorBuilder};
use revm::context_interface::transaction::{AccessList, SignedAuthorization};
use revm::database::CacheDB;
use revm::primitives::hardfork::SpecId;
use serde::{Deserialize, Serialize};
use url::Url;

const SEPOLIA_TX_HASH: &str = "0x680e2abfbccaf6246b4bda0989fc55dee169d0f6aef2ca4c63a17c6a8a39d6cb";

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
    /// Decimal string to preserve u128 precision across JSON parsers.
    gas_price: String,
    basefee: u64,
    difficulty: U256,
    spec: SpecId,
}

impl From<SimEnvOpts> for SimEnvJson {
    fn from(e: SimEnvOpts) -> Self {
        SimEnvJson {
            number: e.number,
            timestamp: e.timestamp,
            gas_limit: e.gas_limit,
            coinbase: e.coinbase,
            prevrandao: e.prevrandao,
            gas_price: e.gas_price.to_string(),
            basefee: e.basefee,
            difficulty: e.difficulty,
            spec: e.spec,
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
    /// Decimal string to preserve u128 precision across JSON parsers.
    gas_price: String,
    access_list: AccessList,
    authorization_list: Vec<SignedAuthorization>,
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
    /// Raw contract bytecode.
    code: Bytes,
    /// Storage slots touched during replay with their pre-block values.
    storage: HashMap<U256, U256>,
}

fn main() -> Result<()> {
    let rpc_url = std::env::var("RPC_URL").expect("RPC_URL env var is required (Sepolia node)");

    // block_in_place inside SimpleRpcDb requires a multi-thread runtime.
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(rpc_url))
}

async fn run(rpc_url: String) -> Result<()> {
    let url: Url = rpc_url.parse()?;

    // Standard Ethereum provider — used for receipt lookup and re-fetch calls.
    let eth_provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);

    // Derive block number and tx index from the receipt.
    let hash: FixedBytes<32> = SEPOLIA_TX_HASH.parse().expect("invalid tx hash constant");
    let receipt = eth_provider
        .get_transaction_receipt(hash)
        .await?
        .expect("tx receipt not found — check RPC_URL points to Sepolia");
    let block_number = receipt.block_number.expect("receipt has no block_number");
    let tx_index = receipt
        .transaction_index
        .expect("receipt has no transaction_index");

    // Build EvmSketch executor — this gives us RootProvider<AnyNetwork> (required
    // by SimpleRpcDb) and a sim_env already populated from the block header.
    eprintln!("Building EvmSketch executor anchored to block {block_number}...");
    let executor: DefaultEvmSketchExecutor = EvmSketchExecutorBuilder::new()
        .rpc_url(url)
        .at_block(BlockNumberOrTag::Number(block_number))
        .build()
        .await?;

    let any_provider = executor.sketch.provider.clone(); // RootProvider<AnyNetwork>
    let sim_env = executor.sim_env();

    // ----------------------------------------------------------------
    // Fetch preceding transactions
    // ----------------------------------------------------------------
    eprintln!("Fetching {tx_index} preceding transactions for block {block_number}...");
    let preceding_txs =
        gas_analyzer_rpc::get_preceding_transactions(&eth_provider, block_number, tx_index).await?;
    eprintln!("Fetched {} preceding txs", preceding_txs.len());

    // ----------------------------------------------------------------
    // Replay to discover which accounts / slots are touched
    // ----------------------------------------------------------------
    // Run against SimpleRpcDb (block N-1) so every missing slot is fetched
    // from the chain on demand.  After replay the cache keys tell us exactly
    // which accounts and slots need to be in the offline fixture.
    let state_block = block_number.saturating_sub(1);
    let simple_db = SimpleRpcDb {
        provider: any_provider,
        block_number: state_block,
    };
    let mut cache_db = CacheDB::new(simple_db);

    eprintln!("Replaying {tx_index} preceding txs to warm the cache (makes RPC calls)...");
    replay_preceding_transactions(&mut cache_db, &preceding_txs, &sim_env)?;
    let account_count = cache_db.cache.accounts.len();
    eprintln!("Replay complete. Touched {account_count} accounts.");

    // Collect all addresses and their storage keys from the warmed cache.
    let touched: Vec<(Address, Vec<U256>)> = cache_db
        .cache
        .accounts
        .iter()
        .map(|(addr, db_acct)| {
            let slots: Vec<U256> = db_acct.storage.keys().cloned().collect();
            (*addr, slots)
        })
        .collect();

    // ----------------------------------------------------------------
    // Re-fetch pre-block state for all touched accounts / slots
    // ----------------------------------------------------------------
    // The CacheDB now holds post-replay state; the bench needs the pre-block
    // state so that running replay from scratch produces consistent timings.
    eprintln!("Re-fetching pre-block state for {account_count} accounts via async RPC...");

    let mut state_snap: HashMap<Address, AccountSnap> = HashMap::new();

    for (addr, slots) in &touched {
        let balance = eth_provider
            .get_balance(*addr)
            .number(state_block)
            .await
            .map_err(|e| anyhow::anyhow!("get_balance failed for {addr}: {e}"))?;
        let nonce = eth_provider
            .get_transaction_count(*addr)
            .number(state_block)
            .await
            .map_err(|e| anyhow::anyhow!("get_transaction_count failed for {addr}: {e}"))?;
        let code = eth_provider
            .get_code_at(*addr)
            .number(state_block)
            .await
            .map_err(|e| anyhow::anyhow!("get_code_at failed for {addr}: {e}"))?;

        let mut storage = HashMap::new();
        for slot in slots {
            let value = eth_provider
                .get_storage_at(*addr, *slot)
                .number(state_block)
                .await
                .map_err(|e| anyhow::anyhow!("get_storage_at failed for {addr}[{slot}]: {e}"))?;
            storage.insert(*slot, value);
        }

        state_snap.insert(
            *addr,
            AccountSnap {
                balance,
                nonce,
                code,
                storage,
            },
        );
    }

    // ----------------------------------------------------------------
    // Serialize
    // ----------------------------------------------------------------
    let tx_jsons: Vec<TxJson> = preceding_txs
        .iter()
        .map(|tx| TxJson {
            from: tx.from,
            to: match tx.kind {
                TxKind::Call(addr) => Some(addr),
                TxKind::Create => None,
            },
            input: tx.input.clone(),
            value: tx.value,
            gas_limit: tx.gas_limit,
            nonce: tx.nonce,
            gas_price: tx.gas_price.to_string(),
            access_list: tx.access_list.clone(),
            authorization_list: tx.authorization_list.clone(),
        })
        .collect();

    let fixture = PrecedingTxsFixture {
        sim_env: sim_env.into(),
        txs: tx_jsons,
    };

    let txs_json = serde_json::to_string_pretty(&fixture)?;
    let state_json = serde_json::to_string_pretty(&state_snap)?;

    std::fs::create_dir_all(std::path::Path::new(PRECEDING_TXS_PATH).parent().unwrap())?;
    std::fs::write(PRECEDING_TXS_PATH, &txs_json)?;
    std::fs::write(PRE_BLOCK_STATE_PATH, &state_json)?;

    eprintln!(
        "Written {} preceding txs (+sim_env) to {PRECEDING_TXS_PATH}",
        fixture.txs.len()
    );
    eprintln!(
        "Written {} account snapshots to {PRE_BLOCK_STATE_PATH}",
        state_snap.len()
    );

    Ok(())
}

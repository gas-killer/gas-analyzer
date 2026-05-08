//! Shared fixture types and loader for the replay and flamegraph benches.
//!
//! `replay.rs` and `flamegraph.rs` both include this file via:
//!   #[path = "common.rs"] mod common;

use std::collections::HashMap;

use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use gas_analyzer_estimator::{PrecedingTx, SimEnvOpts};
use revm::context_interface::transaction::{AccessList, SignedAuthorization};
use revm::database::{CacheDB, EmptyDB};
use revm::primitives::hardfork::SpecId;
use revm::state::{AccountInfo, Bytecode};
use serde::{Deserialize, Serialize};

pub const PRECEDING_TXS_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/benches/fixtures/preceding_txs.json"
);
pub const PRE_BLOCK_STATE_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/benches/fixtures/pre_block_state.json"
);

fn default_spec() -> SpecId {
    SpecId::CANCUN
}

#[derive(Serialize, Deserialize)]
pub struct SimEnvJson {
    pub number: u64,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub coinbase: Address,
    pub prevrandao: B256,
    /// Decimal string to preserve u128 precision across JSON parsers.
    pub gas_price: String,
    pub basefee: u64,
    /// Default U256::ZERO for fixtures generated before this field was added.
    /// Regenerate with `make replay-fixture` to capture the real block difficulty.
    #[serde(default)]
    pub difficulty: U256,
    /// Defaults to CANCUN for fixtures generated before this field was added.
    /// Regenerate with `make replay-fixture` to capture the real block spec.
    #[serde(default = "default_spec")]
    pub spec: SpecId,
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
            difficulty: e.difficulty,
            spec: e.spec,
            value: U256::ZERO,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct TxJson {
    pub from: Address,
    pub to: Option<Address>,
    pub input: Bytes,
    pub value: U256,
    pub gas_limit: u64,
    pub nonce: u64,
    /// Decimal string to preserve u128 precision across JSON parsers.
    pub gas_price: String,
    /// Defaults to empty for fixtures generated before this field was added.
    /// Regenerate with `make replay-fixture` to capture EIP-2930 access lists.
    #[serde(default)]
    pub access_list: AccessList,
    /// Defaults to empty for fixtures generated before this field was added.
    /// Regenerate with `make replay-fixture` to capture EIP-7702 authorization lists.
    #[serde(default)]
    pub authorization_list: Vec<SignedAuthorization>,
}

#[derive(Serialize, Deserialize)]
pub struct PrecedingTxsFixture {
    pub sim_env: SimEnvJson,
    pub txs: Vec<TxJson>,
}

#[derive(Serialize, Deserialize)]
pub struct AccountSnap {
    pub balance: U256,
    pub nonce: u64,
    pub code: Bytes,
    pub storage: HashMap<U256, U256>,
}

pub fn load_replay_fixtures() -> Option<(Vec<PrecedingTx>, CacheDB<EmptyDB>, SimEnvOpts)> {
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

    let fixture: PrecedingTxsFixture = match serde_json::from_str(&txs_json) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "Skipping replay bench: failed to parse preceding_txs.json: {e}\n\
                 Run `make replay-fixture RPC_URL=<sepolia-node>` to regenerate."
            );
            return None;
        }
    };
    let state_snap: HashMap<Address, AccountSnap> = match serde_json::from_str(&state_json) {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "Skipping replay bench: failed to parse pre_block_state.json: {e}\n\
                 Run `make replay-fixture RPC_URL=<sepolia-node>` to regenerate."
            );
            return None;
        }
    };

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
            access_list: t.access_list,
            authorization_list: t.authorization_list,
        })
        .collect();

    let mut cache_db: CacheDB<EmptyDB> = CacheDB::new(EmptyDB::default());
    for (addr, snap) in state_snap {
        let bytecode = if snap.code.is_empty() {
            Bytecode::default()
        } else {
            Bytecode::new_raw(snap.code)
        };
        // Use hash_slow() to match SimpleRpcDb — hardcoding KECCAK_EMPTY for
        // non-empty bytecode produces an inconsistent AccountInfo.
        let code_hash = bytecode.hash_slow();
        cache_db.insert_account_info(
            addr,
            AccountInfo {
                balance: snap.balance,
                nonce: snap.nonce,
                code: Some(bytecode),
                code_hash,
            },
        );
        for (slot, value) in snap.storage {
            cache_db
                .insert_account_storage(addr, slot, value)
                .expect("insert_account_storage on EmptyDB should not fail");
        }
    }

    Some((preceding_txs, cache_db, sim_env))
}

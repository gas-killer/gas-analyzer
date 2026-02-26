//! Gas estimation using revm directly.
//!
//! This crate provides WASM-compatible gas estimation by running state updates
//! through the StateChangeHandlerGasEstimator contract via revm. It is generic
//! over the database backend, allowing the same code to work with:
//! - RPC-backed state (native, via `CacheDB<&RpcDb>`)
//! - Empty state (WASM, via `CacheDB<EmptyDB>`)
//!
//! No reth-evm, no sp1-contract-call, no async, no I/O.

use alloy_dyn_abi::DynSolValue;
use alloy_primitives::{Address, B256, Bytes, U256};
use anyhow::{Result, anyhow};
use revm::context::result::ExecutionResult;
use revm::database::CacheDB;
use revm::state::{AccountInfo, Bytecode};

use gas_analyzer_core::encoding::encode_state_updates_to_sol;
use gas_analyzer_core::types::StateUpdate;

/// Embedded ABI JSON for StateChangeHandlerGasEstimator - loaded at compile time
const ESTIMATOR_ABI_JSON: &str = include_str!("../../../abis/StateChangeHandlerGasEstimator.json");

/// Load the StateChangeHandlerGasEstimator deployed bytecode from the embedded JSON.
fn load_estimator_bytecode() -> Result<Vec<u8>> {
    let json: serde_json::Value = serde_json::from_str(ESTIMATOR_ABI_JSON)
        .map_err(|e| anyhow!("Failed to parse embedded JSON: {}", e))?;

    let bytecode_hex = json
        .get("deployedBytecode")
        .and_then(|v| v.get("object"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing 'deployedBytecode.object' in JSON"))?;

    let bytecode_hex = bytecode_hex.strip_prefix("0x").unwrap_or(bytecode_hex);

    hex::decode(bytecode_hex).map_err(|e| anyhow!("Failed to decode estimator bytecode: {}", e))
}

/// Build the calldata for `runStateUpdatesCall(uint8[], bytes[])` from state updates.
///
/// This encodes the state updates into the calldata format expected by the
/// StateChangeHandlerGasEstimator contract.
pub fn build_gas_estimation_calldata(state_updates: &[StateUpdate]) -> Result<Bytes> {
    let (types, args) = encode_state_updates_to_sol(state_updates);

    let types_array = DynSolValue::Array(
        types
            .iter()
            .map(|x| DynSolValue::Uint(U256::from(*x as u8), 8))
            .collect(),
    );

    let args_array = DynSolValue::Array(
        args.iter()
            .map(|b| DynSolValue::Bytes(b.to_vec()))
            .collect(),
    );

    // Function selector for runStateUpdatesCall(uint8[],bytes[])
    let selector: [u8; 4] = [0x7a, 0x88, 0x8d, 0xbc];

    let tuple = DynSolValue::Tuple(vec![types_array, args_array]);
    let encoded_args = tuple.abi_encode_params();

    let mut calldata = Vec::with_capacity(4 + encoded_args.len());
    calldata.extend_from_slice(&selector);
    calldata.extend_from_slice(&encoded_args);

    Ok(Bytes::from(calldata))
}

/// Estimate gas for executing pre-built calldata against the StateChangeHandlerGasEstimator.
///
/// This injects the estimator contract at `contract_address`, gives the caller
/// plenty of balance, and executes the calldata via revm.
///
/// # Arguments
/// * `cache_db` - A CacheDB wrapping any database backend
/// * `contract_address` - The address to inject the estimator contract at
/// * `caller_address` - The address to use as the caller
/// * `calldata` - The encoded calldata for `runStateUpdatesCall(uint8[], bytes[])`
/// * `gas_limit` - The gas limit for the execution
pub fn estimate_gas_raw<DB>(
    cache_db: &mut CacheDB<DB>,
    contract_address: Address,
    caller_address: Address,
    calldata: Bytes,
    gas_limit: u64,
) -> Result<u64>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    let bytecode_bytes = load_estimator_bytecode()?;

    // Inject the contract bytecode at the target address
    let account_info = AccountInfo {
        balance: U256::from(1_000_000_000_000_000_000_000_000u128), // 1M ETH
        nonce: 0,
        code_hash: B256::ZERO,
        code: Some(Bytecode::new_raw(bytecode_bytes.into())),
    };
    cache_db.insert_account_info(contract_address, account_info);

    // Also give the caller plenty of balance
    let caller_info = AccountInfo {
        balance: U256::from(1_000_000_000_000_000_000_000_000u128),
        nonce: 0,
        code_hash: B256::ZERO,
        code: None,
    };
    cache_db.insert_account_info(caller_address, caller_info);

    // Build and execute via revm directly
    use revm::context::{Context, TxEnv};
    use revm::{ExecuteEvm, MainBuilder, MainContext};

    let ctx = Context::mainnet()
        .with_db(&mut *cache_db)
        .modify_cfg_chained(|cfg| {
            cfg.disable_nonce_check = true;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
            cfg.disable_fee_charge = true;
        })
        .modify_block_chained(|block| {
            block.basefee = 0;
            block.difficulty = U256::ZERO;
            block.prevrandao = Some(B256::ZERO);
        });

    let mut evm = ctx.build_mainnet();

    let tx = TxEnv::builder()
        .caller(caller_address)
        .kind(revm::primitives::TxKind::Call(contract_address))
        .data(calldata)
        .value(U256::ZERO)
        .gas_limit(gas_limit)
        .build()
        .map_err(|e| anyhow!("Failed to build tx env: {:?}", e))?;

    let result = evm
        .transact(tx)
        .map_err(|e| anyhow!("Gas estimation failed: {:?}", e))?;

    match result.result {
        ExecutionResult::Success { gas_used, .. } => Ok(gas_used),
        ExecutionResult::Revert {
            output, gas_used, ..
        } => Err(anyhow!(
            "Gas estimation reverted (gas: {}): {}",
            gas_used,
            output
        )),
        ExecutionResult::Halt {
            reason, gas_used, ..
        } => Err(anyhow!(
            "Gas estimation halted (gas: {}): {:?}",
            gas_used,
            reason
        )),
    }
}

/// Estimate gas for executing a set of state updates.
///
/// This is a convenience function that builds the calldata from state updates
/// and then calls `estimate_gas_raw`.
///
/// # Arguments
/// * `cache_db` - A CacheDB wrapping any database backend
/// * `contract_address` - The address to inject the estimator contract at
/// * `state_updates` - The state updates to estimate gas for
/// * `gas_limit` - The gas limit for the execution
pub fn estimate_state_changes_gas<DB>(
    cache_db: &mut CacheDB<DB>,
    contract_address: Address,
    state_updates: &[StateUpdate],
    gas_limit: u64,
) -> Result<u64>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    let calldata = build_gas_estimation_calldata(state_updates)?;
    let caller_address = Address::from_word(B256::from(U256::from(1)));

    estimate_gas_raw(
        cache_db,
        contract_address,
        caller_address,
        calldata,
        gas_limit,
    )
}

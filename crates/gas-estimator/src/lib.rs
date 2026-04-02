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
use revm::primitives::TxKind;
use revm::state::AccountInfo;

use gas_analyzer_core::encoding::encode_state_updates_to_sol;
use gas_analyzer_core::types::StateUpdate;

/// Environment fields for the gas estimation simulation.
///
/// These are set on revm's `BlockEnv` and `TxEnv` so that contracts reading
/// opcodes like COINBASE, TIMESTAMP, NUMBER, GASLIMIT, GASPRICE, or
/// PREVRANDAO see realistic values.
#[derive(Clone, Debug)]
pub struct SimEnvOpts {
    pub number: u64,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub coinbase: Address,
    pub prevrandao: B256,
    pub gas_price: u128,
}

/// Embedded ABI JSON for StateChangeHandlerGasEstimator - loaded at compile time
const ESTIMATOR_ABI_JSON: &str = include_str!("../../../abis/StateChangeHandlerGasEstimator.json");

/// Compute the isolated storage slot for the implementation address.
/// Mirrors the Solidity constant: `keccak256("gas.estimator.implementation") - 1`
fn impl_slot() -> U256 {
    U256::from_be_bytes(*alloy_primitives::keccak256("gas.estimator.implementation"))
        - U256::from(1)
}

/// Load the StateChangeHandlerGasEstimator deployed bytecode from the embedded artifact.
fn load_estimator_bytecode() -> Result<Vec<u8>> {
    let json: serde_json::Value = serde_json::from_str(ESTIMATOR_ABI_JSON)
        .map_err(|e| anyhow!("Failed to parse embedded JSON: {}", e))?;

    let bytecode_hex = json
        .get("deployedBytecode")
        .and_then(|d| d.get("object"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing 'deployedBytecode.object' in JSON"))?;

    let bytecode_hex = bytecode_hex.strip_prefix("0x").unwrap_or(bytecode_hex);
    hex::decode(bytecode_hex).map_err(|e| anyhow!("Failed to decode bytecode: {}", e))
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
/// * `caller_address` - The address to use as the caller (also used as tx.origin)
/// * `calldata` - The encoded calldata for `runStateUpdatesCall(uint8[], bytes[])`
/// * `sim_env` - Simulation environment fields (block and tx context)
pub fn estimate_gas_raw<DB>(
    cache_db: &mut CacheDB<DB>,
    contract_address: Address,
    caller_address: Address,
    calldata: Bytes,
    sim_env: &SimEnvOpts,
) -> Result<u64>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    // Build and execute via revm directly
    use revm::context::{Context, TxEnv};
    use revm::database_interface::DatabaseRef;
    use revm::{ExecuteEvm, MainBuilder, MainContext};

    // ── Inject the proxy at contract_address ────────────────────────────────
    //
    // StateChangeHandlerGasEstimator routes:
    //   runStateUpdatesCall → own logic
    //   anything else       → DELEGATECALL to `implementation` (= backup_addr)
    //
    // We stash the original contract bytecode at a synthetic backup_addr so
    // that external protocols (e.g. oracles) can callback into contract_address
    // during a state-update CALL and get valid responses through the fallback().
    //
    // The implementation address is stored in an EIP-1967-style isolated storage
    // slot (keccak256("gas.estimator.implementation") - 1), written directly via
    // insert_account_storage — no constructor execution needed.
    let backup_addr = Address::from([0xba; 20]);

    let original_account = cache_db
        .basic_ref(contract_address)
        .ok()
        .flatten()
        .unwrap_or_default();

    if let Some(code) = original_account.code {
        cache_db.insert_account_info(
            backup_addr,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: B256::ZERO,
                code: Some(code),
            },
        );
    }

    let proxy_bytes = load_estimator_bytecode()?;
    cache_db.insert_account_info(
        contract_address,
        AccountInfo {
            balance: original_account.balance,
            nonce: 0,
            code_hash: B256::ZERO,
            code: Some(revm::state::Bytecode::new_raw(proxy_bytes.into())),
        },
    );

    let backup_addr_u256 = U256::from_be_slice(backup_addr.as_slice());
    cache_db
        .insert_account_storage(contract_address, impl_slot(), backup_addr_u256)
        .map_err(|e| anyhow!("Failed to write IMPL_SLOT: {:?}", e))?;

    let ctx = Context::mainnet()
        .with_db(&mut *cache_db)
        .modify_cfg_chained(|cfg| {
            cfg.disable_nonce_check = true;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
            cfg.disable_fee_charge = true;
        })
        .modify_block_chained(|block| {
            block.number = U256::from(sim_env.number);
            block.timestamp = U256::from(sim_env.timestamp);
            block.gas_limit = sim_env.gas_limit;
            block.beneficiary = sim_env.coinbase;
            block.prevrandao = Some(sim_env.prevrandao);
            block.basefee = 0;
            block.difficulty = U256::ZERO;
        });

    let mut evm = ctx.build_mainnet();

    let tx = TxEnv::builder()
        .caller(caller_address)
        .kind(revm::primitives::TxKind::Call(contract_address))
        .data(calldata)
        .value(U256::ZERO)
        .gas_limit(sim_env.gas_limit)
        .gas_price(sim_env.gas_price)
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
/// * `caller_address` - The address to use as the caller (also used as tx.origin)
/// * `state_updates` - The state updates to estimate gas for
/// * `sim_env` - Simulation environment fields (block and tx context)
pub fn estimate_state_changes_gas<DB>(
    cache_db: &mut CacheDB<DB>,
    contract_address: Address,
    caller_address: Address,
    state_updates: &[StateUpdate],
    sim_env: &SimEnvOpts,
) -> Result<u64>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    let calldata = build_gas_estimation_calldata(state_updates)?;

    estimate_gas_raw(
        cache_db,
        contract_address,
        caller_address,
        calldata,
        sim_env,
    )
}

// ============================================================================
// Preceding Transaction Replay
// ============================================================================

/// A simplified representation of a preceding transaction for replay.
///
/// This avoids bringing alloy-rpc-types into the gas-estimator crate,
/// keeping it WASM-compatible. The conversion from alloy's `Transaction`
/// type happens in the calling code.
pub struct PrecedingTx {
    pub from: Address,
    pub kind: TxKind,
    pub input: Bytes,
    pub value: U256,
    pub gas_limit: u64,
    pub nonce: u64,
}

/// Replay preceding transactions against a CacheDB to bring it to the
/// correct mid-block state.
///
/// Given transactions `txs[0..tx_index-1]` from block N, this function
/// executes each one via revm's `transact_commit`, which commits state
/// changes to the underlying CacheDB. After this function returns, the
/// CacheDB reflects the state as if those transactions had already been
/// mined.
///
/// Transaction results (success/revert/halt) are intentionally ignored —
/// in a real block, even a reverted transaction modifies state (nonce
/// increment, gas payment to coinbase).
pub fn replay_preceding_transactions<DB>(
    cache_db: &mut CacheDB<DB>,
    preceding_txs: &[PrecedingTx],
    block_gas_limit: u64,
) -> Result<()>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    use revm::context::{Context, TxEnv};
    use revm::{ExecuteCommitEvm, MainBuilder, MainContext};

    let mut evm = Context::mainnet()
        .with_db(&mut *cache_db)
        .modify_cfg_chained(|cfg| {
            cfg.disable_nonce_check = true;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
            cfg.disable_fee_charge = true;
        })
        .modify_block_chained(|block| {
            block.basefee = 0;
            block.gas_limit = block_gas_limit;
        })
        .build_mainnet();

    for (i, tx) in preceding_txs.iter().enumerate() {
        let revm_tx = TxEnv::builder()
            .caller(tx.from)
            .kind(tx.kind)
            .data(tx.input.clone())
            .value(tx.value)
            .gas_limit(tx.gas_limit)
            .nonce(tx.nonce)
            .build()
            .map_err(|e| anyhow!("Failed to build tx env for preceding tx {}: {:?}", i, e))?;

        // transact_commit executes and commits state changes to the CacheDB.
        // We ignore the result — reverted/halted txs still modify state.
        let _result = evm
            .transact_commit(revm_tx)
            .map_err(|e| anyhow!("Failed to replay preceding tx {}: {:?}", i, e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_dyn_abi::DynSolValue;
    use alloy_primitives::address;
    use alloy_sol_types::{SolError, sol};
    use gas_analyzer_core::types::IStateUpdateTypes;
    use revm::DatabaseCommit;
    use revm::context::result::ExecutionResult;
    use revm::context::{Context, TxEnv};
    use revm::database::{CacheDB, EmptyDB};
    use revm::{ExecuteEvm, MainBuilder, MainContext};

    sol!(
        #[derive(Debug)]
        SimEnvOptsStructs,
        "../../abis/SimEnvOptsStructs.json"
    );

    sol!(
        #[derive(Debug)]
        StateChangeHandlerGasEstimator,
        "../../abis/StateChangeHandlerGasEstimator.json"
    );

    use SimEnvOptsStructs::EnvironmentMismatch;
    use StateChangeHandlerGasEstimator::RevertingContext;

    /// Try to decode an EnvironmentMismatch from a gas estimation error.
    ///
    /// The error chain is: estimate_state_changes_gas returns an anyhow error
    /// whose message contains the hex-encoded revert data. The revert is
    /// RevertingContext(index, target, revertData, callargs) where revertData
    /// is EnvironmentMismatch(expected, actual, explanation).
    fn format_sim_env_error(err: &anyhow::Error) -> String {
        let msg = err.to_string();

        // The error message format is:
        // "Gas estimation reverted (gas: N): 0x<hex>"
        // Find the last "0x" which is the revert data
        let Some(hex_start) = msg.rfind("0x") else {
            return msg;
        };
        let hex_body = &msg[hex_start + 2..];
        // Take only hex chars
        let hex_end = hex_body
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(hex_body.len());
        let hex_str = &hex_body[..hex_end];

        let Ok(bytes) = hex::decode(hex_str) else {
            return msg;
        };

        if bytes.len() < 4 {
            return msg;
        }

        // Try decoding as RevertingContext first (outer error from estimator)
        if let Ok(ctx) = RevertingContext::abi_decode(&bytes)
            && ctx.revertData.len() >= 4
            && let Ok(env_err) = EnvironmentMismatch::abi_decode(&ctx.revertData)
        {
            return format!(
                "EnvironmentMismatch: {}\n  expected: {:?}\n  actual:   {:?}",
                env_err.explanation, env_err.expected, env_err.actual
            );
        }

        // Try decoding as EnvironmentMismatch directly
        if let Ok(env_err) = EnvironmentMismatch::abi_decode(&bytes) {
            return format!(
                "EnvironmentMismatch: {}\n  expected: {:?}\n  actual:   {:?}",
                env_err.explanation, env_err.expected, env_err.actual
            );
        }

        msg
    }

    const SIM_ENV_TEST_MAIN_JSON: &str = include_str!("../../../abis/SimEnvOptsTestMain.json");

    fn load_creation_bytecode(json_str: &str) -> Vec<u8> {
        let json: serde_json::Value = serde_json::from_str(json_str).unwrap();
        let hex_str = json["bytecode"]["object"].as_str().unwrap();
        let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
        hex::decode(hex_str).unwrap()
    }

    /// Deploy SimEnvTestMain into a CacheDB and return (cache_db, sim_env_callee_address).
    ///
    /// SimEnvTestMain's constructor deploys SimEnvCallee with the expected env values.
    /// The SimEnvCallee address is stored in SimEnvTestMain's storage slot 0.
    fn deploy_sim_env_test(caller: Address, sim_env: &SimEnvOpts) -> (CacheDB<EmptyDB>, Address) {
        let constructor_args = DynSolValue::Tuple(vec![
            DynSolValue::Address(caller),
            DynSolValue::Uint(U256::from(sim_env.gas_price), 256),
            DynSolValue::Address(sim_env.coinbase),
            DynSolValue::Uint(U256::from(sim_env.number), 256),
            DynSolValue::Uint(U256::from(sim_env.timestamp), 256),
            DynSolValue::Uint(U256::from(sim_env.gas_limit), 256),
            DynSolValue::Uint(sim_env.prevrandao.into(), 256),
        ]);
        let encoded_args = constructor_args.abi_encode_params();

        let creation_bytecode = load_creation_bytecode(SIM_ENV_TEST_MAIN_JSON);
        let mut deploy_data = creation_bytecode;
        deploy_data.extend_from_slice(&encoded_args);

        let mut cache_db = CacheDB::new(EmptyDB::default());
        cache_db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000_000_000u128),
                nonce: 0,
                code_hash: B256::ZERO,
                code: None,
            },
        );

        let ctx = Context::mainnet()
            .with_db(&mut cache_db)
            .modify_cfg_chained(|cfg| {
                cfg.disable_nonce_check = true;
                cfg.disable_balance_check = true;
                cfg.disable_base_fee = true;
                cfg.disable_fee_charge = true;
            })
            .modify_block_chained(|block| {
                block.number = U256::from(sim_env.number);
                block.timestamp = U256::from(sim_env.timestamp);
                block.gas_limit = sim_env.gas_limit;
                block.beneficiary = sim_env.coinbase;
                block.prevrandao = Some(sim_env.prevrandao);
                block.basefee = 0;
                block.difficulty = U256::ZERO;
            });

        let mut evm = ctx.build_mainnet();

        let deploy_tx = TxEnv::builder()
            .caller(caller)
            .kind(revm::primitives::TxKind::Create)
            .data(deploy_data.into())
            .value(U256::ZERO)
            .gas_limit(30_000_000)
            .gas_price(sim_env.gas_price)
            .build()
            .unwrap();

        let deploy_result = evm.transact(deploy_tx).expect("deploy failed");
        let deployed_address = match deploy_result.result {
            ExecutionResult::Success { output, .. } => output
                .address()
                .copied()
                .expect("CREATE should return deployed address"),
            ExecutionResult::Revert { output, .. } => panic!("Deploy reverted: {}", output),
            ExecutionResult::Halt { reason, .. } => panic!("Deploy halted: {:?}", reason),
        };

        cache_db.commit(deploy_result.state);

        // Read SimEnvCallee address from SimEnvTestMain's storage slot 0
        use revm::database_interface::DatabaseRef;
        let slot_value = cache_db
            .storage_ref(deployed_address, U256::ZERO)
            .expect("failed to read storage");
        let callee_address = Address::from_word(B256::from(slot_value));

        (cache_db, callee_address)
    }

    #[test]
    fn test_sim_env_correct_values() {
        let caller = address!("0x000000000000000000000000000000000000c411");
        let sim_env = SimEnvOpts {
            number: 42,
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            coinbase: address!("0x00000000000000000000000000000000c01ba5e0"),
            prevrandao: B256::from(U256::from(0xdeadbeef_u64)),
            gas_price: 1_000_000_000,
        };

        let (mut cache_db, callee_address) = deploy_sim_env_test(caller, &sim_env);

        // Build a StateUpdate::Call that calls SimEnvCallee.test()
        // selector for test() = 0xf8a8fd6d
        let test_selector = Bytes::from(vec![0xf8, 0xa8, 0xfd, 0x6d]);
        let state_updates = vec![StateUpdate::Call(IStateUpdateTypes::Call {
            target: callee_address,
            value: U256::ZERO,
            callargs: test_selector,
        })];

        // Use any address for the estimator contract — it just needs to not
        // collide with SimEnvCallee
        let estimator_address = address!("0x000000000000000000000000000000000000E570");

        // This should succeed: the estimator replays the CALL to SimEnvCallee,
        // which checks that all env values match what was set in the constructor
        let result = estimate_state_changes_gas(
            &mut cache_db,
            estimator_address,
            caller,
            &state_updates,
            &sim_env,
        );

        assert!(
            result.is_ok(),
            "estimate_state_changes_gas should succeed when SimEnv is correct, got: {}",
            format_sim_env_error(&result.unwrap_err())
        );
    }

    #[test]
    fn test_sim_env_wrong_timestamp_reverts() {
        let caller = address!("0x000000000000000000000000000000000000c411");
        let sim_env = SimEnvOpts {
            number: 42,
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            coinbase: address!("0x00000000000000000000000000000000c01ba5e0"),
            prevrandao: B256::from(U256::from(0xdeadbeef_u64)),
            gas_price: 1_000_000_000,
        };

        let (mut cache_db, callee_address) = deploy_sim_env_test(caller, &sim_env);

        let test_selector = Bytes::from(vec![0xf8, 0xa8, 0xfd, 0x6d]);
        let state_updates = vec![StateUpdate::Call(IStateUpdateTypes::Call {
            target: callee_address,
            value: U256::ZERO,
            callargs: test_selector,
        })];

        let estimator_address = address!("0x000000000000000000000000000000000000E570");

        // Use a wrong timestamp — SimEnvCallee.test() should revert
        let wrong_env = SimEnvOpts {
            timestamp: 999,
            ..sim_env
        };

        let result = estimate_state_changes_gas(
            &mut cache_db,
            estimator_address,
            caller,
            &state_updates,
            &wrong_env,
        );

        assert!(
            result.is_err(),
            "estimate_state_changes_gas should fail when timestamp mismatches"
        );
    }
}

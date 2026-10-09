//! Gas estimation using revm directly.
//!
//! This crate provides WASM-compatible gas estimation by running state updates
//! through the StateChangeHandlerGasEstimator contract via revm. It is generic
//! over the database backend, allowing the same code to work with:
//! - RPC-backed state (native, via `CacheDB<&RpcDb>`)
//! - Empty state (WASM, via `CacheDB<EmptyDB>`)
//!
//! No reth-evm, no sp1-contract-call, no async, no I/O.

use std::collections::HashSet;
use std::sync::{LazyLock, OnceLock};

use alloy_dyn_abi::DynSolValue;
use alloy_primitives::{Address, B256, Bytes, U256};
use anyhow::{Result, anyhow};
use revm::context::result::ExecutionResult;
use revm::context_interface::transaction::{AccessList, SignedAuthorization};
use revm::database::CacheDB;
use revm::primitives::TxKind;
use revm::primitives::hardfork::SpecId;
use revm::state::AccountInfo;

use gas_analyzer_core::encoding::encode_state_updates_to_sol;
use gas_analyzer_core::heuristic::BASE_TX_COST;
use gas_analyzer_core::nested::FrameProgram;
use gas_analyzer_core::types::StateUpdate;

/// EIP-7825 per-tx gas cap, activated in Osaka (Fusaka). The block gas limit
/// can exceed this, but a single tx cannot use more than `2^24` gas.
const EIP7825_TX_GAS_CAP: u64 = 1 << 24;

/// Apply the EIP-7825 per-tx cap if the spec is Osaka or later.
///
/// Pre-Osaka blocks must not be capped — a legitimate ~30M-gas tx would
/// otherwise OOG mid-execution during replay, which (in `transact_commit`)
/// drops storage writes and silently corrupts the CacheDB.
pub(crate) fn effective_tx_gas_limit(gas_limit: u64, spec: SpecId) -> u64 {
    if spec >= SpecId::OSAKA {
        gas_limit.min(EIP7825_TX_GAS_CAP)
    } else {
        gas_limit
    }
}

/// Environment fields for the gas estimation simulation.
///
/// These are set on revm's `BlockEnv` and `TxEnv` so that contracts reading
/// opcodes like COINBASE, TIMESTAMP, NUMBER, GASLIMIT, GASPRICE, BASEFEE,
/// PREVRANDAO, or DIFFICULTY see realistic values.
///
/// `difficulty` is the legacy DIFFICULTY opcode value. Post-Merge it is zero
/// by protocol; pre-Merge it carries real PoW difficulty and is read directly
/// by the opcode under pre-Paris specs.
///
/// `spec` selects the EVM hardfork rules. It must be derived from the block
/// being simulated — using a newer spec for an older block applies wrong gas
/// schedules, opcode availability, and per-tx limits, which can flip
/// success/revert outcomes during preceding-tx replay and corrupt the
/// CacheDB state the analyzed tx will read.
///
/// `value` is the `msg.value` of the proxy invocation. Mirrors the original
/// transaction's `value` so contracts that pass-through ETH (deposit-then-forward,
/// intent settlers, swap routers) can fund value-bearing CALL state updates.
#[derive(Clone, Debug)]
pub struct SimEnvOpts {
    pub number: u64,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub coinbase: Address,
    pub prevrandao: B256,
    pub gas_price: u128,
    pub basefee: u64,
    pub difficulty: U256,
    pub spec: SpecId,
    pub value: U256,
}

const ESTIMATOR_ABI_JSON: &str = include_str!("../../../abis/StateChangeHandlerGasEstimator.json");

static ESTIMATOR_BYTECODE: OnceLock<Bytes> = OnceLock::new();

/// Returns the StateChangeHandlerGasEstimator deployed bytecode, parsed once per process.
///
/// Subsequent calls clone the inner `Arc<[u8]>` — no JSON parse or hex decode.
fn estimator_bytecode() -> Bytes {
    ESTIMATOR_BYTECODE
        .get_or_init(|| {
            let json: serde_json::Value = serde_json::from_str(ESTIMATOR_ABI_JSON)
                .expect("embedded estimator JSON is malformed");
            let hex_str = json["deployedBytecode"]["object"]
                .as_str()
                .expect("missing deployedBytecode.object in estimator artifact");
            let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
            hex::decode(hex_str)
                .expect("invalid hex in estimator bytecode")
                .into()
        })
        .clone()
}

static IMPL_SLOT: LazyLock<U256> = LazyLock::new(|| {
    U256::from_be_bytes(*alloy_primitives::keccak256("gas.estimator.implementation"))
        - U256::from(1)
});

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
    inject_estimator(cache_db, contract_address)?;
    run_estimator(
        cache_db,
        contract_address,
        caller_address,
        calldata,
        sim_env.value,
        sim_env,
        false,
    )
}

/// Where `contract_address`'s original code is kept once the estimator replaces it. Distinct per
/// contract, so several contracts can be injected into one state without overwriting each other's.
fn backup_address(contract_address: Address) -> Address {
    let mut preimage = b"gas.estimator.backup".to_vec();
    preimage.extend_from_slice(contract_address.as_slice());
    Address::from_slice(&alloy_primitives::keccak256(preimage)[12..])
}

/// Replace `contract_address`'s code with the estimator, keeping the original reachable.
///
/// StateChangeHandlerGasEstimator routes `runStateUpdatesCall` to its own logic and anything else
/// to a DELEGATECALL into the address in IMPL_SLOT, an EIP-1967-style isolated slot
/// (keccak256("gas.estimator.implementation") - 1). Stashing the original code there lets external
/// protocols (e.g. oracles) call back into `contract_address` during a state-update CALL and get
/// valid responses. The slot is written directly, so no constructor runs.
fn inject_estimator<DB>(cache_db: &mut CacheDB<DB>, contract_address: Address) -> Result<()>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    use revm::database_interface::DatabaseRef;

    let backup_addr = backup_address(contract_address);

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

    cache_db.insert_account_info(
        contract_address,
        AccountInfo {
            balance: original_account.balance,
            nonce: original_account.nonce,
            code_hash: B256::ZERO,
            code: Some(revm::state::Bytecode::new_raw(estimator_bytecode())),
        },
    );

    let backup_addr_u256 = U256::from_be_slice(backup_addr.as_slice());
    cache_db
        .insert_account_storage(contract_address, *IMPL_SLOT, backup_addr_u256)
        .map_err(|e| anyhow!("Failed to write IMPL_SLOT: {:?}", e))
}

/// Call the estimator at `contract_address` with `calldata`, committing its writes when `commit`.
fn run_estimator<DB>(
    cache_db: &mut CacheDB<DB>,
    contract_address: Address,
    caller_address: Address,
    calldata: Bytes,
    value: U256,
    sim_env: &SimEnvOpts,
    commit: bool,
) -> Result<u64>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    use revm::context::{Context, TxEnv};
    use revm::database_interface::DatabaseRef;
    use revm::{ExecuteCommitEvm, ExecuteEvm, MainBuilder, MainContext};

    // disable_balance_check skips the *pre-flight* check on the caller, but
    // revm still debits the caller during the call's value transfer. If the
    // caller's balance can't cover `value`, the proxy ends up
    // under-credited and any pass-through CALL with `value > 0` halts with
    // OutOfFunds. Top up the caller so the transfer is always well-defined.
    if !value.is_zero() {
        let caller_account = cache_db
            .basic_ref(caller_address)
            .ok()
            .flatten()
            .unwrap_or_default();
        cache_db.insert_account_info(
            caller_address,
            AccountInfo {
                balance: caller_account.balance.saturating_add(value),
                ..caller_account
            },
        );
    }

    let ctx = Context::mainnet()
        .with_db(&mut *cache_db)
        .modify_cfg_chained(|cfg| {
            cfg.disable_nonce_check = true;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
            cfg.disable_fee_charge = true;
            cfg.spec = sim_env.spec;
        })
        .modify_block_chained(|block| {
            block.number = U256::from(sim_env.number);
            block.timestamp = U256::from(sim_env.timestamp);
            block.gas_limit = sim_env.gas_limit;
            block.beneficiary = sim_env.coinbase;
            block.prevrandao = Some(sim_env.prevrandao);
            block.basefee = sim_env.basefee;
            block.difficulty = sim_env.difficulty;
        });

    let mut evm = ctx.build_mainnet();

    let tx_gas_limit = effective_tx_gas_limit(sim_env.gas_limit, sim_env.spec);

    let tx = TxEnv::builder()
        .caller(caller_address)
        .kind(revm::primitives::TxKind::Call(contract_address))
        .data(calldata)
        .value(value)
        .gas_limit(tx_gas_limit)
        .gas_price(sim_env.gas_price)
        .build()
        .map_err(|e| anyhow!("Failed to build tx env: {:?}", e))?;

    let result = if commit {
        evm.transact_commit(tx)
    } else {
        evm.transact(tx).map(|r| r.result)
    }
    .map_err(|e| anyhow!("Gas estimation failed: {:?}", e))?;

    match result {
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
#[tracing::instrument(name = "gas.evm_execute", skip_all, fields(block_number = sim_env.number, state_update_count = state_updates.len()))]
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
// Nested Frame Trees
// ============================================================================

/// Gas to apply each frame of a nested tree, in frame order, run in the order the transaction
/// executed them on one shared state.
///
/// Each program is split at its `NESTED` ops and the pieces run depth-first, the child's pieces
/// at the op that applies it, every piece committed so later ones see its writes. A frame's gas is
/// its pieces' gas, less the base transaction cost of every piece after the first. Separate pieces
/// pay cold access again, so a frame with children is priced slightly high.
///
/// Every piece is sent by `caller_address`: programs never read `msg.sender`, and a frame's real
/// caller is a contract, which revm refuses as a transaction sender (EIP-3607). The root's first
/// piece carries `root_value` and each child's first piece its own `value`, funded by the caller
/// rather than the parent frame.
pub fn estimate_frame_tree_gas<DB>(
    cache_db: &mut CacheDB<DB>,
    frames: &[FrameProgram],
    caller_address: Address,
    root_value: U256,
    sim_env: &SimEnvOpts,
) -> Result<Vec<u64>>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    if frames.is_empty() {
        return Err(anyhow!("a frame tree needs a root frame"));
    }
    // Once per contract: injecting again would stash the estimator itself as the original code.
    let mut injected = HashSet::new();
    for frame in frames {
        if injected.insert(frame.target) {
            inject_estimator(cache_db, frame.target)?;
        }
    }

    let mut frame_gas = vec![0; frames.len()];
    let tree = FrameTreeRun {
        frames,
        caller_address,
        sim_env,
    };
    tree.run(cache_db, 0, root_value, &mut frame_gas)?;
    Ok(frame_gas)
}

struct FrameTreeRun<'a> {
    frames: &'a [FrameProgram],
    caller_address: Address,
    sim_env: &'a SimEnvOpts,
}

impl FrameTreeRun<'_> {
    fn run<DB>(
        &self,
        cache_db: &mut CacheDB<DB>,
        index: usize,
        value: U256,
        frame_gas: &mut [u64],
    ) -> Result<()>
    where
        DB: revm::database_interface::DatabaseRef,
        <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
    {
        let frame = self
            .frames
            .get(index)
            .ok_or_else(|| anyhow!("frame {index} does not exist"))?;
        let mut children = frame.children.iter();
        let mut piece = Vec::new();
        let mut pieces = 0usize;
        let mut gas = 0u64;

        for update in &frame.updates {
            if !matches!(update, StateUpdate::Nested(_)) {
                piece.push(update.clone());
                continue;
            }
            // The first piece runs even when empty: it carries the frame's value and its base cost.
            if pieces == 0 || !piece.is_empty() {
                gas = gas.saturating_add(self.piece(cache_db, index, pieces, &piece, value)?);
                pieces += 1;
                piece.clear();
            }
            let child = *children
                .next()
                .ok_or_else(|| anyhow!("frame {index} has more NESTED ops than children"))?;
            if child <= index {
                return Err(anyhow!(
                    "frame {index} names child {child}, which does not follow it"
                ));
            }
            self.run(cache_db, child, self.frames[child].value, frame_gas)?;
        }
        if pieces == 0 || !piece.is_empty() {
            gas = gas.saturating_add(self.piece(cache_db, index, pieces, &piece, value)?);
        }
        if children.next().is_some() {
            return Err(anyhow!("frame {index} has more children than NESTED ops"));
        }

        frame_gas[index] = gas;
        Ok(())
    }

    /// Run one piece of frame `index`, returning its gas less the base cost after the first piece.
    fn piece<DB>(
        &self,
        cache_db: &mut CacheDB<DB>,
        index: usize,
        piece_index: usize,
        updates: &[StateUpdate],
        value: U256,
    ) -> Result<u64>
    where
        DB: revm::database_interface::DatabaseRef,
        <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
    {
        let frame = &self.frames[index];
        let calldata = build_gas_estimation_calldata(updates)?;
        let value = if piece_index == 0 { value } else { U256::ZERO };
        let gas = run_estimator(
            cache_db,
            frame.target,
            self.caller_address,
            calldata,
            value,
            self.sim_env,
            true,
        )
        .map_err(|e| {
            e.context(format!(
                "frame {index} ({}), piece {piece_index}",
                frame.target
            ))
        })?;
        Ok(if piece_index == 0 {
            gas
        } else {
            gas.saturating_sub(BASE_TX_COST)
        })
    }
}

// ============================================================================
// Preceding Transaction Replay
// ============================================================================

/// A simplified representation of a preceding transaction for replay.
///
/// This avoids bringing alloy-rpc-types into the gas-estimator crate,
/// keeping it WASM-compatible. The conversion from alloy's `Transaction`
/// type happens in the calling code.
///
/// `gas_price` is the *effective* per-gas price of the tx (post-EIP-1559:
/// `min(maxFeePerGas, baseFee + maxPriorityFeePerGas)`), so the GASPRICE
/// opcode returns the same value the original tx observed.
///
/// `access_list` (EIP-2930) pre-warms addresses and storage slots, lowering
/// SLOAD/cold-access costs. Omitting it makes replay gas costs higher than
/// the original tx, and a tight-budget tx can OOG mid-replay; an OOG halt
/// in `transact_commit` does not commit storage writes (only the nonce
/// bump), which silently corrupts the CacheDB state the analyzed tx will
/// then read against.
///
/// `authorization_list` (EIP-7702) carries set-code authorizations. Without
/// it, replay skips the EOA-to-bytecode delegation, so any later tx in the
/// block that calls into those EOAs sees an empty account.
#[derive(Clone, Debug)]
pub struct PrecedingTx {
    pub from: Address,
    pub kind: TxKind,
    pub input: Bytes,
    pub value: U256,
    pub gas_limit: u64,
    pub nonce: u64,
    pub gas_price: u128,
    pub access_list: AccessList,
    pub authorization_list: Vec<SignedAuthorization>,
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
/// `sim_env` supplies the block-level context (number, timestamp, gas
/// limit, coinbase, prevrandao, basefee) so opcodes like BASEFEE,
/// COINBASE, NUMBER, TIMESTAMP, and GASLIMIT return the same values they
/// would in the real block. Per-tx GASPRICE comes from `PrecedingTx::gas_price`.
///
/// Transaction results (success/revert/halt) are intentionally ignored —
/// in a real block even a reverted transaction still bumps the sender's
/// nonce. Fee transfers to the coinbase are *not* applied: replay sets
/// `disable_fee_charge`, so coinbase balance won't move and senders are
/// not debited for gas. Downstream callers must not rely on either.
pub fn replay_preceding_transactions<DB>(
    cache_db: &mut CacheDB<DB>,
    preceding_txs: &[PrecedingTx],
    sim_env: &SimEnvOpts,
) -> Result<Vec<ExecutionResult>>
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
            cfg.spec = sim_env.spec;
        })
        .modify_block_chained(|block| {
            block.number = U256::from(sim_env.number);
            block.timestamp = U256::from(sim_env.timestamp);
            block.gas_limit = sim_env.gas_limit;
            block.beneficiary = sim_env.coinbase;
            block.prevrandao = Some(sim_env.prevrandao);
            block.basefee = sim_env.basefee;
            block.difficulty = sim_env.difficulty;
        })
        .build_mainnet();

    let mut results = Vec::with_capacity(preceding_txs.len());
    for (i, tx) in preceding_txs.iter().enumerate() {
        // No EIP-7825 cap here. With `disable_fee_charge = true` the gas
        // limit is irrelevant to fee accounting, and capping would refuse
        // legitimate >16.7M-gas txs from pre-Osaka blocks. An OOG halt in
        // `transact_commit` would not commit storage writes — only bump
        // the nonce — silently corrupting the CacheDB state seen by every
        // subsequent replay and by the analyzed tx itself.
        let revm_tx = TxEnv::builder()
            .caller(tx.from)
            .kind(tx.kind)
            .data(tx.input.clone())
            .value(tx.value)
            .gas_limit(tx.gas_limit)
            .nonce(tx.nonce)
            .gas_price(tx.gas_price)
            .access_list(tx.access_list.clone())
            .authorization_list_signed(tx.authorization_list.clone())
            .build()
            .map_err(|e| anyhow!("Failed to build tx env for preceding tx {}: {:?}", i, e))?;

        // transact_commit executes and commits state changes to the CacheDB.
        // Reverted/halted txs still bump nonces; results are returned for
        // observability (gas accounting, halt reasons) and may be ignored.
        let result = evm
            .transact_commit(revm_tx)
            .map_err(|e| anyhow!("Failed to replay preceding tx {}: {:?}", i, e))?;
        results.push(result);
    }
    Ok(results)
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
            DynSolValue::Uint(U256::from(sim_env.basefee), 256),
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
                cfg.spec = sim_env.spec;
            })
            .modify_block_chained(|block| {
                block.number = U256::from(sim_env.number);
                block.timestamp = U256::from(sim_env.timestamp);
                block.gas_limit = sim_env.gas_limit;
                block.beneficiary = sim_env.coinbase;
                block.prevrandao = Some(sim_env.prevrandao);
                block.basefee = sim_env.basefee;
                block.difficulty = sim_env.difficulty;
            });

        let mut evm = ctx.build_mainnet();

        let deploy_tx = TxEnv::builder()
            .caller(caller)
            .kind(revm::primitives::TxKind::Create)
            .data(deploy_data.into())
            .value(U256::ZERO)
            // Stay under EIP-7825's 2^24 per-tx cap; SimEnvTestMain's
            // constructor uses well under this.
            .gas_limit(EIP7825_TX_GAS_CAP)
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
            basefee: 25_000_000_000,
            difficulty: U256::ZERO,
            spec: SpecId::OSAKA,
            value: U256::ZERO,
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
            basefee: 25_000_000_000,
            difficulty: U256::ZERO,
            spec: SpecId::OSAKA,
            value: U256::ZERO,
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

    #[test]
    fn test_sim_env_wrong_basefee_reverts() {
        let caller = address!("0x000000000000000000000000000000000000c411");
        let sim_env = SimEnvOpts {
            number: 42,
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            coinbase: address!("0x00000000000000000000000000000000c01ba5e0"),
            prevrandao: B256::from(U256::from(0xdeadbeef_u64)),
            gas_price: 1_000_000_000,
            basefee: 25_000_000_000,
            difficulty: U256::ZERO,
            spec: SpecId::OSAKA,
            value: U256::ZERO,
        };

        let (mut cache_db, callee_address) = deploy_sim_env_test(caller, &sim_env);

        let test_selector = Bytes::from(vec![0xf8, 0xa8, 0xfd, 0x6d]);
        let state_updates = vec![StateUpdate::Call(IStateUpdateTypes::Call {
            target: callee_address,
            value: U256::ZERO,
            callargs: test_selector,
        })];

        let estimator_address = address!("0x000000000000000000000000000000000000E570");

        // Use a wrong basefee — SimEnvCallee.test() should revert
        let wrong_env = SimEnvOpts {
            basefee: 1,
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
            "estimate_state_changes_gas should fail when basefee mismatches"
        );
    }

    #[test]
    fn test_sim_env_wrong_block_number_reverts() {
        let caller = address!("0x000000000000000000000000000000000000c411");
        let sim_env = SimEnvOpts {
            number: 42,
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            coinbase: address!("0x00000000000000000000000000000000c01ba5e0"),
            prevrandao: B256::from(U256::from(0xdeadbeef_u64)),
            gas_price: 1_000_000_000,
            basefee: 25_000_000_000,
            difficulty: U256::ZERO,
            spec: SpecId::OSAKA,
            value: U256::ZERO,
        };

        let (mut cache_db, callee_address) = deploy_sim_env_test(caller, &sim_env);

        let test_selector = Bytes::from(vec![0xf8, 0xa8, 0xfd, 0x6d]);
        let state_updates = vec![StateUpdate::Call(IStateUpdateTypes::Call {
            target: callee_address,
            value: U256::ZERO,
            callargs: test_selector,
        })];

        let estimator_address = address!("0x000000000000000000000000000000000000E570");

        // Use a wrong block number — SimEnvCallee.test() should revert
        let wrong_env = SimEnvOpts {
            number: 999,
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
            "estimate_state_changes_gas should fail when block number mismatches"
        );
    }

    /// Convenience: SimEnvOpts with sensible defaults at a chosen spec.
    fn sim_env_with_spec(spec: SpecId) -> SimEnvOpts {
        SimEnvOpts {
            number: 100,
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            coinbase: address!("0x00000000000000000000000000000000c01ba5e0"),
            prevrandao: B256::ZERO,
            gas_price: 0,
            basefee: 0,
            difficulty: U256::ZERO,
            spec,
            value: U256::ZERO,
        }
    }

    fn fund(cache_db: &mut CacheDB<EmptyDB>, addr: Address) {
        cache_db.insert_account_info(
            addr,
            AccountInfo {
                balance: U256::from(10u128.pow(24)),
                nonce: 0,
                code_hash: B256::ZERO,
                code: None,
            },
        );
    }

    /// EIP-7825's 2^24 per-tx gas cap is an Osaka-and-later rule. Pre-Osaka
    /// specs must pass `gas_limit` through unchanged — capping a historical
    /// 30M-gas tx at 16.7M would OOG it mid-replay and silently drop its
    /// storage writes.
    #[test]
    fn test_effective_tx_gas_limit_only_caps_under_osaka() {
        // Pre-Osaka: cap not applied.
        assert_eq!(
            effective_tx_gas_limit(30_000_000, SpecId::SHANGHAI),
            30_000_000
        );
        assert_eq!(
            effective_tx_gas_limit(30_000_000, SpecId::CANCUN),
            30_000_000
        );
        assert_eq!(
            effective_tx_gas_limit(30_000_000, SpecId::PRAGUE),
            30_000_000
        );

        // Osaka onward: capped at 2^24.
        assert_eq!(
            effective_tx_gas_limit(30_000_000, SpecId::OSAKA),
            EIP7825_TX_GAS_CAP
        );
        // Below the cap is left alone even under Osaka.
        assert_eq!(effective_tx_gas_limit(1_000_000, SpecId::OSAKA), 1_000_000);
    }

    /// A pre-Osaka preceding tx with `gas_limit > 2^24` must execute against
    /// its real limit during replay, not a truncated 16.7M. A `JUMPDEST/JUMP`
    /// gas burner is given 25M gas under Shanghai; its OOG `gas_used` must
    /// exceed 2^24, proving the limit was not capped before being handed
    /// to revm.
    #[test]
    fn test_replay_does_not_apply_eip7825_cap_pre_osaka() {
        let sender = address!("0x000000000000000000000000000000000000beef");
        let mut cache_db = CacheDB::new(EmptyDB::default());
        fund(&mut cache_db, sender);

        // JUMPDEST PUSH1 0 JUMP — infinite loop that burns gas until OOG.
        let burner_init = Bytes::from(hex::decode("5b600056").unwrap());

        let preceding = vec![PrecedingTx {
            from: sender,
            kind: TxKind::Create,
            input: burner_init,
            value: U256::ZERO,
            gas_limit: 25_000_000,
            nonce: 0,
            gas_price: 0,
            access_list: Default::default(),
            authorization_list: Default::default(),
        }];

        let sim_env = sim_env_with_spec(SpecId::SHANGHAI);
        let results =
            replay_preceding_transactions(&mut cache_db, &preceding, &sim_env).expect("replay");

        let result = &results[0];
        assert!(
            matches!(result, ExecutionResult::Halt { .. }),
            "burner should OOG-halt, got {:?}",
            result
        );
        let gas_used = result.gas_used();
        assert!(
            gas_used > EIP7825_TX_GAS_CAP,
            "gas burner only used {} gas — replay path is applying the EIP-7825 cap of {}",
            gas_used,
            EIP7825_TX_GAS_CAP
        );
        assert!(
            gas_used <= 25_000_000,
            "gas burner used {} > tx gas_limit",
            gas_used
        );
    }

    /// Under pre-Paris specs the DIFFICULTY opcode reads from
    /// `BlockEnv::difficulty`, so `SimEnvOpts::difficulty` must be plumbed
    /// through. Pre-deployed runtime `DIFFICULTY PUSH1 0 SSTORE` is invoked
    /// under GRAY_GLACIER and slot 0 of the target must equal
    /// `sim_env.difficulty` afterward.
    #[test]
    fn test_difficulty_propagates_to_block_env() {
        use revm::context::{Context, TxEnv};
        use revm::{ExecuteCommitEvm, MainBuilder, MainContext};

        let sender = address!("0x000000000000000000000000000000000000beef");
        let target = address!("0x00000000000000000000000000000000d1ff1c11");

        // DIFFICULTY (0x44) PUSH1 0 (0x6000) SSTORE (0x55) STOP (0x00)
        let runtime =
            revm::state::Bytecode::new_raw(Bytes::from(hex::decode("4460005500").unwrap()));

        let mut cache_db = CacheDB::new(EmptyDB::default());
        fund(&mut cache_db, sender);
        cache_db.insert_account_info(
            target,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: B256::ZERO,
                code: Some(runtime),
            },
        );

        let mut sim_env = sim_env_with_spec(SpecId::GRAY_GLACIER); // pre-Paris
        sim_env.difficulty = U256::from(0xdeadbeef_u64);

        // Mirror the production replay setup so we directly test the
        // BlockEnv plumbing of `sim_env.difficulty`.
        let mut evm = Context::mainnet()
            .with_db(&mut cache_db)
            .modify_cfg_chained(|cfg| {
                cfg.disable_nonce_check = true;
                cfg.disable_balance_check = true;
                cfg.disable_base_fee = true;
                cfg.disable_fee_charge = true;
                cfg.spec = sim_env.spec;
            })
            .modify_block_chained(|block| {
                block.number = U256::from(sim_env.number);
                block.timestamp = U256::from(sim_env.timestamp);
                block.gas_limit = sim_env.gas_limit;
                block.beneficiary = sim_env.coinbase;
                block.basefee = sim_env.basefee;
                block.difficulty = sim_env.difficulty;
            })
            .build_mainnet();

        let tx = TxEnv::builder()
            .caller(sender)
            .kind(TxKind::Call(target))
            .gas_limit(1_000_000)
            .gas_price(0)
            .build()
            .unwrap();
        evm.transact_commit(tx).expect("difficulty contract call");

        use revm::database_interface::DatabaseRef;
        let stored = cache_db
            .storage_ref(target, U256::ZERO)
            .expect("storage_ref");
        assert_eq!(
            stored,
            U256::from(0xdeadbeef_u64),
            "DIFFICULTY opcode read {:?}, expected sim_env.difficulty 0xdeadbeef",
            stored
        );
    }

    /// `PrecedingTx::access_list` must reach revm's `TxEnv` during replay.
    /// EIP-2930 charges 2400 (address) + 1900 (slot) = 4300 intrinsic gas
    /// per entry, partly offset by 2000 saved on the first warm SLOAD; the
    /// same SLOAD tx with vs without the slot pre-warmed should differ by
    /// exactly 2300 gas. A delta of zero would mean the field is being
    /// dropped before it reaches revm.
    #[test]
    fn test_access_list_threaded_through_replay() {
        use revm::context_interface::transaction::AccessListItem;

        let sender = address!("0x000000000000000000000000000000000000beef");
        let target = address!("0x000000000000000000000000000000000000515a");

        // PUSH1 5 SLOAD STOP — just touches slot 5 once.
        let runtime = revm::state::Bytecode::new_raw(Bytes::from(hex::decode("60055400").unwrap()));

        let make_db = || {
            let mut db = CacheDB::new(EmptyDB::default());
            fund(&mut db, sender);
            db.insert_account_info(
                target,
                AccountInfo {
                    balance: U256::ZERO,
                    nonce: 0,
                    code_hash: B256::ZERO,
                    code: Some(runtime.clone()),
                },
            );
            db
        };

        let base_tx = || PrecedingTx {
            from: sender,
            kind: TxKind::Call(target),
            input: Bytes::new(),
            value: U256::ZERO,
            gas_limit: 100_000,
            nonce: 0,
            gas_price: 0,
            access_list: Default::default(),
            authorization_list: Default::default(),
        };

        let sim_env = sim_env_with_spec(SpecId::SHANGHAI);

        let mut db_no_al = make_db();
        let mut tx_no_al = base_tx();
        let no_al_results =
            replay_preceding_transactions(&mut db_no_al, &[tx_no_al.clone()], &sim_env)
                .expect("replay no_al");

        let mut db_with_al = make_db();
        tx_no_al.access_list = AccessList(vec![AccessListItem {
            address: target,
            storage_keys: vec![B256::from(U256::from(5u64))],
        }]);
        let with_al_results = replay_preceding_transactions(&mut db_with_al, &[tx_no_al], &sim_env)
            .expect("replay with_al");

        let gas_no_al = no_al_results[0].gas_used();
        let gas_with_al = with_al_results[0].gas_used();
        assert_eq!(
            gas_with_al as i64 - gas_no_al as i64,
            2300,
            "access list field is not being threaded into the replay TxEnv \
             (gas_used: with_al={} no_al={})",
            gas_with_al,
            gas_no_al
        );
    }

    /// A signed EIP-7702 authorization carried in
    /// `PrecedingTx::authorization_list` must be applied during replay: the
    /// authority's account in the `CacheDB` should gain the 23-byte
    /// `0xef0100 || delegated_address` indicator code post-replay.
    #[test]
    fn test_authorization_list_applies_delegation() {
        use alloy_signer::SignerSync;
        use alloy_signer_local::PrivateKeySigner;
        use revm::context_interface::transaction::Authorization;
        use revm::database_interface::DatabaseRef;

        // A test private key (32 bytes of 0x42).
        let signer = PrivateKeySigner::from_bytes(&B256::repeat_byte(0x42)).unwrap();
        let authority: Address = signer.address();
        let delegated = address!("0x00000000000000000000000000000000de1e6a7e");

        // EIP-7702 requires chain_id == tx.chain_id || 0. revm's
        // mainnet config defaults to chain_id 1. Sign for chain 1.
        let auth = Authorization {
            chain_id: U256::from(1u64),
            address: delegated,
            nonce: 0,
        };
        let sig = signer.sign_hash_sync(&auth.signature_hash()).unwrap();
        let signed = auth.into_signed(sig);

        let funder = address!("0x000000000000000000000000000000000000beef");
        let recipient = address!("0x000000000000000000000000000000000000d057");
        let mut cache_db = CacheDB::new(EmptyDB::default());
        fund(&mut cache_db, funder);
        // The authority itself needs to exist (nonce 0) for the
        // authorization to apply.
        fund(&mut cache_db, authority);

        // The "preceding tx" that carries the authorization. A simple value
        // transfer is enough — the authorization is applied during
        // intrinsic-gas processing regardless of execution outcome.
        let tx = PrecedingTx {
            from: funder,
            kind: TxKind::Call(recipient),
            input: Bytes::new(),
            value: U256::ZERO,
            gas_limit: 200_000,
            nonce: 0,
            gas_price: 0,
            access_list: Default::default(),
            authorization_list: vec![signed],
        };

        let sim_env = sim_env_with_spec(SpecId::PRAGUE); // 7702 active

        replay_preceding_transactions(&mut cache_db, &[tx], &sim_env).expect("replay");

        let info = cache_db
            .basic_ref(authority)
            .expect("basic_ref")
            .expect("authority must exist");
        let code = info.code.expect("authority must have code post-delegation");
        let bytes = code.original_bytes();
        // EIP-7702 delegation indicator: 0xef0100 || target_address (23 bytes).
        assert_eq!(
            bytes.len(),
            23,
            "expected 23-byte delegation indicator, got {} bytes — \
             authorization_list is not reaching the replay TxEnv",
            bytes.len()
        );
        assert_eq!(&bytes[..3], &[0xef, 0x01, 0x00], "delegation prefix");
        assert_eq!(&bytes[3..], delegated.as_slice(), "delegated address");
    }

    // ========================================================================
    // Unbounded-profile gate: the analytic bound vs. the measured cost
    // ========================================================================

    /// `gas_analyzer_core::sim_profile::estimate_applied_payload_gas` decides which payloads the
    /// unbounded profile accepts, and it has to do so *analytically* — a verdict derived from live
    /// chain state could put two honest operators on opposite sides of the boundary and split
    /// quorum. That makes it a hand-written reimplementation of a cost model, and a hand-written
    /// model contains exactly the terms someone remembered to write down. The first version
    /// omitted the payload's own calldata and the handler's decode/dispatch loop — together ~11%
    /// per store, which is invisible except within a few percent of the ceiling, exactly where an
    /// admission gate lives.
    ///
    /// This test closes that gap permanently by using the measured path as an oracle for the
    /// analytic one: revm executing the real payload against the real handler charges intrinsic
    /// calldata gas and dispatch overhead automatically, so whatever the analytic model forgets
    /// shows up here as a shortfall. Any future dropped term fails this test rather than shipping
    /// as an over-admitting gate.
    ///
    /// The signature floor is passed as 0, deliberately: at 250,000 it would mask a missing
    /// per-update term until the payload grew past ~200 updates.
    #[test]
    fn analytic_bound_dominates_measured_apply_cost() {
        use gas_analyzer_core::sim_profile::estimate_applied_payload_gas;

        let target = address!("0x00000000000000000000000000000000000c0de0");
        let caller = address!("0x000000000000000000000000000000000000beef");

        // CANCUN, not OSAKA: `effective_tx_gas_limit` clamps to EIP-7825's 2^24 under OSAKA, which
        // would OOG the largest payloads here before they could be measured.
        let mut sim_env = sim_env_with_spec(SpecId::CANCUN);
        sim_env.gas_limit = 200_000_000;

        // Slot/value entropy matters: it decides the zero/non-zero split of the encoded payload,
        // and therefore its transport cost. Cover both ends.
        let sparse_store = |i: usize| {
            StateUpdate::Store(IStateUpdateTypes::Store {
                slot: B256::from(U256::from(i as u64 + 1)),
                value: B256::from(U256::from(1u64)),
            })
        };
        let dense_store = |i: usize| {
            StateUpdate::Store(IStateUpdateTypes::Store {
                slot: B256::from(U256::MAX - U256::from(i as u64)),
                value: B256::from(U256::MAX - U256::from(i as u64 * 7 + 3)),
            })
        };
        let fat_log = |i: usize| {
            StateUpdate::Log1(IStateUpdateTypes::Log1 {
                data: Bytes::from(vec![(i % 251) as u8 + 1; 128]),
                topic1: B256::from(U256::from(i as u64 + 1)),
            })
        };

        /// One payload shape to sweep: a name and a per-index update builder.
        type Shape<'a> = (&'a str, &'a dyn Fn(usize) -> StateUpdate);

        let mixed = |i: usize| match i % 3 {
            0 => fat_log(i),
            1 => dense_store(i),
            _ => sparse_store(i),
        };
        let shapes: [Shape; 4] = [
            ("sparse stores", &sparse_store),
            ("dense stores", &dense_store),
            ("fat logs", &fat_log),
            ("mixed", &mixed),
        ];

        for (name, build) in shapes {
            for n in [1usize, 2, 5, 25, 100, 400, 650] {
                let updates: Vec<StateUpdate> = (0..n).map(build).collect();

                let mut db = CacheDB::new(EmptyDB::default());
                fund(&mut db, caller);
                let measured =
                    estimate_state_changes_gas(&mut db, target, caller, &updates, &sim_env)
                        .unwrap_or_else(|e| panic!("{name} n={n}: measurement failed: {e:?}"));

                let analytic = estimate_applied_payload_gas(&updates, 0, 0);

                assert!(
                    analytic >= measured,
                    "{name} n={n}: analytic bound {analytic} is BELOW the measured cost \
                     {measured} (short by {}). The gate would admit a payload that does not fit. \
                     A term is missing from estimate_applied_payload_gas — see its doc comment.",
                    measured - analytic
                );
            }
        }
    }

    // ── Nested frame trees ───────────────────────────────────────────────

    /// Reverts unless storage slot 0 is set; run at a frame target, it is reached through the
    /// estimator's fallback and reads that target's storage.
    const REQUIRE_SLOT0: [u8; 13] = [
        0x60, 0x00, 0x54, 0x15, 0x60, 0x08, 0x57, 0x00, 0x5b, 0x60, 0x00, 0x80, 0xfd,
    ];
    const ALWAYS_REVERT: [u8; 4] = [0x60, 0x00, 0x80, 0xfd];

    fn with_code(cache_db: &mut CacheDB<EmptyDB>, addr: Address, code: &[u8]) {
        let code = revm::state::Bytecode::new_raw(Bytes::copy_from_slice(code));
        cache_db.insert_account_info(
            addr,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 1,
                code_hash: code.hash_slow(),
                code: Some(code),
            },
        );
    }

    fn frame(target: Address, updates: Vec<StateUpdate>, children: Vec<usize>) -> FrameProgram {
        FrameProgram {
            target,
            caller: Address::ZERO,
            value: U256::ZERO,
            calldata_hash: B256::ZERO,
            transition_index: None,
            updates,
            children,
        }
    }

    fn store_slot0() -> StateUpdate {
        StateUpdate::Store(IStateUpdateTypes::Store {
            slot: B256::ZERO,
            value: B256::from(U256::from(1u64)),
        })
    }

    /// Non-empty callargs: the estimator answers empty calldata itself instead of falling back.
    fn call(target: Address) -> StateUpdate {
        StateUpdate::Call(IStateUpdateTypes::Call {
            target,
            value: U256::ZERO,
            callargs: Bytes::from_static(&[0x01]),
        })
    }

    fn nested(target: Address) -> StateUpdate {
        StateUpdate::Nested(IStateUpdateTypes::Nested {
            target,
            value: U256::ZERO,
            childLeaf: B256::ZERO,
        })
    }

    /// A parent that reads what its child wrote succeeds only when the child runs first, at its
    /// `NESTED` op; priced from the state before the transaction, the parent reverts.
    #[test]
    fn test_frame_tree_parent_reads_child_write() {
        let root = address!("0x00000000000000000000000000000000000a0000");
        let child = address!("0x00000000000000000000000000000000000b0000");
        let caller = address!("0x000000000000000000000000000000000000beef");
        let sim_env = sim_env_with_spec(SpecId::CANCUN);
        let frames = [
            frame(root, vec![nested(child), call(child)], vec![1]),
            frame(child, vec![store_slot0()], vec![]),
        ];

        let mut isolated = CacheDB::new(EmptyDB::default());
        with_code(&mut isolated, child, &REQUIRE_SLOT0);
        let root_alone = gas_analyzer_core::nested::without_nested_ops(&frames[0].updates);
        assert!(
            estimate_state_changes_gas(&mut isolated, root, caller, &root_alone, &sim_env).is_err(),
            "the parent must depend on the child's write for this test to mean anything"
        );

        let mut cache_db = CacheDB::new(EmptyDB::default());
        with_code(&mut cache_db, child, &REQUIRE_SLOT0);
        let frame_gas =
            estimate_frame_tree_gas(&mut cache_db, &frames, caller, U256::ZERO, &sim_env)
                .expect("the child's write is visible to the parent's later piece");
        assert_eq!(frame_gas.len(), 2);
        assert!(frame_gas.iter().all(|&g| g > 0));
    }

    /// A child that calls back into its parent sees the parent's earlier writes, through the
    /// parent's own original code even after the child's target was injected too.
    #[test]
    fn test_frame_tree_child_reads_parent_write_through_its_own_backup() {
        let root = address!("0x00000000000000000000000000000000000a0000");
        let child = address!("0x00000000000000000000000000000000000b0000");
        let caller = address!("0x000000000000000000000000000000000000beef");
        let sim_env = sim_env_with_spec(SpecId::CANCUN);
        let frames = [
            frame(root, vec![store_slot0(), nested(child)], vec![1]),
            frame(child, vec![call(root)], vec![]),
        ];

        let mut cache_db = CacheDB::new(EmptyDB::default());
        with_code(&mut cache_db, root, &REQUIRE_SLOT0);
        with_code(&mut cache_db, child, &ALWAYS_REVERT);
        estimate_frame_tree_gas(&mut cache_db, &frames, caller, U256::ZERO, &sim_env)
            .expect("the callback reaches the root's own code and its earlier write");
    }

    /// A trailing empty piece is skipped, so a frame whose writes all precede its `NESTED` op
    /// costs what the same program costs alone.
    #[test]
    fn test_frame_tree_skips_empty_trailing_piece() {
        let root = address!("0x00000000000000000000000000000000000a0000");
        let child = address!("0x00000000000000000000000000000000000b0000");
        let caller = address!("0x000000000000000000000000000000000000beef");
        let sim_env = sim_env_with_spec(SpecId::CANCUN);
        let frames = [
            frame(root, vec![store_slot0(), nested(child)], vec![1]),
            frame(child, vec![store_slot0()], vec![]),
        ];

        let mut cache_db = CacheDB::new(EmptyDB::default());
        let frame_gas =
            estimate_frame_tree_gas(&mut cache_db, &frames, caller, U256::ZERO, &sim_env).unwrap();

        let mut alone = CacheDB::new(EmptyDB::default());
        let root_alone =
            estimate_state_changes_gas(&mut alone, root, caller, &[store_slot0()], &sim_env)
                .unwrap();
        assert_eq!(frame_gas[0], root_alone);
    }

    #[test]
    fn test_frame_tree_rejects_mismatched_children() {
        let root = address!("0x00000000000000000000000000000000000a0000");
        let caller = address!("0x000000000000000000000000000000000000beef");
        let sim_env = sim_env_with_spec(SpecId::CANCUN);

        let too_few = [frame(root, vec![nested(root)], vec![])];
        let err = estimate_frame_tree_gas(
            &mut CacheDB::new(EmptyDB::default()),
            &too_few,
            caller,
            U256::ZERO,
            &sim_env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("more NESTED ops than children"));

        let too_many = [
            frame(root, vec![store_slot0()], vec![1]),
            frame(root, vec![], vec![]),
        ];
        let err = estimate_frame_tree_gas(
            &mut CacheDB::new(EmptyDB::default()),
            &too_many,
            caller,
            U256::ZERO,
            &sim_env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("more children than NESTED ops"));
    }
}

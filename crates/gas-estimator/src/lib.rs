//! Gas estimation using revm directly.
//!
//! This crate provides WASM-compatible gas estimation by running state updates
//! through the StateChangeHandlerGasEstimator contract via revm. It is generic
//! over the database backend, allowing the same code to work with:
//! - RPC-backed state (native, via `CacheDB<&RpcDb>`)
//! - Empty state (WASM, via `CacheDB<EmptyDB>`)
//!
//! No reth-evm, no sp1-contract-call, no async, no I/O.

use std::collections::HashMap;
use std::sync::LazyLock;

use alloy_dyn_abi::DynSolValue;
use alloy_primitives::{Address, B256, Bytes, U256, b256};
use alloy_sol_types::{SolCall, SolError};
use anyhow::{Result, anyhow};
use revm::context::TxEnv;
use revm::context::result::ExecutionResult;
use revm::context_interface::transaction::{AccessList, SignedAuthorization};
use revm::database::CacheDB;
use revm::primitives::TxKind;
use revm::primitives::hardfork::SpecId;
use revm::state::AccountInfo;

pub use gas_analyzer_core::SignedFormat;
use gas_analyzer_core::encoding::encode_state_updates_to_sol;
use gas_analyzer_core::heuristic::{BASE_TX_COST, calldata_gas};
use gas_analyzer_core::nested::FrameProgram;
use gas_analyzer_core::sim_profile::STATE_TRACKER_SLOT;
use gas_analyzer_core::types::{RevertingContext, StateUpdate};

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
///
/// `format` is the signed format of the contracts the program is applied to, which picks the
/// estimator: v0.2.0 settlement checks every STORE against the consumer's protected ranges.
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
    pub format: SignedFormat,
}

const ESTIMATOR_ABI_JSON: &str = include_str!("../../../abis/StateChangeHandlerGasEstimator.json");
const ESTIMATOR_V0_2_ABI_JSON: &str =
    include_str!("../../../abis/StateChangeHandlerGasEstimatorV0_2.json");

/// The deployed bytecode in a Foundry artifact.
fn deployed_bytecode(artifact: &str) -> Bytes {
    let json: serde_json::Value =
        serde_json::from_str(artifact).expect("embedded estimator JSON is malformed");
    let hex_str = json["deployedBytecode"]["object"]
        .as_str()
        .expect("missing deployedBytecode.object in estimator artifact");
    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    hex::decode(hex_str)
        .expect("invalid hex in estimator bytecode")
        .into()
}

/// An estimator's analyzed code and its hash.
fn analyzed_code(artifact: &str) -> (revm::state::Bytecode, B256) {
    let code = revm::state::Bytecode::new_raw(deployed_bytecode(artifact));
    let hash = code.hash_slow();
    (code, hash)
}

/// Each estimator's analyzed code and hash, built once: injection runs per estimate, and
/// hashing the code there dominated `estimate_gas_raw` against an empty state.
static ESTIMATOR_CODE: LazyLock<(revm::state::Bytecode, B256)> =
    LazyLock::new(|| analyzed_code(ESTIMATOR_ABI_JSON));
static ESTIMATOR_V0_2_CODE: LazyLock<(revm::state::Bytecode, B256)> =
    LazyLock::new(|| analyzed_code(ESTIMATOR_V0_2_ABI_JSON));

/// The estimator for `format`'s settlement.
fn estimator_code(format: SignedFormat) -> (revm::state::Bytecode, B256) {
    match format {
        SignedFormat::V0_1 => ESTIMATOR_CODE.clone(),
        SignedFormat::V0_2 => ESTIMATOR_V0_2_CODE.clone(),
    }
}

mod v0_2 {
    alloy_sol_types::sol! {
        /// solidity-sdk v0.2.0's `StorageGuard.ProtectedRange`.
        #[derive(Debug, PartialEq, Eq)]
        struct ProtectedRange {
            uint256 start;
            uint256 end;
            bytes32 mask;
        }

        /// `StateChangeHandlerGasEstimatorV0_2`'s entry point.
        function runStateUpdatesCall(uint8[] types, bytes[] args, ProtectedRange[] ranges);

        /// A v0.2.0 consumer's protected ranges, as its settlement checks them.
        function gasKillerProtectedRanges() external view returns (ProtectedRange[] memory);

        /// A STORE at `index` would change a protected slot.
        #[derive(Debug, PartialEq, Eq)]
        error ProtectedSlotWrite(uint256 index, bytes32 slot);

        /// What the op at `index` pinned isn't what it observes now.
        #[derive(Debug, PartialEq, Eq)]
        error ObservationMismatch(uint256 index, bytes32 expected, bytes32 actual);

        /// The `CALL` at `index` can't give its callee all of `gasLimit`.
        #[derive(Debug, PartialEq, Eq)]
        error InsufficientCallGas(uint256 index, uint256 gasLimit);

        /// The op at `index` isn't in its canonical layout.
        #[derive(Debug, PartialEq, Eq)]
        error MalformedOp(uint256 index);
    }
}

pub use v0_2::{
    InsufficientCallGas, MalformedOp, ObservationMismatch, ProtectedRange, ProtectedSlotWrite,
};

/// solidity-sdk v0.2.0's `GasKillerSDK` ERC-7201 configuration namespace, which settlement
/// protects whole (the low byte spans it).
const SDK_CONFIG_NAMESPACE: B256 =
    b256!("0x1d6f9f139320a34a32f3b29eb8638270178e831962a74100c9e8b433f21e1200");

/// The ranges every solidity-sdk v0.2.0 consumer not behind a proxy protects: the SDK's
/// configuration namespace and the transition counter. Used for a target that doesn't report its
/// own, so a contract that isn't a consumer yet is priced as one would be.
pub fn default_protected_ranges() -> Vec<ProtectedRange> {
    let mut config_end = SDK_CONFIG_NAMESPACE;
    config_end.0[31] = 0xff;
    let range = |start: B256, end: B256| ProtectedRange {
        start: start.into(),
        end: end.into(),
        mask: B256::repeat_byte(0xff),
    };
    vec![
        range(SDK_CONFIG_NAMESPACE, config_end),
        range(STATE_TRACKER_SLOT, STATE_TRACKER_SLOT),
    ]
}

/// Whether `ranges` is a list the SDK's constructor would accept: each range ordered, and each
/// starting after the one before ends. A contract that answers `gasKillerProtectedRanges()` with
/// anything else isn't a consumer, and the guard would fail open on it.
fn well_formed(ranges: &[ProtectedRange]) -> bool {
    ranges.iter().all(|r| r.start <= r.end)
        && ranges.windows(2).all(|pair| pair[0].end < pair[1].start)
}

/// The ranges `target`'s settlement checks STOREs against, read from its code before the
/// estimator replaces it. Empty for a consumer that opted out of the guard; the defaults for a
/// target that doesn't report a well-formed list.
fn protected_ranges<DB>(
    cache_db: &mut CacheDB<DB>,
    target: Address,
    caller_address: Address,
    sim_env: &SimEnvOpts,
) -> Vec<ProtectedRange>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    use revm::{ExecuteEvm, MainBuilder};

    let data = Bytes::from(v0_2::gasKillerProtectedRangesCall {}.abi_encode());
    let reported = estimator_tx(target, caller_address, data, U256::ZERO, sim_env)
        .ok()
        .and_then(|tx| {
            let mut evm = estimator_context(cache_db, sim_env).build_mainnet();
            evm.transact(tx).ok()
        })
        .and_then(|outcome| match outcome.result {
            ExecutionResult::Success { output, .. } => {
                v0_2::gasKillerProtectedRangesCall::abi_decode_returns(output.data()).ok()
            }
            _ => None,
        })
        .filter(|ranges| well_formed(ranges));
    reported.unwrap_or_else(default_protected_ranges)
}

/// Build the calldata for `StateChangeHandlerGasEstimatorV0_2`'s
/// `runStateUpdatesCall(uint8[], bytes[], (uint256,uint256,bytes32)[])`.
pub fn build_gas_estimation_calldata_v0_2(
    state_updates: &[StateUpdate],
    ranges: &[ProtectedRange],
) -> Bytes {
    let (types, args) = encode_state_updates_to_sol(state_updates, SignedFormat::V0_2);
    v0_2::runStateUpdatesCallCall {
        types: types.iter().map(|t| *t as u8).collect(),
        args,
        ranges: ranges.to_vec(),
    }
    .abi_encode()
    .into()
}

/// How a target injected by [`inject_estimator`] applies a program.
#[derive(Clone, Debug)]
enum Handler {
    V0_1,
    /// With the target's protected ranges.
    V0_2(Vec<ProtectedRange>),
}

impl Handler {
    /// The calldata applying `updates`, and the gas of the bytes in it that settlement doesn't
    /// send: the ranges, which a consumer builds in memory. Deducting them keeps both formats'
    /// figures to what settling the program costs.
    fn calldata(&self, updates: &[StateUpdate]) -> Result<(Bytes, u64)> {
        let flat = build_gas_estimation_calldata(updates)?;
        match self {
            Self::V0_1 => Ok((flat, 0)),
            Self::V0_2(ranges) => {
                let data = build_gas_estimation_calldata_v0_2(updates, ranges);
                let extra = calldata_gas(&data).saturating_sub(calldata_gas(&flat));
                Ok((data, extra))
            }
        }
    }
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
    let (types, args) = encode_state_updates_to_sol(state_updates, SignedFormat::V0_1);

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
    if sim_env.format != SignedFormat::V0_1 {
        return Err(anyhow!(
            "estimate_gas_raw takes v0.1.0 calldata; price {:?} programs with \
             estimate_state_changes_gas",
            sim_env.format
        ));
    }
    inject_estimator(cache_db, contract_address, caller_address, sim_env)?;
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

/// Total ETH the `Call` updates send out of the contract applying them.
fn call_value_total(state_updates: &[StateUpdate]) -> U256 {
    state_updates
        .iter()
        .filter_map(|update| match update {
            StateUpdate::Call(call) => Some(call.value),
            _ => None,
        })
        .fold(U256::ZERO, U256::saturating_add)
}

/// Give `contract_address` at least `required` wei before the estimator replaces its code (#127).
///
/// A `Call` update sends its `value` from the contract's own balance. On chain that ETH may have
/// arrived mid-transaction in a way the program doesn't replay, so a contract that held less at
/// the forked block would halt the estimate with `OutOfFunds`, though a CALL costs the same gas
/// whatever the sender's balance. Only pricing is affected: a contract already holding enough is
/// left untouched, so balance checks against it read what they read on chain. Runs before
/// injection, which carries the balance over.
///
/// If the account can't be read, nothing is funded: writing over an account that was never read
/// could lower its real balance and zero its nonce, and would hide the backend failure behind a
/// clean-looking estimate. The estimate then fails as it would without funding.
fn fund_value_calls<DB>(cache_db: &mut CacheDB<DB>, contract_address: Address, required: U256)
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    use revm::database_interface::DatabaseRef;

    if required.is_zero() {
        return;
    }
    let account = match cache_db.basic_ref(contract_address) {
        Ok(account) => account.unwrap_or_default(),
        Err(e) => {
            tracing::debug!(
                %contract_address,
                error = ?e,
                "not funding value calls: account read failed"
            );
            return;
        }
    };
    if account.balance >= required {
        return;
    }
    cache_db.insert_account_info(
        contract_address,
        AccountInfo {
            balance: required,
            ..account
        },
    );
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
///
/// Injects `sim_env.format`'s estimator. For v0.2.0 it first reads the target's protected ranges
/// from its original code, and returns them in the handler.
fn inject_estimator<DB>(
    cache_db: &mut CacheDB<DB>,
    contract_address: Address,
    caller_address: Address,
    sim_env: &SimEnvOpts,
) -> Result<Handler>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    use revm::database_interface::DatabaseRef;

    let handler = match sim_env.format {
        SignedFormat::V0_1 => Handler::V0_1,
        SignedFormat::V0_2 => Handler::V0_2(protected_ranges(
            cache_db,
            contract_address,
            caller_address,
            sim_env,
        )),
    };
    let backup_addr = backup_address(contract_address);

    let original_account = cache_db
        .basic_ref(contract_address)
        .ok()
        .flatten()
        .unwrap_or_default();

    let (estimator, estimator_hash) = estimator_code(sim_env.format);
    let original_hash = match original_account.code {
        Some(code) if !code.is_empty() => {
            let stored = original_account.code_hash;
            let hash = if stored.is_zero() || stored == revm::primitives::KECCAK_EMPTY {
                code.hash_slow()
            } else {
                stored
            };
            // Inserted first, so the original code is what `contracts` keeps under its hash.
            cache_db.insert_account_info(
                backup_addr,
                AccountInfo {
                    balance: U256::ZERO,
                    nonce: 0,
                    code_hash: hash,
                    code: Some(code),
                },
            );
            hash
        }
        _ => estimator_hash,
    };

    cache_db.insert_account_info(
        contract_address,
        AccountInfo {
            balance: original_account.balance,
            nonce: original_account.nonce,
            code_hash: estimator_hash,
            code: Some(estimator),
        },
    );
    // EXTCODEHASH reads the stored hash, so a callee checking its caller's codehash sees the
    // original code, as on chain, while the estimator's code still runs. Set after the insert so
    // `contracts` never maps the original hash to the estimator.
    if let Some(account) = cache_db.cache.accounts.get_mut(&contract_address) {
        account.info.code_hash = original_hash;
    }

    let backup_addr_u256 = U256::from_be_slice(backup_addr.as_slice());
    cache_db
        .insert_account_storage(contract_address, *IMPL_SLOT, backup_addr_u256)
        .map_err(|e| anyhow!("Failed to write IMPL_SLOT: {:?}", e))?;
    Ok(handler)
}

/// Credit the caller with `value` on top of its balance.
///
/// disable_balance_check skips the *pre-flight* check on the caller, but revm still debits the
/// caller during the call's value transfer. If the caller's balance can't cover `value`, the proxy
/// ends up under-credited and any pass-through CALL with `value > 0` halts with OutOfFunds. Top up
/// the caller so the transfer is always well-defined.
fn top_up_caller<DB>(cache_db: &mut CacheDB<DB>, caller_address: Address, value: U256)
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    use revm::database_interface::DatabaseRef;

    if value.is_zero() {
        return;
    }
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

/// The block and config the estimator runs under, over `cache_db`.
fn estimator_context<'a, DB>(
    cache_db: &'a mut CacheDB<DB>,
    sim_env: &SimEnvOpts,
) -> revm::handler::MainnetContext<&'a mut CacheDB<DB>>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    use revm::{Context, MainContext};

    Context::mainnet()
        .with_db(cache_db)
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
}

/// The transaction calling the estimator at `contract_address` with `calldata`.
fn estimator_tx(
    contract_address: Address,
    caller_address: Address,
    calldata: Bytes,
    value: U256,
    sim_env: &SimEnvOpts,
) -> Result<TxEnv> {
    TxEnv::builder()
        .caller(caller_address)
        .kind(TxKind::Call(contract_address))
        .data(calldata)
        .value(value)
        .gas_limit(effective_tx_gas_limit(sim_env.gas_limit, sim_env.spec))
        .gas_price(sim_env.gas_price)
        .build()
        .map_err(|e| anyhow!("Failed to build tx env: {:?}", e))
}

/// The estimator transaction reverted.
///
/// Displays as `Gas estimation reverted (gas: N): 0x<output>`. [`reverting_context`] reads
/// `output` back from an error chain, so callers need not parse that message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EstimateReverted {
    pub gas_used: u64,
    /// The revert data: a `RevertingContext` when a replayed CALL reverted.
    pub output: Bytes,
}

impl std::fmt::Display for EstimateReverted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let gas = self.gas_used;
        if let Ok(write) = ProtectedSlotWrite::abi_decode(&self.output) {
            return write!(
                f,
                "Gas estimation reverted (gas: {gas}): update {} writes protected slot {} \
                 (ProtectedSlotWrite), so the program can never settle",
                write.index, write.slot
            );
        }
        if let Ok(e) = ObservationMismatch::abi_decode(&self.output) {
            return write!(
                f,
                "Gas estimation reverted (gas: {gas}): update {} pinned {} but observes {} \
                 (ObservationMismatch): what the trace saw has changed",
                e.index, e.expected, e.actual
            );
        }
        if let Ok(e) = InsufficientCallGas::abi_decode(&self.output) {
            return write!(
                f,
                "Gas estimation reverted (gas: {gas}): update {} can't give its callee its \
                 gasLimit of {} (InsufficientCallGas)",
                e.index, e.gasLimit
            );
        }
        if let Ok(e) = MalformedOp::abi_decode(&self.output) {
            return write!(
                f,
                "Gas estimation reverted (gas: {gas}): update {} isn't in the handler's layout \
                 (MalformedOp): the program's format doesn't match the estimator's",
                e.index
            );
        }
        write!(
            f,
            "Gas estimation reverted (gas: {}): {}",
            self.gas_used, self.output
        )
    }
}

impl std::error::Error for EstimateReverted {}

/// The `RevertingContext` an estimate failed with, when a replayed CALL reverted.
///
/// Looks through `err`'s whole chain, so context added on top (e.g. a frame tree's frame and
/// piece) does not hide it.
pub fn reverting_context(err: &anyhow::Error) -> Option<RevertingContext> {
    use alloy_sol_types::SolError;

    let reverted = err
        .chain()
        .find_map(|e| e.downcast_ref::<EstimateReverted>())?;
    RevertingContext::abi_decode(&reverted.output).ok()
}

/// The `ProtectedSlotWrite` an estimate failed with: a v0.2.0 program changes a slot its consumer's
/// settlement protects, so it reverts on every attempt.
///
/// Looks through `err`'s whole chain, as [`reverting_context`] does.
pub fn protected_slot_write(err: &anyhow::Error) -> Option<ProtectedSlotWrite> {
    let reverted = err
        .chain()
        .find_map(|e| e.downcast_ref::<EstimateReverted>())?;
    ProtectedSlotWrite::abi_decode(&reverted.output).ok()
}

/// The `ObservationMismatch` an estimate failed with: an op's pin no longer holds against the
/// state the estimate ran on. Looks through `err`'s whole chain, as [`reverting_context`] does.
pub fn observation_mismatch(err: &anyhow::Error) -> Option<ObservationMismatch> {
    let reverted = err
        .chain()
        .find_map(|e| e.downcast_ref::<EstimateReverted>())?;
    ObservationMismatch::abi_decode(&reverted.output).ok()
}

/// The estimator run's gas, or why it failed.
fn estimator_outcome(result: ExecutionResult) -> Result<u64> {
    match result {
        ExecutionResult::Success { gas_used, .. } => Ok(gas_used),
        ExecutionResult::Revert { output, gas_used } => {
            Err(EstimateReverted { gas_used, output }.into())
        }
        ExecutionResult::Halt {
            reason, gas_used, ..
        } => Err(anyhow!(
            "Gas estimation halted (gas: {}): {:?}",
            gas_used,
            reason
        )),
    }
}

/// Run the estimator transaction `tx`, committing its writes when `commit`.
fn execute_estimator<DB>(
    cache_db: &mut CacheDB<DB>,
    tx: TxEnv,
    sim_env: &SimEnvOpts,
    commit: bool,
) -> Result<u64>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    use revm::{ExecuteCommitEvm, ExecuteEvm, MainBuilder};

    let mut evm = estimator_context(cache_db, sim_env).build_mainnet();
    let result = if commit {
        evm.transact_commit(tx)
    } else {
        evm.transact(tx).map(|r| r.result)
    }
    .map_err(|e| anyhow!("Gas estimation failed: {:?}", e))?;
    estimator_outcome(result)
}

/// Top up the caller for `value` and build the transaction calling the estimator at
/// `contract_address` with `calldata`.
fn prepare_estimator_tx<DB>(
    cache_db: &mut CacheDB<DB>,
    contract_address: Address,
    caller_address: Address,
    calldata: Bytes,
    value: U256,
    sim_env: &SimEnvOpts,
) -> Result<TxEnv>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    top_up_caller(cache_db, caller_address, value);
    estimator_tx(contract_address, caller_address, calldata, value, sim_env)
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
    let tx = prepare_estimator_tx(
        cache_db,
        contract_address,
        caller_address,
        calldata,
        value,
        sim_env,
    )?;
    execute_estimator(cache_db, tx, sim_env, commit)
}

/// Prepare `cache_db` for the transaction applying `state_updates` (fund the value calls, inject
/// the estimator, top up the caller) and return that transaction, with the gas to deduct from its
/// result (see [`Handler::calldata`]).
///
/// [`estimate_state_changes_gas`] and [`inspect_state_changes`] both start here, so a traced
/// re-run always runs the transaction that failed.
fn prepare_state_changes<DB>(
    cache_db: &mut CacheDB<DB>,
    contract_address: Address,
    caller_address: Address,
    state_updates: &[StateUpdate],
    sim_env: &SimEnvOpts,
) -> Result<(TxEnv, u64)>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
{
    debug_assert!(
        !state_updates
            .iter()
            .any(|update| matches!(update, StateUpdate::Nested(_))),
        "estimate_state_changes_gas takes a flat program; price nested trees with \
         estimate_frame_tree_gas"
    );
    fund_value_calls(cache_db, contract_address, call_value_total(state_updates));
    let handler = inject_estimator(cache_db, contract_address, caller_address, sim_env)?;
    let (calldata, extra) = handler.calldata(state_updates)?;
    let tx = prepare_estimator_tx(
        cache_db,
        contract_address,
        caller_address,
        calldata,
        sim_env.value,
        sim_env,
    )?;
    Ok((tx, extra))
}

/// Run the transaction [`estimate_state_changes_gas`] runs, under `inspector`, and return its
/// result with the inspector.
///
/// Used to see why an estimate failed (e.g. a trace of the CALL behind a `RevertingContext`).
/// `cache_db` gets the same funding and estimator injection; the transaction's own writes are not
/// committed.
pub fn inspect_state_changes<DB, I>(
    cache_db: &mut CacheDB<DB>,
    contract_address: Address,
    caller_address: Address,
    state_updates: &[StateUpdate],
    sim_env: &SimEnvOpts,
    inspector: I,
) -> Result<(ExecutionResult, I)>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
    I: for<'a> revm::Inspector<revm::handler::MainnetContext<&'a mut CacheDB<DB>>>,
{
    use revm::{InspectEvm, MainBuilder};

    let (tx, _) = prepare_state_changes(
        cache_db,
        contract_address,
        caller_address,
        state_updates,
        sim_env,
    )?;
    let mut evm = estimator_context(cache_db, sim_env).build_mainnet_with_inspector(inspector);
    let result = evm
        .inspect_tx(tx)
        .map_err(|e| anyhow!("Gas estimation failed: {:?}", e))?
        .result;
    Ok((result, evm.inspector))
}

/// Estimate gas for executing a set of state updates.
///
/// This is a convenience function that builds the calldata from state updates and runs it the
/// way [`estimate_gas_raw`] does, after funding the contract for its value calls.
///
/// # Arguments
/// * `cache_db` - A CacheDB wrapping any database backend
/// * `contract_address` - The address to inject the estimator contract at
/// * `caller_address` - The address to use as the caller (also used as tx.origin)
/// * `state_updates` - The state updates to estimate gas for. A flat program: no `Nested` ops,
///   since only `contract_address` is funded for its value calls. Nested trees go through
///   [`estimate_frame_tree_gas`]; strip ops with `gas_analyzer_core::nested::without_nested_ops`.
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
    let (tx, extra) = prepare_state_changes(
        cache_db,
        contract_address,
        caller_address,
        state_updates,
        sim_env,
    )?;
    Ok(execute_estimator(cache_db, tx, sim_env, false)?.saturating_sub(extra))
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
    run_frame_tree(
        cache_db,
        frames,
        caller_address,
        root_value,
        sim_env,
        |db, tx| execute_estimator(db, tx, sim_env, true),
    )
}

/// Run the pieces [`estimate_frame_tree_gas`] runs, each under a fresh inspector from
/// `new_inspector`, and return the inspector of the piece that reverted or halted.
///
/// `None` if no piece failed (the tree ran, or it was rejected before a piece failed). Used to see
/// why a frame tree's estimate failed, e.g. a trace of the CALL behind a `RevertingContext`.
pub fn inspect_frame_tree_failure<DB, I>(
    cache_db: &mut CacheDB<DB>,
    frames: &[FrameProgram],
    caller_address: Address,
    root_value: U256,
    sim_env: &SimEnvOpts,
    mut new_inspector: impl FnMut() -> I,
) -> Option<I>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
    I: for<'a> revm::Inspector<revm::handler::MainnetContext<&'a mut CacheDB<DB>>>,
{
    use revm::{InspectCommitEvm, MainBuilder};

    let mut failed = None;
    let _ = run_frame_tree(
        cache_db,
        frames,
        caller_address,
        root_value,
        sim_env,
        |db, tx| {
            let mut evm =
                estimator_context(db, sim_env).build_mainnet_with_inspector(new_inspector());
            let result = evm
                .inspect_tx_commit(tx)
                .map_err(|e| anyhow!("Gas estimation failed: {:?}", e))?;
            let outcome = estimator_outcome(result);
            if outcome.is_err() {
                failed = Some(evm.inspector);
            }
            outcome
        },
    );
    failed
}

/// Fund and inject every frame's contract, then run the tree's pieces in order with `exec`, which
/// runs one prepared estimator transaction and commits its writes.
fn run_frame_tree<DB, X>(
    cache_db: &mut CacheDB<DB>,
    frames: &[FrameProgram],
    caller_address: Address,
    root_value: U256,
    sim_env: &SimEnvOpts,
    exec: X,
) -> Result<Vec<u64>>
where
    DB: revm::database_interface::DatabaseRef,
    <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
    X: FnMut(&mut CacheDB<DB>, TxEnv) -> Result<u64>,
{
    if frames.is_empty() {
        return Err(anyhow!("a frame tree needs a root frame"));
    }
    // A contract can appear in several frames; fund it for every call it sends across them.
    let mut call_values: HashMap<Address, U256> = HashMap::new();
    for frame in frames {
        let total = call_values.entry(frame.target).or_default();
        *total = total.saturating_add(call_value_total(&frame.updates));
    }
    for (target, required) in call_values {
        fund_value_calls(cache_db, target, required);
    }

    // Once per contract: injecting again would stash the estimator itself as the original code.
    let mut handlers = HashMap::new();
    for frame in frames {
        if let std::collections::hash_map::Entry::Vacant(entry) = handlers.entry(frame.target) {
            entry.insert(inject_estimator(
                cache_db,
                frame.target,
                caller_address,
                sim_env,
            )?);
        }
    }

    let mut frame_gas = vec![0; frames.len()];
    let mut tree = FrameTreeRun {
        frames,
        handlers,
        caller_address,
        sim_env,
        exec,
    };
    tree.run(cache_db, 0, root_value, &mut frame_gas)?;
    Ok(frame_gas)
}

struct FrameTreeRun<'a, X> {
    frames: &'a [FrameProgram],
    /// Each frame contract's handler, from its injection.
    handlers: HashMap<Address, Handler>,
    caller_address: Address,
    sim_env: &'a SimEnvOpts,
    /// Runs one prepared piece and commits its writes.
    exec: X,
}

impl<X> FrameTreeRun<'_, X> {
    fn run<DB>(
        &mut self,
        cache_db: &mut CacheDB<DB>,
        index: usize,
        value: U256,
        frame_gas: &mut [u64],
    ) -> Result<()>
    where
        DB: revm::database_interface::DatabaseRef,
        <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
        X: FnMut(&mut CacheDB<DB>, TxEnv) -> Result<u64>,
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
        &mut self,
        cache_db: &mut CacheDB<DB>,
        index: usize,
        piece_index: usize,
        updates: &[StateUpdate],
        value: U256,
    ) -> Result<u64>
    where
        DB: revm::database_interface::DatabaseRef,
        <DB as revm::database_interface::DatabaseRef>::Error: core::fmt::Debug,
        X: FnMut(&mut CacheDB<DB>, TxEnv) -> Result<u64>,
    {
        let frame = &self.frames[index];
        let (calldata, extra) = self.handlers[&frame.target].calldata(updates)?;
        let value = if piece_index == 0 { value } else { U256::ZERO };
        let gas = prepare_estimator_tx(
            cache_db,
            frame.target,
            self.caller_address,
            calldata,
            value,
            self.sim_env,
        )
        .and_then(|tx| (self.exec)(cache_db, tx))
        .map(|gas| gas.saturating_sub(extra))
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

    /// Try to decode an EnvironmentMismatch from a gas estimation error: the revertData of the
    /// estimator's RevertingContext, or the revert itself.
    fn format_sim_env_error(err: &anyhow::Error) -> String {
        let mismatch = reverting_context(err)
            .and_then(|ctx| EnvironmentMismatch::abi_decode(&ctx.revertData).ok())
            .or_else(|| {
                let reverted = err.downcast_ref::<EstimateReverted>()?;
                EnvironmentMismatch::abi_decode(&reverted.output).ok()
            });
        match mismatch {
            Some(env_err) => format!(
                "EnvironmentMismatch: {}\n  expected: {:?}\n  actual:   {:?}",
                env_err.explanation, env_err.expected, env_err.actual
            ),
            None => err.to_string(),
        }
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
            format: SignedFormat::V0_1,
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
            resultHash: alloy_primitives::B256::ZERO,
            gasLimit: alloy_primitives::U256::MAX,
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
            format: SignedFormat::V0_1,
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
            resultHash: alloy_primitives::B256::ZERO,
            gasLimit: alloy_primitives::U256::MAX,
        })];

        let estimator_address = address!("0x000000000000000000000000000000000000E570");

        // Use a wrong timestamp — SimEnvCallee.test() should revert
        let wrong_env = SimEnvOpts {
            format: SignedFormat::V0_1,
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
            format: SignedFormat::V0_1,
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
            resultHash: alloy_primitives::B256::ZERO,
            gasLimit: alloy_primitives::U256::MAX,
        })];

        let estimator_address = address!("0x000000000000000000000000000000000000E570");

        // Use a wrong basefee — SimEnvCallee.test() should revert
        let wrong_env = SimEnvOpts {
            format: SignedFormat::V0_1,
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
            format: SignedFormat::V0_1,
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
            resultHash: alloy_primitives::B256::ZERO,
            gasLimit: alloy_primitives::U256::MAX,
        })];

        let estimator_address = address!("0x000000000000000000000000000000000000E570");

        // Use a wrong block number — SimEnvCallee.test() should revert
        let wrong_env = SimEnvOpts {
            format: SignedFormat::V0_1,
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
            format: SignedFormat::V0_1,
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

        // v0.2.0 settlement also checks every STORE against the protected ranges, so measure
        // both handlers: the gate doesn't know which one a payload lands in.
        for format in [SignedFormat::V0_1, SignedFormat::V0_2] {
            let sim_env = SimEnvOpts {
                format,
                ..sim_env.clone()
            };
            for (name, build) in shapes {
                for n in [1usize, 2, 5, 25, 100, 400, 650] {
                    let updates: Vec<StateUpdate> = (0..n).map(build).collect();

                    let mut db = CacheDB::new(EmptyDB::default());
                    fund(&mut db, caller);
                    let measured =
                        estimate_state_changes_gas(&mut db, target, caller, &updates, &sim_env)
                            .unwrap_or_else(|e| {
                                panic!("{format:?} {name} n={n}: measurement failed: {e:?}")
                            });

                    let analytic = estimate_applied_payload_gas(&updates, 0, 0);

                    assert!(
                        analytic >= measured,
                        "{format:?} {name} n={n}: analytic bound {analytic} is BELOW the \
                         measured cost {measured} (short by {}). The gate would admit a payload \
                         that does not fit. A term is missing from estimate_applied_payload_gas \
                         — see its doc comment.",
                        measured - analytic
                    );
                }
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
            resultHash: alloy_primitives::B256::ZERO,
            gasLimit: alloy_primitives::U256::MAX,
        })
    }

    fn value_call(target: Address, value: U256) -> StateUpdate {
        StateUpdate::Call(IStateUpdateTypes::Call {
            target,
            value,
            callargs: Bytes::new(),
            resultHash: alloy_primitives::B256::ZERO,
            gasLimit: alloy_primitives::U256::MAX,
        })
    }

    const ONE_ETH: u128 = 10u128.pow(18);

    /// A contract that held no ETH at the forked block still prices a `Call` update that sends
    /// ETH (#127), where the unfunded estimate halts with `OutOfFunds`.
    #[test]
    fn test_value_call_from_unfunded_contract_is_priced() {
        let root = address!("0x00000000000000000000000000000000000a0000");
        let recipient = address!("0x00000000000000000000000000000000000c0000");
        let caller = address!("0x000000000000000000000000000000000000beef");
        let sim_env = sim_env_with_spec(SpecId::CANCUN);
        let updates = vec![value_call(recipient, U256::from(ONE_ETH))];

        let mut unfunded = CacheDB::new(EmptyDB::default());
        let calldata = build_gas_estimation_calldata(&updates).unwrap();
        assert!(
            estimate_gas_raw(&mut unfunded, root, caller, calldata, &sim_env).is_err(),
            "without funding the value call must fail, or this test proves nothing"
        );

        let mut cache_db = CacheDB::new(EmptyDB::default());
        let gas = estimate_state_changes_gas(&mut cache_db, root, caller, &updates, &sim_env)
            .expect("the value call is priced");
        assert!(gas > 0);
    }

    /// As above, through the frame-tree path, summing the calls a contract sends across frames.
    #[test]
    fn test_frame_tree_value_calls_from_unfunded_contract_are_priced() {
        let root = address!("0x00000000000000000000000000000000000a0000");
        let child = address!("0x00000000000000000000000000000000000b0000");
        let recipient = address!("0x00000000000000000000000000000000000c0000");
        let caller = address!("0x000000000000000000000000000000000000beef");
        let sim_env = sim_env_with_spec(SpecId::CANCUN);
        let frames = [
            frame(
                root,
                vec![
                    value_call(recipient, U256::from(ONE_ETH)),
                    nested(child),
                    value_call(recipient, U256::from(2 * ONE_ETH)),
                ],
                vec![1],
            ),
            frame(
                child,
                vec![value_call(recipient, U256::from(ONE_ETH))],
                vec![],
            ),
        ];

        let mut cache_db = CacheDB::new(EmptyDB::default());
        let frame_gas =
            estimate_frame_tree_gas(&mut cache_db, &frames, caller, U256::ZERO, &sim_env)
                .expect("every frame's value calls are priced");
        assert!(frame_gas.iter().all(|&g| g > 0));
    }

    /// Funding only tops up: a contract already holding enough keeps its real balance, so a
    /// balance check against it reads what it read on chain.
    #[test]
    fn test_fund_value_calls_only_tops_up() {
        use revm::database_interface::DatabaseRef;

        let funded = address!("0x00000000000000000000000000000000000a0000");
        let empty = address!("0x00000000000000000000000000000000000b0000");
        let mut cache_db = CacheDB::new(EmptyDB::default());
        cache_db.insert_account_info(
            funded,
            AccountInfo {
                balance: U256::from(5 * ONE_ETH),
                ..Default::default()
            },
        );

        fund_value_calls(&mut cache_db, funded, U256::from(ONE_ETH));
        fund_value_calls(&mut cache_db, empty, U256::from(ONE_ETH));

        let balance = |addr| {
            cache_db
                .basic_ref(addr)
                .unwrap()
                .unwrap_or_default()
                .balance
        };
        assert_eq!(balance(funded), U256::from(5 * ONE_ETH));
        assert_eq!(balance(empty), U256::from(ONE_ETH));
    }

    /// A backend whose every read fails.
    struct FailingDb;

    #[derive(Debug)]
    struct ReadFailed;

    impl std::fmt::Display for ReadFailed {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("read failed")
        }
    }

    impl std::error::Error for ReadFailed {}
    impl revm::database_interface::DBErrorMarker for ReadFailed {}

    impl revm::database_interface::DatabaseRef for FailingDb {
        type Error = ReadFailed;

        fn basic_ref(&self, _: Address) -> Result<Option<AccountInfo>, ReadFailed> {
            Err(ReadFailed)
        }

        fn code_by_hash_ref(&self, _: B256) -> Result<revm::state::Bytecode, ReadFailed> {
            Err(ReadFailed)
        }

        fn storage_ref(&self, _: Address, _: U256) -> Result<U256, ReadFailed> {
            Err(ReadFailed)
        }

        fn block_hash_ref(&self, _: u64) -> Result<B256, ReadFailed> {
            Err(ReadFailed)
        }
    }

    /// An account that can't be read is not funded: writing a guessed account over it would
    /// replace its real balance and nonce, and hide the read failure behind a clean estimate.
    #[test]
    fn test_fund_value_calls_skips_an_unreadable_account() {
        let contract = address!("0x00000000000000000000000000000000000a0000");
        let mut cache_db = CacheDB::new(FailingDb);

        fund_value_calls(&mut cache_db, contract, U256::from(ONE_ETH));

        assert!(
            !cache_db.cache.accounts.contains_key(&contract),
            "no account is written over a failed read"
        );
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "takes a flat program")]
    fn test_flat_estimate_rejects_nested_ops() {
        let root = address!("0x00000000000000000000000000000000000a0000");
        let child = address!("0x00000000000000000000000000000000000b0000");
        let caller = address!("0x000000000000000000000000000000000000beef");
        let mut cache_db = CacheDB::new(EmptyDB::default());
        let _ = estimate_state_changes_gas(
            &mut cache_db,
            root,
            caller,
            &[nested(child)],
            &sim_env_with_spec(SpecId::CANCUN),
        );
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

    // ------------------------------------------------------------------------
    // solidity-sdk v0.2.0 estimator
    // ------------------------------------------------------------------------

    const ROOT_V0_2: Address = address!("0x00000000000000000000000000000000000a0002");
    const CALLER_V0_2: Address = address!("0x000000000000000000000000000000000000beef");

    fn v0_2_env() -> SimEnvOpts {
        SimEnvOpts {
            format: SignedFormat::V0_2,
            ..sim_env_with_spec(SpecId::PRAGUE)
        }
    }

    fn store(slot: B256, value: u64) -> StateUpdate {
        StateUpdate::Store(IStateUpdateTypes::Store {
            slot,
            value: B256::from(U256::from(value)),
        })
    }

    fn config_slot() -> B256 {
        let mut slot = SDK_CONFIG_NAMESPACE;
        slot.0[31] = 0x01;
        slot
    }

    /// Runtime code that answers every call with `data`.
    fn returning(data: &[u8]) -> Vec<u8> {
        let len = u16::try_from(data.len()).unwrap().to_be_bytes();
        // PUSH2 len, PUSH1 14, PUSH1 0, CODECOPY, PUSH2 len, PUSH1 0, RETURN, then `data` at 14.
        let mut code = vec![
            0x61, len[0], len[1], 0x60, 0x0e, 0x60, 0x00, 0x39, 0x61, len[0], len[1], 0x60, 0x00,
            0xf3,
        ];
        code.extend_from_slice(data);
        code
    }

    /// A target whose `gasKillerProtectedRanges()` (like any other call) returns `ranges`.
    fn reporting(ranges: Vec<ProtectedRange>) -> CacheDB<EmptyDB> {
        let mut cache_db = CacheDB::new(EmptyDB::default());
        let data = v0_2::gasKillerProtectedRangesCall::abi_encode_returns(&ranges);
        with_code(&mut cache_db, ROOT_V0_2, &returning(&data));
        cache_db
    }

    fn single(slot: B256) -> ProtectedRange {
        ProtectedRange {
            start: slot.into(),
            end: slot.into(),
            mask: B256::repeat_byte(0xff),
        }
    }

    fn estimate(cache_db: &mut CacheDB<EmptyDB>, updates: &[StateUpdate]) -> Result<u64> {
        estimate_state_changes_gas(cache_db, ROOT_V0_2, CALLER_V0_2, updates, &v0_2_env())
    }

    #[test]
    fn v0_2_calldata_matches_the_artifacts_entry_point() {
        let json: serde_json::Value = serde_json::from_str(ESTIMATOR_V0_2_ABI_JSON).unwrap();
        let selector = json["methodIdentifiers"]
            ["runStateUpdatesCall(uint8[],bytes[],(uint256,uint256,bytes32)[])"]
            .as_str()
            .expect("the v0.2.0 entry point is in the artifact");
        assert_eq!(
            hex::encode(v0_2::runStateUpdatesCallCall::SELECTOR),
            selector
        );
    }

    #[test]
    fn default_ranges_are_well_formed_and_protect_the_counter() {
        let ranges = default_protected_ranges();
        assert!(well_formed(&ranges));
        assert!(ranges.contains(&single(STATE_TRACKER_SLOT)));
        assert!(!well_formed(&[ranges[1].clone(), ranges[0].clone()]));
    }

    /// A target that doesn't report ranges is priced with the defaults: the guard costs more than
    /// the v0.1.0 apply, and the ranges' own calldata is deducted.
    #[test]
    fn v0_2_prices_a_store_with_the_guard() {
        let updates = vec![store(B256::with_last_byte(1), 1)];
        let v0_1 = estimate_state_changes_gas(
            &mut CacheDB::new(EmptyDB::default()),
            ROOT_V0_2,
            CALLER_V0_2,
            &updates,
            &sim_env_with_spec(SpecId::PRAGUE),
        )
        .unwrap();
        let v0_2 = estimate(&mut CacheDB::new(EmptyDB::default()), &updates).unwrap();
        assert!(
            v0_2 > v0_1 && v0_2 - v0_1 < 10_000,
            "v0.1.0 {v0_1}, v0.2.0 {v0_2}"
        );
    }

    #[test]
    fn v0_2_rejects_a_write_to_the_sdk_config() {
        let updates = vec![store(B256::with_last_byte(1), 1), store(config_slot(), 1)];
        let err = estimate(&mut CacheDB::new(EmptyDB::default()), &updates).unwrap_err();
        assert_eq!(
            protected_slot_write(&err),
            Some(ProtectedSlotWrite {
                index: U256::from(1),
                slot: config_slot(),
            })
        );
        assert!(err.to_string().contains("ProtectedSlotWrite"), "{err}");

        let v0_1 = estimate_state_changes_gas(
            &mut CacheDB::new(EmptyDB::default()),
            ROOT_V0_2,
            CALLER_V0_2,
            &updates,
            &sim_env_with_spec(SpecId::PRAGUE),
        );
        assert!(v0_1.is_ok(), "v0.1.0 has no guard");
    }

    /// Settlement has bumped the counter before the program runs; the estimator hasn't, so the
    /// program's copy moves it forward, which the guard allows. A rewind it rejects.
    #[test]
    fn v0_2_counter_moves_forward_never_back() {
        let at_five = || {
            let mut cache_db = CacheDB::new(EmptyDB::default());
            cache_db
                .insert_account_storage(ROOT_V0_2, STATE_TRACKER_SLOT.into(), U256::from(5))
                .unwrap();
            cache_db
        };
        assert!(estimate(&mut at_five(), &[store(STATE_TRACKER_SLOT, 6)]).is_ok());
        assert!(estimate(&mut at_five(), &[store(STATE_TRACKER_SLOT, 5)]).is_ok());
        let err = estimate(&mut at_five(), &[store(STATE_TRACKER_SLOT, 4)]).unwrap_err();
        assert!(protected_slot_write(&err).is_some(), "{err}");
    }

    /// A target's own list replaces the defaults: its range is checked, the SDK config (which a
    /// real consumer's list would include) is not.
    #[test]
    fn v0_2_uses_the_ranges_a_target_reports() {
        let guarded = B256::with_last_byte(7);
        let err =
            estimate(&mut reporting(vec![single(guarded)]), &[store(guarded, 1)]).unwrap_err();
        assert!(protected_slot_write(&err).is_some(), "{err}");
        assert!(
            estimate(
                &mut reporting(vec![single(guarded)]),
                &[store(config_slot(), 1)]
            )
            .is_ok()
        );
    }

    /// A consumer that opted out of the guard reports no ranges, and settles unchecked.
    #[test]
    fn v0_2_runs_an_opted_out_consumer_unguarded() {
        assert!(estimate(&mut reporting(vec![]), &[store(config_slot(), 1)]).is_ok());
    }

    /// A malformed list isn't a consumer's: the defaults apply, rather than a guard that fails open.
    #[test]
    fn v0_2_ignores_a_malformed_list() {
        let descending = vec![
            single(B256::with_last_byte(9)),
            single(B256::with_last_byte(8)),
        ];
        let err = estimate(&mut reporting(descending), &[store(config_slot(), 1)]).unwrap_err();
        assert!(protected_slot_write(&err).is_some(), "{err}");
    }

    #[test]
    fn v0_2_frame_trees_are_guarded() {
        let frames = vec![frame(ROOT_V0_2, vec![store(config_slot(), 1)], vec![])];
        let err = estimate_frame_tree_gas(
            &mut CacheDB::new(EmptyDB::default()),
            &frames,
            CALLER_V0_2,
            U256::ZERO,
            &v0_2_env(),
        )
        .unwrap_err();
        assert!(protected_slot_write(&err).is_some(), "{err:#}");

        let frames = vec![frame(
            ROOT_V0_2,
            vec![store(B256::with_last_byte(1), 1)],
            vec![],
        )];
        let tree = estimate_frame_tree_gas(
            &mut CacheDB::new(EmptyDB::default()),
            &frames,
            CALLER_V0_2,
            U256::ZERO,
            &v0_2_env(),
        )
        .unwrap();
        let flat = estimate(
            &mut CacheDB::new(EmptyDB::default()),
            &[store(B256::with_last_byte(1), 1)],
        )
        .unwrap();
        assert_eq!(tree, vec![flat]);
    }

    #[test]
    fn estimate_gas_raw_refuses_v0_2() {
        let calldata = build_gas_estimation_calldata(&[store_slot0()]).unwrap();
        let err = estimate_gas_raw(
            &mut CacheDB::new(EmptyDB::default()),
            ROOT_V0_2,
            CALLER_V0_2,
            calldata,
            &v0_2_env(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("v0.1.0 calldata"), "{err}");
    }

    // ------------------------------------------------------------------------
    // v0.2.0 program layouts through the v0.2.0 handler
    // ------------------------------------------------------------------------

    /// `keccak256(abi.encodePacked(true))`: a successful call that returned nothing.
    fn empty_success_hash() -> B256 {
        alloy_primitives::keccak256([1u8])
    }

    fn pinned_call(target: Address, result_hash: B256, gas_limit: U256) -> StateUpdate {
        StateUpdate::Call(IStateUpdateTypes::Call {
            target,
            value: U256::ZERO,
            callargs: Bytes::from_static(&[0x01]),
            resultHash: result_hash,
            gasLimit: gas_limit,
        })
    }

    #[test]
    fn v0_2_settles_a_call_whose_result_matches_its_pin() {
        let target = address!("0x00000000000000000000000000000000000c0002");
        let updates = vec![pinned_call(target, empty_success_hash(), U256::MAX)];
        estimate(&mut CacheDB::new(EmptyDB::default()), &updates)
            .expect("the pinned result matches");
    }

    #[test]
    fn v0_2_rejects_a_call_whose_result_moved() {
        let target = address!("0x00000000000000000000000000000000000c0002");
        let updates = vec![pinned_call(target, B256::repeat_byte(0x42), U256::MAX)];
        let err = estimate(&mut CacheDB::new(EmptyDB::default()), &updates).unwrap_err();
        let mismatch = observation_mismatch(&err).expect("an ObservationMismatch");
        assert_eq!(mismatch.index, U256::ZERO);
        assert_eq!(mismatch.expected, B256::repeat_byte(0x42));
        assert_eq!(mismatch.actual, empty_success_hash());
        assert!(err.to_string().contains("ObservationMismatch"), "{err}");
    }

    #[test]
    fn v0_2_honors_a_call_gas_cap() {
        let target = address!("0x00000000000000000000000000000000000c0002");
        let capped = vec![pinned_call(target, B256::ZERO, U256::from(2_300))];
        estimate(&mut CacheDB::new(EmptyDB::default()), &capped).expect("a 2,300 cap settles");
    }

    #[test]
    fn v0_2_checks_balances_and_code() {
        let account = address!("0x00000000000000000000000000000000000c0003");
        let mut cache_db = CacheDB::new(EmptyDB::default());
        cache_db.insert_account_info(
            account,
            AccountInfo {
                balance: U256::from(5),
                ..Default::default()
            },
        );
        let balance = |balance: u64| {
            StateUpdate::BalanceCheck(IStateUpdateTypes::BalanceCheck {
                account,
                balance: U256::from(balance),
            })
        };
        let codehash = StateUpdate::CodehashCheck(IStateUpdateTypes::CodehashCheck {
            account,
            codehash: B256::ZERO,
        });
        estimate(&mut cache_db.clone(), &[balance(5), codehash]).expect("observations hold");
        assert!(
            estimate(&mut cache_db, &[balance(6)]).is_err(),
            "a moved balance fails"
        );
    }

    /// A v0.1.0 estimate of the same program ignores the pins, which v0.1.0 can't carry.
    #[test]
    fn v0_1_ignores_v0_2_pins() {
        let target = address!("0x00000000000000000000000000000000000c0002");
        let mut updates = vec![pinned_call(
            target,
            B256::repeat_byte(0x42),
            U256::from(2_300),
        )];
        updates.push(StateUpdate::BalanceCheck(IStateUpdateTypes::BalanceCheck {
            account: target,
            balance: U256::from(1),
        }));
        estimate_state_changes_gas(
            &mut CacheDB::new(EmptyDB::default()),
            ROOT_V0_2,
            CALLER_V0_2,
            &updates,
            &sim_env_with_spec(SpecId::PRAGUE),
        )
        .expect("v0.1.0 has no pins to fail");
    }
}

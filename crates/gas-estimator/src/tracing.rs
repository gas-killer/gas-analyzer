//! Optional revm `Inspector` that prints a per-frame call trace to stderr
//! and flags any frame that exits with empty returndata after consuming
//! ≥31/32 of its forwarded gas.
//!
//! That signature is what AllowanceHolder's `CheckCall` propagates via
//! `INVALID` when an inner sub-call OOGs, and what revm produces when an
//! opcode is missing from the configured spec (`InstructionResult::NotActivated`).
//! Surfacing it at the originating frame turns an opaque outer revert into
//! a precise "this contract / this selector misbehaved" pointer.
//!
//! Mirrors `estimate_gas_raw`'s setup (proxy injection, cfg flags, sim env)
//! so the traced run reproduces the failing one bit-for-bit.

use alloy_primitives::{Address, B256, Bytes, U256};
use anyhow::{Result, anyhow};
use revm::context::result::ExecutionResult;
use revm::context::{Context, TxEnv};
use revm::context_interface::ContextTr;
use revm::database::CacheDB;
use revm::database_interface::DatabaseRef;
use revm::inspector::{InspectEvm, Inspector};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::{CallInputs, CallOutcome, CallScheme, InstructionResult};
use revm::state::AccountInfo;
use revm::{MainBuilder, MainContext};

use crate::{
    EIP7825_TX_GAS_CAP, SimEnvOpts, build_gas_estimation_calldata, impl_slot,
    load_estimator_bytecode,
};
use gas_analyzer_core::types::StateUpdate;

/// Run gas estimation under a tracing inspector. Same semantics as
/// [`crate::estimate_state_changes_gas`], plus:
///
/// - every CALL frame is logged to stderr with target / caller / selector
///   / forwarded gas / return shape;
/// - the first frame that returns empty + nearly-OOG gets a `^^^` pointer.
///
/// Use this when a normal estimation produces an opaque outer revert.
pub fn estimate_state_changes_gas_traced<DB>(
    cache_db: &mut CacheDB<DB>,
    contract_address: Address,
    caller_address: Address,
    state_updates: &[StateUpdate],
    sim_env: &SimEnvOpts,
) -> Result<u64>
where
    DB: DatabaseRef,
    <DB as DatabaseRef>::Error: core::fmt::Debug,
{
    inject_proxy(cache_db, contract_address)?;

    let calldata = build_gas_estimation_calldata(state_updates)?;

    let ctx = Context::mainnet()
        .with_db(&mut *cache_db)
        .modify_cfg_chained(|cfg| {
            cfg.disable_nonce_check = true;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
            cfg.disable_fee_charge = true;
            cfg.spec = revm::primitives::hardfork::SpecId::OSAKA;
        })
        .modify_block_chained(|block| {
            block.number = U256::from(sim_env.number);
            block.timestamp = U256::from(sim_env.timestamp);
            block.gas_limit = sim_env.gas_limit;
            block.beneficiary = sim_env.coinbase;
            block.prevrandao = Some(sim_env.prevrandao);
            block.basefee = sim_env.basefee;
            block.difficulty = U256::ZERO;
        });

    let mut evm = ctx.build_mainnet_with_inspector(CallTracer::new());

    let tx = TxEnv::builder()
        .caller(caller_address)
        .kind(revm::primitives::TxKind::Call(contract_address))
        .data(calldata)
        .value(U256::ZERO)
        .gas_limit(sim_env.gas_limit.min(EIP7825_TX_GAS_CAP))
        .gas_price(sim_env.gas_price)
        .build()
        .map_err(|e| anyhow!("Failed to build tx env: {:?}", e))?;

    let result = evm
        .inspect_one_tx(tx)
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

// Same proxy-injection dance as `estimate_gas_raw`. Kept as a local helper
// rather than refactored into the main path because `estimate_gas_raw` is
// `where DB::Error: Debug` and we don't want the tracing module to bleed
// generics back into the production hot path.
fn inject_proxy<DB>(cache_db: &mut CacheDB<DB>, contract_address: Address) -> Result<()>
where
    DB: DatabaseRef,
    <DB as DatabaseRef>::Error: core::fmt::Debug,
{
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

    Ok(())
}

/// Per-frame stash captured at `call` so we can compute a meaningful
/// `gas_spent` and flag suspicious exits at `call_end`.
struct Frame {
    target: Address,
    caller: Address,
    scheme: CallScheme,
    gas_limit: u64,
    selector: [u8; 4],
    input_len: usize,
}

struct CallTracer {
    frames: Vec<Frame>,
    /// Cap depth at which we still print to stderr. Keeps very deep call
    /// trees from flooding output while still letting us capture the
    /// first failure marker (which is recorded regardless of depth).
    max_depth: usize,
    /// Throttle: only print the first empty-revert+gas-consumed pointer.
    /// Subsequent ones are usually parents bubbling up the same failure.
    printed_failure: bool,
}

impl CallTracer {
    fn new() -> Self {
        Self {
            frames: Vec::new(),
            max_depth: 256,
            printed_failure: false,
        }
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for CallTracer
where
    CTX: ContextTr,
{
    fn call(&mut self, ctx: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let depth = self.frames.len();
        let bytes: Bytes = inputs.input.bytes(ctx);
        let mut selector = [0u8; 4];
        if bytes.len() >= 4 {
            selector.copy_from_slice(&bytes[..4]);
        }

        if depth < self.max_depth {
            eprintln!(
                "[trace] {}CALL → {} (from {}) gas_limit={} sel=0x{} input_len={} scheme={:?}",
                "  ".repeat(depth),
                inputs.target_address,
                inputs.caller,
                inputs.gas_limit,
                hex::encode(selector),
                bytes.len(),
                inputs.scheme,
            );
        }

        self.frames.push(Frame {
            target: inputs.target_address,
            caller: inputs.caller,
            scheme: inputs.scheme,
            gas_limit: inputs.gas_limit,
            selector,
            input_len: bytes.len(),
        });
        None
    }

    fn call_end(&mut self, _ctx: &mut CTX, _inputs: &CallInputs, outcome: &mut CallOutcome) {
        let frame = self.frames.pop();
        let depth = self.frames.len();
        let result = outcome.result.result;
        let gas_remaining = outcome.result.gas.remaining();
        let out_len = outcome.result.output.len();
        let gas_spent = frame
            .as_ref()
            .map(|f| f.gas_limit.saturating_sub(gas_remaining))
            .unwrap_or(0);

        let is_fail = !matches!(result, InstructionResult::Return | InstructionResult::Stop);

        if depth < self.max_depth {
            eprintln!(
                "[trace] {}END  {:?} gas_spent={} returndata_len={}",
                "  ".repeat(depth),
                result,
                gas_spent,
                out_len,
            );
        }

        if is_fail
            && out_len == 0
            && !self.printed_failure
            && let Some(f) = frame
        {
            // EIP-150 forwards (1 - 1/64) of available gas to the callee, so
            // a callee that OOGs leaves the parent with ~gas_limit/32 remaining
            // (≈ the (1/64)^2 we'd see across one proxy hop). Anything below
            // that threshold is the OOG signature CheckCall escalates to
            // INVALID, OR a `NotActivated` opcode under the wrong spec.
            let threshold = (f.gas_limit / 32).max(1);
            if gas_remaining <= threshold {
                eprintln!(
                    "[trace] {}^^^ EMPTY-REVERT + GAS-CONSUMED in frame: target={} caller={} sel=0x{} input_len={} scheme={:?} gas_limit={} gas_remaining={} (≤ limit/32 = {})",
                    "  ".repeat(depth),
                    f.target,
                    f.caller,
                    hex::encode(f.selector),
                    f.input_len,
                    f.scheme,
                    f.gas_limit,
                    gas_remaining,
                    threshold,
                );
                self.printed_failure = true;
            }
        }
    }
}

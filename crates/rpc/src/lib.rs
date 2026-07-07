//! RPC-based trace fetching functions.
//!
//! This module contains the async functions that require network I/O
//! (alloy-provider, DebugApi). These are separated from the core crate
//! because they are not WASM-compatible.

use std::collections::HashSet;

use alloy::primitives::FixedBytes;
use alloy::rpc::types::BlockOverrides;
use alloy::rpc::types::trace::geth::{
    CallConfig, CallFrame, DefaultFrame, DiffMode, GethDebugTracingCallOptions,
    GethDebugTracingOptions, GethDefaultTracingOptions, GethTrace, PreStateConfig, PreStateFrame,
};
use alloy_eips::BlockId;
use alloy_provider::Provider;
use alloy_provider::ext::DebugApi;
use alloy_rpc_types::TransactionTrait;
use anyhow::{Result, anyhow, bail};

use gas_analyzer_core::SimProfile;
use gas_analyzer_core::trace::compute_state_updates;
use gas_analyzer_core::types::{Opcode, StateUpdate};
use gas_analyzer_estimator::PrecedingTx;

/// Apply a [`SimProfile`]'s environment overrides to a `debug_traceCall`
/// request: the transaction-level gas limit goes on the request itself, the
/// block-level gas limit rides in `blockOverrides`.
///
/// Under [`SimProfile::UnboundedV1`] the overrides are pinned protocol
/// constants (see `gas_analyzer_core::sim_profile`), so they are applied
/// unconditionally — a caller-supplied `gas` field reflects real-chain limits
/// that this profile deliberately lifts. Note the node has the last word:
/// geth/reth clamp `debug_traceCall` gas to `--rpc.gascap` and anvil to its
/// block gas limit, so unbounded extraction requires a node started with the
/// cap lifted (`anvil --disable-block-gas-limit`, `geth --rpc.gascap=0`).
/// A silently-clamping node makes heavy calls OOG, which surfaces as a revert
/// classified for fallback rather than an unsound diff.
fn apply_sim_profile(
    tx_request: &mut alloy::rpc::types::eth::TransactionRequest,
    options: &mut GethDebugTracingCallOptions,
    profile: SimProfile,
) {
    if let Some(tx_gas) = profile.tx_gas_limit_override() {
        tx_request.gas = Some(tx_gas);
    }
    if let Some(block_gas) = profile.block_gas_limit_override() {
        options
            .block_overrides
            .get_or_insert_with(BlockOverrides::default)
            .gas_limit = Some(block_gas);
    }
}

/// Get transaction trace from a provider using debug_traceTransaction.
///
/// `status` must be the receipt status for `tx_hash` (`receipt.status()`).
/// Pass the already-fetched receipt's status to avoid a redundant RPC call.
/// Returns an error immediately if `status` is `false` (transaction reverted).
pub async fn get_tx_trace<P: Provider + DebugApi>(
    provider: &P,
    tx_hash: FixedBytes<32>,
    status: bool,
) -> Result<DefaultFrame> {
    if !status {
        bail!("transaction failed");
    }

    let options = GethDebugTracingOptions {
        config: GethDefaultTracingOptions {
            enable_memory: Some(true),
            ..Default::default()
        },
        ..Default::default()
    };

    let GethTrace::Default(trace) = provider.debug_trace_transaction(tx_hash, options).await?
    else {
        return Err(anyhow!("Expected default trace"));
    };

    Ok(trace)
}

/// Get trace from a simulated call using debug_traceCall.
///
/// This simulates the call at the given block and returns the default-format trace.
/// Requires an RPC that supports debug_traceCall (e.g. Geth, Erigon).
pub async fn get_trace_from_call<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
) -> Result<DefaultFrame>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    get_trace_from_call_with_profile(provider, tx_request, block, SimProfile::Chain).await
}

/// [`get_trace_from_call`] with an explicit [`SimProfile`] controlling the
/// simulated environment (gas limits). `SimProfile::Chain` is identical to
/// the plain function.
pub async fn get_trace_from_call_with_profile<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
    profile: SimProfile,
) -> Result<DefaultFrame>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    let mut tx_request = tx_request.into();
    let mut options = GethDebugTracingCallOptions {
        tracing_options: GethDebugTracingOptions {
            config: GethDefaultTracingOptions {
                enable_memory: Some(true),
                disable_storage: Some(true),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    apply_sim_profile(&mut tx_request, &mut options, profile);

    let GethTrace::Default(trace) = provider
        .debug_trace_call(tx_request, block, options)
        .await
        .map_err(|e| anyhow!("debug_trace_call failed: {}", e))?
    else {
        return Err(anyhow!("Expected default trace from debug_trace_call"));
    };

    Ok(trace)
}

/// Simulate a call and return the `prestateTracer` diff (`diffMode`): per-account pre/post storage.
///
/// Cost is `O(changed slots)` rather than `O(execution steps)`, so this succeeds on heavy-compute
/// tracked functions whose struct-log trace would time out the node. Used together with
/// [`get_call_frame_from_call`] by the prestate fast path (see `gas_analyzer_core::prestate`).
pub async fn get_prestate_diff_from_call<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
) -> Result<DiffMode>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    get_prestate_diff_from_call_with_profile(provider, tx_request, block, SimProfile::Chain).await
}

/// [`get_prestate_diff_from_call`] with an explicit [`SimProfile`] controlling
/// the simulated environment (gas limits). `SimProfile::Chain` is identical to
/// the plain function.
pub async fn get_prestate_diff_from_call_with_profile<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
    profile: SimProfile,
) -> Result<DiffMode>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    let mut tx_request = tx_request.into();
    let mut options = GethDebugTracingCallOptions {
        tracing_options: GethDebugTracingOptions::prestate_tracer(PreStateConfig {
            diff_mode: Some(true),
            disable_code: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    apply_sim_profile(&mut tx_request, &mut options, profile);
    match provider
        .debug_trace_call(tx_request, block, options)
        .await
        .map_err(|e| anyhow!("prestate debug_trace_call failed: {}", e))?
    {
        GethTrace::PreStateTracer(PreStateFrame::Diff(diff)) => Ok(diff),
        other => Err(anyhow!("expected prestate diff frame, got {other:?}")),
    }
}

/// Simulate a call and return the `callTracer` frame (with logs): the call tree + emitted events.
///
/// Used together with [`get_prestate_diff_from_call`] by the prestate fast path to recover events and to
/// classify whether the cheap path is sound (no regular CALL / CREATE / cross-contract storage).
pub async fn get_call_frame_from_call<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
) -> Result<CallFrame>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    get_call_frame_from_call_with_profile(provider, tx_request, block, SimProfile::Chain).await
}

/// [`get_call_frame_from_call`] with an explicit [`SimProfile`] controlling
/// the simulated environment (gas limits). `SimProfile::Chain` is identical to
/// the plain function.
pub async fn get_call_frame_from_call_with_profile<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
    profile: SimProfile,
) -> Result<CallFrame>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    let mut tx_request = tx_request.into();
    let mut options = GethDebugTracingCallOptions {
        tracing_options: GethDebugTracingOptions::call_tracer(CallConfig {
            with_log: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    apply_sim_profile(&mut tx_request, &mut options, profile);
    match provider
        .debug_trace_call(tx_request, block, options)
        .await
        .map_err(|e| anyhow!("callTracer debug_trace_call failed: {}", e))?
    {
        GethTrace::CallTracer(frame) => Ok(frame),
        other => Err(anyhow!("expected call frame, got {other:?}")),
    }
}

/// Compute state updates from an existing transaction using its actual trace.
///
/// `status` must be `receipt.status()` for `tx_hash`. Pass the already-fetched
/// receipt's status to avoid an extra `eth_getTransactionReceipt` round-trip.
///
/// Returns: (state_updates, skipped_opcodes, call_gas_total)
pub async fn compute_state_updates_from_tx<P: Provider + DebugApi>(
    provider: &P,
    tx_hash: FixedBytes<32>,
    status: bool,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>, u64)> {
    // Primary path: use the historical trace via debug_traceTransaction.
    let trace = get_tx_trace(provider, tx_hash, status).await?;
    let struct_logs_len = trace.struct_logs.len();
    if struct_logs_len > 0 {
        return compute_state_updates(trace);
    }

    bail!(
        "debug_traceTransaction returned an empty trace for tx {}. \
         Some RPCs (notably Anvil) omit step-level tracing by default. \
         When using Anvil, start it with --steps-tracing to enable debug_traceTransaction support.",
        tx_hash
    )
}

// ============================================================================
// Block Transaction Fetching
// ============================================================================

/// Fetch preceding transactions from a block for replay.
///
/// Calls `eth_getBlockByNumber(block_number, true)` to get the block with
/// full transaction objects, then converts transactions at indices `0..tx_index`
/// into `PrecedingTx` structs suitable for replay in revm.
///
/// Returns an empty vec if `tx_index` is 0 (first in block).
pub async fn get_preceding_transactions<P: Provider>(
    provider: &P,
    block_number: u64,
    tx_index: u64,
) -> Result<Vec<PrecedingTx>> {
    if tx_index == 0 {
        return Ok(Vec::new());
    }

    let block = provider
        .get_block_by_number(block_number.into())
        .full()
        .await?
        .ok_or_else(|| anyhow!("Block {} not found", block_number))?;

    let base_fee = block.header.base_fee_per_gas;
    let txs: Vec<_> = block.transactions.into_transactions().collect();

    if (tx_index as usize) >= txs.len() {
        bail!(
            "Transaction index {} exceeds block transaction count {}",
            tx_index,
            txs.len()
        );
    }

    let preceding: Vec<PrecedingTx> = txs[..tx_index as usize]
        .iter()
        .map(|tx| {
            let kind = match tx.inner.to() {
                Some(addr) => revm::primitives::TxKind::Call(addr),
                None => revm::primitives::TxKind::Create,
            };
            // EIP-2930 access list pre-warms slots/addresses; without it
            // replay sees higher gas costs than the original tx and can
            // OOG-halt, dropping storage writes from the CacheDB.
            let access_list = tx.inner.access_list().cloned().unwrap_or_default();
            // EIP-7702 set-code authorizations; only present on type-4 txs.
            let authorization_list = tx
                .inner
                .authorization_list()
                .map(<[_]>::to_vec)
                .unwrap_or_default();
            PrecedingTx {
                from: tx.inner.signer(),
                kind,
                input: tx.inner.input().clone(),
                value: tx.inner.value(),
                gas_limit: tx.inner.gas_limit(),
                nonce: tx.inner.nonce(),
                gas_price: tx.inner.effective_gas_price(base_fee),
                access_list,
                authorization_list,
            }
        })
        .collect();

    Ok(preceding)
}

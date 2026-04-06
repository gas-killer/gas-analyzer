//! RPC-based trace fetching functions.
//!
//! This module contains the async functions that require network I/O
//! (alloy-provider, DebugApi). These are separated from the core crate
//! because they are not WASM-compatible.

use std::collections::HashSet;

use alloy::primitives::FixedBytes;
use alloy::rpc::types::trace::geth::{
    DefaultFrame, GethDebugTracingCallOptions, GethDebugTracingOptions, GethDefaultTracingOptions,
    GethTrace,
};
use alloy_eips::BlockId;
use alloy_provider::Provider;
use alloy_provider::ext::DebugApi;
use alloy_rpc_types::TransactionTrait;
use anyhow::{Result, anyhow, bail};

use gas_analyzer_core::trace::compute_state_updates;
use gas_analyzer_core::types::{Opcode, StateUpdate};
use gas_analyzer_estimator::PrecedingTx;

/// Get transaction trace from a provider using debug_traceTransaction.
///
/// This fetches the actual historical trace from an already-executed transaction,
/// ensuring we get the exact values that were stored during the original execution.
pub async fn get_tx_trace<P: Provider + DebugApi>(
    provider: &P,
    tx_hash: FixedBytes<32>,
) -> Result<DefaultFrame> {
    let tx_receipt = provider
        .get_transaction_receipt(tx_hash)
        .await?
        .ok_or_else(|| anyhow!("could not get receipt for tx {}", tx_hash))?;

    if !tx_receipt.status() {
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
    let tx_request = tx_request.into();
    let options = GethDebugTracingCallOptions {
        tracing_options: GethDebugTracingOptions {
            config: GethDefaultTracingOptions {
                enable_memory: Some(true),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };

    let GethTrace::Default(trace) = provider
        .debug_trace_call(tx_request, block, options)
        .await
        .map_err(|e| anyhow!("debug_trace_call failed: {}", e))?
    else {
        return Err(anyhow!("Expected default trace from debug_trace_call"));
    };

    Ok(trace)
}

/// Compute state updates from an existing transaction using its actual trace.
///
/// This is a convenience function that combines `get_tx_trace` and `compute_state_updates`.
///
/// Returns: (state_updates, skipped_opcodes, call_gas_total)
pub async fn compute_state_updates_from_tx<P: Provider + DebugApi>(
    provider: &P,
    tx_hash: FixedBytes<32>,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>, u64)> {
    // Primary path: use the historical trace via debug_traceTransaction.
    let trace = get_tx_trace(provider, tx_hash).await?;
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

    let txs: Vec<_> = block.transactions.into_transactions().collect();

    if (tx_index as usize) > txs.len() {
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
            PrecedingTx {
                from: tx.inner.signer(),
                kind,
                input: tx.inner.input().clone(),
                value: tx.inner.value(),
                gas_limit: tx.inner.gas_limit(),
                nonce: tx.inner.nonce(),
            }
        })
        .collect();

    Ok(preceding)
}

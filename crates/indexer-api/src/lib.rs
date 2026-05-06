//! Thin modular API layer over the gas-analyzer crates.
//!
//! This is the only module that talks to `gas_analyzer_rpc` and
//! `gas_analyzer_evmsketch` directly. Service code (`indexer-service`)
//! depends on the [`Analyzer`] trait, never on gas-analyzer internals.
//!
//! For v1, the orchestration logic mirrors what lives in
//! `crates/cli/src/main.rs:280-380`. The duplication is intentional and
//! tracked: see the plan file for the consolidation follow-up.

use std::sync::Arc;

use alloy::primitives::{Address, FixedBytes};
use alloy::providers::{ProviderBuilder, RootProvider};
use alloy_eips::BlockNumberOrTag;
use alloy_provider::Provider;
use alloy_rpc_types::TransactionTrait;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;

use gas_analyzer_core::{TURETZKY_UPPER_GAS_LIMIT, estimate_gas_from_state_updates};
use gas_analyzer_evmsketch::GasKillerEvmSketchDefault;
use gas_analyzer_rpc::{compute_state_updates_from_tx, get_preceding_transactions};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisReport {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_timestamp: u64,
    pub tx_hash: [u8; 32],
    pub tx_index: u64,
    pub from: [u8; 20],
    pub to: [u8; 20],
    pub function_selector: [u8; 4],
    pub gas_used: u64,
    pub effective_gas_price_wei: u128,
    pub gaskiller_gas_estimate: u64,
    pub gas_saved: u64,
    pub wei_saved: u128,
    pub is_heuristic: bool,
    pub failure_reason: Option<String>,
    pub state_update_count: u32,
    pub skipped_opcodes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SkipReason {
    ContractCreation,
    NoContractCalled,
    BelowGasThreshold,
    Reverted,
    EmptyTrace,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::ContractCreation => write!(f, "contract-creation tx"),
            SkipReason::NoContractCalled => write!(f, "no contract called"),
            SkipReason::BelowGasThreshold => write!(f, "below gas threshold"),
            SkipReason::Reverted => write!(f, "tx reverted on-chain"),
            SkipReason::EmptyTrace => write!(f, "empty debug trace"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AnalyzerError {
    #[error("transaction skipped: {0}")]
    Skipped(SkipReason),
    #[error("receipt not found for tx {0}")]
    ReceiptNotFound(String),
    #[error("rpc error: {0}")]
    Rpc(String),
    #[error("trace error: {0}")]
    Trace(String),
    #[error("estimation failed: {0}")]
    Estimation(String),
}

#[derive(Debug, Clone)]
pub struct AnalyzerConfig {
    pub chain_id: u64,
    /// Skip transactions whose `gas_used` is below this. Default 50_000 — same
    /// spirit as the README's "ignores transactions below the gas limit".
    pub min_gas_used: u64,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            chain_id: 1,
            min_gas_used: 50_000,
        }
    }
}

#[async_trait]
pub trait Analyzer: Send + Sync {
    async fn analyze_tx(&self, tx_hash: FixedBytes<32>) -> Result<AnalysisReport, AnalyzerError>;
}

/// EvmSketch-backed analyzer. Wraps `gas-analyzer-evmsketch` + `gas-analyzer-rpc`.
///
/// Holds a single shared `alloy` provider for direct RPC calls (receipt fetch,
/// trace fetch, preceding-tx fetch). The `GasKillerEvmSketch` itself spins up
/// its own provider internally per call — these RPC calls bypass any rate
/// limiter wrapped around `self.provider`. This is a known v1 limitation of
/// the minimal-touch wrapper approach. Tune the global rate limiter
/// conservatively to compensate.
pub struct EvmSketchAnalyzer {
    rpc_url: Url,
    provider: Arc<RootProvider>,
    config: AnalyzerConfig,
}

impl EvmSketchAnalyzer {
    pub fn new(rpc_url: Url, config: AnalyzerConfig) -> Self {
        let provider = ProviderBuilder::new()
            .disable_recommended_fillers()
            .connect_http(rpc_url.clone());
        Self {
            rpc_url,
            provider: Arc::new(provider),
            config,
        }
    }

    pub fn provider(&self) -> Arc<RootProvider> {
        self.provider.clone()
    }
}

#[async_trait]
impl Analyzer for EvmSketchAnalyzer {
    async fn analyze_tx(&self, tx_hash: FixedBytes<32>) -> Result<AnalysisReport, AnalyzerError> {
        let provider = self.provider.as_ref();

        // 1. Receipt — gates skip decisions.
        let receipt = provider
            .get_transaction_receipt(tx_hash)
            .await
            .map_err(|e| AnalyzerError::Rpc(format!("get_transaction_receipt: {e}")))?
            .ok_or_else(|| AnalyzerError::ReceiptNotFound(format!("0x{}", hex::encode(tx_hash))))?;

        if !receipt.status() {
            return Err(AnalyzerError::Skipped(SkipReason::Reverted));
        }

        let to: Address = receipt
            .to
            .ok_or(AnalyzerError::Skipped(SkipReason::ContractCreation))?;

        let gas_used = receipt.gas_used;
        if gas_used < self.config.min_gas_used {
            return Err(AnalyzerError::Skipped(SkipReason::BelowGasThreshold));
        }

        let block_number = receipt
            .block_number
            .ok_or_else(|| AnalyzerError::Rpc("receipt has no block_number".into()))?;
        let tx_index = receipt
            .transaction_index
            .ok_or_else(|| AnalyzerError::Rpc("receipt has no transaction_index".into()))?;
        let tx_sender = receipt.from;
        let effective_gas_price_wei = receipt.effective_gas_price;

        // 2. Block timestamp — needed for the report row.
        let block = provider
            .get_block_by_number(block_number.into())
            .await
            .map_err(|e| AnalyzerError::Rpc(format!("get_block_by_number: {e}")))?
            .ok_or_else(|| AnalyzerError::Rpc(format!("block {block_number} not found")))?;
        let block_timestamp = block.header.timestamp;

        // 3. Function selector via `eth_getTransactionByHash`.
        let function_selector = provider
            .get_transaction_by_hash(tx_hash)
            .await
            .map_err(|e| AnalyzerError::Rpc(format!("get_transaction_by_hash: {e}")))?
            .and_then(|tx| tx.function_selector().copied())
            .map(|fs| {
                let bytes: [u8; 4] = fs.into();
                bytes
            })
            .unwrap_or([0u8; 4]);

        // 4. Compute state updates from the actual historical trace.
        let (state_updates, skipped_opcodes, call_gas_total) =
            match compute_state_updates_from_tx(provider, tx_hash).await {
                Ok(t) => t,
                Err(e) => {
                    // Tx succeeded on-chain (we checked above) but trace
                    // extraction failed — mirror the CLI's heuristic fallback.
                    tracing::warn!(error = %e, "trace extraction failed; using heuristic");
                    (Vec::new(), std::collections::HashSet::new(), 0u64)
                }
            };

        // 5. Mid-block state — fetch preceding transactions.
        let preceding_txs = match get_preceding_transactions(provider, block_number, tx_index).await
        {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "get_preceding_transactions failed");
                Vec::new()
            }
        };

        // 6. Estimate. Try measured first; on failure, fall back to heuristic.
        let estimate_result: Result<u64, String> = if state_updates.is_empty() {
            Ok(estimate_gas_from_state_updates(&state_updates, call_gas_total))
        } else {
            let gk = GasKillerEvmSketchDefault::builder(self.rpc_url.clone())
                .at_block(BlockNumberOrTag::Number(block_number))
                .build()
                .await
                .map_err(|e| AnalyzerError::Estimation(format!("builder.build: {e}")))?;

            match gk.estimate_state_changes_gas_with_preceding(
                to,
                tx_sender,
                &state_updates,
                &preceding_txs,
            ) {
                Ok(g) => Ok(g),
                Err(e) => Err(format!("{e}")),
            }
        };

        let (gaskiller_gas_estimate, is_heuristic, failure_reason) = match estimate_result {
            Ok(g) => (g + TURETZKY_UPPER_GAS_LIMIT, false, None),
            Err(reason) => {
                let heuristic = estimate_gas_from_state_updates(&state_updates, call_gas_total);
                let truncated = reason.lines().next().unwrap_or("unknown").to_string();
                (heuristic + TURETZKY_UPPER_GAS_LIMIT, true, Some(truncated))
            }
        };

        let gas_saved = gas_used.saturating_sub(gaskiller_gas_estimate);
        let wei_saved = (gas_saved as u128).saturating_mul(effective_gas_price_wei);

        Ok(AnalysisReport {
            chain_id: self.config.chain_id,
            block_number,
            block_timestamp,
            tx_hash: tx_hash.into(),
            tx_index,
            from: tx_sender.into(),
            to: to.into(),
            function_selector,
            gas_used,
            effective_gas_price_wei,
            gaskiller_gas_estimate,
            gas_saved,
            wei_saved,
            is_heuristic,
            failure_reason,
            state_update_count: state_updates.len() as u32,
            skipped_opcodes: skipped_opcodes.into_iter().map(|o| format!("{o:?}")).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_reason_display() {
        assert_eq!(SkipReason::Reverted.to_string(), "tx reverted on-chain");
    }

    #[test]
    fn analyzer_config_defaults() {
        let cfg = AnalyzerConfig::default();
        assert_eq!(cfg.chain_id, 1);
        assert_eq!(cfg.min_gas_used, 50_000);
    }
}

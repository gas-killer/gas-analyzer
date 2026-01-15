//! EvmSketch-based Executor Module
//!
//! This module provides an Anvil-free transaction execution capability using
//! sp1-contract-call's EvmSketch. It allows simulating transactions against
//! RPC-backed state without running a local Anvil instance.

use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::rpc::types::eth::TransactionRequest;
use alloy_eips::BlockNumberOrTag;
use anyhow::{Result, anyhow};
use sp1_cc_client_executor::io::Primitives;
use sp1_cc_client_executor::{CallTraceArena, ContractCalldata, ContractInput};
use sp1_cc_host_executor::EvmSketch;
use url::Url;

// Re-export types for external use
pub use sp1_cc_client_executor::CallTraceArena as TraceArena;
pub use sp1_cc_host_executor::EvmSketch as Sketch;

/// Convert an Alloy TransactionRequest to a sp1-cc ContractInput.
///
/// This handles the mapping between the two transaction formats.
pub fn tx_request_to_contract_input(tx_request: &TransactionRequest) -> Result<ContractInput> {
    // Extract the contract address from the transaction
    let contract_address = match tx_request.to {
        Some(TxKind::Call(addr)) => addr,
        Some(TxKind::Create) => Address::ZERO, // Contract creation
        None => return Err(anyhow!("Transaction must have a 'to' address")),
    };

    // Get the caller address (from field)
    let caller_address = tx_request.from.unwrap_or_default();

    // Get the calldata
    let calldata = tx_request.input.input().cloned().unwrap_or_default();

    // Determine if this is a call or create
    let contract_calldata = match tx_request.to {
        Some(TxKind::Create) => ContractCalldata::Create(calldata),
        _ => ContractCalldata::Call(calldata),
    };

    Ok(ContractInput {
        contract_address,
        caller_address,
        calldata: contract_calldata,
    })
}

// Use types from alloy_provider
use alloy_provider::RootProvider;
use alloy_provider::network::AnyNetwork;

// Import EthPrimitives from reth_primitives (re-exported through sp1-cc-host-executor)
use reth_primitives::EthPrimitives;

/// The default provider type for EvmSketchExecutor
pub type DefaultProvider = RootProvider<AnyNetwork>;
/// The default primitives type  
pub type DefaultPrimitives = EthPrimitives;
/// The default executor type
pub type DefaultEvmSketchExecutor = EvmSketchExecutor<DefaultProvider, DefaultPrimitives>;

/// A wrapper around EvmSketch that provides transaction execution with tracing.
///
/// This executor fetches blockchain state from an RPC endpoint and executes
/// transactions locally using revm, without requiring an Anvil instance.
pub struct EvmSketchExecutor<P, PT> {
    /// The underlying EvmSketch instance
    pub sketch: EvmSketch<P, PT>,
}

/// Builder for EvmSketchExecutor
#[derive(Default)]
pub struct EvmSketchExecutorBuilder {
    rpc_url: Option<Url>,
    block: BlockNumberOrTag,
}

impl EvmSketchExecutorBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the RPC URL for fetching blockchain state.
    pub fn rpc_url(mut self, url: Url) -> Self {
        self.rpc_url = Some(url);
        self
    }

    /// Set the block number to execute at. Defaults to latest.
    pub fn at_block(mut self, block: BlockNumberOrTag) -> Self {
        self.block = block;
        self
    }

    /// Build the EvmSketchExecutor.
    pub async fn build(self) -> Result<DefaultEvmSketchExecutor> {
        let rpc_url = self.rpc_url.ok_or_else(|| anyhow!("RPC URL is required"))?;

        let sketch = EvmSketch::builder()
            .at_block(self.block)
            .el_rpc_url(rpc_url)
            .build()
            .await
            .map_err(|e| anyhow!("Failed to build EvmSketch: {}", e))?;

        Ok(EvmSketchExecutor { sketch })
    }
}

impl DefaultEvmSketchExecutor {
    /// Execute a transaction request and return the execution trace.
    ///
    /// This is the main method for Anvil-free transaction analysis.
    /// It converts the TransactionRequest to a ContractInput and executes
    /// it with tracing enabled.
    ///
    /// This calls the `Primitives::transact_with_trace` method directly,
    /// avoiding the need to modify the sp1-contract-call library.
    pub async fn execute_with_trace(
        &self,
        tx_request: &TransactionRequest,
    ) -> Result<(Bytes, CallTraceArena)> {
        use reth_evm::{ConfigureEvm, EthEvm, Evm};
        use reth_evm_ethereum::EthEvmConfig;
        use revm::context::Context;
        use revm::database::CacheDB;
        use revm::{MainBuilder, MainContext};
        use sp1_cc_client_executor::{TracingInspector, TracingInspectorConfig};

        let input = tx_request_to_contract_input(tx_request)?;

        // Create a cache DB from the sketch's RPC DB
        let cache_db = CacheDB::new(&self.sketch.rpc_db);

        // Build the chain spec from genesis
        let chain_spec = EthPrimitives::build_spec(&self.sketch.genesis)
            .map_err(|e| anyhow!("Failed to build chain spec: {:?}", e))?;

        let header = self.sketch.anchor.header();

        // Build EVM environment from header
        let evm_env = EthEvmConfig::new(chain_spec)
            .evm_env(header)
            .map_err(|e| anyhow!("Failed to build EVM env: {:?}", e))?;

        let mut cfg_env = evm_env.cfg_env;
        let mut block_env = evm_env.block_env;

        // Fix prevrandao for post-merge blocks
        // For post-merge, prevrandao should be set from mix_hash
        // If it's None or the mix_hash was zero, use parent_hash as a fallback
        if block_env.prevrandao.is_none() || block_env.prevrandao == Some(B256::ZERO) {
            block_env.prevrandao = Some(header.parent_hash);
        }

        // Set the base fee to 0 to enable 0 gas price transactions
        block_env.basefee = 0;
        block_env.difficulty = U256::ZERO;
        cfg_env.disable_nonce_check = true;
        cfg_env.disable_balance_check = true;
        cfg_env.disable_fee_charge = true;

        // Create tracing inspector with memory snapshots enabled
        // Memory snapshots are required to capture CALL arguments and LOG data
        let inspector = TracingInspector::new(
            TracingInspectorConfig::default_geth().set_memory_snapshots(true),
        );

        // Build the EVM context
        let evm = Context::mainnet()
            .with_db(cache_db)
            .with_cfg(cfg_env)
            .with_block(block_env)
            .modify_tx_chained(|tx_env| {
                tx_env.gas_limit = header.gas_limit;
            })
            .build_mainnet_with_inspector(inspector);

        let mut evm = EthEvm::new(evm, true); // true enables inspector

        // Execute the transaction
        let result = evm
            .transact(&input)
            .map_err(|e| anyhow!("Execution failed: {}", e))?;

        // Extract the trace from the inspector
        let trace = evm.into_inner().inspector.into_traces();

        // Extract output bytes from the execution result
        let output_bytes = match result.result {
            revm::context::result::ExecutionResult::Success { output, .. } => output.data().clone(),
            revm::context::result::ExecutionResult::Revert { output, .. } => {
                return Err(anyhow!("Execution reverted: {}", output));
            }
            revm::context::result::ExecutionResult::Halt { reason, .. } => {
                return Err(anyhow!("Execution halted: {:?}", reason));
            }
        };

        Ok((output_bytes, trace))
    }

    /// Execute a transaction request without tracing.
    pub async fn execute(&self, tx_request: &TransactionRequest) -> Result<Bytes> {
        let input = tx_request_to_contract_input(tx_request)?;

        let output = self
            .sketch
            .call_raw(&input)
            .await
            .map_err(|e| anyhow!("Execution failed: {}", e))?;

        Ok(output)
    }

    /// Get the block hash that the executor is anchored to.
    pub fn anchor_block_hash(&self) -> B256 {
        self.sketch.anchor.resolve().hash
    }

    /// Get the block number that the executor is anchored to.
    pub fn anchor_block_number(&self) -> u64 {
        self.sketch.anchor.header().number
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tx_request_to_contract_input() {
        use alloy::primitives::{address, bytes};

        let tx_request = TransactionRequest::default()
            .from(address!("0x0000000000000000000000000000000000000001"))
            .to(address!("0x0000000000000000000000000000000000000002"))
            .input(alloy::rpc::types::TransactionInput::new(bytes!(
                "0x12345678"
            )));

        let input = tx_request_to_contract_input(&tx_request).unwrap();

        assert_eq!(
            input.caller_address,
            address!("0x0000000000000000000000000000000000000001")
        );
        assert_eq!(
            input.contract_address,
            address!("0x0000000000000000000000000000000000000002")
        );
        match input.calldata {
            ContractCalldata::Call(data) => {
                assert_eq!(data, bytes!("0x12345678"));
            }
            _ => panic!("Expected Call calldata"),
        }
    }

    #[test]
    fn test_tx_request_no_to_address() {
        let tx_request = TransactionRequest::default();

        let result = tx_request_to_contract_input(&tx_request);
        assert!(result.is_err());
    }
}

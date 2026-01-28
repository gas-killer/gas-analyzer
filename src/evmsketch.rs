//! EvmSketch-based gas estimation module.
//!
//! This module provides Anvil-free gas estimation using sp1-contract-call's
//! EvmSketch for simulating StateChangeHandler execution.
//!
//! State updates are extracted using the shared `core::trace` module
//! via `debug_traceTransaction`, and this module handles the gas estimation
//! by injecting and executing the StateChangeHandlerGasEstimator contract.

use alloy::hex;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::providers::ProviderBuilder;
use alloy::rpc::types::eth::TransactionRequest;
use alloy_eips::BlockId;
use alloy_eips::BlockNumberOrTag;
use alloy_provider::Provider;
use alloy_provider::RootProvider;
use alloy_provider::ext::DebugApi;
use alloy_provider::network::AnyNetwork;
use anyhow::{Result, anyhow};
use reth_primitives::EthPrimitives;
use serde_json::Value;
use sp1_cc_client_executor::io::Primitives;
use sp1_cc_client_executor::{ContractCalldata, ContractInput};
use sp1_cc_host_executor::EvmSketch;
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use url::Url;

use crate::core::{
    Opcode, StateUpdate, compute_state_updates, encode_state_updates_to_abi,
    encode_state_updates_to_sol, estimate_gas_from_operations,
    extract_operation_counts_from_trace, get_trace_from_call,
};

// ============================================================================
// Executor Types
// ============================================================================

/// The default provider type for EvmSketchExecutor
pub type DefaultProvider = RootProvider<AnyNetwork>;
/// The default primitives type  
pub type DefaultPrimitives = EthPrimitives;
/// The default executor type
pub type DefaultEvmSketchExecutor = EvmSketchExecutor<DefaultProvider, DefaultPrimitives>;

// ============================================================================
// Transaction Request Conversion
// ============================================================================

/// Convert an Alloy TransactionRequest to a sp1-cc ContractInput.
///
/// This handles the mapping between the two transaction formats.
pub fn tx_request_to_contract_input(tx_request: &TransactionRequest) -> Result<ContractInput> {
    let contract_address = match tx_request.to {
        Some(TxKind::Call(addr)) => addr,
        Some(TxKind::Create) => Address::ZERO,
        None => return Err(anyhow!("Transaction must have a 'to' address")),
    };

    let caller_address = tx_request.from.unwrap_or_default();
    let calldata = tx_request.input.input().cloned().unwrap_or_default();

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

// ============================================================================
// EvmSketch Executor
// ============================================================================

/// A wrapper around EvmSketch that provides gas estimation capabilities.
///
/// This executor fetches blockchain state from an RPC endpoint and can
/// inject and execute the StateChangeHandlerGasEstimator contract to
/// measure gas costs for state updates.
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
    /// Load the StateChangeHandlerGasEstimator bytecode from the JSON file.
    ///
    /// # Returns
    /// The deployed bytecode as a hex string (without 0x prefix)
    fn load_estimator_bytecode() -> Result<String> {
        // Load from abis/StateChangeHandlerGasEstimator.json (relative to workspace root)
        let json_path = PathBuf::from("abis/StateChangeHandlerGasEstimator.json");
        let json_content = fs::read_to_string(&json_path)
            .map_err(|e| anyhow!("Failed to read JSON file at {:?}: {}", json_path, e))?;

        let json: Value = serde_json::from_str(&json_content)
            .map_err(|e| anyhow!("Failed to parse JSON: {}", e))?;

        // Extract the deployed bytecode from deployedBytecode.object
        let bytecode = json
            .get("deployedBytecode")
            .and_then(|v| v.get("object"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing 'deployedBytecode.object' in JSON"))?;

        // Remove 0x prefix if present
        let bytecode = bytecode.strip_prefix("0x").unwrap_or(bytecode);

        Ok(bytecode.to_string())
    }

    /// Estimate gas for executing a set of state updates.
    ///
    /// This injects the StateChangeHandlerGasEstimator contract into a cached
    /// database and executes the `runStateUpdatesCall` function to measure
    /// the actual gas consumption.
    ///
    /// # Arguments
    /// * `contract_address` - The address to inject the estimator contract at
    /// * `caller_address` - The address to use as the caller
    /// * `calldata` - The encoded calldata for `runStateUpdatesCall(uint8[], bytes[])`
    ///
    /// # Returns
    /// The gas used by the execution
    pub fn estimate_state_changes_gas_raw(
        &self,
        contract_address: Address,
        caller_address: Address,
        calldata: Bytes,
    ) -> Result<u64> {
        use reth_evm::{ConfigureEvm, EthEvm, Evm};
        use reth_evm_ethereum::EthEvmConfig;
        use revm::context::Context;
        use revm::database::CacheDB;
        use revm::state::{AccountInfo, Bytecode};
        use revm::{MainBuilder, MainContext};

        // Load StateChangeHandlerGasEstimator deployed bytecode from JSON file
        let estimator_bytecode = Self::load_estimator_bytecode()?;

        // Decode the bytecode from hex
        let bytecode_bytes = hex::decode(&estimator_bytecode)
            .map_err(|e| anyhow!("Failed to decode estimator bytecode: {}", e))?;

        let mut cache_db = CacheDB::new(&self.sketch.rpc_db);

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

        // Build chain spec
        let chain_spec = EthPrimitives::build_spec(&self.sketch.genesis)
            .map_err(|e| anyhow!("Failed to build chain spec: {:?}", e))?;

        let header = self.sketch.anchor.header();

        // Build EVM environment
        let evm_env = EthEvmConfig::new(chain_spec)
            .evm_env(header)
            .map_err(|e| anyhow!("Failed to build EVM env: {:?}", e))?;

        let mut cfg_env = evm_env.cfg_env;
        let mut block_env = evm_env.block_env;

        // Fix prevrandao for post-merge blocks
        if block_env.prevrandao.is_none() || block_env.prevrandao == Some(B256::ZERO) {
            block_env.prevrandao = Some(header.parent_hash);
        }

        block_env.basefee = 0;
        block_env.difficulty = U256::ZERO;
        cfg_env.disable_nonce_check = true;
        cfg_env.disable_balance_check = true;
        cfg_env.disable_fee_charge = true;

        let input = ContractInput {
            contract_address,
            caller_address,
            calldata: ContractCalldata::Call(calldata),
        };

        use revm::inspector::NoOpInspector;

        let evm = Context::mainnet()
            .with_db(cache_db)
            .with_cfg(cfg_env)
            .with_block(block_env)
            .modify_tx_chained(|tx_env| {
                tx_env.gas_limit = header.gas_limit;
            })
            .build_mainnet_with_inspector(NoOpInspector {});

        let mut evm = EthEvm::new(evm, false);

        let result = evm
            .transact(&input)
            .map_err(|e| anyhow!("Gas estimation failed: {}", e))?;

        match result.result {
            revm::context::result::ExecutionResult::Success { gas_used, .. } => Ok(gas_used),
            revm::context::result::ExecutionResult::Revert {
                output, gas_used, ..
            } => Err(anyhow!(
                "Gas estimation reverted (gas: {}): {}",
                gas_used,
                output
            )),
            revm::context::result::ExecutionResult::Halt {
                reason, gas_used, ..
            } => Err(anyhow!(
                "Gas estimation halted (gas: {}): {:?}",
                gas_used,
                reason
            )),
        }
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

// ============================================================================
// GasKiller Implementation
// ============================================================================

/// Type alias for the default EvmSketch-based GasKiller
pub type GasKillerEvmSketchDefault = GasKillerEvmSketch<
    alloy_provider::RootProvider<alloy_provider::network::AnyNetwork>,
    EthPrimitives,
>;

/// Builder for GasKillerEvmSketch
pub struct GasKillerEvmSketchBuilder {
    rpc_url: Url,
    block: BlockNumberOrTag,
}

impl GasKillerEvmSketchBuilder {
    /// Create a new builder with the required RPC URL.
    pub fn new(rpc_url: Url) -> Self {
        Self {
            rpc_url,
            block: BlockNumberOrTag::Latest,
        }
    }

    /// Set the block to execute at.
    pub fn at_block(mut self, block: BlockNumberOrTag) -> Self {
        self.block = block;
        self
    }

    /// Build the GasKillerEvmSketch instance.
    pub async fn build(self) -> Result<GasKillerEvmSketchDefault> {
        let executor = EvmSketchExecutorBuilder::new()
            .rpc_url(self.rpc_url)
            .at_block(self.block)
            .build()
            .await?;

        Ok(GasKillerEvmSketch { executor })
    }
}

/// EvmSketch-based GasKiller for gas estimation.
///
/// This implementation uses sp1-contract-call's EvmSketch to simulate
/// StateChangeHandler execution against RPC-backed state.
pub struct GasKillerEvmSketch<P, PT> {
    executor: EvmSketchExecutor<P, PT>,
}

impl GasKillerEvmSketchDefault {
    /// Create a new builder for GasKillerEvmSketch.
    pub fn builder(rpc_url: Url) -> GasKillerEvmSketchBuilder {
        GasKillerEvmSketchBuilder::new(rpc_url)
    }

    /// Estimate gas for state changes by actually executing them.
    ///
    /// This injects the StateChangeHandlerGasEstimator contract and
    /// executes the state updates to measure actual gas consumption.
    pub fn estimate_state_changes_gas(
        &self,
        contract_address: Address,
        state_updates: &[StateUpdate],
    ) -> Result<u64> {
        use alloy::dyn_abi::DynSolValue;

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

        let caller_address =
            alloy::primitives::address!("0x0000000000000000000000000000000000000001");

        self.executor.estimate_state_changes_gas_raw(
            contract_address,
            caller_address,
            Bytes::from(calldata),
        )
    }

    /// Estimate gas using a fallback heuristic based on the original transaction trace.
    ///
    /// This extracts operations (SSTORE, LOG, CALL) from the original transaction trace
    /// and applies heuristic costs.
    pub async fn estimate_gas_from_trace<P: Provider + DebugApi>(
        &self,
        provider: &P,
        tx_hash: alloy::primitives::FixedBytes<32>,
    ) -> Result<u64> {
        use crate::core::get_tx_trace;

        let trace = get_tx_trace(provider, tx_hash).await?;
        let operations = extract_operation_counts_from_trace(&trace);
        Ok(estimate_gas_from_operations(&operations))
    }

    /// Get the block number the executor is anchored to.
    pub fn anchor_block_number(&self) -> u64 {
        self.executor.anchor_block_number()
    }

    /// Get the block hash the executor is anchored to.
    pub fn anchor_block_hash(&self) -> B256 {
        self.executor.anchor_block_hash()
    }
}

// ============================================================================
// call_to_encoded_state_updates_with_evmsketch
// ============================================================================

/// Compute encoded state updates and gas estimate for a transaction call using EvmSketch.
///
/// Simulates the call via `debug_traceCall` at the given block, extracts state updates,
/// encodes them to ABI, and estimates gas using EvmSketch. Use this for validator-style
/// analysis without Anvil.
///
/// # Returns
/// `(storage_updates, gas_estimate, is_heuristic, skipped_opcodes)`
pub async fn call_to_encoded_state_updates_with_evmsketch(
    rpc_url: impl AsRef<str>,
    tx_request: TransactionRequest,
    block: BlockNumberOrTag,
) -> Result<(Bytes, u64, bool, HashSet<Opcode>)> {
    let rpc_url = rpc_url.as_ref();
    let url = Url::parse(rpc_url).map_err(|e| anyhow!("Invalid RPC URL: {}", e))?;

    let contract_address = tx_request
        .to
        .and_then(|t| match t {
            TxKind::Call(addr) => Some(addr),
            TxKind::Create => None,
        })
        .ok_or_else(|| anyhow!("Transaction must have a 'to' address"))?;

    let provider = ProviderBuilder::new().connect_http(url.clone());
    let block_id = BlockId::Number(block);
    let trace = get_trace_from_call(&provider, tx_request, block_id).await?;
    let (state_updates, skipped_opcodes, _call_gas_total) = compute_state_updates(trace)?;

    let storage_updates = encode_state_updates_to_abi(&state_updates);

    let gk = GasKillerEvmSketchDefault::builder(url)
        .at_block(block)
        .build()
        .await?;
    let gas_estimate = gk.estimate_state_changes_gas(contract_address, &state_updates)?;

    Ok((storage_updates, gas_estimate, false, skipped_opcodes))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, bytes};

    #[test]
    fn test_tx_request_to_contract_input() {
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

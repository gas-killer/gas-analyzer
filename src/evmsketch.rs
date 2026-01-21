//! EvmSketch-based implementation module.
//!
//! This module provides the Anvil-free GasKiller implementation that uses
//! sp1-contract-call's EvmSketch for transaction simulation.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use alloy::hex;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::rpc::types::eth::TransactionRequest;
use alloy_eips::BlockNumberOrTag;
use alloy_provider::RootProvider;
use alloy_provider::network::AnyNetwork;
use anyhow::{Result, anyhow, bail};
use reth_primitives::EthPrimitives;
use serde_json::Value;
use sp1_cc_client_executor::io::Primitives;
use sp1_cc_client_executor::{CallTraceArena, ContractCalldata, ContractInput};
use sp1_cc_host_executor::EvmSketch;
use url::Url;

use crate::core::{
    IStateUpdateTypes, Opcode, StateUpdate, TURETZKY_UPPER_GAS_LIMIT, encode_state_updates_to_abi,
    encode_state_updates_to_sol,
};

// Re-export types for external use
pub use sp1_cc_client_executor::CallTraceArena as TraceArena;
pub use sp1_cc_host_executor::EvmSketch as Sketch;

// ============================================================================
// Executor Types
// ============================================================================

/// Result of executing a transaction with the EvmSketch executor.
/// Includes output bytes, execution trace, and gas consumption.
#[derive(Debug)]
pub struct EvmExecutionResult {
    /// The output bytes from the execution
    pub output: Bytes,
    /// The execution trace containing all call frames and steps
    pub trace: CallTraceArena,
    /// The actual gas used by the execution
    pub gas_used: u64,
}

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

// ============================================================================
// EvmSketch Executor
// ============================================================================

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
    /// Execute a transaction request and return the execution result with trace and gas.
    ///
    /// This is the main method for Anvil-free transaction analysis.
    /// It converts the TransactionRequest to a ContractInput and executes
    /// it with tracing enabled.
    ///
    /// Returns an `EvmExecutionResult` containing output, trace, and gas_used.
    pub async fn execute_with_trace(
        &self,
        tx_request: &TransactionRequest,
    ) -> Result<EvmExecutionResult> {
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
        let evm_env = EthEvmConfig::new(chain_spec.clone())
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

        // Extract output bytes and gas_used from the execution result
        let (output_bytes, gas_used) = match &result.result {
            revm::context::result::ExecutionResult::Success {
                output, gas_used, ..
            } => (output.data().clone(), *gas_used),
            revm::context::result::ExecutionResult::Revert {
                output, gas_used, ..
            } => {
                return Err(anyhow!(
                    "Execution reverted (gas: {}): {}",
                    gas_used,
                    output
                ));
            }
            revm::context::result::ExecutionResult::Halt {
                reason, gas_used, ..
            } => {
                return Err(anyhow!(
                    "Execution halted (gas: {}): {:?}",
                    gas_used,
                    reason
                ));
            }
        };

        Ok(EvmExecutionResult {
            output: output_bytes,
            trace,
            gas_used,
        })
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

        // Create a CacheDB with the injected contract
        let mut cache_db = CacheDB::new(&self.sketch.rpc_db);

        // Inject the contract bytecode at the target address
        let account_info = AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000_000_000u128), // 1M ETH
            nonce: 0,
            code_hash: B256::ZERO, // Will be computed
            code: Some(Bytecode::new_raw(bytecode_bytes.into())),
        };
        cache_db.insert_account_info(contract_address, account_info);

        // Also give the caller plenty of balance
        let caller_info = AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000_000_000u128), // 1M ETH
            nonce: 0,
            code_hash: B256::ZERO,
            code: None,
        };
        cache_db.insert_account_info(caller_address, caller_info);

        // Build chain spec
        let chain_spec = EthPrimitives::build_spec(&self.sketch.genesis)
            .map_err(|e| anyhow!("Failed to build chain spec: {:?}", e))?;

        let header = self.sketch.anchor.header();

        // Build EVM environment from header (same as execute_with_trace)
        let evm_env = EthEvmConfig::new(chain_spec)
            .evm_env(header)
            .map_err(|e| anyhow!("Failed to build EVM env: {:?}", e))?;

        let mut cfg_env = evm_env.cfg_env;
        let mut block_env = evm_env.block_env;

        // Fix prevrandao for post-merge blocks (same fix as execute_with_trace)
        if block_env.prevrandao.is_none() || block_env.prevrandao == Some(B256::ZERO) {
            block_env.prevrandao = Some(header.parent_hash);
        }

        // Set the base fee to 0 to enable 0 gas price transactions
        block_env.basefee = 0;
        block_env.difficulty = U256::ZERO;
        cfg_env.disable_nonce_check = true;
        cfg_env.disable_balance_check = true;
        cfg_env.disable_fee_charge = true;

        // Create the contract input
        let input = ContractInput {
            contract_address,
            caller_address,
            calldata: ContractCalldata::Call(calldata),
        };

        // Build the EVM context with a NoOp inspector (required for EthEvm::transact)
        use revm::inspector::NoOpInspector;

        let evm = Context::mainnet()
            .with_db(cache_db)
            .with_cfg(cfg_env)
            .with_block(block_env)
            .modify_tx_chained(|tx_env| {
                tx_env.gas_limit = header.gas_limit;
            })
            .build_mainnet_with_inspector(NoOpInspector {});

        let mut evm = EthEvm::new(evm, false); // false = inspector not used for results

        // Execute the transaction
        let result = evm
            .transact(&input)
            .map_err(|e| anyhow!("Gas estimation failed: {}", e))?;

        // Extract gas_used from the execution result
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
}

// ============================================================================
// Opcode Tracer
// ============================================================================

/// Copy memory with bounds checking.
fn copy_memory(memory: &[u8], offset: usize, length: usize) -> Vec<u8> {
    if memory.len() >= offset + length {
        memory[offset..offset + length].to_vec()
    } else {
        let mut result = memory.to_vec();
        result.resize(offset + length, 0);
        result[offset..offset + length].to_vec()
    }
}

/// Compute state updates from a sp1-cc CallTraceArena.
///
/// This processes the trace in the same way as `compute_state_updates`,
/// extracting SSTORE, CALL, and LOG operations as state updates.
///
/// Returns: (state_updates, skipped_opcodes, external_call_gas)
/// - external_call_gas: Total gas used by external CALLs (to be added to heuristic estimate)
pub fn compute_state_updates_from_call_trace(
    trace: &CallTraceArena,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>, u64)> {
    let mut state_updates: Vec<StateUpdate> = Vec::new();
    let mut target_depth: usize = 1;
    let mut skipped_opcodes = HashSet::new();
    let mut external_call_gas: u64 = 0;

    // Collect all steps with their depths from all call frames
    // We need to process in execution order across all nodes
    let nodes = trace.nodes();

    for node in nodes {
        // Call depth: 0-indexed in trace, we use 1-indexed (1 = top level)
        let depth = node.trace.depth + 1;

        // Track gas used by external calls (depth 1 = direct calls from the entry point)
        // These are calls we can't optimize, so we add their gas to the estimate
        if depth == 2 {
            // depth 2 in our 1-indexed system = direct external calls from depth 1
            external_call_gas += node.trace.gas_used;
        }

        for step in &node.trace.steps {
            // Whenever stepping up (leaving a CALL/CALLCODE/DELEGATECALL) reset the target depth
            if depth < target_depth {
                target_depth = depth;
            } else if depth == target_depth {
                let op_name = step.op.as_str();

                // If we're going to step into a new execution context, increase the target depth
                if op_name == "DELEGATECALL" || op_name == "CALLCODE" {
                    target_depth = depth + 1;
                } else if let Some(opcode) =
                    append_state_update_from_call_trace_step(&mut state_updates, step, depth)?
                {
                    skipped_opcodes.insert(opcode);
                }
            }
        }
    }

    Ok((state_updates, skipped_opcodes, external_call_gas))
}

/// Append a state update from a CallTraceStep.
fn append_state_update_from_call_trace_step(
    state_updates: &mut Vec<StateUpdate>,
    step: &sp1_cc_client_executor::CallTraceStep,
    depth: usize,
) -> Result<Option<Opcode>> {
    let op_name = step.op.as_str();

    match op_name {
        "CREATE" | "CREATE2" | "SELFDESTRUCT" => {
            return Ok(Some(op_name.to_string()));
        }
        "DELEGATECALL" | "CALLCODE" => {
            bail!("Calling opcode {:?}, this shouldn't even happen!", op_name);
        }
        _ => {}
    }

    // Extract stack (reversed so index 0 is top of stack)
    let stack: Vec<U256> = step
        .stack
        .as_ref()
        .map(|s| s.iter().rev().copied().collect())
        .unwrap_or_default();

    // Extract memory
    let memory: Vec<u8> = step
        .memory
        .as_ref()
        .map(|m| m.as_bytes().to_vec())
        .unwrap_or_default();

    match op_name {
        "SSTORE" => {
            if stack.len() >= 2 {
                state_updates.push(StateUpdate::Store(IStateUpdateTypes::Store {
                    slot: stack[0].into(),
                    value: stack[1].into(),
                }));
            }
        }
        "CALL" => {
            if stack.len() >= 5 && depth == 1 {
                let args_offset: usize = stack[3].try_into().unwrap_or(0);
                let args_length: usize = stack[4].try_into().unwrap_or(0);
                let args = copy_memory(&memory, args_offset, args_length);
                state_updates.push(StateUpdate::Call(IStateUpdateTypes::Call {
                    target: Address::from_word(stack[1].into()),
                    value: stack[2],
                    callargs: args.into(),
                }));
            }
        }
        "LOG0" => {
            if stack.len() >= 2 && depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log0(IStateUpdateTypes::Log0 {
                    data: data.into(),
                }));
            }
        }
        "LOG1" => {
            if stack.len() >= 3 && depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log1(IStateUpdateTypes::Log1 {
                    data: data.into(),
                    topic1: stack[2].into(),
                }));
            }
        }
        "LOG2" => {
            if stack.len() >= 4 && depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log2(IStateUpdateTypes::Log2 {
                    data: data.into(),
                    topic1: stack[2].into(),
                    topic2: stack[3].into(),
                }));
            }
        }
        "LOG3" => {
            if stack.len() >= 5 && depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log3(IStateUpdateTypes::Log3 {
                    data: data.into(),
                    topic1: stack[2].into(),
                    topic2: stack[3].into(),
                    topic3: stack[4].into(),
                }));
            }
        }
        "LOG4" => {
            if stack.len() >= 6 && depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log4(IStateUpdateTypes::Log4 {
                    data: data.into(),
                    topic1: stack[2].into(),
                    topic2: stack[3].into(),
                    topic3: stack[4].into(),
                    topic4: stack[5].into(),
                }));
            }
        }
        _ => {}
    }

    Ok(None)
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

/// EvmSketch-based GasKiller that doesn't require Anvil.
///
/// This implementation uses sp1-contract-call's EvmSketch to simulate
/// transactions directly against RPC-backed state.
pub struct GasKillerEvmSketch<P, PT> {
    executor: EvmSketchExecutor<P, PT>,
}

impl GasKillerEvmSketchDefault {
    /// Create a new builder for GasKillerEvmSketch.
    ///
    /// # Example
    /// ```ignore
    /// use gas_analyzer_rs::evmsketch::GasKillerEvmSketchDefault;
    /// # async fn example() -> anyhow::Result<()> {
    /// let gk = GasKillerEvmSketchDefault::builder(url::Url::parse("http://localhost:8545")?)
    ///     .at_block(alloy_eips::BlockNumberOrTag::Number(12345))
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn builder(rpc_url: Url) -> GasKillerEvmSketchBuilder {
        GasKillerEvmSketchBuilder::new(rpc_url)
    }

    /// Execute a transaction and get the execution result with trace and gas.
    pub async fn execute_tx_and_get_trace(
        &self,
        tx_request: TransactionRequest,
    ) -> Result<EvmExecutionResult> {
        self.executor.execute_with_trace(&tx_request).await
    }

    /// Execute a transaction and compute state updates from the trace.
    ///
    /// Returns: (state_updates, skipped_opcodes, external_call_gas)
    pub async fn execute_and_compute_state_updates(
        &self,
        tx_request: TransactionRequest,
    ) -> Result<(Vec<StateUpdate>, HashSet<String>, u64)> {
        let result = self.execute_tx_and_get_trace(tx_request).await?;
        compute_state_updates_from_call_trace(&result.trace)
    }

    /// Estimate gas for state changes by actually executing them.
    ///
    /// This injects the StateChangeHandlerGasEstimator contract and
    /// executes the state updates to measure actual gas consumption.
    /// This provides accurate gas measurement similar to the Anvil approach.
    ///
    /// # Arguments
    /// * `contract_address` - The address to inject the estimator contract at
    /// * `state_updates` - The state updates to estimate gas for
    ///
    /// # Returns
    /// The actual gas used by the execution
    pub fn estimate_state_changes_gas(
        &self,
        contract_address: Address,
        state_updates: &[StateUpdate],
    ) -> Result<u64> {
        use alloy::dyn_abi::DynSolValue;

        // Encode the state updates as calldata for runStateUpdatesCall
        let (types, args) = encode_state_updates_to_sol(state_updates);

        // Convert types to DynSolValue::Array of Uint(8) for proper uint8[] encoding
        // Vec<u8> encodes as `bytes`, but we need `uint8[]` (array of 32-byte padded values)
        let types_array = DynSolValue::Array(
            types
                .iter()
                .map(|x| DynSolValue::Uint(U256::from(*x as u8), 8))
                .collect(),
        );

        // Convert args to DynSolValue::Array of Bytes
        let args_array = DynSolValue::Array(
            args.iter()
                .map(|b| DynSolValue::Bytes(b.to_vec()))
                .collect(),
        );

        // Function selector for runStateUpdatesCall(uint8[],bytes[])
        // keccak256("runStateUpdatesCall(uint8[],bytes[])")[0:4] = 0x7a888dbc
        let selector: [u8; 4] = [0x7a, 0x88, 0x8d, 0xbc];

        // Encode the arguments as a tuple
        let tuple = DynSolValue::Tuple(vec![types_array, args_array]);
        let encoded_args = tuple.abi_encode_params();

        // Build the full calldata
        let mut calldata = Vec::with_capacity(4 + encoded_args.len());
        calldata.extend_from_slice(&selector);
        calldata.extend_from_slice(&encoded_args);

        // Use a default caller address
        let caller_address =
            alloy::primitives::address!("0x0000000000000000000000000000000000000001");

        self.executor.estimate_state_changes_gas_raw(
            contract_address,
            caller_address,
            Bytes::from(calldata),
        )
    }

    /// Estimate gas for state changes using a heuristic approach.
    ///
    /// This provides a rough estimate based on known gas costs for each
    /// operation type, PLUS the actual gas used by external calls
    /// (which cannot be optimized).
    ///
    /// # Heuristic gas costs (approximate):
    /// - SSTORE (cold): ~20,000 gas
    /// - CALL: actual gas used (from trace) - cannot optimize external calls
    /// - LOG0-LOG4: ~375 + 375*topics + 8*data_len
    pub fn estimate_state_changes_gas_heuristic(
        &self,
        state_updates: &[StateUpdate],
        external_call_gas: u64,
    ) -> u64 {
        let mut gas = 21000u64; // Base transaction cost

        // Add actual gas used by external calls (cannot be optimized)
        gas += external_call_gas;

        for update in state_updates {
            gas += match update {
                StateUpdate::Store(_) => 20000, // Cold SSTORE
                // CALL gas is already included in external_call_gas from the trace
                StateUpdate::Call(_) => 0,
                StateUpdate::Log0(log) => 375 + log.data.len() as u64 * 8,
                StateUpdate::Log1(log) => 375 + 375 + log.data.len() as u64 * 8,
                StateUpdate::Log2(log) => 375 + 375 * 2 + log.data.len() as u64 * 8,
                StateUpdate::Log3(log) => 375 + 375 * 3 + log.data.len() as u64 * 8,
                StateUpdate::Log4(log) => 375 + 375 * 4 + log.data.len() as u64 * 8,
            };
        }

        gas
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
// Public API Functions
// ============================================================================

/// Compute state updates from a transaction using EvmSketch (no Anvil required).
///
/// Returns: (state_updates, skipped_opcodes, external_call_gas)
pub async fn compute_state_updates_with_evmsketch(
    rpc_url: Url,
    tx_request: TransactionRequest,
    block: BlockNumberOrTag,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>, u64)> {
    let gk = GasKillerEvmSketchDefault::builder(rpc_url)
        .at_block(block)
        .build()
        .await?;

    gk.execute_and_compute_state_updates(tx_request).await
}

/// Compute state updates and ABI-encode them, with gas estimation.
///
/// This is the Anvil-free equivalent of `call_to_encoded_state_updates_with_gas_estimate`.
/// First attempts measured gas estimation via StateChangeHandlerGasEstimator.
/// Falls back to heuristic estimation if measured estimation fails (e.g., due to CALL reverts).
///
/// The gas estimate includes:
/// - Gas for executing state updates via StateChangeHandler
/// - Gas used by external calls (cannot be optimized, must be included)
/// - Turetzky upper gas limit floor cost
///
/// Returns: (encoded_state_updates, gas_estimate, is_heuristic, skipped_opcodes)
/// - `is_heuristic`: true if heuristic was used, false if measured
pub async fn call_to_encoded_state_updates_with_evmsketch(
    rpc_url: Url,
    tx_request: TransactionRequest,
    block: BlockNumberOrTag,
) -> Result<(Bytes, u64, bool, HashSet<Opcode>)> {
    // Extract contract address for gas estimation
    let contract_address = tx_request
        .to
        .and_then(|x| match x {
            TxKind::Call(address) => Some(address),
            TxKind::Create => None,
        })
        .ok_or_else(|| anyhow!("Transaction must have a 'to' address for gas estimation"))?;

    let gk = GasKillerEvmSketchDefault::builder(rpc_url)
        .at_block(block)
        .build()
        .await?;

    let (state_updates, skipped_opcodes, external_call_gas) =
        gk.execute_and_compute_state_updates(tx_request).await?;

    // Try measured gas estimation first, fall back to heuristic if it fails
    let (gas_estimate, is_heuristic) =
        match gk.estimate_state_changes_gas(contract_address, &state_updates) {
            Ok(gas) => {
                // Add external call gas - these calls can't be optimized, so their cost must be included
                (gas + external_call_gas, false)
            }
            Err(_) => {
                // Fall back to heuristic estimation
                let heuristic =
                    gk.estimate_state_changes_gas_heuristic(&state_updates, external_call_gas);
                (heuristic, true)
            }
        };

    // Add the Turetzky upper gas limit floor cost (same as Anvil implementation)
    let gas_estimate = gas_estimate + TURETZKY_UPPER_GAS_LIMIT;

    Ok((
        encode_state_updates_to_abi(&state_updates),
        gas_estimate,
        is_heuristic,
        skipped_opcodes,
    ))
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

    #[test]
    fn test_module_compiles() {
        // This test verifies the module compiles correctly.
        // More comprehensive tests require mocking CallTraceArena.
    }
}

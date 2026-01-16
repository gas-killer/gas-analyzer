//! EvmSketch-based implementation module.
//!
//! This module provides the Anvil-free GasKiller implementation that uses
//! sp1-contract-call's EvmSketch for transaction simulation.

use std::collections::HashSet;

use alloy::hex;
use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::rpc::types::eth::TransactionRequest;
use alloy_eips::BlockNumberOrTag;
use alloy_provider::RootProvider;
use alloy_provider::network::AnyNetwork;
use anyhow::{Result, anyhow, bail};
use reth_primitives::EthPrimitives;
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

        // StateChangeHandlerGasEstimator deployed bytecode (from abis/StateChangeHandlerGasEstimator.json)
        const ESTIMATOR_BYTECODE: &str = "608060405234801561000f575f5ffd5b5060043610610029575f3560e01c80637a888dbc1461002d575b5f5ffd5b61004760048036038101906100429190610719565b610049565b005b6100538282610057565b5050565b8051825114610092576040517f5f6f132c00000000000000000000000000000000000000000000000000000000815260040160405180910390fd5b5f5f90505b8251811015610402575f8382815181106100b4576100b361078f565b5b602002602001015190505f8383815181106100d2576100d161078f565b5b602002602001015190505f60068111156100ef576100ee6107bc565b5b826006811115610102576101016107bc565b5b0361012b575f5f8280602001905181019061011d919061081c565b9150915080825550506103f3565b6001600681111561013f5761013e6107bc565b5b826006811115610152576101516107bc565b5b03610230575f5f5f8380602001905181019061016e9190610963565b9250925092505f5f5a90505f5f845160208601878986f1915081610226575f3d90505f8167ffffffffffffffff8111156101ab576101aa61042c565b5b6040519080825280601f01601f1916602001820160405280156101dd5781602001600182028036833780820191505090505b509050815f602083013e86816040517faa86ecee00000000000000000000000000000000000000000000000000000000815260040161021d929190610a41565b60405180910390fd5b50505050506103f2565b60026006811115610244576102436107bc565b5b826006811115610257576102566107bc565b5b03610280575f818060200190518101906102719190610a6f565b9050805160208201a0506103f1565b60036006811115610294576102936107bc565b5b8260068111156102a7576102a66107bc565b5b036102d5575f5f828060200190518101906102c29190610ab6565b9150915080825160208401a150506103f0565b600460068111156102e9576102e86107bc565b5b8260068111156102fc576102fb6107bc565b5b0361032f575f5f5f838060200190518101906103189190610b10565b9250925092508082845160208601a25050506103ef565b60056006811115610343576103426107bc565b5b826006811115610356576103556107bc565b5b0361038e575f5f5f5f848060200190518101906103739190610b7c565b9350935093509350808284865160208801a3505050506103ee565b6006808111156103a1576103a06107bc565b5b8260068111156103b4576103b36107bc565b5b036103ed575f5f5f5f5f858060200190518101906103d29190610bfc565b9450945094509450945080828486885160208a01a450505050505b5b5b5b5b5b5b50508080600101915050610097565b505050565b5f604051905090565b5f5ffd5b5f5ffd5b5f5ffd5b5f601f19601f8301169050919050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52604160045260245ffd5b6104628261041c565b810181811067ffffffffffffffff821117156104815761048061042c565b5b80604052505050565b5f610493610407565b905061049f8282610459565b919050565b5f67ffffffffffffffff8211156104be576104bd61042c565b5b602082029050602081019050919050565b5f5ffd5b600781106104df575f5ffd5b50565b5f813590506104f0816104d3565b92915050565b5f610508610503846104a4565b61048a565b9050808382526020820190506020840283018581111561052b5761052a6104cf565b5b835b81811015610554578061054088826104e2565b84526020840193505060208101905061052d565b5050509392505050565b5f82601f83011261057257610571610418565b5b81356105828482602086016104f6565b91505092915050565b5f67ffffffffffffffff8211156105a5576105a461042c565b5b602082029050602081019050919050565b5f5ffd5b5f67ffffffffffffffff8211156105d4576105d361042c565b5b6105dd8261041c565b9050602081019050919050565b828183375f83830152505050565b5f61060a610605846105ba565b61048a565b905082815260208101848484011115610626576106256105b6565b5b6106318482856105ea565b509392505050565b5f82601f83011261064d5761064c610418565b5b813561065d8482602086016105f8565b91505092915050565b5f6106786106738461058b565b61048a565b9050808382526020820190506020840283018581111561069b5761069a6104cf565b5b835b818110156106e257803567ffffffffffffffff8111156106c0576106bf610418565b5b8086016106cd8982610639565b8552602085019450505060208101905061069d565b5050509392505050565b5f82601f830112610700576106ff610418565b5b8135610710848260208601610666565b91505092915050565b5f5f6040838503121561072f5761072e610410565b5b5f83013567ffffffffffffffff81111561074c5761074b610414565b5b6107588582860161055e565b925050602083013567ffffffffffffffff81111561077957610778610414565b5b610785858286016106ec565b9150509250929050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52603260045260245ffd5b7f4e487b71000000000000000000000000000000000000000000000000000000005f52602160045260245ffd5b5f819050919050565b6107fb816107e9565b8114610805575f5ffd5b50565b5f81519050610816816107f2565b92915050565b5f5f6040838503121561083257610831610410565b5b5f61083f85828601610808565b925050602061085085828601610808565b9150509250929050565b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f6108838261085a565b9050919050565b61089381610879565b811461089d575f5ffd5b50565b5f815190506108ae8161088a565b92915050565b5f819050919050565b6108c6816108b4565b81146108d0575f5ffd5b50565b5f815190506108e1816108bd565b92915050565b8281835e5f83830152505050565b5f610907610902846105ba565b61048a565b905082815260208101848484011115610923576109226105b6565b5b61092e8482856108e7565b509392505050565b5f82601f83011261094a57610949610418565b5b815161095a8482602086016108f5565b91505092915050565b5f5f5f6060848603121561097a57610979610410565b5b5f610987868287016108a0565b9350506020610998868287016108d3565b925050604084015167ffffffffffffffff8111156109b9576109b8610414565b5b6109c586828701610936565b9150509250925092565b5f6109d98261085a565b9050919050565b6109e9816109cf565b82525050565b5f81519050919050565b5f82825260208201905092915050565b5f610a13826109ef565b610a1d81856109f9565b9350610a2d8185602086016108e7565b610a368161041c565b840191505092915050565b5f604082019050610a545f8301856109e0565b8181036020830152610a668184610a09565b90509392505050565b5f60208284031215610a8457610a83610410565b5b5f82015167ffffffffffffffff811115610aa157610aa0610414565b5b610aad84828501610936565b91505092915050565b5f5f60408385031215610acc57610acb610410565b5b5f83015167ffffffffffffffff811115610ae957610ae8610414565b5b610af585828601610936565b9250506020610b0685828601610808565b9150509250929050565b5f5f5f60608486031215610b2757610b26610410565b5b5f84015167ffffffffffffffff811115610b4457610b43610414565b5b610b5086828701610936565b9350506020610b6186828701610808565b9250506040610b7286828701610808565b9150509250925092565b5f5f5f5f60808587031215610b9457610b93610410565b5b5f85015167ffffffffffffffff811115610bb157610bb0610414565b5b610bbd87828801610936565b9450506020610bce87828801610808565b9350506040610bdf87828801610808565b9250506060610bf087828801610808565b91505092959194509250565b5f5f5f5f5f60a08688031215610c1557610c14610410565b5b5f86015167ffffffffffffffff811115610c3257610c31610414565b5b610c3e88828901610936565b9550506020610c4f88828901610808565b9450506040610c6088828901610808565b9350506060610c7188828901610808565b9250506080610c8288828901610808565b915050929550929590935056fea2646970667358221220bab2dda96c1ab6dd9df9b6cd4ed120835b7fddf38e7f81e3a83a69fcd2c42ac364736f6c634300081e0033";

        // Decode the bytecode from hex
        let bytecode_bytes = hex::decode(ESTIMATOR_BYTECODE)
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
            Ok(gas) => (gas, false),
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

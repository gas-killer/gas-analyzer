use crate::sol_types::StateUpdate;
use alloy::rpc::types::eth::TransactionRequest;
use anyhow::Result;
use url::Url;

// ============================================================================
// Anvil-based GasKiller (legacy implementation)
// ============================================================================

#[cfg(feature = "anvil")]
mod anvil_gk {
    use super::*;
    use crate::sol_types::RevertingContext;
    use alloy::{
        contract,
        dyn_abi::DynSolValue,
        hex,
        network::EthereumWallet,
        node_bindings::{Anvil, AnvilInstance},
        primitives::{Address, Bytes, FixedBytes, Selector, U256},
        providers::{
            Identity, ProviderBuilder, RootProvider,
            ext::DebugApi,
            fillers::{
                BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller,
                WalletFiller,
            },
        },
        rpc::{
            json_rpc::ErrorPayload,
            types::trace::geth::{
                DefaultFrame, GethDebugTracingOptions, GethDefaultTracingOptions, GethTrace,
            },
        },
        signers::local::PrivateKeySigner,
        sol_types::SolError,
        transports::RpcError,
    };
    use alloy_dyn_abi::{ErrorExt, JsonAbiExt};
    use alloy_provider::{Provider, ext::AnvilApi};
    use anyhow::{Context, Error, anyhow, bail};
    use foundry_evm_traces::identifier::SignaturesIdentifier;

    alloy::sol!(
        #[sol(rpc)]
        StateChangeHandlerGasEstimator,
        "res/abi/StateChangeHandlerGasEstimator.json"
    );

    // I really fucking hate rust's type system sometimes
    type ConnectHTTPDefaultProvider = FillProvider<
        JoinFill<
            JoinFill<
                Identity,
                JoinFill<GasFiller, JoinFill<BlobGasFiller, JoinFill<NonceFiller, ChainIdFiller>>>,
            >,
            WalletFiller<EthereumWallet>,
        >,
        RootProvider,
    >;
    pub type GasKillerDefault = GasKiller<ConnectHTTPDefaultProvider>;

    pub struct GasKiller<P> {
        _anvil: AnvilInstance,
        provider: P,
        code: Bytes,
    }

    impl GasKiller<ConnectHTTPDefaultProvider> {
        pub async fn new(fork_url: Url, block_number: Option<u64>) -> Result<Self> {
            let anvil_init = Anvil::new().fork(fork_url.as_str());

            let anvil = if let Some(number) = block_number {
                anvil_init.fork_block_number(number).try_spawn()?
            } else {
                anvil_init.try_spawn()?
            };
            let signer: PrivateKeySigner = anvil.keys()[0].clone().into();
            let provider = ProviderBuilder::new()
                .wallet(signer)
                .connect_http(anvil.endpoint_url());

            let contract = StateChangeHandlerGasEstimator::deploy(provider.clone()).await?;
            let address = *contract.address();
            let code = provider.get_code_at(address).await?;

            Ok(Self {
                _anvil: anvil,
                provider,
                code,
            })
        }

        pub async fn get_tx_trace(&self, tx_hash: FixedBytes<32>) -> Result<DefaultFrame> {
            let options = GethDebugTracingOptions {
                config: GethDefaultTracingOptions {
                    enable_memory: Some(true),
                    ..Default::default()
                },
                ..Default::default()
            };

            let GethTrace::Default(trace) = self
                .provider
                .debug_trace_transaction(tx_hash, options)
                .await?
            else {
                return Err(anyhow!("Expected default trace"));
            };
            Ok(trace)
        }

        pub async fn estimate_state_changes_gas(
            &self,
            contract_address: Address,
            state_updates: &[StateUpdate],
        ) -> Result<u64> {
            let initial_block_number = self.provider.get_block_number().await?;
            let snapshot_id: U256 = self.provider.raw_request("evm_snapshot".into(), ()).await?;
            let original_code = self.provider.get_code_at(contract_address).await?;
            self.provider
                .anvil_set_code(contract_address, self.code.clone())
                .await?;
            let target_contract =
                StateChangeHandlerGasEstimator::new(contract_address, &self.provider);

            self.provider
                .anvil_set_balance(
                    contract_address,
                    U256::from(100000000000000000000000000000u128),
                )
                .await?;

            let (types, args) = crate::encode_state_updates_to_sol(state_updates);
            let types = types.iter().map(|x| *x as u8).collect::<Vec<_>>();
            let tx = target_contract
                .runStateUpdatesCall(types, args)
                .send()
                .await;
            let tx = match tx {
                Ok(tx) => tx,
                Err(e) => return Err(Self::process_simulation_error(e).await),
            };
            let receipt = tx.get_receipt().await?;
            if !receipt.status() {
                bail!("Transaction failed");
            }

            self.provider
                .anvil_set_code(contract_address, original_code)
                .await?;

            let reverted: bool = self
                .provider
                .raw_request("evm_revert".into(), (snapshot_id,))
                .await?;
            assert!(reverted);
            let final_block_number = self.provider.get_block_number().await?;
            assert_eq!(
                initial_block_number, final_block_number,
                "block number should revert to initial state"
            );
            Ok(receipt.gas_used)
        }

        async fn process_simulation_error(error: contract::Error) -> Error {
            let processed_error = match &error {
                contract::Error::TransportError(RpcError::ErrorResp(ErrorPayload {
                    code: 3,
                    data: Some(data),
                    ..
                })) => {
                    let selector_hex = format!("0x{}", hex::encode(RevertingContext::SELECTOR));
                    let data_inner = data.get().trim_matches('"');
                    if !data_inner.starts_with(&selector_hex) {
                        Err(None)
                    } else {
                        Self::process_reverting_context_error(data_inner)
                            .await
                            .map_err(Some)
                    }
                }
                _ => Err(None),
            };

            match processed_error {
                Ok(processed_error) => processed_error,
                Err(Some(processing_error)) => Err::<(), _>(processing_error)
                    .context(format!("error processing error, original error: {}", error))
                    .unwrap_err(),
                Err(None) => error.into(),
            }
        }

        async fn process_reverting_context_error(data: &str) -> Result<anyhow::Error> {
            let reverting_context_error_hex = hex::decode(data).context(
                "something went incredibly wrong, rpc error contained invalid hex value",
            )?;
            let reverting_context = RevertingContext::abi_decode(&reverting_context_error_hex)
                .context("something went incredibly wrong, RevertingContext rpc error wasn't valid abi encoded")?;

            let signatures_identifier = SignaturesIdentifier::new(false).map_err(|e| anyhow!("detected RevertingContext error, but could not access SignaturesIdentifier service. error: {}", e))?;
            let revert_selector = reverting_context
                .revertData
                .get(0..4)
                .map(|bytes| Selector::try_from(bytes).unwrap());
            let error = (match revert_selector {
                Some(revert_selector) => {
                    signatures_identifier.identify_error(revert_selector).await
                }
                None => None,
            })
            .and_then(|identified_error| {
                match identified_error.decode_error(&reverting_context.revertData) {
                    Ok(decoded_error) => Some((identified_error, decoded_error)),
                    _ => None,
                }
            });

            let function_selector = reverting_context
                .callargs
                .get(0..4)
                .map(|bytes| Selector::try_from(bytes).unwrap());
            let function = (match function_selector {
                Some(function_selector) => {
                    signatures_identifier
                        .identify_function(function_selector)
                        .await
                }
                None => None,
            })
            .and_then(|identified_function| {
                match identified_function.abi_decode_input(&reverting_context.callargs) {
                    Ok(decoded_input) => Some((identified_function, decoded_input)),
                    _ => None,
                }
            });
            let target = reverting_context.target;
            let state_update_index = reverting_context.index;

            let function_string = match function {
                Some((identified_function, decoded_input)) => {
                    format!(
                        "{} with values ({})",
                        identified_function.signature(),
                        format_decoded_values(&decoded_input[..])
                    )
                }
                None => format!("Unrecognized function: {:?}", reverting_context.callargs),
            };

            let error_string = match error {
                Some((identified_error, decoded_error)) => {
                    format!(
                        "{} with values ({})",
                        identified_error.signature(),
                        format_decoded_values(&decoded_error.body)
                    )
                }
                None => format!("Unrecognized error: {:?}", reverting_context.revertData),
            };

            Ok(anyhow!(
                "Simulation subcontext reverted. State Update Index: {} Target Address: {}, Called {} Got error: {}",
                state_update_index,
                target,
                function_string,
                error_string
            ))
        }
    }

    fn format_decoded_values(values: &[DynSolValue]) -> String {
        values
            .iter()
            .map(|v| format!("{:?}", v))
            .collect::<Vec<String>>()
            .join(", ")
    }
}

#[cfg(feature = "anvil")]
pub use anvil_gk::*;

// ============================================================================
// EvmSketch-based GasKiller (Anvil-free implementation)
// ============================================================================

#[cfg(feature = "evmsketch")]
pub mod evmsketch_gk {
    use super::*;
    use crate::evmsketch_executor::{EvmSketchExecutor, EvmSketchExecutorBuilder};
    use crate::opcode_tracer::compute_state_updates_from_call_trace;
    use alloy_eips::BlockNumberOrTag;
    use std::collections::HashSet;

    // Import EthPrimitives
    use reth_primitives::EthPrimitives;

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
        /// use gas_analyzer_rs::gk::evmsketch_gk::GasKillerEvmSketchDefault;
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
        ) -> Result<crate::evmsketch_executor::EvmExecutionResult> {
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
            contract_address: alloy::primitives::Address,
            state_updates: &[StateUpdate],
        ) -> Result<u64> {
            use alloy::dyn_abi::DynSolValue;
            use alloy::primitives::Bytes;

            // Encode the state updates as calldata for runStateUpdatesCall
            let (types, args) = crate::encode_state_updates_to_sol(state_updates);

            // Convert types to DynSolValue::Array of Uint(8) for proper uint8[] encoding
            // Vec<u8> encodes as `bytes`, but we need `uint8[]` (array of 32-byte padded values)
            let types_array = DynSolValue::Array(
                types
                    .iter()
                    .map(|x| DynSolValue::Uint(alloy::primitives::U256::from(*x as u8), 8))
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

            self.executor.estimate_state_changes_gas(
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
        pub fn anchor_block_hash(&self) -> alloy::primitives::B256 {
            self.executor.anchor_block_hash()
        }
    }
}

#[cfg(feature = "evmsketch")]
pub use evmsketch_gk::*;

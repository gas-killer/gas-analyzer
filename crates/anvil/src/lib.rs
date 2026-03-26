//! Anvil-based implementation module.
//!
//! This module provides the legacy Anvil-based GasKiller implementation
//! that requires running a local Anvil instance for transaction simulation.

use std::collections::HashSet;

use alloy::{
    contract,
    dyn_abi::DynSolValue,
    hex,
    network::EthereumWallet,
    node_bindings::{Anvil, AnvilInstance},
    primitives::{Address, Bytes, FixedBytes, Selector, TxKind, U256},
    providers::{
        Identity, Provider, ProviderBuilder, RootProvider,
        ext::DebugApi,
        fillers::{
            BlobGasFiller, ChainIdFiller, FillProvider, GasFiller, JoinFill, NonceFiller,
            WalletFiller,
        },
    },
    rpc::{
        json_rpc::ErrorPayload,
        types::{
            TransactionReceipt,
            eth::TransactionRequest,
            trace::geth::{
                DefaultFrame, GethDebugTracingOptions, GethDefaultTracingOptions, GethTrace,
            },
        },
    },
    signers::local::PrivateKeySigner,
    sol_types::SolError,
    transports::RpcError,
};
use alloy_dyn_abi::{ErrorExt, JsonAbiExt};
use alloy_provider::ext::AnvilApi;
use alloy_rpc_types::TransactionTrait;
use anyhow::{Context, Error, Result, anyhow, bail};
use chrono::{DateTime, Utc};
use foundry_evm_traces::identifier::SignaturesIdentifier;
use serde::Serialize;
use url::Url;

/// Compute the isolated storage slot for the implementation address.
/// Mirrors the Solidity constant: `keccak256("gas.estimator.implementation") - 1`
fn impl_slot() -> U256 {
    U256::from_be_bytes(*alloy::primitives::keccak256(
        "gas.estimator.implementation",
    )) - U256::from(1)
}

use gas_analyzer_core::{
    Opcode, RevertingContext, StateUpdate, TURETZKY_UPPER_GAS_LIMIT, compute_state_updates,
    encode_state_updates_to_abi, encode_state_updates_to_sol,
};
use gas_analyzer_rpc::get_tx_trace;

// ============================================================================
// Report Types
// ============================================================================

/// Gas analysis report for a transaction
#[derive(Serialize)]
pub struct GasKillerReport {
    pub time: DateTime<Utc>,
    pub commit: String,
    pub tx_hash: FixedBytes<32>,
    pub block_hash: FixedBytes<32>,
    pub block_number: u64,
    pub gas_used: u64,
    pub gas_cost: u128,
    pub approx_gas_unit_price: f64,
    pub gaskiller_gas_estimate: u64,
    pub gaskiller_estimated_gas_cost: f64,
    pub gas_savings: u64,
    pub percent_savings: f64,
    pub function_selector: FixedBytes<4>,
    pub skipped_opcodes: String,
    pub error_log: Option<String>,
}

impl GasKillerReport {
    pub fn report_error(
        time: DateTime<Utc>,
        receipt: &TransactionReceipt,
        e: &anyhow::Error,
    ) -> Self {
        let commit = env!("GIT_HASH").to_string();

        GasKillerReport {
            time,
            commit,
            tx_hash: receipt.transaction_hash,
            block_hash: receipt.block_hash.unwrap_or_else(|| {
                panic!(
                    "couldn't retrieve block hash for tx {}",
                    receipt.transaction_hash
                )
            }),
            block_number: receipt.block_number.unwrap_or_else(|| {
                panic!(
                    "couldn't retrieve block number for tx {}",
                    receipt.transaction_hash
                )
            }),
            gas_used: receipt.gas_used,
            gas_cost: receipt.effective_gas_price,
            approx_gas_unit_price: receipt.effective_gas_price as f64 / receipt.gas_used as f64,
            gaskiller_gas_estimate: 0,
            gaskiller_estimated_gas_cost: 0.0,
            gas_savings: 0,
            percent_savings: 0.0,
            function_selector: FixedBytes::default(),
            skipped_opcodes: "".to_string(),
            error_log: Some(format!("{e:?}")),
        }
    }

    pub fn from(time: DateTime<Utc>, receipt: &TransactionReceipt, details: ReportDetails) -> Self {
        let commit = env!("GIT_HASH").to_string();

        GasKillerReport {
            time,
            commit,
            tx_hash: receipt.transaction_hash,
            block_hash: receipt.block_hash.unwrap_or_else(|| {
                panic!(
                    "couldn't retrieve block hash for tx {}",
                    receipt.transaction_hash
                )
            }),
            block_number: receipt.block_number.unwrap_or_else(|| {
                panic!(
                    "couldn't retrieve block number for tx {}",
                    receipt.transaction_hash
                )
            }),
            gas_used: receipt.gas_used,
            gas_cost: receipt.effective_gas_price,
            approx_gas_unit_price: details.approx_gas_price_per_unit,
            gaskiller_gas_estimate: details.gaskiller_gas_estimate,
            gaskiller_estimated_gas_cost: details.gaskiller_estimated_gas_cost,
            gas_savings: details.gas_savings,
            percent_savings: details.percent_savings,
            function_selector: details.function_selector,
            skipped_opcodes: details.skipped_opcodes,
            error_log: None,
        }
    }
}

/// Details for gas estimation report
pub struct ReportDetails {
    pub approx_gas_price_per_unit: f64,
    pub gaskiller_gas_estimate: u64,
    pub gaskiller_estimated_gas_cost: f64,
    pub gas_savings: u64,
    pub percent_savings: f64,
    pub function_selector: FixedBytes<4>,
    pub skipped_opcodes: String,
}

// ============================================================================
// GasKiller Implementation
// ============================================================================

alloy::sol!(
    #[sol(rpc)]
    StateChangeHandlerGasEstimator,
    "../../abis/StateChangeHandlerGasEstimator.json"
);

// Provider type alias
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

/// Default GasKiller type alias
pub type GasKillerDefault = GasKiller<ConnectHTTPDefaultProvider>;

/// Anvil-based GasKiller for transaction simulation and gas estimation
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

        let contract = StateChangeHandlerGasEstimator::deploy(
            provider.clone(),
            alloy::primitives::Address::ZERO,
        )
        .await?;
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

        // Stash the original bytecode at a synthetic backup address so the proxy's
        // fallback can DELEGATECALL to it when external protocols (oracles, AMMs)
        // callback into contract_address during a state-update CALL.
        let backup_addr = Address::from([0xba; 20]);
        self.provider
            .anvil_set_code(backup_addr, original_code.clone())
            .await?;

        // Inject the proxy (estimator) at contract_address.
        self.provider
            .anvil_set_code(contract_address, self.code.clone())
            .await?;

        // Write backup_addr into the isolated IMPL_SLOT so the proxy's fallback
        // can find it. Slot = keccak256("gas.estimator.implementation") - 1.
        let backup_addr_b256 =
            alloy::primitives::B256::from(U256::from_be_slice(backup_addr.as_slice()));
        self.provider
            .anvil_set_storage_at(contract_address, impl_slot(), backup_addr_b256)
            .await?;

        let target_contract = StateChangeHandlerGasEstimator::new(contract_address, &self.provider);

        let (types, args) = encode_state_updates_to_sol(state_updates);
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
        let reverting_context_error_hex = hex::decode(data)
            .context("something went incredibly wrong, rpc error contained invalid hex value")?;
        let reverting_context = RevertingContext::abi_decode(&reverting_context_error_hex)
            .context("something went incredibly wrong, RevertingContext rpc error wasn't valid abi encoded")?;

        let signatures_identifier = SignaturesIdentifier::new(false).map_err(|e| anyhow!("detected RevertingContext error, but could not access SignaturesIdentifier service. error: {}", e))?;
        let revert_selector = reverting_context
            .revertData
            .get(0..4)
            .map(|bytes| Selector::try_from(bytes).unwrap());
        let error = (match revert_selector {
            Some(revert_selector) => signatures_identifier.identify_error(revert_selector).await,
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

// ============================================================================
// Trace Functions
// ============================================================================

/// Get trace from a simulated call using Anvil
pub async fn get_trace_from_call(
    rpc_url: Url,
    tx_request: TransactionRequest,
    block_height: Option<u64>,
) -> Result<DefaultFrame> {
    let provider = ProviderBuilder::new().connect_anvil_with_wallet_and_config(|config| {
        let config = config
            .fork(rpc_url)
            .arg("--steps-tracing")
            .arg("--auto-impersonate");
        if let Some(height) = block_height {
            config.arg("--fork-block-number").arg(height.to_string())
        } else {
            config
        }
    })?;
    let tx_receipt = provider
        .send_transaction(tx_request)
        .await?
        .get_receipt()
        .await?;
    if !tx_receipt.status() {
        bail!("transaction failed");
    }
    let tx_hash = tx_receipt.transaction_hash;
    get_tx_trace(&provider, tx_hash).await
}

// ============================================================================
// Public API Functions
// ============================================================================

/// Check if a transaction invokes a smart contract
pub async fn invokes_smart_contract(
    provider: impl Provider,
    receipt: &TransactionReceipt,
) -> Result<bool> {
    let to_address = receipt.to;
    match to_address {
        None => Ok(false),
        Some(address) => {
            let code = provider.get_code_at(address).await?;
            Ok(!code.is_empty())
        }
    }
}

/// Computes state updates and estimates for each transaction one by one, nicer for CLI
pub async fn gas_estimate_block(
    provider: impl Provider,
    all_receipts: Vec<TransactionReceipt>,
    gk: GasKillerDefault,
) -> Result<Vec<GasKillerReport>> {
    let block_number = all_receipts[0]
        .block_number
        .expect("couldn't find block number in receipt");
    let receipts: Vec<_> = all_receipts
        .into_iter()
        .filter(|x| x.gas_used > TURETZKY_UPPER_GAS_LIMIT && x.to.is_some())
        .collect();

    println!("got {} receipts for block {}", receipts.len(), block_number);
    let mut reports = Vec::new();
    for receipt in receipts {
        println!("processing {}", &receipt.transaction_hash);
        reports.push(
            get_report(&provider, receipt.transaction_hash, &receipt, &gk)
                .await
                .unwrap_or_else(|e| GasKillerReport::report_error(Utc::now(), &receipt, &e)),
        );
        println!("done");
    }
    Ok(reports)
}

/// Estimate gas for a single transaction
pub async fn gas_estimate_tx(
    provider: impl Provider,
    tx_hash: FixedBytes<32>,
    gk: &GasKillerDefault,
) -> Result<GasKillerReport> {
    let receipt = provider
        .get_transaction_receipt(tx_hash)
        .await?
        .ok_or_else(|| anyhow!("could not get receipt for tx {}", tx_hash))?;
    let smart_contract_tx = invokes_smart_contract(&provider, &receipt).await?;
    if receipt.gas_used <= TURETZKY_UPPER_GAS_LIMIT
        || !smart_contract_tx
        || receipt.to.is_none()
        || !receipt.status()
    {
        bail!(
            "Skipped: either 1) gas used is less than or equal to TUGL or 2) no smart contract calls are made or 3) contract creation transaction or 4) transaction failed"
        )
    }

    get_report(&provider, tx_hash, &receipt, gk).await
}

/// Generate a report for a transaction
pub async fn get_report(
    provider: impl Provider,
    tx_hash: FixedBytes<32>,
    receipt: &TransactionReceipt,
    gk: &GasKillerDefault,
) -> Result<GasKillerReport> {
    let details = gaskiller_reporter(&provider, tx_hash, gk, receipt).await;
    if let Err(e) = details {
        return Ok(GasKillerReport::report_error(Utc::now(), receipt, &e));
    }

    Ok(GasKillerReport::from(Utc::now(), receipt, details.unwrap()))
}

/// Generate detailed report for a transaction
pub async fn gaskiller_reporter(
    provider: impl Provider,
    tx_hash: FixedBytes<32>,
    gk: &GasKillerDefault,
    receipt: &TransactionReceipt,
) -> Result<ReportDetails> {
    let transaction = provider
        .get_transaction_by_hash(tx_hash)
        .await?
        .ok_or_else(|| anyhow!("could not get receipt for tx {}", tx_hash))?;
    let trace = get_tx_trace(&provider, tx_hash).await?;
    let (state_updates, skipped_opcodes_set, _call_gas_total) = compute_state_updates(trace)?;
    let skipped_opcodes = skipped_opcodes_set
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");
    let gaskiller_gas_estimate = gk
        .estimate_state_changes_gas(
            receipt.to.unwrap(), // already check if this is None in gas_estimate_tx
            &state_updates,
        )
        .await?;
    let gaskiller_gas_estimate = gaskiller_gas_estimate + TURETZKY_UPPER_GAS_LIMIT;
    let gas_used = receipt.gas_used;
    let approx_gas_price_per_unit: f64 = receipt.effective_gas_price as f64 / gas_used as f64;
    let gaskiller_estimated_gas_cost = approx_gas_price_per_unit * gaskiller_gas_estimate as f64;
    let gas_savings = gas_used.saturating_sub(gaskiller_gas_estimate);
    let function_selector = *transaction
        .function_selector()
        .ok_or_else(|| anyhow!("could not get function selector for tx 0x{}", tx_hash))?;
    Ok(ReportDetails {
        approx_gas_price_per_unit,
        gaskiller_gas_estimate,
        gaskiller_estimated_gas_cost,
        gas_savings,
        percent_savings: (gas_savings * 100) as f64 / gas_used as f64,
        function_selector,
        skipped_opcodes,
    })
}

/// Compute state updates and encode with gas estimate
pub async fn call_to_encoded_state_updates_with_gas_estimate(
    url: Url,
    tx_request: TransactionRequest,
    gk: GasKillerDefault,
    block_height: Option<u64>,
) -> Result<(Bytes, u64, HashSet<Opcode>)> {
    let contract_address = tx_request
        .to
        .and_then(|x| match x {
            TxKind::Call(address) => Some(address),
            TxKind::Create => None,
        })
        .ok_or_else(|| anyhow!("receipt does not have to address"))?;
    let trace = get_trace_from_call(url, tx_request, block_height).await?;
    let (state_updates, skipped_opcodes, _call_gas_total) = compute_state_updates(trace)?;
    let gas_estimate = gk
        .estimate_state_changes_gas(contract_address, &state_updates)
        .await?;
    Ok((
        encode_state_updates_to_abi(&state_updates),
        gas_estimate,
        skipped_opcodes,
    ))
}

// ============================================================================
// Transaction Extractor
// ============================================================================

/// Minimal transaction state extractor that reuses existing gas-analyzer-rs functionality
pub struct TxStateExtractor<P: Provider + DebugApi> {
    provider: P,
}

impl<P: Provider + DebugApi> TxStateExtractor<P> {
    /// Create a new extractor with the given provider
    pub fn new(provider: P) -> Self {
        Self { provider }
    }

    /// Extract state updates from a transaction hash
    pub async fn extract_state_updates(&self, tx_hash: FixedBytes<32>) -> Result<Vec<StateUpdate>> {
        // Use existing get_tx_trace function
        let trace = get_tx_trace(&self.provider, tx_hash).await?;

        // Use existing compute_state_updates function
        let (state_updates, _skipped, _call_gas_total) = compute_state_updates(trace)?;

        Ok(state_updates)
    }

    /// Extract state updates with transaction metadata
    pub async fn extract_with_metadata(
        &self,
        tx_hash: FixedBytes<32>,
    ) -> Result<StateUpdateReport> {
        let receipt = self
            .provider
            .get_transaction_receipt(tx_hash)
            .await?
            .ok_or_else(|| anyhow!("Transaction not found"))?;

        let tx = self
            .provider
            .get_transaction_by_hash(tx_hash)
            .await?
            .ok_or_else(|| anyhow!("Transaction not found"))?;

        if !receipt.status() {
            return Err(anyhow!("Transaction failed"));
        }

        let trace = get_tx_trace(&self.provider, tx_hash).await?;
        let (state_updates, _skipped, _call_gas_total) = compute_state_updates(trace)?;

        Ok(StateUpdateReport {
            tx_hash,
            block_number: receipt.block_number.unwrap_or(0),
            from: receipt.from,
            to: tx.inner.to(),
            value: tx.inner.value(),
            gas_used: receipt.gas_used as u128,
            status: receipt.status(),
            state_updates,
        })
    }
}

/// Convenience function to create an extractor from RPC URL
pub fn tx_extractor_from_rpc_url(
    rpc_url: &str,
) -> Result<TxStateExtractor<impl Provider + DebugApi>> {
    let provider = ProviderBuilder::new().connect_http(rpc_url.parse()?);
    Ok(TxStateExtractor::new(provider))
}

/// Report containing state updates and transaction metadata
#[derive(Debug)]
pub struct StateUpdateReport {
    pub tx_hash: FixedBytes<32>,
    pub block_number: u64,
    pub from: Address,
    pub to: Option<Address>,
    pub value: U256,
    pub gas_used: u128,
    pub status: bool,
    pub state_updates: Vec<StateUpdate>,
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use gas_analyzer_core::constants::*;
    use gas_analyzer_core::{IStateUpdateTypes, StateUpdateType, decode_state_updates_tuple};

    // Local sol! with #[sol(rpc)] for test that needs SimpleStorageInstance
    alloy::sol! {
        #[sol(rpc)]
        contract SimpleStorage {
            function set(uint256 x) public;
        }
    }
    use alloy::primitives::{U256, address, b256, bytes};
    use csv::Writer;
    use std::fs::File;

    #[test]
    fn test_stateupdatetype_tuple_encoding() -> Result<()> {
        // Test encoding as (StateUpdateType[], bytes[]) tuple
        let state_updates = vec![
            StateUpdate::Store(IStateUpdateTypes::Store {
                slot: b256!("debfdfd5a50ad117c10898d68b5ccf0893c6b40d4f443f902e2e7646601bdeaf"),
                value: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            }),
            StateUpdate::Log0(IStateUpdateTypes::Log0 {
                data: Bytes::from(vec![0x00, 0x00, 0x6f, 0xee]),
            }),
            StateUpdate::Log1(IStateUpdateTypes::Log1 {
                data: Bytes::from(vec![0x00, 0x00, 0x6f, 0xee]),
                topic1: b256!("fd3dfbb3da06b2710848916c65866a3d0e050047402579a6e1714261137c19c6"),
            }),
        ];

        let encoded = encode_state_updates_to_abi(&state_updates);
        let (types, data) = decode_state_updates_tuple(&encoded)?;
        assert_eq!(
            types,
            vec![U256::from(0u8), U256::from(2u8), U256::from(3u8),]
        );
        assert_eq!(data.len(), 3);
        Ok(())
    }

    #[test]
    fn test_encoding_format() -> Result<()> {
        // Create multiple state updates to match the chisel example
        let state_updates = vec![
            StateUpdate::Store(IStateUpdateTypes::Store {
                slot: b256!("debfdfd5a50ad117c10898d68b5ccf0893c6b40d4f443f902e2e7646601bdeaf"),
                value: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            }),
            StateUpdate::Log0(IStateUpdateTypes::Log0 {
                data: Bytes::from(vec![0x00, 0x00, 0x6f, 0xee]),
            }),
            StateUpdate::Log1(IStateUpdateTypes::Log1 {
                data: Bytes::from(vec![0x00, 0x00, 0x6f, 0xee]),
                topic1: b256!("fd3dfbb3da06b2710848916c65866a3d0e050047402579a6e1714261137c19c6"),
            }),
        ];

        let encoded = encode_state_updates_to_abi(&state_updates);

        let (types, data) = decode_state_updates_tuple(&encoded)?;
        assert_eq!(types.len(), 3, "Should have 3 state updates");
        assert_eq!(data.len(), 3, "Should have 3 data entries");
        assert_eq!(types[0], U256::from(StateUpdateType::STORE as u8));
        assert_eq!(types[1], U256::from(StateUpdateType::LOG0 as u8));
        assert_eq!(types[2], U256::from(StateUpdateType::LOG1 as u8));

        // Verify the encoding doesn't start with 0x20 (the extra wrapper)
        // The first 32 bytes should be 0x40 (offset to types[]), not 0x20
        if encoded.len() >= 32 {
            let first_word = &encoded[0..32];
            let is_wrapper = {
                let mut expected = [0u8; 32];
                expected[31] = 0x20;
                first_word == expected
            };
            if is_wrapper {
                bail!(
                    "Encoding still has the extra wrapper! First 32 bytes should be the offset to types[] (0x40), not a wrapper (0x20)."
                );
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_csv_writer() -> Result<()> {
        dotenv::dotenv().ok();

        let rpc_url: Url = std::env::var("RPC_URL")
            .expect("RPC_URL must be set")
            .parse()?;
        let provider = ProviderBuilder::new().connect_http(rpc_url.clone());
        let gk = GasKillerDefault::new(rpc_url, None).await?;
        let report = gas_estimate_tx(provider, SIMPLE_ARRAY_ITERATION_TX_HASH, &gk).await?;

        let _ = File::create("test.csv")?;
        let mut writer = Writer::from_path("test.csv")?;

        writer.serialize(report)?;
        writer.flush()?;
        Ok(())
    }

    #[tokio::test]
    async fn test_estimate_state_changes_gas_set() -> Result<()> {
        dotenv::dotenv().ok();

        let rpc_url: Url = std::env::var("RPC_URL")
            .expect("RPC_URL must be set")
            .parse()?;
        let provider = ProviderBuilder::new().connect_http(rpc_url.clone());

        let tx_hash = SIMPLE_STORAGE_SET_TX_HASH;
        let trace = get_tx_trace(&provider, tx_hash).await?;
        let (state_updates, _, _) = compute_state_updates(trace)?;

        let gk = GasKillerDefault::new(rpc_url, None).await?;
        let gas_estimate = gk
            .estimate_state_changes_gas(SIMPLE_STORAGE_ADDRESS, &state_updates)
            .await?;
        assert_eq!(gas_estimate, 32549);
        Ok(())
    }

    #[tokio::test]
    async fn test_estimate_state_changes_gas_access_control() -> Result<()> {
        dotenv::dotenv().ok();

        let rpc_url: Url = std::env::var("RPC_URL")
            .expect("RPC_URL must be set")
            .parse()?;
        let provider = ProviderBuilder::new().connect_http(rpc_url.clone());

        let tx_hash = ACCESS_CONTROL_MAIN_RUN_TX_HASH;
        let trace = get_tx_trace(&provider, tx_hash).await?;
        let (state_updates, _, _) = compute_state_updates(trace)?;

        let gk = GasKillerDefault::new(rpc_url, None).await?;
        let gas_estimate = gk
            .estimate_state_changes_gas(ACCESS_CONTROL_MAIN_ADDRESS, &state_updates)
            .await?;
        assert_eq!(gas_estimate, 37185);
        Ok(())
    }

    #[tokio::test]
    async fn test_estimate_state_changes_gas_access_control_failure() -> Result<()> {
        dotenv::dotenv().ok();

        let rpc_url: Url = std::env::var("RPC_URL")
            .expect("RPC_URL must be set")
            .parse()?;
        let provider = ProviderBuilder::new().connect_http(rpc_url.clone());

        let tx_hash = ACCESS_CONTROL_MAIN_RUN_TX_HASH;
        let trace = get_tx_trace(&provider, tx_hash).await?;
        let (state_updates, _, _) = compute_state_updates(trace)?;

        let gk = GasKillerDefault::new(rpc_url, None).await?;
        let gas_estimate = gk
            .estimate_state_changes_gas(FAKE_ADDRESS, &state_updates)
            .await;
        // Check that the error contains a certain substring
        let error_msg = match gas_estimate {
            Ok(_) => bail!("Expected error, got Ok"),
            Err(e) => e.to_string(),
        };

        // cast sig "RevertingContext(address,bytes)"
        assert!(error_msg.contains("custom error 0xaa86ecee"));

        Ok(())
    }

    #[tokio::test]
    async fn test_compute_state_updates_set() -> Result<()> {
        dotenv::dotenv().ok();

        let rpc_url = std::env::var("RPC_URL")
            .expect("RPC_URL must be set")
            .parse()?;
        let provider = ProviderBuilder::new().connect_http(rpc_url);

        let tx_hash = SIMPLE_STORAGE_SET_TX_HASH;
        let trace = get_tx_trace(&provider, tx_hash).await?;
        let (state_updates, _, _) = compute_state_updates(trace)?;

        assert_eq!(state_updates.len(), 2);
        assert!(matches!(state_updates[0], StateUpdate::Store(_)));
        let StateUpdate::Store(store) = &state_updates[0] else {
            bail!("Expected Store");
        };

        assert_eq!(
            store.slot,
            b256!("0x0000000000000000000000000000000000000000000000000000000000000000")
        );
        assert_eq!(
            store.value,
            b256!("0x0000000000000000000000000000000000000000000000000000000000000001")
        );

        assert!(matches!(state_updates[1], StateUpdate::Log1(_)));
        let StateUpdate::Log1(log) = &state_updates[1] else {
            bail!("Expected Log1");
        };
        assert_eq!(
            log.data,
            bytes!("0x0000000000000000000000000000000000000000000000000000000000000001")
        );
        assert_eq!(
            log.topic1,
            b256!("0x9455957c3b77d1d4ed071e2b469dd77e37fc5dfd3b4d44dc8a997cc97c7b3d49")
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_compute_state_updates_deposit() -> Result<()> {
        dotenv::dotenv().ok();

        let rpc_url = std::env::var("RPC_URL")
            .expect("RPC_URL must be set")
            .parse()?;
        let provider = ProviderBuilder::new().connect_http(rpc_url);

        let tx_hash = SIMPLE_STORAGE_DEPOSIT_TX_HASH;
        let trace = get_tx_trace(&provider, tx_hash).await?;
        let (state_updates, _, _) = compute_state_updates(trace)?;

        assert_eq!(state_updates.len(), 2);
        assert!(matches!(state_updates[0], StateUpdate::Store(_)));
        let StateUpdate::Store(store) = &state_updates[0] else {
            bail!("Expected Store");
        };

        assert_eq!(
            store.slot,
            b256!("0xd39f411965777aebc20f6582612fc3429023e1f0775535ae437442d61471d6fc")
        );
        assert_eq!(
            store.value,
            b256!("0x0000000000000000000000000000000000000000000000000de0b6b3a7640000")
        );

        assert!(matches!(state_updates[1], StateUpdate::Log2(_)));
        let StateUpdate::Log2(log) = &state_updates[1] else {
            bail!("Expected Log2");
        };
        assert_eq!(
            log.data,
            bytes!("0x0000000000000000000000000000000000000000000000000de0b6b3a7640000")
        );
        assert_eq!(
            log.topic1,
            b256!("0x8ad64a0ac7700dd8425ab0499f107cb6e2cd1581d803c5b8c1c79dcb8190b1af")
        );
        assert_eq!(
            log.topic2,
            b256!("0x000000000000000000000000ff467a85932cf543df50255f00a8a829c12a3a11")
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_compute_state_updates_delegatecall() -> Result<()> {
        dotenv::dotenv().ok();

        let rpc_url = std::env::var("RPC_URL")
            .expect("RPC_URL must be set")
            .parse()?;
        let provider = ProviderBuilder::new().connect_http(rpc_url);

        let tx_hash = DELEGATECALL_CONTRACT_MAIN_RUN_TX_HASH;
        let trace = get_tx_trace(&provider, tx_hash).await?;
        let (state_updates, _, _) = compute_state_updates(trace)?;

        assert_eq!(state_updates.len(), 4);
        let StateUpdate::Store(IStateUpdateTypes::Store { slot, value }) = &state_updates[0] else {
            bail!("Expected Store, got {:?}", state_updates[0]);
        };
        assert_eq!(
            *slot,
            b256!("0x0000000000000000000000000000000000000000000000000000000000000003")
        );
        assert_eq!(
            *value,
            b256!("0x0000000000000000000000000000000000000000000000000000000000000001")
        );

        let StateUpdate::Call(IStateUpdateTypes::Call {
            target,
            value,
            callargs,
        }) = &state_updates[1]
        else {
            bail!("Expected Call, got {:?}", state_updates[1]);
        };
        assert_eq!(*target, DELEGATE_CONTRACT_A_ADDRESS);
        assert_eq!(*value, U256::from(0));
        assert_eq!(*callargs, bytes!("0xaea01afc"));

        let StateUpdate::Store(IStateUpdateTypes::Store { slot, value }) = &state_updates[2] else {
            bail!("Expected Store, got {:?}", state_updates[2]);
        };
        assert_eq!(
            *slot,
            b256!("0x0000000000000000000000000000000000000000000000000000000000000002")
        );
        assert_eq!(
            *value,
            b256!("0x0000000000000000000000000000000000000000000000000de0b6b3a7640000")
        ); // 1 ether (use cast to-dec)

        let StateUpdate::Store(IStateUpdateTypes::Store { slot, value }) = &state_updates[3] else {
            bail!("Expected Store, got {:?}", state_updates[3]);
        };
        assert_eq!(
            *slot,
            b256!("0x0000000000000000000000000000000000000000000000000000000000000002")
        );
        assert_eq!(
            *value,
            b256!("0x00000000000000000000000000000000000000000000000029a2241af62c0000")
        ); // 3 ether (use cast to-dec)

        Ok(())
    }

    #[tokio::test]
    async fn test_compute_state_updates_call_external() -> Result<()> {
        dotenv::dotenv().ok();

        let rpc_url = std::env::var("RPC_URL")
            .expect("RPC_URL must be set")
            .parse()?;
        let provider = ProviderBuilder::new().connect_http(rpc_url);

        let tx_hash = SIMPLE_STORAGE_CALL_EXTERNAL_TX_HASH;
        let trace = get_tx_trace(&provider, tx_hash).await?;
        let (state_updates, _, _) = compute_state_updates(trace)?;

        assert_eq!(state_updates.len(), 1);
        assert!(matches!(state_updates[0], StateUpdate::Call(_)));
        let StateUpdate::Call(call) = &state_updates[0] else {
            bail!("Expected Call");
        };

        assert_eq!(
            call.target,
            address!("0x60141225789a7fe3048a289bfaef289f1d7a484e")
        );
        assert_eq!(call.value, U256::from(0));
        assert_eq!(call.callargs, bytes!("0x3a32b549"));

        Ok(())
    }

    #[tokio::test]
    async fn test_compute_state_update_simulate_call() -> Result<()> {
        dotenv::dotenv().ok();

        let rpc_url: Url = std::env::var("RPC_URL")
            .expect("RPC_URL must be set")
            .parse()?;

        let provider = ProviderBuilder::new().connect_http(rpc_url.clone());

        let simple_storage =
            SimpleStorage::SimpleStorageInstance::new(SIMPLE_STORAGE_ADDRESS, &provider);
        let tx_request = simple_storage.set(U256::from(1)).into_transaction_request();

        let trace = get_trace_from_call(rpc_url, tx_request, None).await?;
        let (state_updates, _, _) = compute_state_updates(trace)?;

        assert_eq!(state_updates.len(), 2);
        assert!(matches!(state_updates[0], StateUpdate::Store(_)));
        let StateUpdate::Store(store) = &state_updates[0] else {
            bail!("Expected Store");
        };

        assert_eq!(
            store.slot,
            b256!("0x0000000000000000000000000000000000000000000000000000000000000000")
        );
        assert_eq!(
            store.value,
            b256!("0x0000000000000000000000000000000000000000000000000000000000000001")
        );

        assert!(matches!(state_updates[1], StateUpdate::Log1(_)));
        let StateUpdate::Log1(log) = &state_updates[1] else {
            bail!("Expected Log1");
        };
        assert_eq!(
            log.data,
            bytes!("0x0000000000000000000000000000000000000000000000000000000000000001")
        );
        assert_eq!(
            log.topic1,
            b256!("0x9455957c3b77d1d4ed071e2b469dd77e37fc5dfd3b4d44dc8a997cc97c7b3d49")
        );
        Ok(())
    }
}

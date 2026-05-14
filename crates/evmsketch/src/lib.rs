//! EvmSketch-based gas estimation module.
//!
//! This module provides Anvil-free gas estimation using sp1-contract-call's
//! EvmSketch for simulating StateChangeHandler execution.
//!
//! State updates are extracted using the shared `core::trace` module
//! via `debug_traceTransaction`, and gas estimation is delegated to the
//! `gas-analyzer-estimator` crate which uses revm directly.

use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::providers::ProviderBuilder;
use alloy::rpc::types::eth::TransactionRequest;
use alloy_eips::BlockId;
use alloy_eips::BlockNumberOrTag;
use alloy_genesis::ChainConfig;
use alloy_hardforks::{EthereumChainHardforks, EthereumHardfork, ForkCondition};
use alloy_provider::Provider;
use alloy_provider::RootProvider;
use alloy_provider::ext::DebugApi;
use alloy_provider::network::AnyNetwork;
use anyhow::{Result, anyhow};
use lru::LruCache;
use reth_primitives::EthPrimitives;
use revm::database::CacheDB;
use revm::primitives::hardfork::SpecId;
use sp1_cc_client_executor::{ContractCalldata, ContractInput};
use sp1_cc_host_executor::{EvmSketch, Genesis};
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use url::Url;

/// Ethereum mainnet chain ID.
pub const MAINNET_CHAIN_ID: u64 = 1;
/// Ethereum Sepolia chain ID.
pub const SEPOLIA_CHAIN_ID: u64 = 11_155_111;
/// Gnosis Chain chain ID.
pub const GNOSIS_CHAIN_ID: u64 = 100;

/// Map a chain ID to the corresponding `Genesis` for `EvmSketch` and the
/// matching `EthereumChainHardforks` for spec derivation.
///
/// Unsupported chain IDs return an error rather than silently defaulting to
/// mainnet, which would produce a wrong `SpecId` whenever the active hardfork
/// on the target chain differs from mainnet at the same height/timestamp.
pub fn chain_id_to_genesis_and_spec(chain_id: u64) -> Result<(Genesis, EthereumChainHardforks)> {
    match chain_id {
        MAINNET_CHAIN_ID => Ok((Genesis::Mainnet, EthereumChainHardforks::mainnet())),
        SEPOLIA_CHAIN_ID => Ok((Genesis::Sepolia, EthereumChainHardforks::sepolia())),
        GNOSIS_CHAIN_ID => Ok((Genesis::Custom(gnosis_chain_config()), gnosis_hardforks())),
        other => Err(anyhow!(
            "unsupported chain id {other}: only mainnet ({}), sepolia ({}), and gnosis ({}) are supported",
            MAINNET_CHAIN_ID,
            SEPOLIA_CHAIN_ID,
            GNOSIS_CHAIN_ID,
        )),
    }
}

fn gnosis_chain_config() -> ChainConfig {
    ChainConfig {
        chain_id: GNOSIS_CHAIN_ID,
        homestead_block: Some(0),
        eip150_block: Some(0),
        eip155_block: Some(0),
        eip158_block: Some(0),
        byzantium_block: Some(0),
        constantinople_block: Some(0),
        petersburg_block: Some(0),
        istanbul_block: Some(0),
        berlin_block: Some(16_101_500),
        london_block: Some(19_040_000),
        shanghai_time: Some(1_690_889_660),
        cancun_time: Some(1_710_181_820),
        prague_time: Some(1_746_021_820),
        ..Default::default()
    }
}

fn gnosis_hardforks() -> EthereumChainHardforks {
    // Paris (Merge, Dec-08-2022) is omitted: its TTD-based ForkCondition has no
    // fork_block, so is_paris_active_at_block always returns false. This is
    // harmless because all current Gnosis blocks are post-Shanghai, and
    // spec_by_timestamp_and_block_number checks timestamp forks first.
    EthereumChainHardforks::new([
        (EthereumHardfork::Frontier, ForkCondition::Block(0)),
        (EthereumHardfork::Homestead, ForkCondition::Block(0)),
        (EthereumHardfork::Tangerine, ForkCondition::Block(0)),
        (EthereumHardfork::SpuriousDragon, ForkCondition::Block(0)),
        (EthereumHardfork::Byzantium, ForkCondition::Block(0)),
        (EthereumHardfork::Constantinople, ForkCondition::Block(0)),
        (EthereumHardfork::Petersburg, ForkCondition::Block(0)),
        (EthereumHardfork::Istanbul, ForkCondition::Block(0)),
        (EthereumHardfork::Berlin, ForkCondition::Block(16_101_500)),
        (EthereumHardfork::London, ForkCondition::Block(19_040_000)),
        (
            EthereumHardfork::Shanghai,
            ForkCondition::Timestamp(1_690_889_660),
        ),
        (
            EthereumHardfork::Cancun,
            ForkCondition::Timestamp(1_710_181_820),
        ),
        (
            EthereumHardfork::Prague,
            ForkCondition::Timestamp(1_746_021_820),
        ),
    ])
}

pub mod simple_rpc_db;
use simple_rpc_db::SimpleRpcDb;

use gas_analyzer_core::{
    Opcode, StateUpdate, compute_state_updates, encode_state_updates_to_abi,
    estimate_gas_from_operations, extract_operation_counts_from_trace,
};
use gas_analyzer_estimator::{PrecedingTx, SimEnvOpts};
use gas_analyzer_rpc::get_trace_from_call;

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
// Executor Cache
// ============================================================================

/// Thread-safe LRU cache of pre-built [`DefaultEvmSketchExecutor`]s.
///
/// `build()` costs ~80–120 ms (2× `eth_getBlockByNumber`).  An executor is safe
/// to reuse across requests at the same block height: `sim_env()` reads only
/// immutable header fields, and each gas estimate constructs its own `CacheDB`.
/// Keyed by `(rpc_url, block_number)` — no extra RPC is needed for a cache hit.
///
/// A capacity of 4 covers all realistic burst scenarios on mainnet (~12 s blocks).
pub struct EvmSketchExecutorCache {
    inner: Mutex<LruCache<(String, u64), Arc<DefaultEvmSketchExecutor>>>,
}

impl EvmSketchExecutorCache {
    /// Create a new cache with the given capacity.
    ///
    /// # Panics
    /// Panics if `capacity` is zero.
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(LruCache::new(
                NonZeroUsize::new(capacity).expect("cache capacity must be non-zero"),
            )),
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("executor cache mutex poisoned")
            .len()
    }

    /// Return a cached executor for `(rpc_url, block_number)`, building one if absent.
    ///
    /// Two concurrent callers that both miss may each call `build()`; the later
    /// `put` overwrites the earlier entry.  Both executors are equivalent, so this
    /// is safe — it only wastes one build in the rare cold-start race.
    pub async fn get_or_build(
        &self,
        rpc_url: &str,
        block_number: u64,
    ) -> Result<Arc<DefaultEvmSketchExecutor>> {
        let key = (rpc_url.to_string(), block_number);
        {
            let mut cache = self.inner.lock().expect("executor cache mutex poisoned");
            if let Some(exec) = cache.get(&key) {
                return Ok(Arc::clone(exec));
            }
        }

        let url = Url::parse(rpc_url).map_err(|e| anyhow!("Invalid RPC URL: {}", e))?;
        let exec = Arc::new(
            EvmSketchExecutorBuilder::new()
                .rpc_url(url)
                .at_block(BlockNumberOrTag::Number(block_number))
                .build()
                .await?,
        );
        {
            let mut cache = self.inner.lock().expect("executor cache mutex poisoned");
            cache.put(key, Arc::clone(&exec));
        }
        Ok(exec)
    }
}

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
    /// The chain ID detected from the RPC at build time.
    pub chain_id: u64,
    /// The EVM spec (hardfork) for the anchored block, computed once at build
    /// time from the chain's hardfork schedule and the anchor header.
    pub spec: SpecId,
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
    ///
    /// Queries `eth_chainId` from the RPC and selects the matching `Genesis`
    /// and hardfork schedule. Errors on unsupported chains: silently defaulting
    /// to mainnet when pointed at a different network would yield wrong hardfork
    /// activation and corrupt gas estimation.
    pub async fn build(self) -> Result<DefaultEvmSketchExecutor> {
        let rpc_url = self.rpc_url.ok_or_else(|| anyhow!("RPC URL is required"))?;

        let chain_probe = RootProvider::<AnyNetwork>::new_http(rpc_url.clone());
        let chain_id = chain_probe
            .get_chain_id()
            .await
            .map_err(|e| anyhow!("Failed to query eth_chainId: {}", e))?;
        let (genesis, hardforks) = chain_id_to_genesis_and_spec(chain_id)?;

        let sketch = EvmSketch::builder()
            .at_block(self.block)
            .el_rpc_url(rpc_url)
            .with_genesis(genesis)
            .build()
            .await
            .map_err(|e| anyhow!("Failed to build EvmSketch: {}", e))?;

        let spec = alloy_evm::spec(&hardforks, sketch.anchor.header());

        Ok(EvmSketchExecutor {
            sketch,
            chain_id,
            spec,
        })
    }
}

impl DefaultEvmSketchExecutor {
    /// Estimate gas for executing a set of state updates using pre-built calldata.
    ///
    /// Delegates to the shared gas-analyzer-estimator crate which uses revm directly.
    ///
    /// Storage is read at `block_number - 1` via `SimpleRpcDb`, matching the
    /// other estimation entry points. Anchoring to `block_number` itself
    /// returns post-block state from `eth_getStorageAt` — the analyzed tx
    /// would observe its own writes already applied.
    pub fn estimate_state_changes_gas_raw(
        &self,
        contract_address: Address,
        caller_address: Address,
        calldata: Bytes,
        gas_price: u128,
    ) -> Result<u64> {
        let state_block = self.anchor_block_number().saturating_sub(1);
        let simple_db = SimpleRpcDb::new(self.sketch.provider.clone(), state_block);
        let mut cache_db = CacheDB::new(simple_db);
        let mut sim_env = self.sim_env();
        sim_env.gas_price = gas_price;
        gas_analyzer_estimator::estimate_gas_raw(
            &mut cache_db,
            contract_address,
            caller_address,
            calldata,
            &sim_env,
        )
    }

    /// Estimate gas for a set of state updates at the anchor block.
    ///
    /// Constructs a fresh `CacheDB` backed by `SimpleRpcDb` at `block_number - 1`
    /// (pre-block state) so each call is independent of any prior estimation on
    /// this executor.
    pub fn estimate_state_changes_gas(
        &self,
        contract_address: Address,
        caller_address: Address,
        state_updates: &[gas_analyzer_core::StateUpdate],
    ) -> Result<u64> {
        let state_block = self.anchor_block_number().saturating_sub(1);
        let simple_db = SimpleRpcDb::new(self.sketch.provider.clone(), state_block);
        let mut cache_db = CacheDB::new(simple_db);
        let sim_env = self.sim_env();
        gas_analyzer_estimator::estimate_state_changes_gas(
            &mut cache_db,
            contract_address,
            caller_address,
            state_updates,
            &sim_env,
        )
    }

    /// Build a `SimEnv` from the anchored block header.
    ///
    /// `gas_price` defaults to 0 since it is a transaction-level field;
    /// callers with access to the original transaction can override it.
    /// `basefee` comes from the header (0 for pre-EIP-1559 blocks).
    /// `spec` is the pre-computed `SpecId` stored at build time;
    /// `difficulty` carries the legacy PoW value (zero post-Merge).
    pub fn sim_env(&self) -> SimEnvOpts {
        let header = self.sketch.anchor.header();
        SimEnvOpts {
            number: header.number,
            timestamp: header.timestamp,
            gas_limit: header.gas_limit,
            coinbase: header.beneficiary,
            prevrandao: header.mix_hash,
            gas_price: 0,
            basefee: header.base_fee_per_gas.unwrap_or(0),
            difficulty: header.difficulty,
            spec: self.spec,
            value: U256::ZERO,
        }
    }

    /// Returns the chain ID detected from the RPC at build time.
    pub fn chain_id(&self) -> u64 {
        self.chain_id
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
    #[tracing::instrument(name = "evmsketch.build", skip(self), fields(block_number = %self.block))]
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
    /// Delegates to the shared gas-analyzer-estimator crate using a `SimpleRpcDb`
    /// backed by standard RPC calls (`eth_getStorageAt`, `eth_getBalance`, etc.)
    /// instead of sp1-cc's `BasicRpcDb` which requires `eth_getProof`. This avoids
    /// the "proof window" limitation on Reth and other nodes.
    #[tracing::instrument(name = "evmsketch.estimate", skip_all, fields(block_number = self.executor.anchor_block_number(), state_update_count = state_updates.len()))]
    pub fn estimate_state_changes_gas(
        &self,
        contract_address: Address,
        caller_address: Address,
        state_updates: &[StateUpdate],
    ) -> Result<u64> {
        // Use block_number - 1 (pre-transaction state), matching the Anvil path.
        let state_block = self.executor.anchor_block_number().saturating_sub(1);
        let simple_db = SimpleRpcDb::new(self.executor.sketch.provider.clone(), state_block);
        let mut cache_db = CacheDB::new(simple_db);
        let sim_env = self.executor.sim_env();
        gas_analyzer_estimator::estimate_state_changes_gas(
            &mut cache_db,
            contract_address,
            caller_address,
            state_updates,
            &sim_env,
        )
    }

    /// Estimate gas for state changes, replaying preceding transactions first.
    ///
    /// Creates a single CacheDB, replays all preceding transactions to bring
    /// it to the correct mid-block state, then runs gas estimation on that
    /// state. This ensures the simulation sees the same state the original
    /// transaction executed against.
    ///
    /// If `preceding_txs` is empty (first-in-block), this behaves identically
    /// to `estimate_state_changes_gas`.
    pub fn estimate_state_changes_gas_with_preceding(
        &self,
        contract_address: Address,
        caller_address: Address,
        state_updates: &[StateUpdate],
        preceding_txs: &[PrecedingTx],
        tx_value: U256,
    ) -> Result<u64> {
        // Source storage from block N-1 (pre-block state). Anchoring to
        // `block_number` itself makes RPC reads return state at the *end* of
        // block N — after every tx in that block (including the one we're
        // analyzing) has already been applied. `replay_preceding_transactions`
        // would then re-apply txs `[0..tx_index)` on top of post-block state,
        // compounding the error.
        //
        // Reading from N-1 puts the DB in the correct pre-block state, and
        // the subsequent replay brings it to the right mid-block point.
        //
        // The most visible failure mode is any replayed call whose logic
        // depends on state mutated earlier in the same block — e.g. an
        // EIP-2612 `permit` whose nonce was already consumed by the original
        // tx, causing signature recovery to mismatch and the call to revert
        // with "invalid signature".
        let state_block = self.executor.anchor_block_number().saturating_sub(1);
        let simple_db = SimpleRpcDb::new(self.executor.sketch.provider.clone(), state_block);
        let mut cache_db = CacheDB::new(simple_db);
        let mut sim_env = self.executor.sim_env();

        if !preceding_txs.is_empty() {
            gas_analyzer_estimator::replay_preceding_transactions(
                &mut cache_db,
                preceding_txs,
                &sim_env,
            )?;
        }

        sim_env.value = tx_value;
        gas_analyzer_estimator::estimate_state_changes_gas(
            &mut cache_db,
            contract_address,
            caller_address,
            state_updates,
            &sim_env,
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
        use gas_analyzer_rpc::get_tx_trace;

        let receipt = provider
            .get_transaction_receipt(tx_hash)
            .await?
            .ok_or_else(|| anyhow::anyhow!("could not get receipt for tx {}", tx_hash))?;
        let trace = get_tx_trace(provider, tx_hash, receipt.status()).await?;
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
/// encodes them to ABI, and estimates gas. The executor build step (~80–120 ms,
/// 2× `eth_getBlockByNumber`) is skipped on cache hits.
///
/// Pass a persistent `EvmSketchExecutorCache` shared across requests for the best
/// performance.  For one-shot use (benchmarks, CLI) pass `&EvmSketchExecutorCache::new(1)`.
///
/// # Returns
/// `(storage_updates, gas_estimate, is_heuristic, skipped_opcodes)`
#[tracing::instrument(name = "evmsketch.encode", skip_all, fields(block_number, state_update_count = tracing::field::Empty))]
pub async fn call_to_encoded_state_updates_with_evmsketch(
    cache: &EvmSketchExecutorCache,
    rpc_url: impl AsRef<str>,
    tx_request: TransactionRequest,
    block_number: u64,
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

    let caller_address = tx_request.from.unwrap_or_default();

    let provider = ProviderBuilder::new().connect_http(url);
    let block_id = BlockId::Number(BlockNumberOrTag::Number(block_number));
    let trace = get_trace_from_call(&provider, tx_request, block_id).await?;
    let (state_updates, skipped_opcodes, _call_gas_total) = compute_state_updates(trace)?;
    tracing::Span::current().record("state_update_count", state_updates.len());

    let storage_updates = encode_state_updates_to_abi(&state_updates);

    let executor = cache.get_or_build(rpc_url, block_number).await?;
    let gas_estimate =
        executor.estimate_state_changes_gas(contract_address, caller_address, &state_updates)?;

    Ok((storage_updates, gas_estimate, false, skipped_opcodes))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, bytes};

    /// Two `get_or_build` calls for the same key must return `Arc`s that point to the
    /// same allocation (i.e. the second call is a cache hit, not a new build).
    #[tokio::test]
    #[ignore = "requires RPC_URL env var"]
    async fn test_executor_cache_hit_returns_same_arc() {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
        let cache = EvmSketchExecutorCache::new(4);

        // Fetch the latest block number so we have a concrete number to key on.
        let provider = ProviderBuilder::new().connect_http(Url::parse(&rpc_url).unwrap());
        let block_number = provider.get_block_number().await.unwrap();

        let first = cache.get_or_build(&rpc_url, block_number).await.unwrap();
        let second = cache.get_or_build(&rpc_url, block_number).await.unwrap();

        assert!(
            Arc::ptr_eq(&first, &second),
            "second get_or_build must return the cached Arc, not a newly built executor"
        );
    }

    /// A capacity-1 cache must evict the old entry when a new key is inserted,
    /// so a subsequent lookup of the evicted key misses and rebuilds a fresh Arc.
    #[tokio::test]
    #[ignore = "requires RPC_URL env var"]
    async fn test_executor_cache_lru_eviction() {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
        let cache = EvmSketchExecutorCache::new(1);

        let provider = ProviderBuilder::new().connect_http(Url::parse(&rpc_url).unwrap());
        let block_b = provider.get_block_number().await.unwrap();
        let block_a = block_b.saturating_sub(1);

        // Insert block_a — cache has 1 entry.
        let arc_a1 = cache.get_or_build(&rpc_url, block_a).await.unwrap();
        assert_eq!(cache.len(), 1);

        // Insert block_b — evicts block_a, cache still has 1 entry.
        cache.get_or_build(&rpc_url, block_b).await.unwrap();
        assert_eq!(
            cache.len(),
            1,
            "capacity-1 cache must not grow beyond 1 entry"
        );

        // Re-request block_a — must be a miss, yielding a freshly built Arc.
        let arc_a2 = cache.get_or_build(&rpc_url, block_a).await.unwrap();
        assert!(
            !Arc::ptr_eq(&arc_a1, &arc_a2),
            "block_a Arc must be rebuilt after eviction, not returned from cache"
        );
    }

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

    /// `SimEnvOpts::spec` must reflect the mainnet hardfork active at the
    /// anchored block. Synthesized headers at known mainnet
    /// Berlin/London/Paris/Shanghai/Cancun heights are fed through the
    /// same `alloy_evm::spec(&EthSpec::mainnet(), ...)` call `sim_env()`
    /// uses; an accidental chainspec swap (e.g. to Sepolia, where these
    /// heights differ) would surface as a wrong `SpecId`.
    #[test]
    fn test_sim_env_spec_derivation_against_mainnet() {
        use alloy_consensus::Header;
        use alloy_evm::eth::spec::EthSpec;
        use revm::primitives::hardfork::SpecId;

        let mainnet = EthSpec::mainnet();

        // Mainnet historical fork heights / timestamps. Hardcoded so the
        // test fails loudly if the chainspec is ever swapped (e.g. for
        // Sepolia, where these heights are different).
        let cases: &[(&str, u64, u64, SpecId)] = &[
            ("Berlin", 12_244_000, 0, SpecId::BERLIN),
            ("London", 12_965_000, 0, SpecId::LONDON),
            ("Paris", 15_537_394, 0, SpecId::MERGE),
            ("Shanghai", 17_034_870, 1_681_338_455, SpecId::SHANGHAI),
            ("Cancun", 19_426_587, 1_710_338_135, SpecId::CANCUN),
        ];

        for &(name, number, timestamp, expected) in cases {
            let header = Header {
                number,
                timestamp,
                ..Default::default()
            };
            let actual = alloy_evm::spec(&mainnet, &header);
            assert_eq!(
                actual, expected,
                "{name} header (block {number}, ts {timestamp}) mapped to {actual:?}, \
                 expected {expected:?} — sim_env() may be using the wrong chainspec",
            );
        }

        // A wildly future timestamp should map to the latest known spec
        // (at least Prague — chainspec library may have rolled forward).
        let future = Header {
            number: 50_000_000,
            timestamp: 5_000_000_000,
            ..Default::default()
        };
        let fut_spec = alloy_evm::spec(&mainnet, &future);
        assert!(
            fut_spec >= SpecId::PRAGUE,
            "future block should map to at least Prague, got {:?}",
            fut_spec
        );
    }

    /// `SimpleRpcDb::storage_ref` must issue `eth_getStorageAt` with its
    /// configured `block_number` as the block tag — the gas estimators rely
    /// on this to anchor reads at block N-1 (pre-block state) rather than
    /// at the post-block state of the anchor itself. A recording
    /// `tower::Service` captures the JSON-RPC params so the block tag can
    /// be asserted directly.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_simple_rpc_db_queries_at_configured_block() {
        use alloy_json_rpc::{RequestPacket, Response, ResponsePacket};
        use alloy_provider::RootProvider;
        use alloy_provider::network::AnyNetwork;
        use alloy_rpc_client::RpcClient;
        use alloy_transport::{TransportError, TransportFut, mock::Asserter};
        use revm::database_interface::DatabaseRef;
        use std::sync::{Arc, Mutex};
        use std::task::{Context, Poll};
        use tower::Service;

        use crate::simple_rpc_db::SimpleRpcDb;
        use alloy::primitives::{U256, address};

        /// A tower::Service that records every JSON-RPC request and pulls
        /// canned responses from an `Asserter`.
        #[derive(Clone)]
        struct RecordingTransport {
            asserter: Asserter,
            requests: Arc<Mutex<Vec<(String, String)>>>,
        }

        impl Service<RequestPacket> for RecordingTransport {
            type Response = ResponsePacket;
            type Error = TransportError;
            type Future = TransportFut<'static>;

            fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
                Poll::Ready(Ok(()))
            }

            fn call(&mut self, req: RequestPacket) -> Self::Future {
                let me = self.clone();
                Box::pin(async move {
                    match req {
                        RequestPacket::Single(r) => {
                            let method = r.method().to_string();
                            let params =
                                r.params().map(|p| p.get().to_string()).unwrap_or_default();
                            me.requests.lock().unwrap().push((method, params));
                            let payload = me.asserter.pop_response().expect("response queue empty");
                            Ok(ResponsePacket::Single(Response {
                                id: r.id().clone(),
                                payload,
                            }))
                        }
                        RequestPacket::Batch(_) => unreachable!("batch not used in this test"),
                    }
                })
            }
        }

        let asserter = Asserter::new();
        // `eth_getStorageAt` returns a 32-byte hex value. Push 0x...42.
        asserter.push_success(&format!("0x{:0>64}", "42"));

        let transport = RecordingTransport {
            asserter,
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let recorded = transport.requests.clone();
        let client = RpcClient::new(transport, true);
        let provider: RootProvider<AnyNetwork> = RootProvider::new(client);

        let db = SimpleRpcDb::new(provider, 99);

        // SimpleRpcDb's storage_ref blocks the current thread; spawn_blocking
        // gives it the worker thread it needs under multi_thread runtime.
        let val = tokio::task::spawn_blocking(move || {
            db.storage_ref(
                address!("0x0000000000000000000000000000000000001234"),
                U256::from(7u64),
            )
        })
        .await
        .expect("join")
        .expect("storage_ref");

        assert_eq!(val, U256::from(0x42u64));

        let reqs = recorded.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1, "expected exactly one RPC call");
        assert_eq!(reqs[0].0, "eth_getStorageAt", "wrong RPC method");
        // 99 = 0x63. Block tag is the third positional param.
        assert!(
            reqs[0].1.contains("\"0x63\""),
            "request params {:?} did not carry block tag 0x63 (=99) — \
             SimpleRpcDb is ignoring its configured block_number, which \
             would defeat N-1 anchoring in the gas estimators",
            reqs[0].1
        );
    }

    /// `chain_id_to_genesis_and_spec` must accept mainnet (1), Sepolia
    /// (11_155_111), and Gnosis (100) and reject anything else. Silently
    /// mapping an unknown chain ID to mainnet would let `sim_env()` derive
    /// the wrong `SpecId` for any non-mainnet target.
    #[test]
    fn test_chain_id_to_genesis_and_spec_supported_and_rejected_chains() {
        use alloy_hardforks::{EthereumHardfork, EthereumHardforks, ForkCondition};

        let (mainnet_genesis, mainnet_spec) =
            chain_id_to_genesis_and_spec(MAINNET_CHAIN_ID).expect("mainnet should be supported");
        assert!(matches!(mainnet_genesis, Genesis::Mainnet));
        // Sanity-check: mainnet activates Cancun at 1_710_338_135. If this
        // drifts the upstream chainspec changed.
        assert_eq!(
            mainnet_spec.ethereum_fork_activation(EthereumHardfork::Cancun),
            ForkCondition::Timestamp(1_710_338_135),
        );

        let (sepolia_genesis, sepolia_spec) =
            chain_id_to_genesis_and_spec(SEPOLIA_CHAIN_ID).expect("sepolia should be supported");
        assert!(matches!(sepolia_genesis, Genesis::Sepolia));
        assert_eq!(
            sepolia_spec.ethereum_fork_activation(EthereumHardfork::Cancun),
            ForkCondition::Timestamp(1_706_655_072),
        );

        let (gnosis_genesis, gnosis_spec) =
            chain_id_to_genesis_and_spec(GNOSIS_CHAIN_ID).expect("gnosis should be supported");
        assert!(matches!(gnosis_genesis, Genesis::Custom(_)));
        assert_eq!(
            gnosis_spec.ethereum_fork_activation(EthereumHardfork::Cancun),
            ForkCondition::Timestamp(1_710_181_820),
        );
        assert_eq!(
            gnosis_spec.ethereum_fork_activation(EthereumHardfork::Prague),
            ForkCondition::Timestamp(1_746_021_820),
        );

        // Holesky (17_000) and Anvil (31_337) must error rather than silently
        // pretending to be mainnet — the wrong chainspec produces wrong
        // hardfork activation, which corrupts gas estimation.
        assert!(chain_id_to_genesis_and_spec(17_000).is_err());
        assert!(chain_id_to_genesis_and_spec(31_337).is_err());
        assert!(chain_id_to_genesis_and_spec(0).is_err());
    }

    /// At a header with a timestamp that falls *between* the Sepolia and
    /// mainnet Cancun activation timestamps, `alloy_evm::spec` must return
    /// different `SpecId`s for the two chains. This pins the fact that
    /// chain selection is load-bearing — picking the wrong `EthSpec` here
    /// would silently misclassify Sepolia headers as Shanghai when they
    /// are already Cancun (or vice versa for the symmetric range).
    #[test]
    fn test_sepolia_spec_diverges_from_mainnet() {
        use alloy_consensus::Header;
        use alloy_evm::eth::spec::EthSpec;
        use revm::primitives::hardfork::SpecId;

        // Sepolia Cancun: 1_706_655_072. Mainnet Cancun: 1_710_338_135.
        // Pick a timestamp strictly inside that window.
        const TS_BETWEEN_SEPOLIA_AND_MAINNET_CANCUN: u64 = 1_708_000_000;

        let header = Header {
            // Block number high enough to be post-Shanghai on both networks.
            number: 18_000_000,
            timestamp: TS_BETWEEN_SEPOLIA_AND_MAINNET_CANCUN,
            ..Default::default()
        };

        let mainnet_spec = alloy_evm::spec(&EthSpec::mainnet(), &header);
        let sepolia_spec = alloy_evm::spec(&EthSpec::sepolia(), &header);

        assert_eq!(
            mainnet_spec,
            SpecId::SHANGHAI,
            "mainnet at ts {} should still be Shanghai (Cancun activates at 1_710_338_135)",
            TS_BETWEEN_SEPOLIA_AND_MAINNET_CANCUN,
        );
        assert_eq!(
            sepolia_spec,
            SpecId::CANCUN,
            "sepolia at ts {} should already be Cancun (activated at 1_706_655_072)",
            TS_BETWEEN_SEPOLIA_AND_MAINNET_CANCUN,
        );
        assert_ne!(
            mainnet_spec, sepolia_spec,
            "specs must differ across chains in the inter-activation window — \
             a hardcoded mainnet EthSpec would silently break Sepolia analysis here",
        );
    }

    /// Gnosis Cancun activated at 1_710_181_820 — ~156 s before mainnet
    /// (1_710_338_135). A header timestamped inside that window must resolve
    /// to CANCUN on Gnosis but SHANGHAI on mainnet, proving that
    /// `gnosis_hardforks()` is wired correctly and is not just an alias for
    /// `EthereumChainHardforks::mainnet()`.
    #[test]
    fn test_gnosis_spec_diverges_from_mainnet() {
        use alloy_consensus::Header;
        use revm::primitives::hardfork::SpecId;

        // Strictly between Gnosis Cancun (1_710_181_820) and mainnet Cancun
        // (1_710_338_135).
        const TS_BETWEEN_GNOSIS_AND_MAINNET_CANCUN: u64 = 1_710_260_000;

        let header = Header {
            number: 33_000_000,
            timestamp: TS_BETWEEN_GNOSIS_AND_MAINNET_CANCUN,
            ..Default::default()
        };

        let (_, gnosis_hardforks) =
            chain_id_to_genesis_and_spec(GNOSIS_CHAIN_ID).expect("gnosis supported");
        let (_, mainnet_hardforks) =
            chain_id_to_genesis_and_spec(MAINNET_CHAIN_ID).expect("mainnet supported");

        let gnosis_spec = alloy_evm::spec(&gnosis_hardforks, &header);
        let mainnet_spec = alloy_evm::spec(&mainnet_hardforks, &header);

        assert_eq!(
            gnosis_spec,
            SpecId::CANCUN,
            "gnosis at ts {} should be CANCUN (activated at 1_710_181_820)",
            TS_BETWEEN_GNOSIS_AND_MAINNET_CANCUN,
        );
        assert_eq!(
            mainnet_spec,
            SpecId::SHANGHAI,
            "mainnet at ts {} should still be SHANGHAI (Cancun activates at 1_710_338_135)",
            TS_BETWEEN_GNOSIS_AND_MAINNET_CANCUN,
        );
    }
}

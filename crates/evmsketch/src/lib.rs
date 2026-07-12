//! EvmSketch-based gas estimation module.
//!
//! This module provides Anvil-free gas estimation using sp1-contract-call's
//! EvmSketch for simulating StateChangeHandler execution.
//!
//! State updates are extracted using the shared `core::trace` module
//! via `debug_traceTransaction`, and gas estimation is delegated to the
//! `gas-analyzer-estimator` crate which uses revm directly.

use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::rpc::types::eth::TransactionRequest;
use alloy_eips::BlockId;
use alloy_eips::BlockNumberOrTag;
use alloy_genesis::ChainConfig;
use alloy_hardforks::{EthereumChainHardforks, EthereumHardfork, ForkCondition};
use alloy_provider::Provider;
use alloy_provider::RootProvider;
use alloy_provider::ext::DebugApi;
use alloy_provider::network::{AnyNetwork, Ethereum};
use anyhow::{Context as _, Result, anyhow};
use lru::LruCache;
use reth_primitives::EthPrimitives;
use revm::database::CacheDB;
use revm::primitives::hardfork::SpecId;
use sp1_cc_client_executor::{ContractCalldata, ContractInput};
use sp1_cc_host_executor::{EvmSketch, Genesis};
use std::collections::{HashMap, HashSet};
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

pub mod local_exec;
pub mod overlay_mount;
pub mod simple_rpc_db;
pub use local_exec::{LocalBlockEnv, LocalStateCache};
pub use overlay_mount::{OverlayMount, OverlayStateDb};
use simple_rpc_db::{SimpleRpcDb, prefetch_slots_into_cache};

// Re-exported so downstream consumers (e.g. the Gas Killer service) can name
// the profile without a direct gas-analyzer-core dependency.
pub use gas_analyzer_core::{OverlayEnv, SimProfile};

use gas_analyzer_core::{
    Opcode, PrestateEligibility, StateUpdate, build_state_updates_from_prestate,
    classify_prestate_eligibility, compute_state_updates, encode_state_updates_to_abi,
    estimate_gas_from_operations, extract_operation_counts_from_trace, validate_unbounded_shape,
};
use gas_analyzer_estimator::{PrecedingTx, SimEnvOpts};
use gas_analyzer_rpc::{
    get_call_frame_from_call_with_env, get_prestate_diff_from_call_with_env,
    get_trace_from_call_with_env,
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
// Executor Cache
// ============================================================================

/// Thread-safe LRU cache of pre-built [`DefaultEvmSketchExecutor`]s, with a
/// companion provider cache so the same HTTP connection pool is reused across
/// all `debug_traceCall` requests to the same endpoint.
///
/// An executor cache miss costs ~50–70 ms (1× `eth_getBlockByNumber`) once the
/// chain ID for that URL is known; the very first miss per URL also pays
/// `eth_chainId` (~16–50 ms extra). An executor is safe to reuse across requests
/// at the same block height: `sim_env()` reads only immutable header fields, and
/// each gas estimate constructs its own `CacheDB`. Executors are keyed by
/// `(rpc_url, block_number)`. Trace providers are keyed by `rpc_url` only — a
/// single [`RootProvider`] is shared across all block heights for the same
/// endpoint, avoiding repeated TCP/TLS handshakes.
///
/// Chain ID is static per network: it is fetched once per RPC URL and reused
/// across all block-number entries for that URL, eliminating one `eth_chainId`
/// round-trip (~16–50 ms) on every subsequent executor cache miss.
///
/// A capacity of 4 covers all realistic burst scenarios on mainnet (~12 s blocks).
pub struct EvmSketchExecutorCache {
    inner: Mutex<LruCache<(String, u64), Arc<DefaultEvmSketchExecutor>>>,
    chain_ids: Mutex<HashMap<String, u64>>,
    /// One `RootProvider<Ethereum>` per RPC URL, used for `debug_traceCall`.
    /// `RootProvider` is `Clone` (Arc-backed), so callers share one HTTP
    /// connection pool rather than creating a new one per invocation.
    trace_providers: Mutex<HashMap<String, RootProvider<Ethereum>>>,
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
            chain_ids: Mutex::new(HashMap::new()),
            trace_providers: Mutex::new(HashMap::new()),
        }
    }

    /// Return a cached trace provider for `rpc_url`, creating one if absent.
    ///
    /// The provider is a `RootProvider<Ethereum>` (built via
    /// `ProviderBuilder::new().connect_http`) so it satisfies the
    /// `Provider + DebugApi` bounds required by `get_trace_from_call`.
    /// Subsequent calls for the same URL return a clone of the same underlying
    /// provider — cheap Arc bump, shared connection pool.
    fn get_or_create_trace_provider(&self, rpc_url: &str) -> Result<RootProvider<Ethereum>> {
        {
            let providers = self
                .trace_providers
                .lock()
                .expect("trace_providers mutex poisoned");
            if let Some(p) = providers.get(rpc_url) {
                return Ok(p.clone());
            }
        }
        let url = Url::parse(rpc_url).map_err(|e| anyhow!("Invalid RPC URL: {}", e))?;
        // RootProvider::new_http gives a plain provider without gas/nonce/chain-id
        // fillers, which are unnecessary for debug_traceCall.
        let provider = RootProvider::<Ethereum>::new_http(url);
        {
            let mut providers = self
                .trace_providers
                .lock()
                .expect("trace_providers mutex poisoned");
            providers
                .entry(rpc_url.to_string())
                .or_insert_with(|| provider.clone());
        }
        Ok(provider)
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
    ///
    /// Chain ID is fetched at most once per RPC URL across all block numbers.
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

        // Chain ID is static per network — fetch once and reuse across blocks.
        let chain_id = {
            self.chain_ids
                .lock()
                .expect("chain_id cache mutex poisoned")
                .get(rpc_url)
                .copied()
        };
        let chain_id = match chain_id {
            Some(id) => id,
            None => {
                let provider = RootProvider::<AnyNetwork>::new_http(url.clone());
                let id = provider
                    .get_chain_id()
                    .await
                    .map_err(|e| anyhow!("Failed to query eth_chainId: {}", e))?;
                self.chain_ids
                    .lock()
                    .expect("chain_id cache mutex poisoned")
                    .insert(rpc_url.to_string(), id);
                id
            }
        };

        let exec = Arc::new(
            EvmSketchExecutorBuilder::new()
                .rpc_url(url)
                .with_chain_id(chain_id)
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
    chain_id: Option<u64>,
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

    /// Supply a pre-fetched chain ID, skipping the `eth_chainId` probe in `build`.
    ///
    /// Use this when the chain ID is already known (e.g. cached from a prior
    /// call) to avoid an extra round-trip per executor build.
    pub fn with_chain_id(mut self, chain_id: u64) -> Self {
        self.chain_id = Some(chain_id);
        self
    }

    /// Build the EvmSketchExecutor.
    ///
    /// Queries `eth_chainId` from the RPC if not already supplied via
    /// [`with_chain_id`](Self::with_chain_id), then selects the matching `Genesis`
    /// and hardfork schedule. Errors on unsupported chains: silently defaulting
    /// to mainnet when pointed at a different network would yield wrong hardfork
    /// activation and corrupt gas estimation.
    pub async fn build(self) -> Result<DefaultEvmSketchExecutor> {
        let rpc_url = self.rpc_url.ok_or_else(|| anyhow!("RPC URL is required"))?;

        let chain_id = match self.chain_id {
            Some(id) => id,
            None => RootProvider::<AnyNetwork>::new_http(rpc_url.clone())
                .get_chain_id()
                .await
                .map_err(|e| anyhow!("Failed to query eth_chainId: {}", e))?,
        };
        let (genesis, hardforks) = chain_id_to_genesis_and_spec(chain_id)?;

        let sketch = EvmSketch::builder()
            .at_block(self.block)
            .el_rpc_url(rpc_url)
            .with_genesis(genesis)
            .without_state_root_seed()
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

    /// Estimate gas for a set of state updates, prefetching storage slots first.
    ///
    /// Like `estimate_state_changes_gas` but accepts a map of `address →
    /// storage keys` sourced from the transaction's EIP-2930 access list (or
    /// any other prior knowledge). Before EVM execution begins, each address
    /// in `storage_hints` is fetched via a single `eth_getProof` call that
    /// returns both account metadata and the listed slot values, eliminating
    /// the per-slot `eth_getStorageAt` round-trips for those keys.
    ///
    /// Cold-miss slots not present in `storage_hints` still fall back to
    /// `eth_getStorageAt` during execution — an incomplete hint set is safe.
    pub async fn estimate_state_changes_gas_with_hints(
        &self,
        contract_address: Address,
        caller_address: Address,
        state_updates: &[gas_analyzer_core::StateUpdate],
        storage_hints: &HashMap<Address, Vec<B256>>,
    ) -> Result<u64> {
        let state_block = self.anchor_block_number().saturating_sub(1);
        let simple_db = SimpleRpcDb::new(self.sketch.provider.clone(), state_block);
        let mut cache_db = CacheDB::new(simple_db);

        prefetch_slots_into_cache(&mut cache_db, storage_hints)
            .await
            .context("storage slot prefetch failed")?;

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
            // Prefetch storage slots declared in preceding-tx access lists so
            // that replay_preceding_transactions reads from the in-process cache
            // instead of issuing per-slot eth_getStorageAt calls.
            let mut hints: HashMap<Address, Vec<B256>> = HashMap::new();
            for tx in preceding_txs {
                for item in tx.access_list.iter() {
                    if !item.storage_keys.is_empty() {
                        hints
                            .entry(item.address)
                            .or_default()
                            .extend(item.storage_keys.iter().copied());
                    }
                }
            }

            if !hints.is_empty() {
                let handle = tokio::runtime::Handle::try_current()
                    .context("no tokio runtime for storage prefetch")?;
                tokio::task::block_in_place(|| {
                    handle.block_on(prefetch_slots_into_cache(&mut cache_db, &hints))
                })
                .context("prefetch storage slots for preceding txs")?;
            }

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
    /// `status` must be `receipt.status()` for `tx_hash`. Pass the already-fetched
    /// receipt's status to avoid an extra `eth_getTransactionReceipt` round-trip.
    /// Extracts operations (SSTORE, LOG, CALL) from the trace and applies heuristic costs.
    pub async fn estimate_gas_from_trace<P: Provider + DebugApi>(
        &self,
        provider: &P,
        tx_hash: alloy::primitives::FixedBytes<32>,
        status: bool,
    ) -> Result<u64> {
        use gas_analyzer_rpc::get_tx_trace;

        let trace = get_tx_trace(provider, tx_hash, status).await?;
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

/// Derive prefetch hints from a set of state updates.
///
/// - `Store`: the slot is at `contract_address`; batching all slots into one
///   `eth_getProof` call returns every value in a single round-trip.
/// - `Call`: the target address will be loaded by revm; an empty key list
///   prefetches just account info (balance/nonce/code), eliminating one
///   blocking `basic_ref` call during gas estimation.
/// - `Log*`: carry no addressable state and produce no hints.
fn hints_from_state_updates(
    contract_address: Address,
    state_updates: &[StateUpdate],
) -> HashMap<Address, Vec<B256>> {
    let mut hints: HashMap<Address, Vec<B256>> = HashMap::new();
    // Always prefetch contract_address account info even if there are no Store updates.
    hints.entry(contract_address).or_default();
    for update in state_updates {
        match update {
            StateUpdate::Store(s) => {
                hints.entry(contract_address).or_default().push(s.slot);
            }
            StateUpdate::Call(c) => {
                hints.entry(c.target).or_default();
            }
            _ => {}
        }
    }
    hints
}

/// Compute encoded state updates and gas estimate for a transaction call using EvmSketch.
///
/// Simulates the call via `debug_traceCall` at the given block, extracts state updates,
/// encodes them to ABI, and estimates gas. The executor build step (~50–70 ms,
/// 1× `eth_getBlockByNumber`; plus `eth_chainId` on the first miss per URL) is
/// skipped on cache hits. The HTTP connection pool for
/// `rpc_url` is managed by `cache` and shared across calls — pass a persistent
/// `EvmSketchExecutorCache` for best performance; for one-shot use pass
/// `&EvmSketchExecutorCache::new(1)`.
///
/// # Returns
/// `(storage_updates, gas_estimate, is_heuristic, skipped_opcodes)`
pub async fn call_to_encoded_state_updates_with_evmsketch(
    cache: &EvmSketchExecutorCache,
    rpc_url: impl AsRef<str>,
    tx_request: TransactionRequest,
    block_number: u64,
) -> Result<(Bytes, u64, bool, HashSet<Opcode>)> {
    call_to_encoded_state_updates_with_evmsketch_profiled(
        cache,
        rpc_url,
        tx_request,
        block_number,
        SimProfile::Chain,
    )
    .await
}

/// [`call_to_encoded_state_updates_with_evmsketch`] with an explicit
/// [`SimProfile`] for the tracked-function simulation.
///
/// Under [`SimProfile::UnboundedV1`] the call is simulated with the pinned
/// unbounded gas limits (`gas_analyzer_core::sim_profile`) so arbitrarily
/// heavy compute can execute off-chain, and the extracted updates must pass
/// [`validate_unbounded_shape`] (at most one `Store`, no `CREATE`) — a
/// violation is a hard error, because a payload that writes N slots scales
/// on-chain like a plain contract and defeats the mode's purpose.
///
/// Two things intentionally stay on the real chain's environment:
/// - the **gas estimate** for applying the payload (`verifyAndUpdate` lands in
///   a real block, so it must be priced under real limits);
/// - any `Call` ops inside the payload (they re-execute on-chain at real gas).
///
/// The node serving `debug_traceCall` must have its execution cap lifted
/// (`anvil --disable-block-gas-limit`, `geth --rpc.gascap=0`); otherwise
/// heavy calls OOG inside the tracer and extraction fails.
#[tracing::instrument(name = "evmsketch.encode", skip_all, fields(block_number, profile = ?profile, state_update_count = tracing::field::Empty))]
pub async fn call_to_encoded_state_updates_with_evmsketch_profiled(
    cache: &EvmSketchExecutorCache,
    rpc_url: impl AsRef<str>,
    tx_request: TransactionRequest,
    block_number: u64,
    profile: SimProfile,
) -> Result<(Bytes, u64, bool, HashSet<Opcode>)> {
    call_to_encoded_state_updates_with_evmsketch_env(
        cache,
        rpc_url,
        tx_request,
        block_number,
        profile,
        None,
    )
    .await
}

/// [`call_to_encoded_state_updates_with_evmsketch_profiled`] additionally
/// mounting a pinned [`OverlayEnv`] (verified against the consumer's
/// manifest) as code state-overrides for the tracked-function simulation —
/// the `UNBOUNDED_V2` overlay mode (`docs/UNBOUNDED_OVERLAYS.md`). `None` is
/// identical to the profiled function.
///
/// The overlay participates in simulation only; the payload gas estimate
/// stays on the real chain env, and overlaid chunks are STOP-prefixed pure
/// data, so they can never appear as payload `Store`/`Create` targets.
#[tracing::instrument(name = "evmsketch.encode_env", skip_all, fields(block_number, profile = ?profile, overlay = overlay.is_some(), state_update_count = tracing::field::Empty))]
pub async fn call_to_encoded_state_updates_with_evmsketch_env(
    cache: &EvmSketchExecutorCache,
    rpc_url: impl AsRef<str>,
    tx_request: TransactionRequest,
    block_number: u64,
    profile: SimProfile,
    overlay: Option<&OverlayEnv>,
) -> Result<(Bytes, u64, bool, HashSet<Opcode>)> {
    let rpc_url = rpc_url.as_ref();
    let provider = cache.get_or_create_trace_provider(rpc_url)?;

    let contract_address = tx_request
        .to
        .and_then(|t| match t {
            TxKind::Call(addr) => Some(addr),
            TxKind::Create => None,
        })
        .ok_or_else(|| anyhow!("Transaction must have a 'to' address"))?;

    let caller_address = tx_request.from.unwrap_or_default();

    // Collect storage hints from the EIP-2930 access list before tx_request
    // is consumed by get_trace_from_call. Address-only entries (no storage keys)
    // are included with an empty slot list so their account info is prefetched.
    let mut storage_hints: HashMap<Address, Vec<B256>> = HashMap::new();
    if let Some(al) = &tx_request.access_list {
        for item in al.iter() {
            storage_hints
                .entry(item.address)
                .or_default()
                .extend(item.storage_keys.iter().copied());
        }
    }

    let block_id = BlockId::Number(BlockNumberOrTag::Number(block_number));

    // Extract the state updates (hybrid prestate/struct-log path) and build the executor
    // concurrently — they are independent, so on a cache miss this hides the executor build behind
    // the trace fetch.
    let ((state_updates, skipped_opcodes), executor) = tokio::try_join!(
        extract_state_updates_hybrid(
            &provider,
            tx_request,
            block_id,
            contract_address,
            profile,
            overlay
        ),
        cache.get_or_build(rpc_url, block_number),
    )?;
    tracing::Span::current().record("state_update_count", state_updates.len());

    // The unbounded profile's whole bargain: compute may be unbounded, the
    // on-chain payload may not. Enforce it before signing/estimating anything.
    if profile.requires_unbounded_shape() {
        let shape = validate_unbounded_shape(&state_updates).map_err(|violation| {
            anyhow!(
                "unbounded-profile payload shape violation for consumer {contract_address}: {violation}"
            )
        })?;
        tracing::debug!(
            stores = shape.stores,
            calls = shape.calls,
            logs = shape.logs,
            "unbounded payload shape OK"
        );
    }

    let storage_updates = encode_state_updates_to_abi(&state_updates);

    // Merge access-list hints with slots derived from state_updates.
    // Store slots all land at contract_address (one eth_getProof call for all of them).
    // Call targets get account-info-only prefetch (empty key list).
    let mut all_hints = hints_from_state_updates(contract_address, &state_updates);
    for (addr, slots) in storage_hints {
        all_hints.entry(addr).or_default().extend(slots);
    }

    let gas_estimate = executor
        .estimate_state_changes_gas_with_hints(
            contract_address,
            caller_address,
            &state_updates,
            &all_hints,
        )
        .await?;

    Ok((storage_updates, gas_estimate, false, skipped_opcodes))
}

// ============================================================================
// call_to_encoded_state_updates_local (gas-analyzer#169)
// ============================================================================

/// Which engine executes the tracked call: the historical `debug_traceCall`
/// path, or the in-process local path (issue #169).
///
/// Mirrors [`SimProfile`]'s role as a versioned, string-nameable selector so
/// callers like gas-killer/service can flip executors per-deployment (e.g.
/// `GK_SIM_EXECUTOR=rpc|local`) without a direct dependency on this crate's
/// internal types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SimExecutor {
    /// `debug_traceCall` against the configured RPC — the historical path.
    /// Required for overlay-mode consumers served through a shared anvil
    /// fork (`stateOverrides`), and the only path available on RPCs without
    /// in-process access to blob artifacts.
    #[default]
    Rpc,
    /// In-process revm execution (this module): the RPC becomes a pure state
    /// backend, and overlays mount natively from RAM or mmapped files with
    /// zero request-body serialization.
    Local,
}

impl SimExecutor {
    /// Parse the profile-style string form (`"rpc"` / `"local"`, ASCII
    /// case-insensitive). Unknown values are an error rather than a silent
    /// default — a typo'd `GK_SIM_EXECUTOR` should fail loudly, not silently
    /// fall back to the slower/centralized path.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "rpc" => Ok(SimExecutor::Rpc),
            "local" => Ok(SimExecutor::Local),
            other => Err(anyhow!(
                "unknown executor {other:?}: expected \"rpc\" or \"local\""
            )),
        }
    }

    /// The canonical string form, inverse of [`SimExecutor::parse`].
    pub fn as_str(&self) -> &'static str {
        match self {
            SimExecutor::Rpc => "rpc",
            SimExecutor::Local => "local",
        }
    }
}

impl std::str::FromStr for SimExecutor {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

impl std::fmt::Display for SimExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// True local execution (issue #169): the tracked call is re-executed
/// **in-process** with [`local_exec`] instead of delegating to
/// `debug_traceCall`. The RPC at `rpc_url` is used only as a lazy state
/// backend (accounts/storage/code fetched on first touch through a
/// block-scoped [`foundry_fork_db::SharedBackend`], shared via `state_cache`
/// across concurrent traces of the same block) — never as the executor.
///
/// Mirrors [`call_to_encoded_state_updates_with_evmsketch_env`]'s signature
/// and return type exactly, so callers can select between the two paths
/// (e.g. via [`SimExecutor`]) without touching downstream code:
/// `(storage_updates, gas_estimate, is_heuristic, skipped_opcodes)`.
///
/// `overlay` mounts natively (in-RAM [`OverlayEnv`] or an mmapped
/// [`OverlayMount`] built via [`OverlayMount::from_files`] for
/// multi-gigabyte artifacts) — no `stateOverrides` JSON is ever produced,
/// which is what makes 35 GB models servable at all (see module docs on
/// `local_exec` and `overlay_mount`).
///
/// The gas estimate for applying the resulting payload still executes
/// against the real chain env via the shared [`EvmSketchExecutorCache`] —
/// unbounded compute stays off-chain, the payload's on-chain cost stays
/// priced under real limits, exactly like the RPC path.
#[tracing::instrument(
    name = "evmsketch.encode_local",
    skip_all,
    fields(block_number, profile = ?profile, overlay = overlay.is_some(), state_update_count = tracing::field::Empty)
)]
pub async fn call_to_encoded_state_updates_local(
    executor_cache: &EvmSketchExecutorCache,
    state_cache: &LocalStateCache,
    rpc_url: impl AsRef<str>,
    tx_request: TransactionRequest,
    block_number: u64,
    profile: SimProfile,
    overlay: Option<&OverlayEnv>,
) -> Result<(Bytes, u64, bool, HashSet<Opcode>)> {
    let rpc_url = rpc_url.as_ref();

    let contract_address = tx_request
        .to
        .and_then(|t| match t {
            TxKind::Call(addr) => Some(addr),
            TxKind::Create => None,
        })
        .ok_or_else(|| anyhow!("Transaction must have a 'to' address"))?;
    let caller_address = tx_request.from.unwrap_or_default();

    let mut storage_hints: HashMap<Address, Vec<B256>> = HashMap::new();
    if let Some(al) = &tx_request.access_list {
        for item in al.iter() {
            storage_hints
                .entry(item.address)
                .or_default()
                .extend(item.storage_keys.iter().copied());
        }
    }

    let local_tx = local_exec::LocalTxRequest::from_request(&tx_request)?;

    // The executor (built against the same rpc_url/block) supplies both the
    // pinned block env for the trace and the RootProvider<AnyNetwork> the
    // remote-state backend fetches through, plus the gas estimate afterward.
    let executor = executor_cache.get_or_build(rpc_url, block_number).await?;
    let block_env = LocalBlockEnv::from_executor(&executor);
    let backend = state_cache.backend_for(rpc_url, block_number, executor.sketch.provider.clone());
    let overlay_mount = overlay
        .map(|env| state_cache.overlay_mount_for(env))
        .transpose()?;

    let (state_updates, skipped_opcodes) = local_exec::extract_state_updates_local(
        backend,
        overlay_mount,
        block_env,
        local_tx,
        profile,
        contract_address,
    )
    .await?;
    tracing::Span::current().record("state_update_count", state_updates.len());

    if profile.requires_unbounded_shape() {
        let shape = validate_unbounded_shape(&state_updates).map_err(|violation| {
            anyhow!(
                "unbounded-profile payload shape violation for consumer {contract_address}: {violation}"
            )
        })?;
        tracing::debug!(
            stores = shape.stores,
            calls = shape.calls,
            logs = shape.logs,
            "unbounded payload shape OK (local executor)"
        );
    }

    let storage_updates = encode_state_updates_to_abi(&state_updates);

    let mut all_hints = hints_from_state_updates(contract_address, &state_updates);
    for (addr, slots) in storage_hints {
        all_hints.entry(addr).or_default().extend(slots);
    }

    let gas_estimate = executor
        .estimate_state_changes_gas_with_hints(
            contract_address,
            caller_address,
            &state_updates,
            &all_hints,
        )
        .await?;

    Ok((storage_updates, gas_estimate, false, skipped_opcodes))
}

/// Extract `(state_updates, skipped_opcodes)` for a simulated call, preferring the cheap prestate fast
/// path and falling back to the struct-log path when it can't be used.
///
/// The fast path (`prestateTracer` diff + `callTracer` logs, `O(changed slots)`) is what lets
/// heavy-compute tracked functions — whose struct-log trace times out the node — be extracted at all.
/// It is taken only when [`classify_prestate_eligibility`] proves it sound (no revert of the top-level
/// call or a transparent DELEGATECALL/CALLCODE frame — a reverted STATICCALL is read-only and stays
/// eligible — no cross-contract storage, no regular CALL / CREATE / SELFDESTRUCT); otherwise, and on any tracer error (e.g. a node that does
/// not support these tracers), we fall back to `get_trace_from_call` + `compute_state_updates`, which is
/// the previous behaviour. The fallback owns `tx_request`, so the fast path only borrows it.
async fn extract_state_updates_hybrid<P: Provider + DebugApi>(
    provider: &P,
    tx_request: TransactionRequest,
    block: BlockId,
    consumer: Address,
    profile: SimProfile,
    overlay: Option<&OverlayEnv>,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>)> {
    if let Ok(Some(updates)) =
        try_prestate_fast_path(provider, &tx_request, block, consumer, profile, overlay).await
    {
        return Ok((updates, HashSet::new()));
    }
    let trace = get_trace_from_call_with_env(provider, tx_request, block, profile, overlay).await?;
    let (state_updates, skipped_opcodes, _call_gas_total) = compute_state_updates(trace)?;
    Ok((state_updates, skipped_opcodes))
}

/// Try the prestate fast path: `Ok(Some(updates))` when eligible, `Ok(None)` when the call needs the
/// struct-log fallback, `Err` when the prestate/call tracers are unavailable or fail (also a fallback
/// signal). Never returns an unsound diff.
async fn try_prestate_fast_path<P: Provider + DebugApi>(
    provider: &P,
    tx_request: &TransactionRequest,
    block: BlockId,
    consumer: Address,
    profile: SimProfile,
    overlay: Option<&OverlayEnv>,
) -> Result<Option<Vec<StateUpdate>>> {
    // The two tracer calls are independent — run them concurrently so the fast path costs one
    // round-trip, not two.
    let (diff, frame) = tokio::try_join!(
        get_prestate_diff_from_call_with_env(provider, tx_request.clone(), block, profile, overlay),
        get_call_frame_from_call_with_env(provider, tx_request.clone(), block, profile, overlay),
    )?;
    match classify_prestate_eligibility(&frame, &diff, consumer) {
        PrestateEligibility::Eligible => Ok(Some(build_state_updates_from_prestate(
            consumer, &diff, &frame,
        ))),
        PrestateEligibility::Fallback(reason) => {
            tracing::debug!(reason = %reason, "prestate fast path not eligible; using struct-log path");
            Ok(None)
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, bytes};
    use alloy::providers::ProviderBuilder;
    use gas_analyzer_core::types::IStateUpdateTypes;
    use gas_analyzer_rpc::{
        get_call_frame_from_call, get_prestate_diff_from_call, get_trace_from_call,
    };

    /// `with_chain_id` must store the supplied value so `build` can bypass the
    /// `eth_chainId` probe.
    #[test]
    fn test_builder_with_chain_id_stores_value() {
        let builder = EvmSketchExecutorBuilder::new().with_chain_id(SEPOLIA_CHAIN_ID);
        assert_eq!(builder.chain_id, Some(SEPOLIA_CHAIN_ID));

        let builder_none = EvmSketchExecutorBuilder::new();
        assert_eq!(builder_none.chain_id, None);
    }

    /// `EvmSketchExecutorCache` must issue `eth_chainId` at most once per RPC
    /// URL regardless of how many distinct block numbers are requested.
    ///
    /// Verified by building executors for two different block numbers on the
    /// same URL and checking that the second build finds a cached chain_id.
    /// (The direct assertion is on the internal `chain_ids` map having exactly
    /// one entry after both builds.)
    #[tokio::test]
    #[ignore = "requires RPC_URL env var"]
    async fn test_chain_id_cached_across_blocks() {
        let rpc_url = std::env::var("RPC_URL").expect("RPC_URL must be set");
        let cache = EvmSketchExecutorCache::new(4);

        let provider = ProviderBuilder::new().connect_http(Url::parse(&rpc_url).unwrap());
        let latest = provider.get_block_number().await.unwrap();

        // Build for two consecutive blocks — chain_id must be fetched only once.
        let _ = cache.get_or_build(&rpc_url, latest).await.unwrap();
        let _ = cache
            .get_or_build(&rpc_url, latest.saturating_sub(1))
            .await
            .unwrap();

        let ids = cache
            .chain_ids
            .lock()
            .expect("chain_id cache mutex poisoned");
        assert_eq!(
            ids.len(),
            1,
            "expected exactly one chain_id cache entry for the URL, got {}",
            ids.len()
        );
        assert!(
            ids.contains_key(&rpc_url),
            "chain_id not cached under the expected key"
        );
    }

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

    /// `hints_from_state_updates` must always include contract_address (even
    /// with no Store updates), map Store slots to it, add Call targets with
    /// empty slot lists, and produce no entry for logs.
    #[test]
    fn test_hints_from_state_updates() {
        use alloy::primitives::Bytes;

        let contract = address!("0x000000000000000000000000000000000000DEAD");
        let call_target = address!("0x000000000000000000000000000000000000BEEF");
        let slot1 = B256::from(U256::from(1u64));
        let slot2 = B256::from(U256::from(2u64));

        let updates = vec![
            StateUpdate::Store(IStateUpdateTypes::Store {
                slot: slot1,
                value: B256::ZERO,
            }),
            StateUpdate::Store(IStateUpdateTypes::Store {
                slot: slot2,
                value: B256::ZERO,
            }),
            StateUpdate::Call(IStateUpdateTypes::Call {
                target: call_target,
                value: U256::ZERO,
                callargs: Bytes::new(),
            }),
            StateUpdate::Log0(IStateUpdateTypes::Log0 { data: Bytes::new() }),
        ];

        let hints = hints_from_state_updates(contract, &updates);

        // Both Store slots map to contract_address.
        assert_eq!(hints.get(&contract), Some(&vec![slot1, slot2]));

        // Call target is present with an empty slot list (account-only prefetch).
        assert_eq!(hints.get(&call_target), Some(&vec![]));

        // Log produces no entry — only contract and call_target.
        assert_eq!(hints.len(), 2);
    }

    /// contract_address must be prefetched even when there are no Store updates.
    #[test]
    fn test_hints_from_state_updates_contract_always_present() {
        use alloy::primitives::Bytes;

        let contract = address!("0x000000000000000000000000000000000000DEAD");
        let updates = vec![StateUpdate::Log0(IStateUpdateTypes::Log0 {
            data: Bytes::new(),
        })];

        let hints = hints_from_state_updates(contract, &updates);

        assert_eq!(hints.get(&contract), Some(&vec![]));
        assert_eq!(hints.len(), 1);
    }

    // ========================================================================
    // Hybrid prestate/struct-log extraction — anvil integration tests
    //
    // These spawn a local `anvil` (installed in CI via foundry-toolchain) and
    // exercise the REAL tracers end-to-end: the prestate fast path and the
    // struct-log path are run against the same deployed bytecode and their
    // outputs compared. Contracts are hand-assembled runtime bytecode set via
    // `anvil_setCode` — no solc/forge build step.
    // ========================================================================

    /// A local anvil instance on an OS-assigned free port, killed on drop.
    struct LocalAnvil {
        child: std::process::Child,
        url: String,
    }

    impl LocalAnvil {
        async fn spawn() -> LocalAnvil {
            Self::spawn_with(&[]).await
        }

        /// Spawn with extra anvil flags — e.g. `--disable-block-gas-limit`,
        /// which the unbounded-profile tests need so `debug_traceCall` honors
        /// the lifted tx gas limit instead of clamping to the 30M default.
        async fn spawn_with(extra_args: &[&str]) -> LocalAnvil {
            // Grab a free port, release it, and hand it to anvil. The tiny
            // reuse window is acceptable for tests.
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("bind to pick a free port")
                .local_addr()
                .expect("local_addr")
                .port();
            let child = std::process::Command::new("anvil")
                .args(["--port", &port.to_string(), "--silent"])
                .args(extra_args)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect(
                    "failed to spawn `anvil` — these tests need foundry \
                     (https://getfoundry.sh) on PATH; CI installs it via foundry-toolchain",
                );
            let anvil = LocalAnvil {
                child,
                url: format!("http://127.0.0.1:{port}"),
            };
            let provider = anvil.provider();
            let mut last_err = None;
            for _ in 0..100 {
                match provider.get_chain_id().await {
                    Ok(_) => return anvil,
                    // Yield to the runtime instead of blocking the executor thread.
                    Err(e) => last_err = Some(e),
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            // stdout/stderr are suppressed, so surface the last probe error.
            panic!("anvil did not become ready within 5s; last error: {last_err:?}");
        }

        fn provider(&self) -> RootProvider<Ethereum> {
            RootProvider::<Ethereum>::new_http(Url::parse(&self.url).expect("valid url"))
        }
    }

    impl Drop for LocalAnvil {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    async fn set_code(provider: &RootProvider<Ethereum>, addr: Address, code: Bytes) {
        let _: serde_json::Value = provider
            .raw_request("anvil_setCode".into(), (addr, code))
            .await
            .expect("anvil_setCode");
    }

    async fn set_storage(
        provider: &RootProvider<Ethereum>,
        addr: Address,
        slot: B256,
        value: B256,
    ) {
        let _: serde_json::Value = provider
            .raw_request("anvil_setStorageAt".into(), (addr, slot, value))
            .await
            .expect("anvil_setStorageAt");
    }

    /// A `debug_traceCall` request from anvil's first funded account.
    fn call_request(to: Address) -> TransactionRequest {
        TransactionRequest::default()
            .from(address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"))
            .to(to)
            .gas_limit(3_000_000)
    }

    /// 32-byte topic with the given last byte, as hex for a PUSH32 immediate.
    fn topic_hex(last: u8) -> String {
        format!("{}{last:02x}", "00".repeat(31))
    }

    fn hex_code(spaced_hex: &str) -> Bytes {
        Bytes::from(alloy::hex::decode(spaced_hex.replace(' ', "")).expect("valid hex"))
    }

    fn addr_hex(a: Address) -> String {
        alloy::hex::encode(a.as_slice())
    }

    /// SSTORE(1,0x42); MSTORE(0,0xab); LOG1(mem[0..32], topic 0xaa);
    /// SSTORE(2,5); SSTORE(2,0) — net zero; SSTORE(5,0) — zeroes a pre-seeded slot; STOP.
    fn eligible_code() -> Bytes {
        hex_code(&format!(
            "6042600155 60ab600052 7f{}60206000a1 6005600255 6000600255 6000600555 00",
            topic_hex(0xaa),
        ))
    }

    /// LOG1(topic 0xaa); DELEGATECALL(lib); POP; LOG1(topic 0xbb); STOP.
    /// True emission order with the lib's log is aa, cc, bb — a naive DFS
    /// over the call tree would give aa, bb, cc.
    fn delegate_root_code(lib: Address) -> Bytes {
        hex_code(&format!(
            "7f{}60006000a1 6000600060006000 73{} 5af450 7f{}60006000a1 00",
            topic_hex(0xaa),
            addr_hex(lib),
            topic_hex(0xbb),
        ))
    }

    /// LOG1(topic 0xcc); SSTORE(3,7); STOP — runs in the root's storage context.
    fn delegate_lib_code() -> Bytes {
        hex_code(&format!("7f{}60006000a1 6007600355 00", topic_hex(0xcc)))
    }

    /// CALL(callee, no args, no value); POP; SSTORE(4,1); STOP.
    fn caller_code(callee: Address) -> Bytes {
        hex_code(&format!(
            "60006000600060006000 73{} 5af150 6001600455 00",
            addr_hex(callee),
        ))
    }

    /// SSTORE(1,9); STOP — the callee's own storage, reproduced only by CALL replay.
    fn callee_code() -> Bytes {
        hex_code("6009600155 00")
    }

    /// SSTORE(1,0x42); LOG1(topic 0xdd, no data); REVERT(0,0).
    fn reverter_code() -> Bytes {
        hex_code(&format!(
            "6042600155 7f{}60006000a1 60006000fd",
            topic_hex(0xdd)
        ))
    }

    /// DELEGATECALL(reverter); POP — swallow the failure; SSTORE(2,0x22); STOP.
    fn catcher_code(lib: Address) -> Bytes {
        hex_code(&format!(
            "6000600060006000 73{} 5af450 6022600255 00",
            addr_hex(lib),
        ))
    }

    fn store_up(slot: u8, value: u8) -> StateUpdate {
        StateUpdate::Store(IStateUpdateTypes::Store {
            slot: B256::with_last_byte(slot),
            value: B256::with_last_byte(value),
        })
    }

    fn log1_up(topic: u8, data: Bytes) -> StateUpdate {
        StateUpdate::Log1(IStateUpdateTypes::Log1 {
            data,
            topic1: B256::with_last_byte(topic),
        })
    }

    /// Compare update lists by their ABI encoding — the equality that matters,
    /// since the encoded bytes are what ships to `verifyAndUpdate`.
    fn assert_updates_eq(actual: &[StateUpdate], expected: &[StateUpdate], ctx: &str) {
        assert_eq!(
            encode_state_updates_to_abi(actual),
            encode_state_updates_to_abi(expected),
            "{ctx}:\n  actual:   {actual:?}\n  expected: {expected:?}"
        );
    }

    /// Classify a call the way `try_prestate_fast_path` does, using the real tracers.
    async fn classify_call(provider: &RootProvider<Ethereum>, to: Address) -> PrestateEligibility {
        let (diff, frame) = tokio::try_join!(
            get_prestate_diff_from_call(provider, call_request(to), BlockId::latest()),
            get_call_frame_from_call(provider, call_request(to), BlockId::latest()),
        )
        .expect("prestate/callTracer debug_traceCall failed");
        classify_prestate_eligibility(&frame, &diff, to)
    }

    /// Run BOTH extraction paths against the same deployed call.
    /// Returns (hybrid result, struct-log-only result).
    async fn extract_both(
        provider: &RootProvider<Ethereum>,
        to: Address,
    ) -> ((Vec<StateUpdate>, HashSet<Opcode>), Vec<StateUpdate>) {
        let hybrid = extract_state_updates_hybrid(
            provider,
            call_request(to),
            BlockId::latest(),
            to,
            SimProfile::Chain,
            None,
        )
        .await
        .expect("hybrid extraction failed");
        let trace = get_trace_from_call(provider, call_request(to), BlockId::latest())
            .await
            .expect("struct-log debug_traceCall failed");
        let (structlog, _, _) = compute_state_updates(trace).expect("compute_state_updates failed");
        (hybrid, structlog)
    }

    /// Eligible call: the fast path must produce the same final storage and the
    /// same event stream as the struct-log path — net-deduplicated (the write-
    /// then-restore of slot 2 disappears, intermediate values collapse) with
    /// STOREs slot-sorted ahead of logs.
    #[tokio::test]
    async fn test_fast_path_matches_struct_log_on_eligible_call() {
        let anvil = LocalAnvil::spawn().await;
        let provider = anvil.provider();
        let consumer = address!("0x0000000000000000000000000000000000001001");
        set_code(&provider, consumer, eligible_code()).await;
        // Pre-seed slot 5 so the contract's SSTORE(5,0) is a real zeroing write.
        set_storage(
            &provider,
            consumer,
            B256::with_last_byte(5),
            B256::with_last_byte(0x99),
        )
        .await;

        assert!(
            matches!(
                classify_call(&provider, consumer).await,
                PrestateEligibility::Eligible
            ),
            "self-contained SSTORE+LOG call must be prestate-eligible"
        );

        let ((fast, skipped), structlog) = extract_both(&provider, consumer).await;
        assert!(skipped.is_empty(), "fast path reports no skipped opcodes");

        let word_ab = Bytes::copy_from_slice(B256::with_last_byte(0xab).as_slice());
        assert_updates_eq(
            &fast,
            &[
                store_up(1, 0x42),
                store_up(5, 0),
                log1_up(0xaa, word_ab.clone()),
            ],
            "fast path: net stores (slot-sorted, net-zero slot 2 omitted, zeroing kept), then logs",
        );
        assert_updates_eq(
            &structlog,
            &[
                store_up(1, 0x42),
                log1_up(0xaa, word_ab),
                store_up(2, 5),
                store_up(2, 0),
                store_up(5, 0),
            ],
            "struct-log path: every write in execution order",
        );
    }

    /// Logs emitted around a DELEGATECALL must come out in true emission order
    /// (root log, lib log, root log) on BOTH paths — this is the `position`/
    /// `index` interleaving working against a real tracer, and the delegatecall
    /// SSTORE must land on the root's storage.
    #[tokio::test]
    async fn test_delegatecall_log_ordering_matches_between_paths() {
        let anvil = LocalAnvil::spawn().await;
        let provider = anvil.provider();
        let root = address!("0x0000000000000000000000000000000000001002");
        let lib = address!("0x0000000000000000000000000000000000001003");
        set_code(&provider, root, delegate_root_code(lib)).await;
        set_code(&provider, lib, delegate_lib_code()).await;

        assert!(
            matches!(
                classify_call(&provider, root).await,
                PrestateEligibility::Eligible
            ),
            "delegatecall-only call must be prestate-eligible"
        );

        let ((fast, _), structlog) = extract_both(&provider, root).await;
        assert_updates_eq(
            &fast,
            &[
                store_up(3, 7),
                log1_up(0xaa, Bytes::new()),
                log1_up(0xcc, Bytes::new()),
                log1_up(0xbb, Bytes::new()),
            ],
            "fast path: delegatecall store on root, logs interleaved aa, cc, bb",
        );

        let logs_only = |updates: &[StateUpdate]| -> Vec<String> {
            updates
                .iter()
                .filter(|u| matches!(u, StateUpdate::Log1(_)))
                .map(|u| format!("{u:?}"))
                .collect()
        };
        assert_eq!(
            logs_only(&fast),
            logs_only(&structlog),
            "event order must match the struct-log path's true emission order"
        );
    }

    /// A regular CALL at target depth is not representable by a net diff: the
    /// dispatcher must fall back and return exactly what the struct-log path
    /// returns (a replayable CALL op + the consumer's own store; the callee's
    /// internals excluded).
    #[tokio::test]
    async fn test_hybrid_falls_back_on_regular_call() {
        let anvil = LocalAnvil::spawn().await;
        let provider = anvil.provider();
        let caller = address!("0x0000000000000000000000000000000000001004");
        let callee = address!("0x0000000000000000000000000000000000001005");
        set_code(&provider, caller, caller_code(callee)).await;
        set_code(&provider, callee, callee_code()).await;

        assert!(
            matches!(
                classify_call(&provider, caller).await,
                PrestateEligibility::Fallback(_)
            ),
            "regular CALL must force the struct-log fallback"
        );

        let ((hybrid, _), structlog) = extract_both(&provider, caller).await;
        assert_updates_eq(
            &hybrid,
            &structlog,
            "hybrid must equal the struct-log path on fallback",
        );
        assert_updates_eq(
            &hybrid,
            &[
                StateUpdate::Call(IStateUpdateTypes::Call {
                    target: callee,
                    value: U256::ZERO,
                    callargs: Bytes::new(),
                }),
                store_up(4, 1),
            ],
            "fallback output: CALL op (callee internals excluded) then own store",
        );
    }

    /// A reverted call must fall back. Without the classifier's `error` check
    /// the fast path would classify this Eligible and emit the LOG the tracer
    /// still reports for the reverted root frame (anvil does not prune it) on
    /// top of an empty storage diff — an event that never happened. Fallback
    /// keeps the hybrid path byte-identical to the previous behaviour.
    #[tokio::test]
    async fn test_hybrid_falls_back_on_reverted_call() {
        let anvil = LocalAnvil::spawn().await;
        let provider = anvil.provider();
        let reverter = address!("0x0000000000000000000000000000000000001006");
        set_code(&provider, reverter, reverter_code()).await;

        assert!(
            matches!(
                classify_call(&provider, reverter).await,
                PrestateEligibility::Fallback(_)
            ),
            "reverted call must force the struct-log fallback"
        );

        let ((hybrid, _), structlog) = extract_both(&provider, reverter).await;
        assert_updates_eq(
            &hybrid,
            &structlog,
            "hybrid must equal the struct-log path for reverted calls",
        );
    }

    /// A DELEGATECALL child that reverts while its parent succeeds: the child's
    /// ops rolled back (absent from the net diff) but the struct-log path still
    /// extracts them at target depth. Only fallback keeps the two paths equal.
    #[tokio::test]
    async fn test_hybrid_falls_back_on_caught_reverted_delegatecall() {
        let anvil = LocalAnvil::spawn().await;
        let provider = anvil.provider();
        let catcher = address!("0x0000000000000000000000000000000000001007");
        let reverter = address!("0x0000000000000000000000000000000000001006");
        set_code(&provider, catcher, catcher_code(reverter)).await;
        set_code(&provider, reverter, reverter_code()).await;

        assert!(
            matches!(
                classify_call(&provider, catcher).await,
                PrestateEligibility::Fallback(_)
            ),
            "caught-revert delegatecall must force the struct-log fallback"
        );

        let ((hybrid, _), structlog) = extract_both(&provider, catcher).await;
        assert_updates_eq(
            &hybrid,
            &structlog,
            "hybrid must equal the struct-log path when a transparent frame reverted",
        );
        assert_updates_eq(
            &hybrid,
            &[
                store_up(1, 0x42),
                log1_up(0xdd, Bytes::new()),
                store_up(2, 0x22),
            ],
            "struct-log output includes the rolled-back delegatecall ops (pre-existing \
             struct-log behaviour) plus the parent's store",
        );
    }

    /// Shape contract of the two new rpc helpers against a real node: the
    /// prestate diff carries the consumer's changed slots, and the call frame
    /// carries logs with `position` populated.
    #[tokio::test]
    async fn test_prestate_rpc_helpers_return_diff_and_frame() {
        let anvil = LocalAnvil::spawn().await;
        let provider = anvil.provider();
        let consumer = address!("0x0000000000000000000000000000000000001001");
        set_code(&provider, consumer, eligible_code()).await;

        let diff =
            get_prestate_diff_from_call(&provider, call_request(consumer), BlockId::latest())
                .await
                .expect("prestate tracer");
        let post_storage = &diff
            .post
            .get(&consumer)
            .expect("consumer must appear in post state")
            .storage;
        assert_eq!(
            post_storage.get(&B256::with_last_byte(1)),
            Some(&B256::with_last_byte(0x42)),
            "diff must carry the consumer's changed slot"
        );

        let frame = get_call_frame_from_call(&provider, call_request(consumer), BlockId::latest())
            .await
            .expect("call tracer");
        assert_eq!(
            frame.logs.len(),
            1,
            "callTracer must return the emitted log"
        );
        assert!(
            frame.logs[0].position.is_some(),
            "log position must be populated — ordered_target_depth_logs depends on it"
        );
        assert_eq!(
            frame.logs[0].topics.as_deref(),
            Some(&[B256::with_last_byte(0xaa)][..]),
            "log topic must round-trip"
        );
    }

    // ========================================================================
    // Unbounded profile (SimProfile::UnboundedV1) — anvil integration tests
    //
    // These spawn anvil with `--disable-block-gas-limit` (the node-side
    // requirement the profile documents) and prove the mode's two halves:
    // compute beyond any real block extracts fine, and the extracted payload
    // must still be single-slot shaped.
    // ========================================================================

    /// ~40M-gas busy loop (1,000,000 iterations × ~40 gas), then exactly one
    /// SSTORE(1, 0x42) and one LOG1(topic 0xee) — the single-slot commitment
    /// shape with compute far beyond a 30M block.
    ///
    /// Layout: PUSH3 1_000_000; loop{ DUP1 ISZERO PUSH1 end JUMPI; PUSH1 1
    /// SWAP1 SUB; PUSH1 4 JUMP }; end: POP; SSTORE; LOG1; STOP.
    fn gigagas_burner_code() -> Bytes {
        hex_code(&format!(
            "620f4240 5b 80 15 6011 57 6001 90 03 6004 56 5b 50 6042600155 7f{}60006000a1 00",
            topic_hex(0xee),
        ))
    }

    /// Same busy loop, but writing TWO slots — a consumer that is not
    /// commitment-shaped and must be rejected by the unbounded profile.
    fn gigagas_two_slot_code() -> Bytes {
        hex_code("620f4240 5b 80 15 6011 57 6001 90 03 6004 56 5b 50 6042600155 6043600255 00")
    }

    /// Under `Chain` the burner OOGs (tx gas 3M < 40M needed): the #165 revert
    /// classification forces fallback. Under `UnboundedV1` the same call — same
    /// request, gas lifted to the pinned 2^40 override — extracts exactly
    /// [Store(1,0x42), Log1(0xee)] via the prestate fast path.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_unbounded_profile_extracts_beyond_block_gas_limit() {
        let anvil = LocalAnvil::spawn_with(&["--disable-block-gas-limit"]).await;
        let provider = anvil.provider();
        let consumer = address!("0x0000000000000000000000000000000000002001");
        set_code(&provider, consumer, gigagas_burner_code()).await;

        // Chain profile: the 3M tx gas from call_request stands, the loop
        // OOGs, and the reverted root frame must classify as Fallback.
        let chain_classification = classify_call(&provider, consumer).await;
        assert!(
            matches!(
                &chain_classification,
                PrestateEligibility::Fallback(reason) if reason.contains("reverted/failed")
            ),
            "OOG under Chain profile must force the struct-log fallback, never an unsound diff; \
             got {chain_classification:?}"
        );

        // UnboundedV1: identical request; the profile's pinned tx-gas override
        // replaces the request's 3M and the burner completes.
        let (updates, skipped) = extract_state_updates_hybrid(
            &provider,
            call_request(consumer),
            BlockId::latest(),
            consumer,
            SimProfile::UnboundedV1,
            None,
        )
        .await
        .expect("unbounded extraction must succeed on a >30M-gas call");
        assert!(skipped.is_empty(), "no opcodes should be skipped");
        assert_updates_eq(
            &updates,
            &[store_up(1, 0x42), log1_up(0xee, Bytes::new())],
            "40M gas of compute must reduce to one Store and one Log",
        );
        validate_unbounded_shape(&updates).expect("burner payload is commitment-shaped");
    }

    /// The XL gas tier (`SimProfile::UnboundedV1Xl`, pinned 2^43 override)
    /// must extract the identical payload the V1 tier does on the same call:
    /// the tier only moves the OOG ceiling, never the extraction semantics or
    /// the shape gate.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_unbounded_xl_profile_extracts_beyond_block_gas_limit() {
        let anvil = LocalAnvil::spawn_with(&["--disable-block-gas-limit"]).await;
        let provider = anvil.provider();
        let consumer = address!("0x0000000000000000000000000000000000002003");
        set_code(&provider, consumer, gigagas_burner_code()).await;

        let (updates, skipped) = extract_state_updates_hybrid(
            &provider,
            call_request(consumer),
            BlockId::latest(),
            consumer,
            SimProfile::UnboundedV1Xl,
            None,
        )
        .await
        .expect("XL extraction must succeed on a >30M-gas call");
        assert!(skipped.is_empty(), "no opcodes should be skipped");
        assert_updates_eq(
            &updates,
            &[store_up(1, 0x42), log1_up(0xee, Bytes::new())],
            "the XL tier must extract the same single-slot payload as V1",
        );
        validate_unbounded_shape(&updates).expect("burner payload is commitment-shaped");
    }

    /// A two-slot writer extracts fine but must fail the unbounded shape
    /// gate — the exact check `call_to_encoded_state_updates_with_evmsketch_profiled`
    /// applies before estimating/encoding.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_unbounded_profile_rejects_multi_slot_payload() {
        let anvil = LocalAnvil::spawn_with(&["--disable-block-gas-limit"]).await;
        let provider = anvil.provider();
        let consumer = address!("0x0000000000000000000000000000000000002002");
        set_code(&provider, consumer, gigagas_two_slot_code()).await;

        let (updates, _) = extract_state_updates_hybrid(
            &provider,
            call_request(consumer),
            BlockId::latest(),
            consumer,
            SimProfile::UnboundedV1,
            None,
        )
        .await
        .expect("extraction itself succeeds; only the shape gate rejects");

        let violation = validate_unbounded_shape(&updates)
            .expect_err("two Store ops must violate the single-slot invariant");
        assert_eq!(
            violation,
            gas_analyzer_core::UnboundedShapeViolation::TooManyStores { count: 2 }
        );
    }

    // ========================================================================
    // RPC path vs local path — byte-identical differential tests (#169)
    //
    // Same deployed bytecode, same anvil block: extract via the RPC path
    // (extract_state_updates_hybrid, real debug_traceCall tracers) and via
    // the local path (local_exec::extract_state_updates_local, in-process
    // revm over a SharedBackend pinned to anvil) and assert the ABI-encoded
    // payload bytes are identical. `EvmSketchExecutorCache`/`EvmSketch` only
    // support mainnet/sepolia/gnosis genesis (chain_id_to_genesis_and_spec),
    // so these tests drive extraction directly rather than through the full
    // call_to_encoded_state_updates_* entry points — the gas-estimate half
    // downstream of extraction is identical code shared by both paths.
    // ========================================================================

    use foundry_fork_db::{BlockchainDb, SharedBackend, cache::BlockchainDbMeta};
    use local_exec::LocalTxRequest;

    /// anvil's default hardfork (Prague) and chain id (31337), used to build
    /// a [`LocalBlockEnv`] without going through `EvmSketchExecutorBuilder`
    /// (which rejects non-mainnet/sepolia/gnosis chain ids).
    const ANVIL_CHAIN_ID: u64 = 31_337;

    fn any_provider(anvil: &LocalAnvil) -> RootProvider<AnyNetwork> {
        RootProvider::<AnyNetwork>::new_http(Url::parse(&anvil.url).expect("valid url"))
    }

    /// Fetch anvil's current header and build the matching [`LocalBlockEnv`]
    /// — the same fields `LocalBlockEnv::from_executor` would pull from an
    /// anchored `EvmSketchExecutor`, sourced directly via `eth_getBlockByNumber`
    /// since anvil's chain id can't build one.
    async fn anvil_local_block_env(provider: &RootProvider<AnyNetwork>) -> LocalBlockEnv {
        let block = provider
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .expect("eth_getBlockByNumber failed")
            .expect("latest block must exist");
        LocalBlockEnv {
            chain_id: ANVIL_CHAIN_ID,
            spec: SpecId::PRAGUE,
            number: block.header.number,
            timestamp: block.header.timestamp,
            gas_limit: block.header.gas_limit,
            coinbase: block.header.beneficiary,
            prevrandao: block.header.mix_hash.unwrap_or_default(),
            basefee: block.header.base_fee_per_gas.unwrap_or(0),
            difficulty: block.header.difficulty,
        }
    }

    /// A `SharedBackend` pinned to anvil's current block — the local path's
    /// remote-state DB for these tests.
    fn anvil_shared_backend(provider: RootProvider<AnyNetwork>, block_number: u64) -> SharedBackend {
        let db = BlockchainDb::new(BlockchainDbMeta::default(), None);
        SharedBackend::spawn_backend_thread(provider, db, Some(BlockId::number(block_number)))
    }

    /// Extract via the local path against a live anvil instance: same
    /// dispatch rules as [`extract_state_updates_hybrid`], but executed
    /// in-process against a `SharedBackend`.
    async fn extract_local_via_anvil(
        anvil: &LocalAnvil,
        to: Address,
        profile: SimProfile,
        overlay: Option<Arc<OverlayMount>>,
    ) -> (Vec<StateUpdate>, HashSet<Opcode>) {
        let provider = any_provider(anvil);
        let env = anvil_local_block_env(&provider).await;
        let backend = anvil_shared_backend(provider, env.number);
        let tx = LocalTxRequest::from_request(&call_request(to)).expect("valid tx request");
        local_exec::extract_state_updates_local(backend, overlay, env, tx, profile, to)
            .await
            .expect("local extraction failed")
    }

    /// Run both paths against the same deployed call and assert their
    /// ABI-encoded payloads are byte-identical — the acceptance bar issue
    /// #169 sets for the local executor.
    async fn assert_rpc_and_local_identical(
        anvil: &LocalAnvil,
        to: Address,
        profile: SimProfile,
        overlay_env: Option<&OverlayEnv>,
        overlay_mount: Option<Arc<OverlayMount>>,
        ctx: &str,
    ) -> (Vec<StateUpdate>, Vec<StateUpdate>) {
        let provider = anvil.provider();
        let (rpc_updates, rpc_skipped) = extract_state_updates_hybrid(
            &provider,
            call_request(to),
            BlockId::latest(),
            to,
            profile,
            overlay_env,
        )
        .await
        .unwrap_or_else(|e| panic!("{ctx}: RPC-path extraction failed: {e:?}"));

        let (local_updates, local_skipped) =
            extract_local_via_anvil(anvil, to, profile, overlay_mount).await;

        assert_eq!(
            encode_state_updates_to_abi(&rpc_updates),
            encode_state_updates_to_abi(&local_updates),
            "{ctx}: RPC-path and local-path encoded payloads diverged\n  rpc:   {rpc_updates:?}\n  local: {local_updates:?}"
        );
        assert_eq!(
            rpc_skipped, local_skipped,
            "{ctx}: skipped_opcodes must match between paths"
        );
        (rpc_updates, local_updates)
    }

    /// Case 1: simple single-slot store (no logs, no calls) — the baseline
    /// prestate-fast-path shape.
    #[tokio::test(flavor = "multi_thread")]
    async fn differential_simple_store() {
        let anvil = LocalAnvil::spawn().await;
        let consumer = address!("0x0000000000000000000000000000000000003001");
        set_code(&anvil.provider(), consumer, hex_code("6042600155 00")).await;

        let (rpc, _) = assert_rpc_and_local_identical(
            &anvil,
            consumer,
            SimProfile::Chain,
            None,
            None,
            "simple_store",
        )
        .await;
        assert_updates_eq(&rpc, &[store_up(1, 0x42)], "simple_store shape");
    }

    /// Case 2: multi-slot store — net-diff dedup (write-then-restore
    /// disappears) and slot-sorted ordering must agree between paths.
    #[tokio::test(flavor = "multi_thread")]
    async fn differential_multi_slot() {
        let anvil = LocalAnvil::spawn().await;
        let provider = anvil.provider();
        let consumer = address!("0x0000000000000000000000000000000000003002");
        set_code(&provider, consumer, eligible_code()).await;
        set_storage(
            &provider,
            consumer,
            B256::with_last_byte(5),
            B256::with_last_byte(0x99),
        )
        .await;

        let (rpc, _) = assert_rpc_and_local_identical(
            &anvil,
            consumer,
            SimProfile::Chain,
            None,
            None,
            "multi_slot",
        )
        .await;
        let word_ab = Bytes::copy_from_slice(B256::with_last_byte(0xab).as_slice());
        assert_updates_eq(
            &rpc,
            &[store_up(1, 0x42), store_up(5, 0), log1_up(0xaa, word_ab)],
            "multi_slot shape (also covers logs — see eligible_code)",
        );
    }

    /// Case 3: logs, specifically DELEGATECALL log interleaving — true
    /// emission order (aa, cc, bb) must agree between paths.
    #[tokio::test(flavor = "multi_thread")]
    async fn differential_logs_delegatecall_interleaving() {
        let anvil = LocalAnvil::spawn().await;
        let provider = anvil.provider();
        let root = address!("0x0000000000000000000000000000000000003003");
        let lib = address!("0x0000000000000000000000000000000000003004");
        set_code(&provider, root, delegate_root_code(lib)).await;
        set_code(&provider, lib, delegate_lib_code()).await;

        let (rpc, _) = assert_rpc_and_local_identical(
            &anvil,
            root,
            SimProfile::Chain,
            None,
            None,
            "logs_delegatecall_interleaving",
        )
        .await;
        assert_updates_eq(
            &rpc,
            &[
                store_up(3, 7),
                log1_up(0xaa, Bytes::new()),
                log1_up(0xcc, Bytes::new()),
                log1_up(0xbb, Bytes::new()),
            ],
            "delegatecall interleaving shape",
        );
    }

    /// Case 4: nested regular CALL — forces the struct-log/replay-script
    /// fallback on both paths; the replayable CALL op (callee internals
    /// excluded) plus the caller's own store must agree.
    #[tokio::test(flavor = "multi_thread")]
    async fn differential_nested_call() {
        let anvil = LocalAnvil::spawn().await;
        let provider = anvil.provider();
        let caller = address!("0x0000000000000000000000000000000000003005");
        let callee = address!("0x0000000000000000000000000000000000003006");
        set_code(&provider, caller, caller_code(callee)).await;
        set_code(&provider, callee, callee_code()).await;

        let (rpc, _) = assert_rpc_and_local_identical(
            &anvil,
            caller,
            SimProfile::Chain,
            None,
            None,
            "nested_call",
        )
        .await;
        assert_updates_eq(
            &rpc,
            &[
                StateUpdate::Call(IStateUpdateTypes::Call {
                    target: callee,
                    value: U256::ZERO,
                    callargs: Bytes::new(),
                }),
                store_up(4, 1),
            ],
            "nested CALL shape",
        );
    }

    /// Case 5: revert — pre-revert ops must still be extracted identically
    /// (revert-unaware struct-log semantics) on both paths.
    #[tokio::test(flavor = "multi_thread")]
    async fn differential_revert() {
        let anvil = LocalAnvil::spawn().await;
        let reverter = address!("0x0000000000000000000000000000000000003007");
        set_code(&anvil.provider(), reverter, reverter_code()).await;

        let (rpc, _) =
            assert_rpc_and_local_identical(&anvil, reverter, SimProfile::Chain, None, None, "revert")
                .await;
        assert_updates_eq(
            &rpc,
            &[store_up(1, 0x42), log1_up(0xdd, Bytes::new())],
            "reverted call shape: pre-revert ops captured",
        );
    }

    /// Case 6: overlay mode — RPC path mounts the overlay via
    /// `anvil_setCode` (`stateOverrides`-equivalent for a real node), local
    /// path mounts the same bytes natively via `OverlayMount::from_env`.
    /// A consumer that `EXTCODECOPY`s a chunk and commits one word of it
    /// must produce byte-identical payloads under both mounting strategies.
    #[tokio::test(flavor = "multi_thread")]
    async fn differential_overlay_mode() {
        let anvil = LocalAnvil::spawn().await;
        let provider = anvil.provider();

        let payload: Vec<u8> = (0..100u32).map(|i| (i % 256) as u8).collect();
        let overlay_env = OverlayEnv::from_blobs(&payload, b"tok").expect("overlay env");
        let chunk = overlay_env.overlays[0].address;
        let overlay_mount =
            Arc::new(OverlayMount::from_env(&overlay_env, overlay_env.manifest).expect("mount"));

        let consumer = address!("0x0000000000000000000000000000000000003008");
        // EXTCODECOPY(chunk, dest 0, offset 1, size 32); SSTORE(1, MLOAD(0));
        // LOG1(mem[0..32], topic 0xee); STOP.
        set_code(
            &provider,
            consumer,
            hex_code(&format!(
                "6020 6001 6000 73{} 3c 600051600155 7f{}60206000a1 00",
                addr_hex(chunk),
                topic_hex(0xee),
            )),
        )
        .await;

        // RPC path: no anvil_setCode for the chunk itself — apply_overlay_env
        // mounts it as a stateOverrides code override for the trace call, the
        // real UNBOUNDED_V2 transport. The chunk account is never deployed on
        // anvil, mirroring "no rootDirectory" mode on-chain.
        let (rpc, _) = assert_rpc_and_local_identical(
            &anvil,
            consumer,
            SimProfile::Chain,
            Some(&overlay_env),
            Some(overlay_mount),
            "overlay_mode",
        )
        .await;

        let expected_word = B256::from_slice(&payload[0..32]);
        assert_updates_eq(
            &rpc,
            &[
                StateUpdate::Store(IStateUpdateTypes::Store {
                    slot: B256::with_last_byte(1),
                    value: expected_word,
                }),
                StateUpdate::Log1(IStateUpdateTypes::Log1 {
                    data: Bytes::copy_from_slice(expected_word.as_slice()),
                    topic1: B256::with_last_byte(0xee),
                }),
            ],
            "overlay-mode shape: chunk bytes readable identically via stateOverrides and native mount",
        );
    }
}

//! True local execution path (gas-analyzer#169): in-process revm with a lazy
//! remote-state database.
//!
//! The RPC path delegates every analysis to `debug_traceCall` — three tracer
//! round-trips per task, ~0.35 Ggas/s through a shared anvil fork, and
//! overlays inlined as `stateOverrides` JSON. This module executes the
//! tracked call **inside the analyzer process**: the RPC becomes a pure state
//! backend (accounts / storage / code / block hashes fetched on first touch
//! and cached per block), overlays mount natively via
//! [`crate::overlay_mount::OverlayStateDb`], and one or two in-process revm
//! runs replace the three tracers.
//!
//! # Consensus contract
//!
//! The extraction must be **byte-identical** to the RPC path — operators sign
//! the encoded payload. Three layers guarantee that:
//!
//! 1. **State**: [`foundry_fork_db::SharedBackend`] pinned to the anchor
//!    block serves exactly what `eth_getBalance/getTransactionCount/getCode/
//!    getStorageAt` at that block return — the same values a
//!    `debug_traceCall` at that block executes against.
//! 2. **Env**: [`build_cfg_and_block`] / [`build_tx_env`] mirror what
//!    `gas_analyzer_rpc::apply_sim_profile` puts on the wire (tx-gas and
//!    block-gas overrides for [`SimProfile::UnboundedV1`] /
//!    [`SimProfile::UnboundedV1Xl`], pinned constants from
//!    `gas_analyzer_core::sim_profile`) plus the `debug_traceCall`
//!    call-semantics defaults (no base-fee/balance/nonce enforcement, block
//!    header env of the anchored block).
//! 3. **Extraction**: [`extract_state_updates_local_blocking`] reproduces the
//!    hybrid dispatcher — a cheap classification pass (the
//!    `prestateTracer`/`callTracer` equivalent: net storage diff from revm's
//!    finalized state + target-depth logs + call-shape from inspector hooks),
//!    falling back to a replay-script pass (the struct-log equivalent:
//!    pre-execution opcode capture at target depth, revert-unaware) exactly
//!    when [`gas_analyzer_core::classify_prestate_eligibility`] would.
//!
//! Because revm executes the bytes, **revm version bumps are
//! consensus-affecting** (see the pin comments in this crate's `Cargo.toml`).
//!
//! # Known semantic gaps vs the RPC tracers (documented, not papered over)
//!
//! * A target-depth `LOG*`/`CALL`/`CREATE` that *itself* runs out of gas
//!   mid-instruction: geth's struct log records the failing opcode entry and
//!   the RPC fallback extracts it; the local replay pass captures ops
//!   pre-execution too, but revm surfaces some pre-frame failures (e.g. CALL
//!   OOG during its own gas charge) without a step entry. Both paths agree on
//!   every test case; the residual divergence window is "the halting
//!   instruction is itself a state-update opcode at target depth".
//! * geth `callTracer` implementations differ on precompile call frames; the
//!   local classifier counts a target-depth `CALL` to a precompile as a
//!   regular CALL (fallback), which matches anvil's tracer.
//! * `skipped_opcodes` is always empty, faithfully mirroring the RPC path:
//!   `compute_state_updates` filters to state-update opcodes *before* its
//!   SELFDESTRUCT/TSTORE skip check, so that path never reports skips either.

use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::rpc::types::eth::TransactionRequest;
use alloy_eips::BlockId;
use alloy_provider::RootProvider;
use alloy_provider::network::AnyNetwork;
use anyhow::{Result, anyhow};
use foundry_fork_db::{
    BlockchainDb, SharedBackend, backend::BlockingMode, cache::BlockchainDbMeta,
};
use lru::LruCache;
use revm::bytecode::opcode;
use revm::context::result::ExecutionResult;
use revm::context::{Context, TxEnv};
use revm::context_interface::transaction::{AccessList, SignedAuthorization};
use revm::database::CacheDB;
use revm::database_interface::DatabaseRef;
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::interpreter_types::{Jumps, MemoryTr};
use revm::interpreter::{
    CallInputs, CallOutcome, CallScheme, CreateInputs, CreateOutcome, Interpreter,
};
use revm::primitives::Log;
use revm::primitives::hardfork::SpecId;
use revm::state::EvmState;
use revm::{InspectEvm, Inspector, MainBuilder, MainContext};

use gas_analyzer_core::types::IStateUpdateTypes;
use gas_analyzer_core::{Opcode, OverlayEnv, PrestateEligibility, SimProfile, StateUpdate};

use crate::DefaultEvmSketchExecutor;
use crate::overlay_mount::{OverlayMount, OverlayMountSet, OverlayStateDb};

// ============================================================================
// Block environment
// ============================================================================

/// The pinned per-block environment a local trace executes under: the
/// anchored header's fields plus the chain's spec — exactly what
/// `debug_traceCall` at that block uses server-side, and the same fields
/// [`crate::EvmSketchExecutor::sim_env`] mirrors for the payload gas
/// estimate.
#[derive(Debug, Clone)]
pub struct LocalBlockEnv {
    /// Chain id (CHAINID opcode).
    pub chain_id: u64,
    /// EVM hardfork for the anchored block. Consensus-critical: operators on
    /// different specs diverge at gas-schedule boundaries.
    pub spec: SpecId,
    /// Anchored block number.
    pub number: u64,
    /// Anchored block timestamp.
    pub timestamp: u64,
    /// Header gas limit (pre-override; profiles may lift it).
    pub gas_limit: u64,
    /// COINBASE.
    pub coinbase: Address,
    /// PREVRANDAO (header mix_hash).
    pub prevrandao: B256,
    /// BASEFEE (0 for pre-EIP-1559 blocks).
    pub basefee: u64,
    /// Legacy DIFFICULTY (zero post-Merge).
    pub difficulty: U256,
}

impl LocalBlockEnv {
    /// Build from an anchored executor (header + chain id + spec resolved at
    /// executor build time).
    pub fn from_executor(executor: &DefaultEvmSketchExecutor) -> Self {
        let header = executor.sketch.anchor.header();
        Self {
            chain_id: executor.chain_id,
            spec: executor.spec,
            number: header.number,
            timestamp: header.timestamp,
            gas_limit: header.gas_limit,
            coinbase: header.beneficiary,
            prevrandao: header.mix_hash,
            basefee: header.base_fee_per_gas.unwrap_or(0),
            difficulty: header.difficulty,
        }
    }
}

// ============================================================================
// Transaction request (owned, thread-movable)
// ============================================================================

/// The subset of a [`TransactionRequest`] the local trace consumes, owned so
/// it can move into the blocking execution task.
#[derive(Debug, Clone)]
pub(crate) struct LocalTxRequest {
    pub from: Address,
    pub to: Address,
    pub input: Bytes,
    pub value: U256,
    /// Caller-supplied tx gas (profile overrides win; falls back to the
    /// block gas limit like anvil/geth do for gas-less call requests).
    pub gas: Option<u64>,
    /// Effective gas price for the GASPRICE opcode (0 when the request
    /// carries no fee fields — `debug_traceCall` semantics).
    pub gas_price: u128,
    pub nonce: Option<u64>,
    pub access_list: AccessList,
    pub authorization_list: Vec<SignedAuthorization>,
}

impl LocalTxRequest {
    pub(crate) fn from_request(request: &TransactionRequest) -> Result<Self> {
        let to = request
            .to
            .and_then(|t| match t {
                TxKind::Call(addr) => Some(addr),
                TxKind::Create => None,
            })
            .ok_or_else(|| anyhow!("Transaction must have a 'to' address"))?;
        Ok(Self {
            from: request.from.unwrap_or_default(),
            to,
            input: request.input.input().cloned().unwrap_or_default(),
            value: request.value.unwrap_or_default(),
            gas: request.gas,
            gas_price: request
                .gas_price
                .or(request.max_fee_per_gas)
                .unwrap_or_default(),
            nonce: request.nonce,
            access_list: request.access_list.clone().unwrap_or_default(),
            authorization_list: request.authorization_list.clone().unwrap_or_default(),
        })
    }
}

// ============================================================================
// Block-scoped remote-state backends + overlay mounts
// ============================================================================

/// Block-scoped shared state for local execution: one
/// [`SharedBackend`] per `(rpc_url, block)` (read-through cache shared by all
/// concurrent traces of that block; LRU eviction drops the backend — and its
/// fetch thread — once the round moves on) and one verified [`OverlayMount`]
/// per manifest.
///
/// This is the local path's counterpart to [`crate::EvmSketchExecutorCache`]:
/// pass one persistent instance (e.g. `Arc<LocalStateCache>`) across calls so
/// concurrent traces of the same block share both the remote-state backend
/// and the executor cache backing the gas estimate.
pub struct LocalStateCache {
    backends: Mutex<LruCache<(String, u64), SharedBackend>>,
    overlay_mounts: Mutex<LruCache<B256, Arc<OverlayMount>>>,
}

impl Default for LocalStateCache {
    fn default() -> Self {
        Self {
            // Same burst window as the executor cache: a handful of recent
            // blocks. Eviction after the round bounds the remote-state cache
            // exactly as issue #169's memory notes require.
            backends: Mutex::new(LruCache::new(NonZeroUsize::new(4).expect("non-zero"))),
            // Sized above the number of concurrently served models: two
            // pinned models mounted simultaneously (multi-overlay) plus
            // headroom, so a third transient mount can never evict a resident
            // model — re-mounting a file-backed model costs a full streaming
            // keccak pass over the blobs (minutes for 35 GB artifacts).
            overlay_mounts: Mutex::new(LruCache::new(NonZeroUsize::new(4).expect("non-zero"))),
        }
    }
}

impl LocalStateCache {
    /// A [`SharedBackend`] pinned to `block_number` on `rpc_url`, spawning
    /// one (with its own fetch thread) on first use.
    pub(crate) fn backend_for(
        &self,
        rpc_url: &str,
        block_number: u64,
        provider: RootProvider<AnyNetwork>,
    ) -> SharedBackend {
        let key = (rpc_url.to_string(), block_number);
        let mut cache = self.backends.lock().expect("backend cache mutex poisoned");
        if let Some(backend) = cache.get(&key) {
            return backend.clone();
        }
        let db = BlockchainDb::new(BlockchainDbMeta::default(), None);
        let backend =
            SharedBackend::spawn_backend_thread(provider, db, Some(BlockId::number(block_number)));
        cache.put(key, backend.clone());
        backend
    }

    /// The in-memory [`OverlayMount`] for `env`, memoized by manifest so the
    /// per-chunk address map is built once per model, not once per call. The
    /// env must already be verified against the consumer's pinned manifest
    /// (the same caller contract as the RPC overlay path); `from_env`
    /// re-verifies against `env.manifest` structurally.
    pub(crate) fn overlay_mount_for(&self, env: &OverlayEnv) -> Result<Arc<OverlayMount>> {
        let mut cache = self
            .overlay_mounts
            .lock()
            .expect("overlay mount cache mutex poisoned");
        if let Some(mount) = cache.get(&env.manifest) {
            return Ok(mount.clone());
        }
        let mount = Arc::new(
            OverlayMount::from_env(env, env.manifest).map_err(|e| anyhow!("overlay mount: {e}"))?,
        );
        cache.put(env.manifest, mount.clone());
        Ok(mount)
    }

    /// The mmap-backed [`OverlayMount`] for the artifact blobs at
    /// `weights_path`/`tokenizer_path`, memoized by `manifest` on the same
    /// LRU as [`Self::overlay_mount_for`] — a multi-gigabyte model is mounted
    /// (streaming-hashed and index-built) once per process, not once per
    /// call.
    ///
    /// [`OverlayMount::from_files`] recomputes the manifest from the blobs
    /// via a streaming keccak pass and hard-fails on any mismatch against
    /// `manifest` ([`OverlayError::ManifestMismatch`] surfaced through the
    /// returned `anyhow::Error`) — the same verification contract as
    /// [`Self::overlay_mount_for`]/`OverlayEnv::verify`. Callers must not
    /// mutate the blob files while the returned mount is in use (the `mmap`
    /// contract `OverlayMount::from_files` documents).
    pub fn overlay_mount_from_files(
        &self,
        weights_path: impl AsRef<std::path::Path>,
        tokenizer_path: impl AsRef<std::path::Path>,
        manifest: B256,
    ) -> Result<Arc<OverlayMount>> {
        let mut cache = self
            .overlay_mounts
            .lock()
            .expect("overlay mount cache mutex poisoned");
        if let Some(mount) = cache.get(&manifest) {
            return Ok(mount.clone());
        }
        // Don't hold the mutex across the mount build: `from_files` streams a
        // full pass over multi-gigabyte blobs to verify the manifest. Two
        // concurrent callers that both miss may each mount; the later `put`
        // overwrites the earlier entry. Both mounts are equivalent (same
        // verified manifest, same index), so this only wastes one mount in
        // the rare cold-start race — the same trade-off
        // `EvmSketchExecutorCache::get_or_build` documents.
        drop(cache);

        let mount = Arc::new(OverlayMount::from_files(
            weights_path,
            tokenizer_path,
            manifest,
        )?);

        let mut cache = self
            .overlay_mounts
            .lock()
            .expect("overlay mount cache mutex poisoned");
        cache.put(manifest, mount.clone());
        Ok(mount)
    }

    /// The composite [`OverlayMountSet`] for several file-backed models at
    /// once — the multi-overlay construction path. Each `(weights_path,
    /// tokenizer_path, manifest)` spec resolves through
    /// [`Self::overlay_mount_from_files`]: same mandatory streaming-keccak
    /// verification against the pinned manifest, same manifest-keyed
    /// memoization, so N models are hashed + indexed once per process, not
    /// once per call.
    ///
    /// Chunk addresses are derived per-manifest, so distinct manifests yield
    /// disjoint address sets and the composite lookup is sound (see
    /// [`OverlayMountSet`]). Duplicate manifests in `specs` are refused
    /// outright: two specs naming the same commitment is caller
    /// misconfiguration (the later one's paths would be silently ignored via
    /// the cache), and this failure mode should be loud.
    pub fn overlay_mount_set_from_files<W, T>(
        &self,
        specs: &[(W, T, B256)],
    ) -> Result<OverlayMountSet>
    where
        W: AsRef<std::path::Path>,
        T: AsRef<std::path::Path>,
    {
        let mut seen = std::collections::HashSet::with_capacity(specs.len());
        let mut mounts = Vec::with_capacity(specs.len());
        for (weights_path, tokenizer_path, manifest) in specs {
            if !seen.insert(*manifest) {
                return Err(anyhow!(
                    "duplicate overlay manifest {manifest} in multi-mount spec: \
                     each mounted model must have a distinct pinned manifest"
                ));
            }
            mounts.push(self.overlay_mount_from_files(weights_path, tokenizer_path, *manifest)?);
        }
        Ok(OverlayMountSet::new(mounts))
    }
}

// ============================================================================
// Env construction (mirrors apply_sim_profile + debug_traceCall semantics)
// ============================================================================

/// Resolve the pinned gas limits for a trace: profile overrides win
/// unconditionally (they are protocol constants — see
/// `gas_analyzer_core::sim_profile`), mirroring `apply_sim_profile` on the
/// RPC path; under [`SimProfile::Chain`] the request's gas (or the header
/// limit) stands.
fn resolve_gas_limits(env: &LocalBlockEnv, tx: &LocalTxRequest, profile: SimProfile) -> (u64, u64) {
    let block_gas_limit = profile.block_gas_limit_override().unwrap_or(env.gas_limit);
    let tx_gas_limit = profile
        .tx_gas_limit_override()
        .or(tx.gas)
        .unwrap_or(block_gas_limit);
    (block_gas_limit, tx_gas_limit)
}

/// Build the tx env for the traced call. `chain_id` is deliberately `None`:
/// `debug_traceCall` requests carry no chain id and the node performs no
/// check (revm's builder would otherwise default to `Some(1)` and reject any
/// non-mainnet trace).
fn build_tx_env(tx: &LocalTxRequest, tx_gas_limit: u64) -> Result<TxEnv> {
    let mut builder = TxEnv::builder()
        .caller(tx.from)
        .kind(TxKind::Call(tx.to))
        .data(tx.input.clone())
        .value(tx.value)
        .gas_limit(tx_gas_limit)
        .gas_price(tx.gas_price)
        .chain_id(None)
        .access_list(tx.access_list.clone())
        .authorization_list_signed(tx.authorization_list.clone());
    if let Some(nonce) = tx.nonce {
        builder = builder.nonce(nonce);
    }
    builder
        .build()
        .map_err(|e| anyhow!("failed to build local trace tx env: {e:?}"))
}

// ============================================================================
// Pass 1: classification inspector (prestate/callTracer equivalent)
// ============================================================================

/// Convert a raw log (topics + data) into the matching LOG0..LOG4 update —
/// the same mapping `gas_analyzer_core::prestate` applies to `callTracer`
/// logs.
fn log_to_update(topics: &[B256], data: Bytes) -> StateUpdate {
    match topics.len() {
        0 => StateUpdate::Log0(IStateUpdateTypes::Log0 { data }),
        1 => StateUpdate::Log1(IStateUpdateTypes::Log1 {
            data,
            topic1: topics[0],
        }),
        2 => StateUpdate::Log2(IStateUpdateTypes::Log2 {
            data,
            topic1: topics[0],
            topic2: topics[1],
        }),
        3 => StateUpdate::Log3(IStateUpdateTypes::Log3 {
            data,
            topic1: topics[0],
            topic2: topics[1],
            topic3: topics[2],
        }),
        _ => StateUpdate::Log4(IStateUpdateTypes::Log4 {
            data,
            topic1: topics[0],
            topic2: topics[1],
            topic3: topics[2],
            topic4: topics[3],
        }),
    }
}

/// Pass-1 inspector: tracks the target-depth frame context (root +
/// `DELEGATECALL`/`CALLCODE` descendants), records the first
/// fast-path-disqualifying event, and collects target-depth logs in true
/// emission order. Its `step` is the default no-op, so the classification
/// pass runs at full interpreter speed.
///
/// Together with the finalized state diff and the execution result this is
/// everything [`gas_analyzer_core::classify_prestate_eligibility`] reads from
/// the two cheap tracers.
#[derive(Debug, Default)]
struct ClassifyInspector {
    /// Per-frame "is target depth" flags (stack).
    frames: Vec<bool>,
    /// First reason the fast path is unsound, if any.
    fallback: Option<String>,
    /// Target-depth logs in emission order.
    logs: Vec<StateUpdate>,
}

impl ClassifyInspector {
    fn note(&mut self, reason: String) {
        if self.fallback.is_none() {
            self.fallback = Some(reason);
        }
    }

    fn in_target_frame(&self) -> bool {
        self.frames.last().copied().unwrap_or(false)
    }
}

impl<CTX> Inspector<CTX, EthInterpreter> for ClassifyInspector {
    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let target = match self.frames.last() {
            None => true, // root frame — always target depth
            Some(false) => false,
            Some(true) => match inputs.scheme {
                CallScheme::DelegateCall | CallScheme::CallCode => true,
                CallScheme::Call => {
                    self.note(format!(
                        "regular CALL to {} at target depth (needs CALL replay)",
                        inputs.target_address
                    ));
                    false
                }
                CallScheme::StaticCall => false, // read-only — safely ignored
            },
        };
        self.frames.push(target);
        None
    }

    fn call_end(&mut self, _context: &mut CTX, inputs: &CallInputs, outcome: &mut CallOutcome) {
        let Some(was_target) = self.frames.pop() else {
            return;
        };
        // A reverted transparent frame (delegatecall/callcode) rolls its ops
        // back out of the net diff while the replay-script path still
        // extracts them — same rule as classify_prestate_eligibility. The
        // root frame's status is judged from the ExecutionResult instead.
        if was_target && !self.frames.is_empty() && !outcome.result.result.is_ok() {
            self.note(format!(
                "reverted {:?} at target depth ({:?}); rolled-back ops need the replay-script path",
                inputs.scheme, outcome.result.result
            ));
        }
    }

    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        if self.frames.last().copied().unwrap_or(true) {
            self.note(
                "CREATE/CREATE2 at target depth (contract creation is not representable)"
                    .to_string(),
            );
        }
        self.frames.push(false);
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.frames.pop();
    }

    fn selfdestruct(&mut self, _contract: Address, _target: Address, _value: U256) {
        if self.in_target_frame() {
            self.note("SELFDESTRUCT at target depth (not representable)".to_string());
        }
    }

    fn log(&mut self, _interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX, log: Log) {
        if self.in_target_frame() {
            self.logs
                .push(log_to_update(log.data.topics(), log.data.data.clone()));
        }
    }
}

/// The pure-classification twin of
/// [`gas_analyzer_core::classify_prestate_eligibility`], over local-execution
/// artifacts instead of tracer frames: execution result ⇔ root frame error,
/// finalized state diff ⇔ prestate diff, inspector notes ⇔ call-tree scan.
fn classify_local(
    result: &ExecutionResult,
    state: &EvmState,
    inspector: &ClassifyInspector,
    consumer: Address,
) -> PrestateEligibility {
    if !result.is_success() {
        let why = match result {
            ExecutionResult::Revert { .. } => "execution reverted".to_string(),
            ExecutionResult::Halt { reason, .. } => format!("halted: {reason:?}"),
            ExecutionResult::Success { .. } => unreachable!(),
        };
        return PrestateEligibility::Fallback(format!(
            "call reverted/failed ({why}); replay-script extraction of rolled-back ops can't be \
             mirrored by a net diff"
        ));
    }
    for (address, account) in state {
        if *address != consumer
            && account
                .storage
                .values()
                .any(|slot| slot.present_value != slot.original_value)
        {
            return PrestateEligibility::Fallback(format!(
                "non-consumer account {address} changed storage (needs CALL replay)"
            ));
        }
    }
    if let Some(reason) = &inspector.fallback {
        return PrestateEligibility::Fallback(reason.clone());
    }
    PrestateEligibility::Eligible
}

/// Build the eligible-path updates from the finalized state: the consumer's
/// net storage diff (changed slots only, slot-sorted — the exact set and
/// order `build_state_updates_from_prestate` derives from a prestate diff)
/// followed by the target-depth logs in emission order.
fn build_updates_from_state_diff(
    consumer: Address,
    state: &EvmState,
    logs: Vec<StateUpdate>,
) -> Vec<StateUpdate> {
    let mut stores: Vec<(B256, B256)> = state
        .get(&consumer)
        .map(|account| {
            account
                .storage
                .iter()
                .filter(|(_, slot)| slot.present_value != slot.original_value)
                .map(|(key, slot)| (B256::from(*key), B256::from(slot.present_value)))
                .collect()
        })
        .unwrap_or_default();
    stores.sort_by(|a, b| a.0.cmp(&b.0));

    let mut updates: Vec<StateUpdate> = stores
        .into_iter()
        .map(|(slot, value)| StateUpdate::Store(IStateUpdateTypes::Store { slot, value }))
        .collect();
    updates.extend(logs);
    updates
}

// ============================================================================
// Pass 2: replay-script inspector (struct-log equivalent)
// ============================================================================

/// Pass-2 inspector: captures the replay script exactly as
/// `gas_analyzer_core::compute_state_updates` extracts it from a struct log.
///
/// Ops are captured **pre-execution** (in `step`, like a struct-log entry) at
/// target depth only, in execution order, revert-unaware: a `CALL` is
/// captured as a replayable op and its whole subtree excluded; `STATICCALL`
/// and initcode subtrees are excluded; `DELEGATECALL`/`CALLCODE` are
/// transparent; SELFDESTRUCT/TSTORE are ignored (mirroring the struct-log
/// path, whose op filter drops them before its skip bookkeeping).
#[derive(Debug, Default)]
struct ReplayScriptInspector {
    frames: Vec<bool>,
    updates: Vec<StateUpdate>,
}

impl ReplayScriptInspector {
    fn in_target_frame(&self) -> bool {
        self.frames.last().copied().unwrap_or(false)
    }
}

/// Copy `[offset, offset+len)` of the current frame's memory, zero-padding
/// past the current size — byte-identical to `gas_analyzer_core::copy_memory`
/// over a struct-log memory snapshot (which is also captured pre-op).
fn copy_frame_memory(interp: &Interpreter<EthInterpreter>, offset: U256, len: U256) -> Vec<u8> {
    let Ok(len) = usize::try_from(len) else {
        // A length that overflows usize can only belong to an instruction
        // about to fail; the struct-log path would abort on it as well.
        return Vec::new();
    };
    if len == 0 {
        return Vec::new();
    }
    let mut out = vec![0u8; len];
    if let Ok(offset) = usize::try_from(offset) {
        let mem_len = interp.memory.size();
        if offset < mem_len {
            let copy = len.min(mem_len - offset);
            out[..copy].copy_from_slice(&interp.memory.slice(offset..offset + copy));
        }
    }
    out
}

impl<CTX> Inspector<CTX, EthInterpreter> for ReplayScriptInspector {
    fn step(&mut self, interp: &mut Interpreter<EthInterpreter>, _context: &mut CTX) {
        if !self.in_target_frame() {
            return;
        }
        let op = interp.bytecode.opcode();
        match op {
            opcode::SSTORE => {
                let (Ok(slot), Ok(value)) = (interp.stack.peek(0), interp.stack.peek(1)) else {
                    return;
                };
                self.updates
                    .push(StateUpdate::Store(IStateUpdateTypes::Store {
                        slot: slot.into(),
                        value: value.into(),
                    }));
            }
            opcode::LOG0..=opcode::LOG4 => {
                let topic_count = (op - opcode::LOG0) as usize;
                let (Ok(offset), Ok(len)) = (interp.stack.peek(0), interp.stack.peek(1)) else {
                    return;
                };
                let mut topics = [B256::ZERO; 4];
                for (i, topic) in topics.iter_mut().enumerate().take(topic_count) {
                    let Ok(value) = interp.stack.peek(2 + i) else {
                        return;
                    };
                    *topic = value.into();
                }
                let data: Bytes = copy_frame_memory(interp, offset, len).into();
                self.updates.push(match topic_count {
                    0 => StateUpdate::Log0(IStateUpdateTypes::Log0 { data }),
                    1 => StateUpdate::Log1(IStateUpdateTypes::Log1 {
                        data,
                        topic1: topics[0],
                    }),
                    2 => StateUpdate::Log2(IStateUpdateTypes::Log2 {
                        data,
                        topic1: topics[0],
                        topic2: topics[1],
                    }),
                    3 => StateUpdate::Log3(IStateUpdateTypes::Log3 {
                        data,
                        topic1: topics[0],
                        topic2: topics[1],
                        topic3: topics[2],
                    }),
                    _ => StateUpdate::Log4(IStateUpdateTypes::Log4 {
                        data,
                        topic1: topics[0],
                        topic2: topics[1],
                        topic3: topics[2],
                        topic4: topics[3],
                    }),
                });
            }
            opcode::CALL => {
                // Stack (top-first): gas, address, value, argsOffset, argsLength.
                let (Ok(address), Ok(value)) = (interp.stack.peek(1), interp.stack.peek(2)) else {
                    return;
                };
                let (Ok(args_offset), Ok(args_len)) = (interp.stack.peek(3), interp.stack.peek(4))
                else {
                    return;
                };
                let callargs: Bytes = copy_frame_memory(interp, args_offset, args_len).into();
                self.updates
                    .push(StateUpdate::Call(IStateUpdateTypes::Call {
                        target: Address::from_word(address.into()),
                        value,
                        callargs,
                    }));
            }
            opcode::CREATE => {
                // Stack (top-first): value, offset, size.
                let (Ok(value), Ok(offset), Ok(size)) = (
                    interp.stack.peek(0),
                    interp.stack.peek(1),
                    interp.stack.peek(2),
                ) else {
                    return;
                };
                let initcode: Bytes = copy_frame_memory(interp, offset, size).into();
                self.updates
                    .push(StateUpdate::Create(IStateUpdateTypes::Create {
                        value,
                        initcode,
                    }));
            }
            opcode::CREATE2 => {
                // Stack (top-first): value, offset, size, salt.
                let (Ok(value), Ok(offset), Ok(size), Ok(salt)) = (
                    interp.stack.peek(0),
                    interp.stack.peek(1),
                    interp.stack.peek(2),
                    interp.stack.peek(3),
                ) else {
                    return;
                };
                let initcode: Bytes = copy_frame_memory(interp, offset, size).into();
                self.updates
                    .push(StateUpdate::Create2(IStateUpdateTypes::Create2 {
                        salt: salt.into(),
                        value,
                        initcode,
                    }));
            }
            _ => {}
        }
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let target = match self.frames.last() {
            None => true, // root
            Some(false) => false,
            // A captured CALL's subtree is excluded wholesale; only the
            // transparent schemes extend target depth.
            Some(true) => matches!(
                inputs.scheme,
                CallScheme::DelegateCall | CallScheme::CallCode
            ),
        };
        self.frames.push(target);
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.frames.pop();
    }

    fn create(&mut self, _context: &mut CTX, _inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        // The Create/Create2 op was already captured at step time; initcode
        // runs one level below target depth and is excluded.
        self.frames.push(false);
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.frames.pop();
    }
}

// ============================================================================
// Trace runner + extraction dispatcher
// ============================================================================

/// One in-process execution of the tracked call: fresh `CacheDB` over the
/// overlay-aware view of `db`, env mirroring the RPC trace request, chosen
/// inspector attached. Returns the execution result, the finalized state and
/// the inspector for post-processing. Nothing is committed anywhere.
fn run_pass<DB, INSP>(
    db: DB,
    overlay: OverlayMountSet,
    env: &LocalBlockEnv,
    tx: &LocalTxRequest,
    profile: SimProfile,
    inspector: INSP,
) -> Result<(ExecutionResult, EvmState, INSP)>
where
    DB: DatabaseRef,
    DB::Error: core::fmt::Debug,
    INSP: for<'a> Inspector<
            Context<
                revm::context::BlockEnv,
                TxEnv,
                revm::context::CfgEnv,
                CacheDB<OverlayStateDb<DB>>,
            >,
            EthInterpreter,
        >,
{
    let (block_gas_limit, tx_gas_limit) = resolve_gas_limits(env, tx, profile);

    let cache_db = CacheDB::new(OverlayStateDb::new_multi(db, overlay));
    let ctx = Context::mainnet()
        .with_db(cache_db)
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = env.chain_id;
            cfg.spec = env.spec;
            // debug_traceCall / eth_call semantics: the node fills defaults
            // and skips fee enforcement for simulated calls.
            cfg.disable_nonce_check = true;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
            cfg.disable_fee_charge = true;
            // A simulated call is not subject to block building constraints
            // (and the unbounded profiles require the cap lifted server-side:
            // `anvil --disable-block-gas-limit`, `geth --rpc.gascap=0`).
            cfg.disable_block_gas_limit = true;
            if profile.tx_gas_limit_override().is_some() {
                // The unbounded profiles deliberately ignore EIP-7825's 2^24
                // per-tx cap (see sim_profile docs); without this, an Osaka
                // spec would clamp the pinned 2^40/2^43 limits.
                cfg.tx_gas_limit_cap = Some(u64::MAX);
            }
        })
        .modify_block_chained(|block| {
            block.number = U256::from(env.number);
            block.timestamp = U256::from(env.timestamp);
            block.gas_limit = block_gas_limit;
            block.beneficiary = env.coinbase;
            block.prevrandao = Some(env.prevrandao);
            block.basefee = env.basefee;
            block.difficulty = env.difficulty;
        });

    let mut evm = ctx.build_mainnet_with_inspector(inspector);
    let tx_env = build_tx_env(tx, tx_gas_limit)?;
    let outcome = evm
        .inspect_tx(tx_env)
        .map_err(|e| anyhow!("local trace execution failed: {e:?}"))?;
    Ok((outcome.result, outcome.state, evm.inspector))
}

/// Extract `(state_updates, skipped_opcodes)` for a call entirely in-process —
/// the local twin of the RPC path's hybrid dispatcher, with identical
/// dispatch rules and byte-identical outputs.
///
/// Blocking: `db` misses hit the network synchronously (SharedBackend in
/// `Block` mode); call from `spawn_blocking` (see
/// [`extract_state_updates_local`]).
pub(crate) fn extract_state_updates_local_blocking<DB>(
    db: DB,
    overlay: OverlayMountSet,
    env: &LocalBlockEnv,
    tx: &LocalTxRequest,
    profile: SimProfile,
    consumer: Address,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>)>
where
    DB: DatabaseRef + Clone,
    DB::Error: core::fmt::Debug,
{
    // Pass 1 — cheap classification run (full interpreter speed, no step
    // hook): the prestateTracer + callTracer equivalent in ONE execution
    // where the RPC fast path costs two.
    let (result, state, inspector) = run_pass(
        db.clone(),
        overlay.clone(),
        env,
        tx,
        profile,
        ClassifyInspector::default(),
    )?;

    match classify_local(&result, &state, &inspector, consumer) {
        PrestateEligibility::Eligible => Ok((
            build_updates_from_state_diff(consumer, &state, inspector.logs),
            HashSet::new(),
        )),
        PrestateEligibility::Fallback(reason) => {
            tracing::debug!(reason = %reason, "local fast path not eligible; running replay-script pass");
            // Pass 2 — struct-log-equivalent capture. Re-executes against the
            // (now warm) shared backend, exactly as the RPC path re-traces
            // with the struct-log tracer on fallback.
            let (_result, _state, inspector) = run_pass(
                db,
                overlay,
                env,
                tx,
                profile,
                ReplayScriptInspector::default(),
            )?;
            // skipped_opcodes deliberately empty — see module docs.
            Ok((inspector.updates, HashSet::new()))
        }
    }
}

/// Async wrapper: runs [`extract_state_updates_local_blocking`] on the
/// blocking pool with the backend switched to plain-blocking mode.
pub(crate) async fn extract_state_updates_local(
    backend: SharedBackend,
    overlay: OverlayMountSet,
    env: LocalBlockEnv,
    tx: LocalTxRequest,
    profile: SimProfile,
    consumer: Address,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>)> {
    let backend = backend.with_blocking_mode(BlockingMode::Block);
    tokio::task::spawn_blocking(move || {
        extract_state_updates_local_blocking(backend, overlay, &env, &tx, profile, consumer)
    })
    .await
    .map_err(|e| anyhow!("local extraction task panicked: {e}"))?
}

// ============================================================================
// Tests (pure revm — no anvil, no network)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;
    use gas_analyzer_core::encode_state_updates_to_abi;
    use revm::database::{CacheDB as SeedDb, EmptyDB};
    use revm::state::{AccountInfo, Bytecode};

    const CALLER: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    fn test_env() -> LocalBlockEnv {
        LocalBlockEnv {
            chain_id: 31_337,
            spec: SpecId::PRAGUE,
            number: 100,
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            coinbase: address!("0x00000000000000000000000000000000c01ba5e0"),
            prevrandao: B256::ZERO,
            basefee: 0,
            difficulty: U256::ZERO,
        }
    }

    fn test_tx(to: Address) -> LocalTxRequest {
        LocalTxRequest {
            from: CALLER,
            to,
            input: Bytes::new(),
            value: U256::ZERO,
            gas: Some(3_000_000),
            gas_price: 0,
            nonce: None,
            access_list: Default::default(),
            authorization_list: Default::default(),
        }
    }

    fn hex_code(spaced_hex: &str) -> Bytes {
        Bytes::from(alloy::hex::decode(spaced_hex.replace(' ', "")).expect("valid hex"))
    }

    fn topic_hex(last: u8) -> String {
        format!("{}{last:02x}", "00".repeat(31))
    }

    fn addr_hex(a: Address) -> String {
        alloy::hex::encode(a.as_slice())
    }

    fn seed_db() -> SeedDb<EmptyDB> {
        SeedDb::new(EmptyDB::default())
    }

    fn set_code(db: &mut SeedDb<EmptyDB>, addr: Address, code: Bytes) {
        db.insert_account_info(
            addr,
            AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash: alloy::primitives::keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
            },
        );
    }

    fn extract(
        db: &SeedDb<EmptyDB>,
        consumer: Address,
        profile: SimProfile,
    ) -> (Vec<StateUpdate>, HashSet<Opcode>) {
        extract_state_updates_local_blocking(
            db.clone(),
            OverlayMountSet::default(),
            &test_env(),
            &test_tx(consumer),
            profile,
            consumer,
        )
        .expect("local extraction")
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

    fn assert_updates_eq(actual: &[StateUpdate], expected: &[StateUpdate], ctx: &str) {
        assert_eq!(
            encode_state_updates_to_abi(actual),
            encode_state_updates_to_abi(expected),
            "{ctx}:\n  actual:   {actual:?}\n  expected: {expected:?}"
        );
    }

    /// SSTORE(1,0x42); MSTORE(0,0xab); LOG1(mem[0..32], topic 0xaa);
    /// SSTORE(2,5); SSTORE(2,0) — net zero; SSTORE(5,0) — zeroes a seeded
    /// slot; STOP. (Same program as the RPC-path anvil test.)
    fn eligible_code() -> Bytes {
        hex_code(&format!(
            "6042600155 60ab600052 7f{}60206000a1 6005600255 6000600255 6000600555 00",
            topic_hex(0xaa),
        ))
    }

    #[test]
    fn eligible_call_yields_net_slot_sorted_stores_then_logs() {
        let mut db = seed_db();
        let consumer = address!("0x0000000000000000000000000000000000001001");
        set_code(&mut db, consumer, eligible_code());
        db.insert_account_storage(consumer, U256::from(5u64), U256::from(0x99u64))
            .expect("seed slot 5");

        let (updates, skipped) = extract(&db, consumer, SimProfile::Chain);
        assert!(skipped.is_empty());
        let word_ab = Bytes::copy_from_slice(B256::with_last_byte(0xab).as_slice());
        assert_updates_eq(
            &updates,
            &[store_up(1, 0x42), store_up(5, 0), log1_up(0xaa, word_ab)],
            "fast path: net stores (slot-sorted, net-zero slot 2 omitted, zeroing kept), then logs",
        );
    }

    /// Root: LOG(aa); DELEGATECALL(lib); LOG(bb). Lib: LOG(cc); SSTORE(3,7).
    /// True order aa, cc, bb; the delegate store lands on the root.
    #[test]
    fn delegatecall_logs_interleave_in_emission_order() {
        let mut db = seed_db();
        let root = address!("0x0000000000000000000000000000000000001002");
        let lib = address!("0x0000000000000000000000000000000000001003");
        set_code(
            &mut db,
            root,
            hex_code(&format!(
                "7f{}60006000a1 6000600060006000 73{} 5af450 7f{}60006000a1 00",
                topic_hex(0xaa),
                addr_hex(lib),
                topic_hex(0xbb),
            )),
        );
        set_code(
            &mut db,
            lib,
            hex_code(&format!("7f{}60006000a1 6007600355 00", topic_hex(0xcc))),
        );

        let (updates, _) = extract(&db, root, SimProfile::Chain);
        assert_updates_eq(
            &updates,
            &[
                store_up(3, 7),
                log1_up(0xaa, Bytes::new()),
                log1_up(0xcc, Bytes::new()),
                log1_up(0xbb, Bytes::new()),
            ],
            "delegatecall store on root, logs interleaved aa, cc, bb",
        );
    }

    /// Regular CALL forces the replay-script pass: a replayable CALL op (the
    /// callee's internals excluded) plus the consumer's own store.
    #[test]
    fn regular_call_falls_back_to_replay_script() {
        let mut db = seed_db();
        let caller = address!("0x0000000000000000000000000000000000001004");
        let callee = address!("0x0000000000000000000000000000000000001005");
        set_code(
            &mut db,
            caller,
            hex_code(&format!(
                "60006000600060006000 73{} 5af150 6001600455 00",
                addr_hex(callee),
            )),
        );
        set_code(&mut db, callee, hex_code("6009600155 00"));

        let (updates, _) = extract(&db, caller, SimProfile::Chain);
        assert_updates_eq(
            &updates,
            &[
                StateUpdate::Call(IStateUpdateTypes::Call {
                    target: callee,
                    value: U256::ZERO,
                    callargs: Bytes::new(),
                }),
                store_up(4, 1),
            ],
            "fallback output: CALL op then own store",
        );
    }

    /// Value + args CALL: MSTORE(0, ..deadbeef); CALL(callee, value 5,
    /// args mem[28..32]); SSTORE(4,1). The consumer is funded so the value
    /// transfer succeeds; the callee's own SSTORE stays excluded.
    #[test]
    fn call_op_carries_value_and_callargs() {
        let mut db = seed_db();
        let caller = address!("0x0000000000000000000000000000000000001008");
        let callee = address!("0x0000000000000000000000000000000000001005");
        set_code(
            &mut db,
            caller,
            hex_code(&format!(
                "63deadbeef 600052 6000 6000 6004 601c 6005 73{} 61ffff f150 6001600455 00",
                addr_hex(callee),
            )),
        );
        // Fund the consumer so CALL's value transfer is well-defined.
        let mut info = db.basic_ref(caller).unwrap().unwrap();
        info.balance = U256::from(1_000_000u64);
        db.insert_account_info(caller, info);
        set_code(&mut db, callee, hex_code("6009600155 00"));

        let (updates, _) = extract(&db, caller, SimProfile::Chain);
        assert_updates_eq(
            &updates,
            &[
                StateUpdate::Call(IStateUpdateTypes::Call {
                    target: callee,
                    value: U256::from(5u64),
                    callargs: Bytes::from(vec![0xde, 0xad, 0xbe, 0xef]),
                }),
                store_up(4, 1),
            ],
            "CALL op must carry value and the args bytes copied from memory",
        );
    }

    /// A reverted root still yields the pre-revert ops (revert-unaware
    /// struct-log semantics).
    #[test]
    fn reverted_call_extracts_rolled_back_ops() {
        let mut db = seed_db();
        let reverter = address!("0x0000000000000000000000000000000000001006");
        set_code(
            &mut db,
            reverter,
            hex_code(&format!(
                "6042600155 7f{}60006000a1 60006000fd",
                topic_hex(0xdd)
            )),
        );

        let (updates, _) = extract(&db, reverter, SimProfile::Chain);
        assert_updates_eq(
            &updates,
            &[store_up(1, 0x42), log1_up(0xdd, Bytes::new())],
            "reverted root: ops captured pre-revert",
        );
    }

    /// A caught reverted DELEGATECALL: rolled-back child ops are still
    /// extracted at target depth (plus the parent's store) — the exact case
    /// the net diff cannot represent.
    #[test]
    fn caught_reverted_delegatecall_keeps_rolled_back_ops() {
        let mut db = seed_db();
        let catcher = address!("0x0000000000000000000000000000000000001007");
        let reverter = address!("0x0000000000000000000000000000000000001006");
        set_code(
            &mut db,
            catcher,
            hex_code(&format!(
                "6000600060006000 73{} 5af450 6022600255 00",
                addr_hex(reverter),
            )),
        );
        set_code(
            &mut db,
            reverter,
            hex_code(&format!(
                "6042600155 7f{}60006000a1 60006000fd",
                topic_hex(0xdd)
            )),
        );

        let (updates, _) = extract(&db, catcher, SimProfile::Chain);
        assert_updates_eq(
            &updates,
            &[
                store_up(1, 0x42),
                log1_up(0xdd, Bytes::new()),
                store_up(2, 0x22),
            ],
            "rolled-back delegatecall ops + parent store, in execution order",
        );
    }

    /// CREATE at target depth: captured as a replayable Create op with the
    /// initcode copied from memory; the initcode's own SSTORE excluded.
    #[test]
    fn create_captured_with_initcode() {
        let mut db = seed_db();
        let creator = address!("0x0000000000000000000000000000000000001009");
        // PUSH6 initcode; PUSH1 0 MSTORE (right-aligned at bytes 26..32);
        // CREATE(value 0, offset 26, size 6); POP; STOP.
        // initcode 600160015500 = SSTORE(1,1); STOP.
        set_code(
            &mut db,
            creator,
            hex_code("65600160015500 600052 6006 601a 6000 f050 00"),
        );

        let (updates, _) = extract(&db, creator, SimProfile::Chain);
        assert_updates_eq(
            &updates,
            &[StateUpdate::Create(IStateUpdateTypes::Create {
                value: U256::ZERO,
                initcode: Bytes::from(vec![0x60, 0x01, 0x60, 0x01, 0x55, 0x00]),
            })],
            "CREATE captured with initcode from memory; constructor internals excluded",
        );
    }

    /// ~40M-gas busy loop then one SSTORE + LOG1: OOGs under Chain (3M tx
    /// gas), completes under the pinned UnboundedV1/V1Xl overrides and
    /// reduces to the single-slot payload — the profile-mirroring test.
    #[test]
    fn unbounded_profiles_lift_gas_beyond_chain_limits() {
        let mut db = seed_db();
        let burner = address!("0x0000000000000000000000000000000000002001");
        set_code(
            &mut db,
            burner,
            hex_code(&format!(
                "620f4240 5b 80 15 6011 57 6001 90 03 6004 56 5b 50 6042600155 7f{}60006000a1 00",
                topic_hex(0xee),
            )),
        );

        // Chain profile: 3M tx gas stands → OOG halt → replay-script pass,
        // which extracts nothing (the loop dies before the SSTORE).
        let (chain_updates, _) = extract(&db, burner, SimProfile::Chain);
        assert_updates_eq(
            &chain_updates,
            &[],
            "OOG under Chain: no ops reached, replay script is empty",
        );

        // Both unbounded tiers: pinned override replaces the request's 3M.
        for profile in [SimProfile::UnboundedV1, SimProfile::UnboundedV1Xl] {
            let (updates, skipped) = extract(&db, burner, profile);
            assert!(skipped.is_empty());
            assert_updates_eq(
                &updates,
                &[store_up(1, 0x42), log1_up(0xee, Bytes::new())],
                "40M gas of compute must reduce to one Store and one Log",
            );
        }
    }

    /// The env mirroring: profile overrides win over the request gas and the
    /// header limit; Chain leaves them alone.
    #[test]
    fn resolve_gas_limits_mirrors_apply_sim_profile() {
        let env = test_env();
        let tx = test_tx(Address::ZERO);

        assert_eq!(
            resolve_gas_limits(&env, &tx, SimProfile::Chain),
            (30_000_000, 3_000_000),
            "Chain: header block limit, request tx gas"
        );
        let mut no_gas = tx.clone();
        no_gas.gas = None;
        assert_eq!(
            resolve_gas_limits(&env, &no_gas, SimProfile::Chain),
            (30_000_000, 30_000_000),
            "Chain without request gas: fall back to the block limit"
        );
        assert_eq!(
            resolve_gas_limits(&env, &tx, SimProfile::UnboundedV1),
            (1 << 40, 1 << 40),
            "UnboundedV1: pinned 2^40 overrides both, ignoring request gas"
        );
        assert_eq!(
            resolve_gas_limits(&env, &tx, SimProfile::UnboundedV1Xl),
            (1 << 43, 1 << 43),
            "UnboundedV1Xl: pinned 2^43 tier"
        );
    }

    /// Overlay mount participates in local execution: a consumer that
    /// EXTCODECOPYs a chunk and commits one word of it must see the mounted
    /// bytes without any chunk account existing in the underlying DB.
    #[test]
    fn overlay_chunk_readable_via_extcodecopy() {
        let payload: Vec<u8> = (0..100u32).map(|i| (i % 256) as u8).collect();
        let env = OverlayEnv::from_blobs(&payload, b"tok").expect("overlay env");
        let mount = Arc::new(OverlayMount::from_env(&env, env.manifest).expect("mount"));
        let chunk = env.overlays[0].address;

        let mut db = seed_db();
        let consumer = address!("0x000000000000000000000000000000000000200a");
        // EXTCODECOPY(chunk, dest 0, offset 1, size 32); SSTORE(1, MLOAD(0));
        // LOG1(mem[0..32], topic 0xee); STOP.
        set_code(
            &mut db,
            consumer,
            hex_code(&format!(
                "6020 6001 6000 73{} 3c 600051600155 7f{}60206000a1 00",
                addr_hex(chunk),
                topic_hex(0xee),
            )),
        );

        let (updates, _) = extract_state_updates_local_blocking(
            db,
            mount.into(),
            &test_env(),
            &test_tx(consumer),
            SimProfile::Chain,
            consumer,
        )
        .expect("local extraction with overlay");

        // Chunk code is 0x00 || payload, so offset 1..33 is payload[0..32].
        let expected_word = B256::from_slice(&payload[0..32]);
        assert_updates_eq(
            &updates,
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
            "overlay bytes must be readable through EXTCODECOPY and commit correctly",
        );
    }

    /// TWO models mounted simultaneously (multi-overlay): a consumer that
    /// EXTCODECOPYs one chunk from each model and commits both words must see
    /// each model's own bytes — the composite lookup resolves both manifests'
    /// disjoint address sets in a single execution, with no chunk account
    /// existing in the underlying DB for either model.
    #[test]
    fn two_model_mount_set_serves_both_models_in_one_call() {
        let payload_a: Vec<u8> = (0..100u32).map(|i| (i % 256) as u8).collect();
        let payload_b: Vec<u8> = (0..100u32).map(|i| ((i * 7 + 3) % 253) as u8).collect();
        let env_a = OverlayEnv::from_blobs(&payload_a, b"tok-a").expect("overlay env a");
        let env_b = OverlayEnv::from_blobs(&payload_b, b"tok-b").expect("overlay env b");
        assert_ne!(env_a.manifest, env_b.manifest);
        let chunk_a = env_a.overlays[0].address;
        let chunk_b = env_b.overlays[0].address;

        let mounts: OverlayMountSet = [
            Arc::new(OverlayMount::from_env(&env_a, env_a.manifest).expect("mount a")),
            Arc::new(OverlayMount::from_env(&env_b, env_b.manifest).expect("mount b")),
        ]
        .into_iter()
        .collect();

        let mut db = seed_db();
        let consumer = address!("0x000000000000000000000000000000000000200b");
        // EXTCODECOPY(chunkA, dest 0, offset 1, size 32); SSTORE(1, MLOAD(0));
        // EXTCODECOPY(chunkB, dest 0, offset 1, size 32); SSTORE(2, MLOAD(0));
        // LOG1(mem[0..32], topic 0xee); STOP.
        set_code(
            &mut db,
            consumer,
            hex_code(&format!(
                "6020 6001 6000 73{} 3c 600051600155 6020 6001 6000 73{} 3c 600051600255 7f{}60206000a1 00",
                addr_hex(chunk_a),
                addr_hex(chunk_b),
                topic_hex(0xee),
            )),
        );

        let (updates, _) = extract_state_updates_local_blocking(
            db,
            mounts,
            &test_env(),
            &test_tx(consumer),
            SimProfile::Chain,
            consumer,
        )
        .expect("local extraction with two-model overlay set");

        let word_a = B256::from_slice(&payload_a[0..32]);
        let word_b = B256::from_slice(&payload_b[0..32]);
        assert_ne!(
            word_a, word_b,
            "distinct payloads required for the test to bite"
        );
        assert_updates_eq(
            &updates,
            &[
                StateUpdate::Store(IStateUpdateTypes::Store {
                    slot: B256::with_last_byte(1),
                    value: word_a,
                }),
                StateUpdate::Store(IStateUpdateTypes::Store {
                    slot: B256::with_last_byte(2),
                    value: word_b,
                }),
                StateUpdate::Log1(IStateUpdateTypes::Log1 {
                    data: Bytes::copy_from_slice(word_b.as_slice()),
                    topic1: B256::with_last_byte(0xee),
                }),
            ],
            "each model's chunk must resolve to its own bytes through the composite mount set",
        );
    }

    /// `overlay_mount_set_from_files` builds each model once (manifest-keyed
    /// memoization) and refuses duplicate manifests in one spec list.
    #[test]
    fn mount_set_from_files_memoizes_and_rejects_duplicates() {
        use std::io::Write as _;

        let payload_a: Vec<u8> = (0..300u32).map(|i| (i % 251) as u8).collect();
        let payload_b: Vec<u8> = (0..300u32).map(|i| ((i * 7 + 3) % 253) as u8).collect();
        let env_a = OverlayEnv::from_blobs(&payload_a, b"tok-a").expect("env a");
        let env_b = OverlayEnv::from_blobs(&payload_b, b"tok-b").expect("env b");

        let dir = tempfile::tempdir().expect("tempdir");
        let write = |name: &str, bytes: &[u8]| {
            let path = dir.path().join(name);
            let mut f = std::fs::File::create(&path).expect("create blob");
            f.write_all(bytes).expect("write blob");
            path
        };
        let wa = write("wa.bin", &payload_a);
        let ta = write("ta.bin", b"tok-a");
        let wb = write("wb.bin", &payload_b);
        let tb = write("tb.bin", b"tok-b");

        let cache = LocalStateCache::default();
        let specs = vec![
            (wa.clone(), ta.clone(), env_a.manifest),
            (wb.clone(), tb.clone(), env_b.manifest),
        ];
        let set = cache
            .overlay_mount_set_from_files(&specs)
            .expect("two-model set");
        assert_eq!(set.len(), 2);
        assert_eq!(set.manifests(), vec![env_a.manifest, env_b.manifest]);

        // Second build returns the memoized mounts (same Arcs).
        let again = cache
            .overlay_mount_set_from_files(&specs)
            .expect("memoized set");
        assert_eq!(again.manifests(), set.manifests());

        // Duplicate manifest in one spec list is refused loudly.
        let dup = vec![
            (wa.clone(), ta.clone(), env_a.manifest),
            (wa, ta, env_a.manifest),
        ];
        let err = cache
            .overlay_mount_set_from_files(&dup)
            .expect_err("duplicate manifest must be refused");
        assert!(err.to_string().contains("duplicate overlay manifest"));
    }
}

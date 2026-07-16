//! `gk-fast-view` — the Phase-4 revmc AOT/JIT fast executor for overlay-mounted
//! EVM view calls.
//!
//! This is the productionized twin of the analyzer's revm-31 interpreter view
//! path (`evmsketch::call_view_local_multi` → `local_exec::call_view_local_blocking`
//! → `run_pass`). It reproduces that path's environment EXACTLY —
//! `eth_call`/`debug_traceCall` fee/nonce semantics, the pinned block env, the
//! profile gas overrides, the overlay-aware state db — but dispatches bytecode
//! execution through **revmc**: the fixed engine bytecode is JIT-compiled ONCE
//! (memoized by codehash), then every view call runs on the compiled artifact,
//! with the interpreter as the fallback for any un-compiled codehash (overlay
//! chunks are STOP-prefixed data, never executed as code, so they never need
//! compilation).
//!
//! ## Consensus contract
//!
//! Operators compare `keccak(returndata)`; gas may differ but **returndata must
//! not**. The proven [`revmc-harness`] spike established that the revmc-compiled
//! path is byte-identical to the revm-41 interpreter on returndata + gas +
//! halt-class. This crate closes the remaining gap — that the revm-41+revmc path
//! is byte-identical *to the revm-31 interpreter* on returndata — via the
//! golden-fixture differential test in `tests/consensus.rs` (see that file for
//! the cross-version methodology).
//!
//! Reverts and halts are LOUD errors (`Err`), never a silent empty output — a
//! committee member must surface a segment's `expectXIn` witness revert rather
//! than hash-commit an empty result (parity with `call_view_local_blocking`).

pub mod job;
pub mod overlay;

use std::time::{Duration, Instant};

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use revm_context::{BlockEnv, CfgEnv, Context, Journal, TxEnv};
use revm_context_interface::result::ExecutionResult;
use revm_database::{CacheDB, EmptyDB};
use revm_database_interface::{DBErrorMarker, Database, DatabaseRef};
use revm_handler::{ExecuteEvm, MainBuilder};
use revm_primitives::{
    Address, B256, Bytes, KECCAK_EMPTY, TxKind, U256, hardfork::SpecId, keccak256, map::B256Map,
};
use revmc::{EvmCompiler, EvmLlvmBackend};
use revmc_context::{JitEvm, RawEvmCompilerFn};

// Force-link the builtin symbols the JIT-compiled bytecode calls into
// (EXTCODECOPY, KECCAK256, ...). Same requirement as the harness.
use revmc_builtins as _;

pub use overlay::{
    CodeOverlay, OVERLAY_CHUNK_PAYLOAD, OverlayEnv, OverlayMount, OverlayMountSet, OverlayStateDb,
    WarmHandle, WarmStateDb, overlay_chunk_address, overlay_manifest_hash,
};

/// The concrete state stack the persistent warm cache wraps: the job's base
/// contracts (`CacheDB<EmptyDB>`) viewed through the overlay mount set, memoized
/// by a process-lifetime read-through cache. Immutable per key, so reusing it
/// across jobs is byte-safe.
type WarmBaseDb = WarmStateDb<OverlayStateDb<CacheDB<EmptyDB>>>;

/// Cache key for the persistent warm state: the mount set's manifests (which
/// commit to the exact overlay bytes) plus a fingerprint of the base-state db.
/// Same key ⇒ same immutable state ⇒ safe to reuse the resolved reads; any
/// difference (a different overlay OR different base contracts) forces a rebuild.
#[derive(Clone, PartialEq, Eq)]
struct WarmKey {
    manifests: Vec<B256>,
    base_fingerprint: B256,
}

// ============================================================================
// Profile — byte-identical gas overrides to gas_analyzer_core::SimProfile
// ============================================================================

/// Block gas limit for the `UnboundedV1` profile: 2^40 (mirrors
/// `UNBOUNDED_V1_BLOCK_GAS_LIMIT`).
pub const UNBOUNDED_V1_BLOCK_GAS_LIMIT: u64 = 1 << 40;
/// Tx gas limit for the `UnboundedV1` profile (equal to the block limit).
pub const UNBOUNDED_V1_TX_GAS_LIMIT: u64 = UNBOUNDED_V1_BLOCK_GAS_LIMIT;
/// Block gas limit for the `UnboundedV1Xl` profile: 2^43.
pub const UNBOUNDED_V1_XL_BLOCK_GAS_LIMIT: u64 = 1 << 43;
/// Tx gas limit for the `UnboundedV1Xl` profile (equal to the block limit).
pub const UNBOUNDED_V1_XL_TX_GAS_LIMIT: u64 = UNBOUNDED_V1_XL_BLOCK_GAS_LIMIT;

/// The EVM environment profile a view call is executed under. A faithful mirror
/// of `gas_analyzer_core::SimProfile`'s gas-override behaviour — the only part
/// of `SimProfile` that affects execution (the payload-shape validation it also
/// carries is a state-diff concern, irrelevant to a view call).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Profile {
    /// Mirror the real chain (request/header gas stands).
    #[default]
    Chain,
    /// Unbounded execution, 2^40 gas tier.
    UnboundedV1,
    /// Unbounded execution, 2^43 gas tier (Qwen3.5-35B-A3B inference).
    UnboundedV1Xl,
}

impl Profile {
    pub fn tx_gas_limit_override(&self) -> Option<u64> {
        match self {
            Profile::Chain => None,
            Profile::UnboundedV1 => Some(UNBOUNDED_V1_TX_GAS_LIMIT),
            Profile::UnboundedV1Xl => Some(UNBOUNDED_V1_XL_TX_GAS_LIMIT),
        }
    }
    pub fn block_gas_limit_override(&self) -> Option<u64> {
        match self {
            Profile::Chain => None,
            Profile::UnboundedV1 => Some(UNBOUNDED_V1_BLOCK_GAS_LIMIT),
            Profile::UnboundedV1Xl => Some(UNBOUNDED_V1_XL_BLOCK_GAS_LIMIT),
        }
    }
}

// ============================================================================
// Block env + tx request (mirror LocalBlockEnv / LocalTxRequest)
// ============================================================================

/// The pinned per-block environment a view call executes under — the same
/// fields `evmsketch::LocalBlockEnv` carries (the anchored header + chain spec).
#[derive(Debug, Clone)]
pub struct ViewEnv {
    pub chain_id: u64,
    pub spec: SpecId,
    pub number: u64,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub coinbase: Address,
    pub prevrandao: B256,
    pub basefee: u64,
    pub difficulty: U256,
}

impl Default for ViewEnv {
    fn default() -> Self {
        Self {
            chain_id: 1,
            spec: SpecId::CANCUN,
            number: 0,
            timestamp: 0,
            gas_limit: 30_000_000,
            coinbase: Address::ZERO,
            prevrandao: B256::ZERO,
            basefee: 0,
            difficulty: U256::ZERO,
        }
    }
}

/// The subset of a view-call request the executor consumes (mirror of
/// `evmsketch::local_exec::LocalTxRequest`).
#[derive(Debug, Clone)]
pub struct ViewTx {
    pub from: Address,
    pub to: Address,
    pub input: Bytes,
    pub value: U256,
    pub gas: Option<u64>,
    pub gas_price: u128,
    pub nonce: Option<u64>,
}

impl ViewTx {
    /// A gas-less, value-less call — the common segment shape.
    pub fn call(from: Address, to: Address, input: impl Into<Bytes>, gas: u64) -> Self {
        Self {
            from,
            to,
            input: input.into(),
            value: U256::ZERO,
            gas: Some(gas),
            gas_price: 0,
            nonce: None,
        }
    }
}

// ============================================================================
// FastView — compile-once-run-many executor
// ============================================================================

/// The revmc-backed fast executor. Holds the LLVM JIT compiler and a
/// codehash → compiled-fn map; the compiler owns the JIT'd machine code, so it
/// must outlive every call (guaranteed: both live in this struct).
///
/// A single [`FastView`] is pinned to one [`SpecId`] (the engine bytecode is
/// compiled for that spec's gas schedule). The seg engine is fixed per model
/// and the chain spec is fixed per deployment, so one `FastView` compiles the
/// engine once and serves every segment call — the compile-once-run-many win.
pub struct FastView {
    compiler: EvmCompiler<EvmLlvmBackend>,
    functions: B256Map<RawEvmCompilerFn>,
    spec: SpecId,
    /// Verified overlay mounts, memoized by manifest so `OverlayMount::from_files`'s
    /// streaming-keccak verify runs AT MOST ONCE per distinct overlay for this
    /// executor's lifetime. Multi-entry: a transiently-mounted model never evicts
    /// a resident one (mirrors the interpreter `LocalStateCache.overlay_mounts`).
    mount_cache: HashMap<B256, Arc<OverlayMount>>,
    /// The persistent, process-lifetime warm state cache (one live entry, keyed
    /// by `WarmKey`). Reused whenever the next job's overlay + base match, so the
    /// 2nd..Nth job resolves overlay chunk reads from RAM. Single-entry keeps it
    /// simple; the *expensive* verify is already amortized by `mount_cache`, so a
    /// key change only re-resolves (cheap) reads, never re-verifies.
    warm: Option<(WarmKey, Arc<WarmBaseDb>)>,
    /// Count of overlay mounts actually built (cache misses) — for the
    /// amortization test's deterministic proof that the verify ran once.
    mount_build_count: usize,
    /// Count of warm-state caches actually built (cache misses).
    warm_build_count: usize,
}

impl FastView {
    /// A new executor pinned to `spec`, with an empty compiled-fn cache.
    pub fn new(spec: SpecId) -> Result<Self> {
        let compiler =
            EvmCompiler::new_llvm(false).map_err(|e| anyhow!("EvmCompiler::new_llvm: {e}"))?;
        Ok(Self {
            compiler,
            functions: B256Map::default(),
            spec,
            mount_cache: HashMap::new(),
            warm: None,
            mount_build_count: 0,
            warm_build_count: 0,
        })
    }

    /// Number of distinct contract bytecodes compiled so far.
    pub fn compiled_count(&self) -> usize {
        self.functions.len()
    }

    /// Number of overlay mounts actually built via their `build` closure (cache
    /// misses). For an amortized daemon serving one overlay this is 1.
    pub fn mount_builds(&self) -> usize {
        self.mount_build_count
    }

    /// Number of warm-state caches actually built (cache misses). For an
    /// amortized daemon serving identical jobs this is 1.
    pub fn warm_builds(&self) -> usize {
        self.warm_build_count
    }

    /// Get-or-build a verified overlay mount, memoized by `manifest`. The
    /// manifest commits to the exact overlay bytes (`keccak(keccak(weights) ||
    /// keccak(tokenizer))`), so a cache hit is sound: identical manifest ⇒
    /// identical immutable overlay. `build` (which for the file-backed path runs
    /// the streaming-keccak verify over the whole weights blob) is invoked ONLY
    /// on a miss.
    pub fn mount_get_or_build<F>(&mut self, manifest: B256, build: F) -> Result<Arc<OverlayMount>>
    where
        F: FnOnce() -> Result<OverlayMount>,
    {
        if let Some(mount) = self.mount_cache.get(&manifest) {
            return Ok(mount.clone());
        }
        let mount = Arc::new(build()?);
        self.mount_build_count += 1;
        self.mount_cache.insert(manifest, mount.clone());
        Ok(mount)
    }

    /// Whether the bytecode with this codehash has been compiled.
    pub fn is_compiled(&self, codehash: &B256) -> bool {
        self.functions.contains_key(codehash)
    }

    /// AOT/JIT-compile `code` (idempotent, memoized by codehash) and register it
    /// for dispatch. Call this to pre-warm the engine bytecode before the first
    /// timed segment call. `code` must be the raw runtime bytecode (the bytes
    /// `EXTCODECOPY`/deploy would yield), NOT analyzed.
    pub fn warm_code(&mut self, code: &[u8]) -> Result<B256> {
        let codehash = keccak256(code);
        if !self.functions.contains_key(&codehash) {
            // Unique per-codehash name; only compiled once, so no name clash.
            let name = format!("engine_{codehash:x}");
            // SAFETY: the returned fn pointer is owned by `self.compiler`, which
            // outlives every use of `self.functions` (same struct). We never
            // call a compiled fn after the compiler is dropped.
            let f = unsafe { self.compiler.jit(&name, code, self.spec) }
                .map_err(|e| anyhow!("jit-compile engine bytecode ({codehash:x}): {e}"))?;
            self.functions.insert(codehash, f.into_inner());
        }
        Ok(codehash)
    }

    /// Execute a read-only call against `base` (the base-state [`DatabaseRef`],
    /// e.g. an in-memory seed or an RPC-backed lazy fetcher) with `mounts`
    /// consulted first (overlay chunk accounts), returning the call's **raw
    /// return data**. Reverts and halts are `Err`.
    ///
    /// The target contract's bytecode is auto-compiled on first sight (memoized
    /// by codehash) and dispatched through revmc; every other codehash falls
    /// back to the interpreter. This mirrors `call_view_local_blocking`'s
    /// environment (see `run_pass`) so the returndata is consensus-identical to
    /// the revm-31 interpreter path.
    pub fn call_view<DB>(
        &mut self,
        base: DB,
        mounts: OverlayMountSet,
        env: &ViewEnv,
        tx: &ViewTx,
        profile: Profile,
    ) -> Result<Bytes>
    where
        DB: DatabaseRef,
        DB::Error: DBErrorMarker + core::fmt::Debug,
    {
        self.call_view_inner(base, mounts, env, tx, profile, true)
    }

    /// Execute the view call through the **pure revm-41 interpreter** — the JIT
    /// is never consulted (the compiled-fn map handed to `JitEvm` is empty, so
    /// every codehash falls back to the interpreter).
    ///
    /// This is the consensus-gate reference twin of [`call_view`](Self::call_view):
    /// running the SAME real engine bytecode + overlay through both this and
    /// `call_view` isolates whether a divergence is a revmc codegen bug (the two
    /// differ) or a cross-revm-version / env difference (both differ from the
    /// revm-31 golden together). See `tests/real_engine.rs`.
    pub fn call_view_interpreted<DB>(
        &mut self,
        base: DB,
        mounts: OverlayMountSet,
        env: &ViewEnv,
        tx: &ViewTx,
        profile: Profile,
    ) -> Result<Bytes>
    where
        DB: DatabaseRef,
        DB::Error: DBErrorMarker + core::fmt::Debug,
    {
        self.call_view_inner(base, mounts, env, tx, profile, false)
    }

    fn call_view_inner<DB>(
        &mut self,
        base: DB,
        mounts: OverlayMountSet,
        env: &ViewEnv,
        tx: &ViewTx,
        profile: Profile,
        use_jit: bool,
    ) -> Result<Bytes>
    where
        DB: DatabaseRef,
        DB::Error: DBErrorMarker + core::fmt::Debug,
    {
        if env.spec != self.spec {
            return Err(anyhow!(
                "FastView compiled for spec {:?} but view env requests {:?}; \
                 spec is consensus-critical — build a FastView per spec",
                self.spec,
                env.spec
            ));
        }

        // Compose the overlay-aware state db, then resolve + compile the target
        // engine bytecode BEFORE moving the db into the CacheDB.
        let overlay_db = OverlayStateDb::new_multi(base, mounts);
        let target_code = fetch_code(&overlay_db, tx.to)
            .map_err(|e| anyhow!("resolve target {} code: {e:?}", tx.to))?;
        if use_jit {
            if let Some(code) = &target_code {
                if !code.is_empty() {
                    self.warm_code(code)?;
                }
            }
        }

        let (block_gas_limit, tx_gas_limit) = resolve_gas_limits(env, tx, profile);
        let cache_db = CacheDB::new(overlay_db);
        self.dispatch_view(cache_db, env, block_gas_limit, tx, tx_gas_limit, profile, use_jit)
    }

    /// The consensus-critical transact + returndata extraction, factored out so
    /// the fresh-per-call ([`call_view_inner`](Self::call_view_inner)) and the
    /// warm-cache ([`call_view_warm`](Self::call_view_warm)) paths share ONE
    /// implementation — they differ only in the `Database` handed in, never in
    /// how execution is run or how the result is turned into returndata.
    ///
    /// Uses `transact` (NOT `transact_commit`): a view call returns/reverts
    /// without committing, so nothing is written back to `cache_db` — the reason
    /// a persistent read-through cache underneath is byte-safe.
    fn dispatch_view<D>(
        &self,
        cache_db: D,
        env: &ViewEnv,
        block_gas_limit: u64,
        tx: &ViewTx,
        tx_gas_limit: u64,
        profile: Profile,
        use_jit: bool,
    ) -> Result<Bytes>
    where
        D: Database,
        D::Error: DBErrorMarker + core::fmt::Debug,
    {
        let inner = build_ctx(cache_db, env, block_gas_limit, profile).build_mainnet();
        // `functions` is a cheap HashMap of raw fn pointers; clone per call so
        // the compiled-fn cache stays owned by `self` across calls. An empty map
        // (the `!use_jit` path) makes `JitEvm` dispatch every codehash to the
        // interpreter.
        let functions = if use_jit {
            self.functions.clone()
        } else {
            B256Map::default()
        };
        let mut evm = JitEvm::new(inner, functions);
        let tx_env = build_tx_env(tx, tx_gas_limit)?;
        let out = evm
            .transact(tx_env)
            .map_err(|e| anyhow!("fast view-call execution failed: {e:?}"))?;
        drop(evm); // release borrow of compiled fns before returning

        match out.result {
            ExecutionResult::Success { output, gas, .. } => {
                let _ = gas;
                Ok(output.into_data())
            }
            ExecutionResult::Revert { output, gas, .. } => Err(anyhow!(
                "view call reverted (tx_gas_used {}): 0x{}",
                gas.tx_gas_used(),
                hex_encode(&output)
            )),
            ExecutionResult::Halt { reason, gas, .. } => Err(anyhow!(
                "view call halted: {reason:?} (tx_gas_used {})",
                gas.tx_gas_used()
            )),
        }
    }

    /// The **amortized** execution path the persistent daemon uses. Reuses a
    /// warm, read-through overlay/base state cache keyed by (mount manifests +
    /// `base_fingerprint`), so the 2nd..Nth identical job resolves overlay chunk
    /// reads from RAM instead of re-materializing them. `build_base` is invoked
    /// ONLY on a cache miss (a key change), so a warm hit rebuilds neither the
    /// base db nor the overlay view.
    ///
    /// Byte-identical to [`call_view`](Self::call_view): the warm layer is a pure
    /// memoizing passthrough over the same immutable `OverlayStateDb`, and the
    /// per-call `CacheDB` on top is fresh exactly as in the fresh path. Different
    /// manifest ⇒ different `WarmKey` ⇒ a rebuilt (never reused) cache.
    #[allow(clippy::too_many_arguments)]
    pub fn call_view_warm<F>(
        &mut self,
        manifests: Vec<B256>,
        base_fingerprint: B256,
        mounts: OverlayMountSet,
        build_base: F,
        env: &ViewEnv,
        tx: &ViewTx,
        profile: Profile,
    ) -> Result<Bytes>
    where
        F: FnOnce() -> CacheDB<EmptyDB>,
    {
        if env.spec != self.spec {
            return Err(anyhow!(
                "FastView compiled for spec {:?} but view env requests {:?}; \
                 spec is consensus-critical — build a FastView per spec",
                self.spec,
                env.spec
            ));
        }

        let key = WarmKey {
            manifests,
            base_fingerprint,
        };
        let warm: Arc<WarmBaseDb> = match &self.warm {
            Some((k, arc)) if *k == key => arc.clone(),
            _ => {
                // Cache miss (first job, or the overlay/base changed): build the
                // immutable base+overlay view ONCE and wrap it in a fresh warm
                // read-through cache. `build_base` (which populates the engine +
                // caller accounts) runs only here.
                let inner = OverlayStateDb::new_multi(build_base(), mounts);
                let arc = Arc::new(WarmStateDb::new(inner));
                self.warm_build_count += 1;
                self.warm = Some((key, arc.clone()));
                arc
            }
        };
        let handle = WarmHandle(warm);

        // Resolve + compile the target engine bytecode BEFORE moving the warm
        // handle into the per-call CacheDB (mirrors the fresh path). Both the
        // resolve and every execution read hit the warm memo on the 2nd+ job.
        let target_code = fetch_code(&handle, tx.to)
            .map_err(|e| anyhow!("resolve target {} code: {e:?}", tx.to))?;
        if let Some(code) = &target_code {
            if !code.is_empty() {
                self.warm_code(code)?;
            }
        }

        let (block_gas_limit, tx_gas_limit) = resolve_gas_limits(env, tx, profile);
        let cache_db = CacheDB::new(handle);
        self.dispatch_view(cache_db, env, block_gas_limit, tx, tx_gas_limit, profile, true)
    }

    /// Like [`call_view`](Self::call_view) but also returns the wall-clock
    /// execution time (compile time excluded — pre-warm with
    /// [`warm_code`](Self::warm_code) first for a steady-state number).
    pub fn call_view_timed<DB>(
        &mut self,
        base: DB,
        mounts: OverlayMountSet,
        env: &ViewEnv,
        tx: &ViewTx,
        profile: Profile,
    ) -> Result<(Bytes, Duration)>
    where
        DB: DatabaseRef,
        DB::Error: DBErrorMarker + core::fmt::Debug,
    {
        let t0 = Instant::now();
        let out = self.call_view(base, mounts, env, tx, profile)?;
        Ok((out, t0.elapsed()))
    }
}

/// The concrete mainnet context type — pins the `Journal` entry + chain/local
/// type params so `Context::new` / `build_mainnet` type-inference resolves
/// (mirrors `revmc-harness::MainnetContext`).
type FastCtx<DB> = Context<BlockEnv, TxEnv, CfgEnv, DB, Journal<DB>, ()>;

/// Build the view-call context — a field-for-field mirror of `run_pass`. The
/// disable_* flags reproduce eth_call/debug_traceCall semantics (the node fills
/// defaults and skips fee/nonce/balance/block-cap enforcement for a simulated
/// call); they are exposed via the `optional_*` cargo features on revm-context.
fn build_ctx<DB: Database>(
    db: DB,
    env: &ViewEnv,
    block_gas_limit: u64,
    profile: Profile,
) -> FastCtx<DB> {
    let has_gas_override = profile.tx_gas_limit_override().is_some();
    Context::new(db, env.spec)
        .modify_cfg_chained(|cfg: &mut CfgEnv| {
            cfg.chain_id = env.chain_id;
            cfg.spec = env.spec;
            cfg.disable_nonce_check = true;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
            cfg.disable_fee_charge = true;
            cfg.disable_block_gas_limit = true;
            if has_gas_override {
                cfg.tx_gas_limit_cap = Some(u64::MAX);
            }
        })
        .modify_block_chained(|block: &mut BlockEnv| {
            block.number = U256::from(env.number);
            block.timestamp = U256::from(env.timestamp);
            block.gas_limit = block_gas_limit;
            block.beneficiary = env.coinbase;
            block.prevrandao = Some(env.prevrandao);
            block.basefee = env.basefee;
            block.difficulty = env.difficulty;
        })
}

/// Resolve a target address's raw runtime bytecode from the (overlay-aware)
/// database: inline code if present, else by codehash, else `None` for an EOA /
/// empty account.
fn fetch_code<DB>(db: &DB, address: Address) -> Result<Option<Vec<u8>>, DB::Error>
where
    DB: DatabaseRef,
{
    let Some(info) = db.basic_ref(address)? else {
        return Ok(None);
    };
    if let Some(code) = info.code {
        if !code.is_empty() {
            return Ok(Some(code.original_bytes().to_vec()));
        }
        return Ok(None);
    }
    if info.code_hash == KECCAK_EMPTY || info.code_hash == B256::ZERO {
        return Ok(None);
    }
    let code = db.code_by_hash_ref(info.code_hash)?;
    if code.is_empty() {
        Ok(None)
    } else {
        Ok(Some(code.original_bytes().to_vec()))
    }
}

/// Mirror of `local_exec::resolve_gas_limits`.
fn resolve_gas_limits(env: &ViewEnv, tx: &ViewTx, profile: Profile) -> (u64, u64) {
    let block_gas_limit = profile.block_gas_limit_override().unwrap_or(env.gas_limit);
    let tx_gas_limit = profile
        .tx_gas_limit_override()
        .or(tx.gas)
        .unwrap_or(block_gas_limit);
    (block_gas_limit, tx_gas_limit)
}

/// Mirror of `local_exec::build_tx_env`: `chain_id` is deliberately `None`
/// (simulated calls carry none; revm would otherwise default to Some(1) and
/// reject a non-mainnet trace).
fn build_tx_env(tx: &ViewTx, tx_gas_limit: u64) -> Result<TxEnv> {
    let mut builder = TxEnv::builder()
        .caller(tx.from)
        .kind(TxKind::Call(tx.to))
        .data(tx.input.clone())
        .value(tx.value)
        .gas_limit(tx_gas_limit)
        .gas_price(tx.gas_price)
        .chain_id(None);
    if let Some(nonce) = tx.nonce {
        builder = builder.nonce(nonce);
    }
    builder
        .build()
        .map_err(|e| anyhow!("failed to build view-call tx env: {e:?}"))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

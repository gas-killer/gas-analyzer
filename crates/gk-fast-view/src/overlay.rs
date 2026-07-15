//! revm-41 port of the analyzer's overlay-mount machinery
//! (`crates/evmsketch/src/overlay_mount.rs` + `crates/core/src/overlay.rs`).
//!
//! The address derivation, manifest hashing and chunk layout are reproduced
//! **byte-for-byte** from `gas_analyzer_core::overlay` — the same keccak
//! preimages, the same `OVERLAY_CHUNK_PAYLOAD`, the same `0x00 || payload`
//! chunk code — so a chunk address / mounted code computed here is bit-identical
//! to the one the revm-31 interpreter path mounts. That identity is the whole
//! reason a revmc-compiled view call can be a consensus-equivalent drop-in.
//!
//! Ported to revm-41's `DatabaseRef`; only the revm crate paths change vs the
//! revm-31 original, the semantics (phantom code-only accounts, empty storage,
//! transparent passthrough for a non-chunk address) are preserved exactly.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context as _, Result, anyhow};
use lru::LruCache;
use revm_database_interface::DatabaseRef;
use revm_primitives::{Address, B256, Bytes, U256, keccak256};
use revm_state::{AccountInfo, Bytecode};

// ============================================================================
// Derivation — byte-identical to gas_analyzer_core::overlay
// ============================================================================

/// Payload bytes per overlay chunk: EIP-170 code-size limit (24,576) minus the
/// leading STOP byte. Chunks are mounted as `0x00 || payload`.
pub const OVERLAY_CHUNK_PAYLOAD: usize = 24_575;

/// Domain separator for overlay address derivation (versioned).
pub const OVERLAY_DOMAIN_V1: &[u8] = b"gaskiller.llm.overlay.v1";

/// `keccak256(keccak256(weightsBlob) || keccak256(tokenizerBlob))`.
pub fn overlay_manifest_hash(weights_blob: &[u8], tokenizer_blob: &[u8]) -> B256 {
    let mut pre = [0u8; 64];
    pre[..32].copy_from_slice(keccak256(weights_blob).as_slice());
    pre[32..].copy_from_slice(keccak256(tokenizer_blob).as_slice());
    keccak256(pre)
}

/// The derived phantom address of chunk `index` under `manifest`. Mirrors
/// `Qwen3Engine.overlayChunkAddress` (solidity-sdk) and
/// `gas_analyzer_core::overlay::overlay_chunk_address`.
pub fn overlay_chunk_address(manifest: B256, index: u64) -> Address {
    let mut pre = Vec::with_capacity(OVERLAY_DOMAIN_V1.len() + 32 + 8);
    pre.extend_from_slice(OVERLAY_DOMAIN_V1);
    pre.extend_from_slice(manifest.as_slice());
    pre.extend_from_slice(&index.to_be_bytes());
    Address::from_slice(&keccak256(pre)[12..])
}

/// One pinned code binding: `code` (`0x00 || payload`) mounted at `address`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeOverlay {
    pub address: Address,
    pub code: Bytes,
}

/// A verified overlay set for one model — manifest + chunk bindings in global
/// chunk order (weight chunks first, then tokenizer). Byte-identical layout to
/// `gas_analyzer_core::overlay::OverlayEnv`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OverlayEnv {
    pub manifest: B256,
    pub overlays: Vec<CodeOverlay>,
}

impl OverlayEnv {
    /// Chunk two blobs, derive every address, build the overlay set.
    pub fn from_blobs(weights_blob: &[u8], tokenizer_blob: &[u8]) -> Result<Self> {
        if weights_blob.is_empty() || tokenizer_blob.is_empty() {
            return Err(anyhow!("overlay blob is empty"));
        }
        let manifest = overlay_manifest_hash(weights_blob, tokenizer_blob);
        let mut overlays = Vec::new();
        for blob in [weights_blob, tokenizer_blob] {
            for chunk in blob.chunks(OVERLAY_CHUNK_PAYLOAD) {
                let mut code = Vec::with_capacity(1 + chunk.len());
                code.push(0x00);
                code.extend_from_slice(chunk);
                overlays.push(CodeOverlay {
                    address: overlay_chunk_address(manifest, overlays.len() as u64),
                    code: code.into(),
                });
            }
        }
        Ok(OverlayEnv { manifest, overlays })
    }
}

// ============================================================================
// OverlayMount — the revm-41 DatabaseRef-consulted lookup
// ============================================================================

pub const DEFAULT_OVERLAY_MEMO_CHUNKS: usize = 1024;

#[derive(Debug, Clone, Copy)]
struct ChunkRef {
    blob: u8,
    offset: usize,
    len: usize,
}

enum OverlaySourceInner {
    /// Chunk code (`0x00 || payload`) resident in RAM, keyed by derived address.
    InMemory(HashMap<Address, Bytes>),
    /// Blobs memory-mapped from disk; payloads materialized on demand.
    Files {
        maps: Vec<memmap2::Mmap>,
        index: HashMap<Address, ChunkRef>,
    },
}

/// A verified, address-indexed overlay set the executor's database consults
/// before any base-state fetch.
pub struct OverlayMount {
    manifest: B256,
    source: OverlaySourceInner,
    memo: Mutex<LruCache<Address, (B256, Bytecode)>>,
}

impl std::fmt::Debug for OverlayMount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (kind, chunks) = match &self.source {
            OverlaySourceInner::InMemory(m) => ("in-memory", m.len()),
            OverlaySourceInner::Files { index, .. } => ("mmap-files", index.len()),
        };
        f.debug_struct("OverlayMount")
            .field("manifest", &self.manifest)
            .field("source", &kind)
            .field("chunks", &chunks)
            .finish()
    }
}

impl OverlayMount {
    fn new(manifest: B256, source: OverlaySourceInner) -> Self {
        Self {
            manifest,
            source,
            memo: Mutex::new(LruCache::new(
                NonZeroUsize::new(DEFAULT_OVERLAY_MEMO_CHUNKS).expect("non-zero"),
            )),
        }
    }

    /// Mount an already-built [`OverlayEnv`] after verifying it against
    /// `pinned_manifest` (refuses on mismatch, like the revm-31 original).
    pub fn from_env(env: &OverlayEnv, pinned_manifest: B256) -> Result<Self> {
        if env.manifest != pinned_manifest {
            return Err(anyhow!(
                "overlay manifest {} != pinned {}; refusing to mount unverified bytes",
                env.manifest,
                pinned_manifest
            ));
        }
        let map = env
            .overlays
            .iter()
            .map(|o| (o.address, o.code.clone()))
            .collect();
        Ok(Self::new(env.manifest, OverlaySourceInner::InMemory(map)))
    }

    /// Mount pre-derived `(address, code)` chunk bindings directly, keyed under
    /// `manifest`. No blob re-derivation: for callers that already hold the
    /// verified chunk set (e.g. the consensus-gate fixtures, which mount the
    /// exact chunks the revm-31 golden generator derived). Production paths use
    /// [`OverlayMount::from_env`] / [`OverlayMount::from_files`].
    pub fn from_pairs(manifest: B256, pairs: impl IntoIterator<Item = (Address, Bytes)>) -> Self {
        Self::new(
            manifest,
            OverlaySourceInner::InMemory(pairs.into_iter().collect()),
        )
    }

    /// Mount the two artifact blobs directly from disk via `mmap`, verifying the
    /// recomputed manifest against the pinned one before anything is served.
    /// Chunk boundaries and derived addresses are identical to
    /// [`OverlayEnv::from_blobs`]; chunk bytes are copied into heap only when
    /// execution actually touches the chunk.
    pub fn from_files(
        weights_path: impl AsRef<Path>,
        tokenizer_path: impl AsRef<Path>,
        pinned_manifest: B256,
    ) -> Result<Self> {
        let mut maps = Vec::with_capacity(2);
        for path in [weights_path.as_ref(), tokenizer_path.as_ref()] {
            let file = std::fs::File::open(path)
                .with_context(|| format!("open overlay blob {}", path.display()))?;
            // SAFETY: the artifact files are immutable published blobs; the
            // manifest check below refuses any content that does not hash to
            // the pinned commitment.
            let map = unsafe { memmap2::Mmap::map(&file) }
                .with_context(|| format!("mmap overlay blob {}", path.display()))?;
            if map.is_empty() {
                return Err(anyhow!(
                    "overlay blob {} is empty; refusing to mount",
                    path.display()
                ));
            }
            maps.push(map);
        }

        let manifest = overlay_manifest_hash(&maps[0], &maps[1]);
        if manifest != pinned_manifest {
            return Err(anyhow!(
                "overlay blobs hash to manifest {manifest}, but the pinned manifest is \
                 {pinned_manifest}; refusing to mount unverified bytes"
            ));
        }

        let mut index = HashMap::new();
        let mut chunk_idx: u64 = 0;
        for (blob, map) in maps.iter().enumerate() {
            let mut offset = 0usize;
            while offset < map.len() {
                let len = OVERLAY_CHUNK_PAYLOAD.min(map.len() - offset);
                index.insert(
                    overlay_chunk_address(manifest, chunk_idx),
                    ChunkRef {
                        blob: blob as u8,
                        offset,
                        len,
                    },
                );
                offset += len;
                chunk_idx += 1;
            }
        }

        Ok(Self::new(manifest, OverlaySourceInner::Files { maps, index }))
    }

    pub fn with_memo_capacity(mut self, chunks: usize) -> Self {
        self.memo = Mutex::new(LruCache::new(
            NonZeroUsize::new(chunks).expect("memo capacity must be non-zero"),
        ));
        self
    }

    pub fn manifest(&self) -> B256 {
        self.manifest
    }

    pub fn chunk_count(&self) -> usize {
        match &self.source {
            OverlaySourceInner::InMemory(m) => m.len(),
            OverlaySourceInner::Files { index, .. } => index.len(),
        }
    }

    /// O(1) membership check that never materializes chunk bytes.
    pub fn contains(&self, address: &Address) -> bool {
        match &self.source {
            OverlaySourceInner::InMemory(m) => m.contains_key(address),
            OverlaySourceInner::Files { index, .. } => index.contains_key(address),
        }
    }

    /// The `(code_hash, bytecode)` mounted at `address`, materializing (and
    /// memoizing) it if needed. `None` when the address is not an overlay chunk.
    pub fn account_code(&self, address: &Address) -> Option<(B256, Bytecode)> {
        if let Some(hit) = self
            .memo
            .lock()
            .expect("overlay memo mutex poisoned")
            .get(address)
        {
            return Some(hit.clone());
        }

        let code: Bytes = match &self.source {
            OverlaySourceInner::InMemory(m) => m.get(address)?.clone(),
            OverlaySourceInner::Files { maps, index } => {
                let chunk = index.get(address)?;
                let payload = &maps[chunk.blob as usize][chunk.offset..chunk.offset + chunk.len];
                let mut code = Vec::with_capacity(1 + payload.len());
                code.push(0x00);
                code.extend_from_slice(payload);
                code.into()
            }
        };

        let entry = (keccak256(&code), Bytecode::new_raw(code));
        self.memo
            .lock()
            .expect("overlay memo mutex poisoned")
            .put(*address, entry.clone());
        Some(entry)
    }
}

/// An ordered collection of verified [`OverlayMount`]s consulted as ONE
/// composite lookup. Distinct manifests derive disjoint address sets, so trying
/// each mount in turn is sound and order-independent.
#[derive(Debug, Clone, Default)]
pub struct OverlayMountSet {
    mounts: Vec<std::sync::Arc<OverlayMount>>,
}

impl OverlayMountSet {
    pub fn new(mounts: Vec<std::sync::Arc<OverlayMount>>) -> Self {
        Self { mounts }
    }

    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.mounts.len()
    }

    pub fn manifests(&self) -> Vec<B256> {
        self.mounts.iter().map(|m| m.manifest()).collect()
    }

    pub fn contains(&self, address: &Address) -> bool {
        self.mounts.iter().any(|m| m.contains(address))
    }

    pub fn account_code(&self, address: &Address) -> Option<(B256, Bytecode)> {
        self.mounts.iter().find_map(|m| m.account_code(address))
    }
}

impl From<std::sync::Arc<OverlayMount>> for OverlayMountSet {
    fn from(mount: std::sync::Arc<OverlayMount>) -> Self {
        Self::new(vec![mount])
    }
}

impl From<Option<std::sync::Arc<OverlayMount>>> for OverlayMountSet {
    fn from(mount: Option<std::sync::Arc<OverlayMount>>) -> Self {
        Self::new(mount.into_iter().collect())
    }
}

impl FromIterator<std::sync::Arc<OverlayMount>> for OverlayMountSet {
    fn from_iter<I: IntoIterator<Item = std::sync::Arc<OverlayMount>>>(iter: I) -> Self {
        Self::new(iter.into_iter().collect())
    }
}

/// A [`DatabaseRef`] layer that serves overlay chunk accounts locally and
/// delegates everything else to `inner`. With an empty [`OverlayMountSet`] it is
/// a transparent passthrough. Faithful revm-41 port of the revm-31 original.
#[derive(Debug)]
pub struct OverlayStateDb<DB> {
    inner: DB,
    mounts: OverlayMountSet,
}

impl<DB> OverlayStateDb<DB> {
    pub fn new(inner: DB, overlay: Option<std::sync::Arc<OverlayMount>>) -> Self {
        Self::new_multi(inner, overlay.into())
    }

    pub fn new_multi(inner: DB, mounts: OverlayMountSet) -> Self {
        Self { inner, mounts }
    }

    fn overlay_hit(&self, address: &Address) -> Option<(B256, Bytecode)> {
        self.mounts.account_code(address)
    }
}

impl<DB: DatabaseRef> DatabaseRef for OverlayStateDb<DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some((code_hash, code)) = self.overlay_hit(&address) {
            // Phantom data contract: fresh account carrying only code (balance
            // 0, nonce 0, empty storage) — parity with the revm-31 mount.
            return Ok(Some(AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash,
                code: Some(code),
                ..Default::default()
            }));
        }
        self.inner.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        // Overlay code is always inlined in basic_ref, so revm never resolves it
        // by hash; delegate for the underlying chain's accounts.
        self.inner.code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if self.mounts.contains(&address) {
            return Ok(U256::ZERO);
        }
        self.inner.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash_ref(number)
    }
}

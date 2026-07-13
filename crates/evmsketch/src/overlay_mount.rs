//! Native mounting of pinned code overlays (`UNBOUNDED_V2`) for the local
//! execution path.
//!
//! The RPC path serializes every overlay chunk as hex JSON `stateOverrides`
//! (~2× blob size per tracer call — ~1.2 GB request bodies for the 600 MB
//! Qwen3-0.6B artifacts, flatly impossible for 35 GB models). Here the overlay
//! is a lookup the revm [`DatabaseRef`] consults *before* the remote-state
//! fetch: zero serialization, zero transport.
//!
//! Two sources are supported behind one type, [`OverlayMount`]:
//!
//! * **In-memory** ([`OverlayMount::from_env`]): wraps an already-built
//!   [`OverlayEnv`] (chunk bytes resident in RAM). Right for small/synthetic
//!   overlays and for callers that already hold the blobs.
//! * **Memory-mapped files** ([`OverlayMount::from_files`]): the artifact
//!   blobs stay on disk; an `address → (blob, offset, len)` index is built at
//!   mount time and chunk code is materialized **on demand**, so mounting a
//!   35 GB model costs one streaming hash pass (required anyway to verify the
//!   manifest) plus a ~2 M-entry index — not 35 GB of heap. Materialized
//!   chunks are held in a bounded LRU memo.
//!
//! # Verification is mandatory
//!
//! Both constructors take the **pinned manifest** and refuse bytes that do
//! not reproduce it ([`OverlayError::ManifestMismatch`]), mirroring
//! [`OverlayEnv::verify`]: overlay bytes travel out-of-band and are a
//! liveness dependency only — nobody can forge data against the on-chain
//! commitment because unverifiable bytes never mount.
//!
//! # Semantics vs `stateOverrides`
//!
//! A mounted chunk account reports `balance = 0`, `nonce = 0` and empty
//! storage. Chunk addresses are keccak-derived phantom addresses
//! (`gas_analyzer_core::overlay::overlay_chunk_address`), so a pre-existing
//! account at one is a ~2^-160 event; a geth `stateOverrides` code override
//! would preserve that account's balance/nonce/storage where we report empty.
//! Chunks are STOP-prefixed pure data — consumers only `EXTCODECOPY` /
//! `EXTCODESIZE` them — so this difference is unobservable in practice and is
//! accepted in exchange for never issuing remote fetches for overlay
//! addresses.
//!
//! # Memory bound for huge models
//!
//! The bounded memo caps *this* module's residency, but revm's journal keeps
//! every account (code included) loaded during a single transaction, so a
//! trace that touches every chunk of an N-GB model still journals ~N GB of
//! analyzed bytecode until the transaction ends. Dense models therefore
//! remain bounded by touched-code size; MoE-style consumers (the 35 GB
//! Qwen3.5-A3B tier) touch only active-expert chunks per call, which is what
//! the lazy file source is for.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Mutex;

use alloy::primitives::{Address, B256, Bytes, U256, keccak256};
use anyhow::{Context as _, Result, anyhow};
use gas_analyzer_core::{
    OVERLAY_CHUNK_PAYLOAD, OverlayEnv, OverlayError, overlay_chunk_address, overlay_manifest_hash,
};
use lru::LruCache;
use revm::database_interface::DatabaseRef;
use revm::state::{AccountInfo, Bytecode};

/// Default bound on materialized chunks held by the memo: 1024 chunks ≈ 25 MB
/// of raw code (plus revm analysis overhead). Hot chunks stay resident;
/// eviction only costs a re-copy + re-keccak on the next touch.
pub const DEFAULT_OVERLAY_MEMO_CHUNKS: usize = 1024;

/// Location of one chunk's payload inside a mounted blob file.
#[derive(Debug, Clone, Copy)]
struct ChunkRef {
    /// Index into [`OverlaySourceInner::Files::maps`].
    blob: u8,
    /// Payload byte offset within the blob.
    offset: usize,
    /// Payload length (≤ [`OVERLAY_CHUNK_PAYLOAD`]).
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

/// A verified, address-indexed overlay set the local executor's database
/// consults before any remote state fetch. See the module docs for the two
/// sources and their trade-offs.
pub struct OverlayMount {
    manifest: B256,
    source: OverlaySourceInner,
    /// Materialized `(code_hash, analyzed bytecode)` per touched chunk.
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
    /// Mount an already-built [`OverlayEnv`] after verifying it against the
    /// pinned `manifest` (refuses on mismatch, like [`OverlayEnv::verify`]).
    pub fn from_env(env: &OverlayEnv, pinned_manifest: B256) -> Result<Self, OverlayError> {
        env.verify(pinned_manifest)?;
        let map = env
            .overlays
            .iter()
            .map(|o| (o.address, o.code.clone()))
            .collect();
        Ok(Self::new(env.manifest, OverlaySourceInner::InMemory(map)))
    }

    /// Mount the two artifact blobs (weights, tokenizer — the layout committed
    /// by `overlay_manifest_hash`) directly from disk via `mmap`, verifying
    /// the recomputed manifest against the pinned one before anything is
    /// served. Chunk boundaries and derived addresses are identical to
    /// [`OverlayEnv::from_blobs`]; chunk *bytes* are only copied into heap
    /// when execution actually touches the chunk.
    ///
    /// The files must be immutable while mounted (standard `mmap` contract);
    /// they are hashed at mount time, so pre-mount tampering is refused and
    /// post-mount mutation is outside the threat model (same trust as the
    /// in-memory blobs the RPC path inlines).
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
                // Parity with OverlayEnv::from_blobs (OverlayError::EmptyBlob).
                return Err(anyhow!(
                    "overlay blob {} is empty; refusing to mount",
                    path.display()
                ));
            }
            maps.push(map);
        }

        // One streaming pass over both blobs — the same hashing the manifest
        // commitment requires, so mounting adds no extra full-file reads.
        let manifest = overlay_manifest_hash(&maps[0], &maps[1]);
        if manifest != pinned_manifest {
            return Err(anyhow!(
                "{}",
                OverlayError::ManifestMismatch {
                    computed: manifest,
                    expected: pinned_manifest,
                }
            ));
        }

        // Address → (blob, offset, len) index in global chunk order (weights
        // chunks first, then tokenizer) — mirrors OverlayEnv::from_blobs.
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

        Ok(Self::new(
            manifest,
            OverlaySourceInner::Files { maps, index },
        ))
    }

    fn new(manifest: B256, source: OverlaySourceInner) -> Self {
        Self {
            manifest,
            source,
            memo: Mutex::new(LruCache::new(
                NonZeroUsize::new(DEFAULT_OVERLAY_MEMO_CHUNKS).expect("non-zero"),
            )),
        }
    }

    /// Override the bounded materialized-chunk memo capacity.
    ///
    /// # Panics
    /// Panics if `chunks` is zero.
    pub fn with_memo_capacity(mut self, chunks: usize) -> Self {
        self.memo = Mutex::new(LruCache::new(
            NonZeroUsize::new(chunks).expect("memo capacity must be non-zero"),
        ));
        self
    }

    /// The verified manifest these bindings belong to.
    pub fn manifest(&self) -> B256 {
        self.manifest
    }

    /// Number of chunk bindings.
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
    /// memoizing) it if needed. `None` when the address is not an overlay
    /// chunk.
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
                // Mounted code is `0x00 || payload`: the leading STOP mirrors
                // the SSTORE2-style data contracts of directory mode, so
                // consumer read offsets are identical in both modes.
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
/// composite lookup — the multi-model mounting primitive.
///
/// Chunk addresses are derived per-manifest
/// (`keccak256("gaskiller.llm.overlay.v1" || manifest || u64be(i))[12..]`,
/// see `gas_analyzer_core::overlay::overlay_chunk_address`), so the address
/// sets of mounts with **distinct manifests are disjoint** (a cross-manifest
/// collision is a ~2^-160 event). Trying each mount in turn is therefore a
/// sound composite: at most one mount can resolve any address, and mount
/// order cannot affect what execution observes. An empty set is a transparent
/// passthrough, equivalent to running with no overlay at all.
///
/// Cheap to clone: the set is a `Vec` of [`Arc`](std::sync::Arc)s, and each
/// mount's chunk memo is shared across clones.
#[derive(Debug, Clone, Default)]
pub struct OverlayMountSet {
    mounts: Vec<std::sync::Arc<OverlayMount>>,
}

impl OverlayMountSet {
    /// A composite over `mounts`. Callers are expected to pass mounts with
    /// distinct manifests (see the type docs for why that makes the composite
    /// sound); [`crate::LocalStateCache::overlay_mount_set_from_files`]
    /// enforces this for the file-backed construction path.
    pub fn new(mounts: Vec<std::sync::Arc<OverlayMount>>) -> Self {
        Self { mounts }
    }

    /// `true` when the set holds no mounts (transparent passthrough).
    pub fn is_empty(&self) -> bool {
        self.mounts.is_empty()
    }

    /// Number of mounted models.
    pub fn len(&self) -> usize {
        self.mounts.len()
    }

    /// The verified manifests of the mounted models, in mount order.
    pub fn manifests(&self) -> Vec<B256> {
        self.mounts.iter().map(|m| m.manifest()).collect()
    }

    /// O(mounts) membership check that never materializes chunk bytes.
    pub fn contains(&self, address: &Address) -> bool {
        self.mounts.iter().any(|m| m.contains(address))
    }

    /// The `(code_hash, bytecode)` mounted at `address` across all models,
    /// materializing (and memoizing, per mount) it if needed. `None` when the
    /// address is not a chunk of any mounted model. Disjointness (see type
    /// docs) guarantees at most one mount resolves.
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
/// delegates everything else to `inner` (the lazy remote-state backend).
///
/// With an empty [`OverlayMountSet`] it is a transparent passthrough, so the
/// local executor uses one database type for plain, single-overlay and
/// multi-overlay traces alike.
#[derive(Debug)]
pub struct OverlayStateDb<DB> {
    inner: DB,
    mounts: OverlayMountSet,
}

impl<DB> OverlayStateDb<DB> {
    /// Wrap `inner`, consulting `overlay` (if any) before it — the
    /// single-model convenience form of [`OverlayStateDb::new_multi`].
    pub fn new(inner: DB, overlay: Option<std::sync::Arc<OverlayMount>>) -> Self {
        Self::new_multi(inner, overlay.into())
    }

    /// Wrap `inner`, consulting every mount in `mounts` before it. The
    /// mounts' address sets are disjoint (per-manifest derived addresses —
    /// see [`OverlayMountSet`]), so this composite is order-independent.
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
            // Phantom data contract: fresh account carrying only code (see
            // module docs for the stateOverrides-parity discussion).
            return Ok(Some(AccountInfo {
                balance: U256::ZERO,
                nonce: 0,
                code_hash,
                code: Some(code),
            }));
        }
        self.inner.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        // Overlay code is always inlined in basic_ref, so revm never resolves
        // it by hash; delegate for the underlying chain's accounts.
        self.inner.code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if self.mounts.contains(&address) {
            // Never-deployed derived address: empty storage, no remote fetch.
            return Ok(U256::ZERO);
        }
        self.inner.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash_ref(number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::database::{CacheDB, EmptyDB};
    use std::io::Write as _;
    use std::sync::Arc;

    fn synthetic_blobs() -> (Vec<u8>, Vec<u8>) {
        // 2 full weight chunks + a tail, plus a sub-chunk tokenizer, with
        // non-repeating content so offset bugs cannot cancel out.
        let weights: Vec<u8> = (0..OVERLAY_CHUNK_PAYLOAD * 2 + 137)
            .map(|i| (i % 251) as u8)
            .collect();
        let tokenizer: Vec<u8> = (0..613).map(|i| (i % 241) as u8).collect();
        (weights, tokenizer)
    }

    fn write_temp(dir: &tempfile::TempDir, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).expect("create temp blob");
        f.write_all(bytes).expect("write temp blob");
        path
    }

    /// The mmap-backed file source must be indistinguishable from the
    /// in-memory OverlayEnv source: same manifest, same address set, and
    /// byte-identical (code_hash, bytecode) for every chunk.
    #[test]
    fn file_source_matches_in_memory_source_chunk_for_chunk() {
        let (weights, tokenizer) = synthetic_blobs();
        let env = OverlayEnv::from_blobs(&weights, &tokenizer).expect("env");

        let dir = tempfile::tempdir().expect("tempdir");
        let wpath = write_temp(&dir, "weights.bin", &weights);
        let tpath = write_temp(&dir, "tokenizer.bin", &tokenizer);

        let mem = OverlayMount::from_env(&env, env.manifest).expect("mount env");
        let files = OverlayMount::from_files(&wpath, &tpath, env.manifest).expect("mount files");

        assert_eq!(mem.manifest(), files.manifest());
        assert_eq!(mem.chunk_count(), env.overlays.len());
        assert_eq!(files.chunk_count(), env.overlays.len());

        for overlay in &env.overlays {
            let (mh, mc) = mem
                .account_code(&overlay.address)
                .expect("in-memory chunk present");
            let (fh, fc) = files
                .account_code(&overlay.address)
                .expect("file chunk present");
            assert_eq!(mh, fh, "code hash diverged at {}", overlay.address);
            assert_eq!(
                mc.original_bytes(),
                fc.original_bytes(),
                "code bytes diverged at {}",
                overlay.address
            );
            assert_eq!(
                fc.original_bytes(),
                overlay.code,
                "file chunk must equal the OverlayEnv chunk (STOP prefix included)"
            );
            assert_eq!(fh, keccak256(&overlay.code), "hash must be keccak(code)");
        }

        // Non-chunk address resolves on neither source.
        let stranger = Address::repeat_byte(0x42);
        assert!(mem.account_code(&stranger).is_none());
        assert!(files.account_code(&stranger).is_none());
    }

    /// A memo smaller than the chunk count must still serve every chunk
    /// correctly (eviction only re-materializes).
    #[test]
    fn tiny_memo_still_serves_all_chunks() {
        let (weights, tokenizer) = synthetic_blobs();
        let env = OverlayEnv::from_blobs(&weights, &tokenizer).expect("env");
        let dir = tempfile::tempdir().expect("tempdir");
        let wpath = write_temp(&dir, "w.bin", &weights);
        let tpath = write_temp(&dir, "t.bin", &tokenizer);

        let files = OverlayMount::from_files(&wpath, &tpath, env.manifest)
            .expect("mount")
            .with_memo_capacity(1);

        // Two passes: second pass re-materializes evicted chunks.
        for _ in 0..2 {
            for overlay in &env.overlays {
                let (h, c) = files.account_code(&overlay.address).expect("chunk");
                assert_eq!(c.original_bytes(), overlay.code);
                assert_eq!(h, keccak256(&overlay.code));
            }
        }
    }

    /// Both constructors must refuse bytes that do not hash to the pinned
    /// manifest — the mount is where verification happens.
    #[test]
    fn wrong_pin_is_refused_by_both_sources() {
        let (weights, tokenizer) = synthetic_blobs();
        let env = OverlayEnv::from_blobs(&weights, &tokenizer).expect("env");

        let err = OverlayMount::from_env(&env, B256::ZERO).expect_err("wrong pin");
        assert!(matches!(err, OverlayError::ManifestMismatch { .. }));

        let dir = tempfile::tempdir().expect("tempdir");
        let wpath = write_temp(&dir, "w.bin", &weights);
        let tpath = write_temp(&dir, "t.bin", &tokenizer);
        let err =
            OverlayMount::from_files(&wpath, &tpath, B256::ZERO).expect_err("wrong pin for files");
        assert!(err.to_string().contains("refusing to mount"));

        // Tampered blob against the honest pin is refused too.
        let mut tampered = weights.clone();
        tampered[7] ^= 0xFF;
        let wpath2 = write_temp(&dir, "w2.bin", &tampered);
        assert!(OverlayMount::from_files(&wpath2, &tpath, env.manifest).is_err());
    }

    #[test]
    fn empty_blob_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wpath = write_temp(&dir, "w.bin", &[]);
        let tpath = write_temp(&dir, "t.bin", b"tok");
        let err = OverlayMount::from_files(&wpath, &tpath, B256::ZERO).expect_err("empty blob");
        assert!(err.to_string().contains("empty"));
    }

    /// The DatabaseRef layer: overlay accounts come back as fresh accounts
    /// carrying only code; overlay storage is zero without touching the inner
    /// DB; everything else passes through.
    #[test]
    fn overlay_state_db_serves_chunks_and_delegates_rest() {
        let (weights, tokenizer) = synthetic_blobs();
        let env = OverlayEnv::from_blobs(&weights, &tokenizer).expect("env");
        let mount = Arc::new(OverlayMount::from_env(&env, env.manifest).expect("mount"));

        // Inner DB with one known non-overlay account.
        let mut inner = CacheDB::new(EmptyDB::default());
        let plain = Address::repeat_byte(0x77);
        inner.insert_account_info(
            plain,
            AccountInfo {
                balance: U256::from(123u64),
                nonce: 9,
                code_hash: revm::primitives::KECCAK_EMPTY,
                code: None,
            },
        );

        let db = OverlayStateDb::new(inner, Some(mount));

        let chunk_addr = env.overlays[0].address;
        let info = db
            .basic_ref(chunk_addr)
            .expect("basic_ref ok")
            .expect("overlay account exists");
        assert_eq!(info.balance, U256::ZERO);
        assert_eq!(info.nonce, 0);
        assert_eq!(
            info.code.as_ref().expect("code inlined").original_bytes(),
            env.overlays[0].code
        );
        assert_eq!(info.code_hash, keccak256(&env.overlays[0].code));

        assert_eq!(
            db.storage_ref(chunk_addr, U256::from(5u64)).expect("ok"),
            U256::ZERO,
            "overlay accounts have empty storage"
        );

        let passthrough = db
            .basic_ref(plain)
            .expect("basic_ref ok")
            .expect("plain account exists");
        assert_eq!(passthrough.balance, U256::from(123u64));
        assert_eq!(passthrough.nonce, 9);

        // No overlay: pure passthrough (chunk address resolves to nothing).
        let db_plain: OverlayStateDb<CacheDB<EmptyDB>> =
            OverlayStateDb::new(CacheDB::new(EmptyDB::default()), None);
        assert!(db_plain.basic_ref(chunk_addr).expect("ok").is_none());
    }

    /// A second synthetic model with content distinct from [`synthetic_blobs`]
    /// so cross-model mixups can never produce matching bytes.
    fn synthetic_blobs_b() -> (Vec<u8>, Vec<u8>) {
        let weights: Vec<u8> = (0..OVERLAY_CHUNK_PAYLOAD + 311)
            .map(|i| ((i * 7 + 3) % 253) as u8)
            .collect();
        let tokenizer: Vec<u8> = (0..419).map(|i| ((i * 11 + 5) % 239) as u8).collect();
        (weights, tokenizer)
    }

    /// The multi-model composite: two models' manifests derive disjoint
    /// address sets, every chunk of each model resolves through the set to
    /// exactly its own mount's bytes, a stranger address resolves on none,
    /// and an empty set never resolves anything.
    #[test]
    fn mount_set_serves_two_disjoint_models() {
        let (wa, ta) = synthetic_blobs();
        let (wb, tb) = synthetic_blobs_b();
        let env_a = OverlayEnv::from_blobs(&wa, &ta).expect("env a");
        let env_b = OverlayEnv::from_blobs(&wb, &tb).expect("env b");
        assert_ne!(env_a.manifest, env_b.manifest, "distinct models required");

        let mount_a = Arc::new(OverlayMount::from_env(&env_a, env_a.manifest).expect("mount a"));
        let mount_b = Arc::new(OverlayMount::from_env(&env_b, env_b.manifest).expect("mount b"));

        // Disjointness: no address of one model's chunk set appears in the
        // other's (the KEY FACT that makes the composite lookup safe).
        for o in &env_a.overlays {
            assert!(
                !mount_b.contains(&o.address),
                "model A chunk {} collided into model B's address set",
                o.address
            );
        }
        for o in &env_b.overlays {
            assert!(
                !mount_a.contains(&o.address),
                "model B chunk {} collided into model A's address set",
                o.address
            );
        }

        let set = OverlayMountSet::new(vec![mount_a, mount_b]);
        assert_eq!(set.len(), 2);
        assert!(!set.is_empty());
        assert_eq!(set.manifests(), vec![env_a.manifest, env_b.manifest]);

        for (env, tag) in [(&env_a, "A"), (&env_b, "B")] {
            for overlay in &env.overlays {
                assert!(set.contains(&overlay.address));
                let (hash, code) = set.account_code(&overlay.address).unwrap_or_else(|| {
                    panic!("model {tag} chunk {} must resolve", overlay.address)
                });
                assert_eq!(
                    code.original_bytes(),
                    overlay.code,
                    "model {tag} chunk {} bytes diverged through the set",
                    overlay.address
                );
                assert_eq!(hash, keccak256(&overlay.code));
            }
        }

        let stranger = Address::repeat_byte(0x42);
        assert!(!set.contains(&stranger));
        assert!(set.account_code(&stranger).is_none());

        let empty = OverlayMountSet::default();
        assert!(empty.is_empty());
        assert!(empty.account_code(&env_a.overlays[0].address).is_none());
    }

    /// The DatabaseRef layer over a two-model set: both models' chunks come
    /// back as fresh code-only accounts, chunk storage is zero without
    /// touching the inner DB, and non-chunk accounts pass through.
    #[test]
    fn overlay_state_db_multi_serves_both_models_and_delegates_rest() {
        let (wa, ta) = synthetic_blobs();
        let (wb, tb) = synthetic_blobs_b();
        let env_a = OverlayEnv::from_blobs(&wa, &ta).expect("env a");
        let env_b = OverlayEnv::from_blobs(&wb, &tb).expect("env b");
        let set = OverlayMountSet::new(vec![
            Arc::new(OverlayMount::from_env(&env_a, env_a.manifest).expect("mount a")),
            Arc::new(OverlayMount::from_env(&env_b, env_b.manifest).expect("mount b")),
        ]);

        let mut inner = CacheDB::new(EmptyDB::default());
        let plain = Address::repeat_byte(0x77);
        inner.insert_account_info(
            plain,
            AccountInfo {
                balance: U256::from(123u64),
                nonce: 9,
                code_hash: revm::primitives::KECCAK_EMPTY,
                code: None,
            },
        );
        let db = OverlayStateDb::new_multi(inner, set);

        for env in [&env_a, &env_b] {
            let chunk_addr = env.overlays[0].address;
            let info = db
                .basic_ref(chunk_addr)
                .expect("basic_ref ok")
                .expect("chunk account exists");
            assert_eq!(info.balance, U256::ZERO);
            assert_eq!(info.nonce, 0);
            assert_eq!(
                info.code.as_ref().expect("code inlined").original_bytes(),
                env.overlays[0].code
            );
            assert_eq!(
                db.storage_ref(chunk_addr, U256::from(5u64)).expect("ok"),
                U256::ZERO,
            );
        }

        let passthrough = db
            .basic_ref(plain)
            .expect("basic_ref ok")
            .expect("plain account exists");
        assert_eq!(passthrough.balance, U256::from(123u64));
        assert_eq!(passthrough.nonce, 9);
    }
}

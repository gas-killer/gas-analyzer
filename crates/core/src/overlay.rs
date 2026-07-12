//! Pinned code overlays for unbounded simulation (`UNBOUNDED_V2`).
//!
//! An overlay mounts immutable byte blobs — model weights, lookup tables, any
//! large read-only data — as contract *code* in the simulation environment,
//! without the blobs ever being deployed on-chain. The consumer contract
//! commits to a single `manifestHash`; chunk addresses are derived from it, so
//! no on-chain directory exists either. To EVM execution an overlaid chunk is
//! indistinguishable from a deployed data contract (`EXTCODESIZE` /
//! `EXTCODECOPY` behave identically), which is why consumer bytecode is
//! byte-for-byte the same whether its data is deployed or overlaid.
//!
//! # Determinism is protocol-critical (same bargain as `sim_profile`)
//!
//! `UnboundedV1` established the rule: the simulation env may deviate from the
//! real chain only through **versioned protocol constants** shared bit-exactly
//! by operators, this analyzer, and the SP1 slashing guest. Overlays extend
//! the pinned env from `{gas limits}` to `{gas limits, address → code}`. The
//! combined commitment ([`env_commitment`]) must be bound into the guest's
//! `chainConfigHash` exactly like V1's gas overrides — a fraud proof simulated
//! under different (or missing) overlay bytes then cannot verify, and honest
//! operators using the pinned bytes cannot be slashed.
//!
//! # Availability, not trust
//!
//! Overlay bytes travel out-of-band (HuggingFace, IPFS, mirrors). That is a
//! *liveness* dependency only: bytes that do not reproduce the manifest hash
//! are refused at mount time ([`OverlayEnv::from_blobs`] hashes what it is
//! given), so nobody can forge data against the commitment — at worst the
//! model is temporarily unservable until someone re-serves the blobs.
//!
//! # Consumer-side counterpart
//!
//! The address derivation below mirrors `Qwen3Engine.overlayChunkAddress` in
//! gas-killer/solidity-sdk (`src/examples/onchain-llm/`), which resolves the
//! same addresses on-chain when its `rootDirectory` is `address(0)`.

use alloy_primitives::{Address, B256, Bytes, keccak256};

/// Payload bytes per overlay chunk: the EIP-170 code-size limit (24,576)
/// minus the leading `STOP` byte. Chunks are mounted as `0x00 || payload`,
/// mirroring the SSTORE2-style data contracts the solidity-sdk deploys in
/// directory mode, so consumer read offsets are identical in both modes.
pub const OVERLAY_CHUNK_PAYLOAD: usize = 24_575;

/// Domain separator for overlay address derivation (versioned: a change is a
/// new overlay protocol version).
pub const OVERLAY_DOMAIN_V1: &[u8] = b"gaskiller.llm.overlay.v1";

/// Domain tag hashed into [`env_commitment`] when overlays are present.
pub const ENV_COMMITMENT_DOMAIN_V2: &[u8] = b"gaskiller.env.unbounded.v2";

/// Domain tag hashed into [`env_commitment`] for the overlay-free V1 env.
pub const ENV_COMMITMENT_DOMAIN_V1: &[u8] = b"gaskiller.env.unbounded.v1";

/// One pinned code binding: `code` is mounted at `address` for the duration
/// of the simulation. `code` includes the leading `0x00` STOP byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeOverlay {
    /// The phantom data-contract address (derived, never deployed).
    pub address: Address,
    /// The full runtime code to mount (`0x00 || chunk payload`).
    pub code: Bytes,
}

/// A verified overlay set for one model: the manifest hash plus every chunk
/// binding, in global chunk order (weight chunks first, then tokenizer).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OverlayEnv {
    /// `keccak256(keccak256(weightsBlob) || keccak256(tokenizerBlob))`.
    pub manifest: B256,
    /// Derived-address code bindings for every chunk.
    pub overlays: Vec<CodeOverlay>,
}

/// Why an overlay set could not be built or verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayError {
    /// The blobs hash to a different manifest than the consumer committed to.
    ManifestMismatch {
        /// The manifest recomputed from the provided blobs.
        computed: B256,
        /// The manifest the consumer (or task) pinned.
        expected: B256,
    },
    /// A blob was empty; an overlay with no bytes is always a mistake.
    EmptyBlob,
}

impl core::fmt::Display for OverlayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            OverlayError::ManifestMismatch { computed, expected } => write!(
                f,
                "overlay blobs hash to manifest {computed}, but the pinned manifest is {expected}; \
                 refusing to mount unverified bytes"
            ),
            OverlayError::EmptyBlob => write!(f, "overlay blob is empty"),
        }
    }
}

impl std::error::Error for OverlayError {}

/// The manifest hash committed on-chain by overlay-mode consumers:
/// `keccak256(keccak256(weightsBlob) || keccak256(tokenizerBlob))`.
pub fn overlay_manifest_hash(weights_blob: &[u8], tokenizer_blob: &[u8]) -> B256 {
    let mut pre = [0u8; 64];
    pre[..32].copy_from_slice(keccak256(weights_blob).as_slice());
    pre[32..].copy_from_slice(keccak256(tokenizer_blob).as_slice());
    keccak256(pre)
}

/// The derived phantom address of chunk `index` under `manifest`.
///
/// Mirrors `Qwen3Engine.overlayChunkAddress` (solidity-sdk):
/// `address(uint160(uint256(keccak256(abi.encodePacked(
///     "gaskiller.llm.overlay.v1", manifestHash, uint64(index))))))`.
pub fn overlay_chunk_address(manifest: B256, index: u64) -> Address {
    let mut pre = Vec::with_capacity(OVERLAY_DOMAIN_V1.len() + 32 + 8);
    pre.extend_from_slice(OVERLAY_DOMAIN_V1);
    pre.extend_from_slice(manifest.as_slice());
    pre.extend_from_slice(&index.to_be_bytes());
    Address::from_slice(&keccak256(pre)[12..])
}

impl OverlayEnv {
    /// Chunk two blobs, derive every address, and build the overlay set.
    ///
    /// This is the only constructor, so an `OverlayEnv`'s manifest always
    /// matches its bytes; call [`OverlayEnv::verify`] afterwards to check the
    /// result against an externally pinned manifest.
    pub fn from_blobs(weights_blob: &[u8], tokenizer_blob: &[u8]) -> Result<Self, OverlayError> {
        if weights_blob.is_empty() || tokenizer_blob.is_empty() {
            return Err(OverlayError::EmptyBlob);
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

    /// Refuse the env unless its manifest matches the pinned `expected` one.
    pub fn verify(&self, expected: B256) -> Result<(), OverlayError> {
        if self.manifest != expected {
            return Err(OverlayError::ManifestMismatch {
                computed: self.manifest,
                expected,
            });
        }
        Ok(())
    }
}

/// The versioned commitment over the full simulation environment: gas limits
/// plus the overlay set (when present). This value — not just the gas
/// constants — is what the SP1 slashing guest must bind into
/// `chainConfigHash` for overlay-mode consumers, so that a fraud proof under
/// different weights cannot verify and honest operators cannot be slashed.
///
/// Layout (all fixed-width, keccak over concatenation):
/// `domain || block_gas_limit_be || tx_gas_limit_be [|| manifest || n_be ||
///  (address || keccak(code)) * n]` with overlays in chunk order.
pub fn env_commitment(
    block_gas_limit: u64,
    tx_gas_limit: u64,
    overlay: Option<&OverlayEnv>,
) -> B256 {
    let mut pre = Vec::new();
    match overlay {
        None => pre.extend_from_slice(ENV_COMMITMENT_DOMAIN_V1),
        Some(_) => pre.extend_from_slice(ENV_COMMITMENT_DOMAIN_V2),
    }
    pre.extend_from_slice(&block_gas_limit.to_be_bytes());
    pre.extend_from_slice(&tx_gas_limit.to_be_bytes());
    if let Some(env) = overlay {
        pre.extend_from_slice(env.manifest.as_slice());
        pre.extend_from_slice(&(env.overlays.len() as u64).to_be_bytes());
        for o in &env.overlays {
            pre.extend_from_slice(o.address.as_slice());
            pre.extend_from_slice(keccak256(&o.code).as_slice());
        }
    }
    keccak256(pre)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim_profile::{
        UNBOUNDED_V1_BLOCK_GAS_LIMIT, UNBOUNDED_V1_TX_GAS_LIMIT, UNBOUNDED_V1_XL_BLOCK_GAS_LIMIT,
        UNBOUNDED_V1_XL_TX_GAS_LIMIT,
    };
    use alloy_primitives::{address, b256};

    /// Cross-ecosystem pin: these vectors are reproduced by
    /// `Qwen3Engine.overlayChunkAddress` (solidity-sdk, verified via
    /// `cast call`) and by tools/deploy_anvil.py (pycryptodome keccak).
    /// manifest = keccak256(keccak256(b"weights") || keccak256(b"tok")).
    #[test]
    fn derivation_matches_solidity_vectors() {
        let manifest = overlay_manifest_hash(b"weights", b"tok");
        assert_eq!(
            manifest,
            b256!("0x78273dda294c05581c6c3ccdb68f94d8df3e54038539debf5e3219534b9ee19f")
        );
        assert_eq!(
            overlay_chunk_address(manifest, 0),
            address!("0xcf1fbae4bca1b750ab9d36d4d37913e012f8ef18")
        );
        assert_eq!(
            overlay_chunk_address(manifest, 1),
            address!("0xd0a3009805eecd2b4e342d87d451d8df79ce030b")
        );
        assert_eq!(
            overlay_chunk_address(manifest, 24_298),
            address!("0x9c8f5689b42824b7699cf0d0f179553e91402922")
        );
    }

    #[test]
    fn from_blobs_chunks_and_prefixes() {
        // 2.5 chunks of weights, sub-chunk tokenizer
        let weights = vec![0xAB; OVERLAY_CHUNK_PAYLOAD * 2 + 100];
        let tok = vec![0xCD; 500];
        let env = OverlayEnv::from_blobs(&weights, &tok).unwrap();
        assert_eq!(env.overlays.len(), 4);
        assert_eq!(env.overlays[0].code.len(), 1 + OVERLAY_CHUNK_PAYLOAD);
        assert_eq!(env.overlays[0].code[0], 0x00);
        assert_eq!(env.overlays[2].code.len(), 1 + 100);
        assert_eq!(env.overlays[3].code.len(), 1 + 500);
        // addresses follow the global index order
        for (i, o) in env.overlays.iter().enumerate() {
            assert_eq!(o.address, overlay_chunk_address(env.manifest, i as u64));
        }
        env.verify(env.manifest).unwrap();
    }

    #[test]
    fn verify_refuses_wrong_manifest() {
        let env = OverlayEnv::from_blobs(b"weights", b"tok").unwrap();
        let err = env.verify(B256::ZERO).unwrap_err();
        assert!(matches!(err, OverlayError::ManifestMismatch { .. }));
        assert!(err.to_string().contains("refusing to mount"));
    }

    #[test]
    fn empty_blob_is_refused() {
        assert_eq!(
            OverlayEnv::from_blobs(b"", b"tok").unwrap_err(),
            OverlayError::EmptyBlob
        );
    }

    #[test]
    fn env_commitment_is_version_separated_and_content_bound() {
        let v1 = env_commitment(
            UNBOUNDED_V1_BLOCK_GAS_LIMIT,
            UNBOUNDED_V1_TX_GAS_LIMIT,
            None,
        );
        let env = OverlayEnv::from_blobs(b"weights", b"tok").unwrap();
        let v2 = env_commitment(
            UNBOUNDED_V1_BLOCK_GAS_LIMIT,
            UNBOUNDED_V1_TX_GAS_LIMIT,
            Some(&env),
        );
        assert_ne!(v1, v2, "overlay presence must change the commitment");

        let other = OverlayEnv::from_blobs(b"weights2", b"tok").unwrap();
        let v2b = env_commitment(
            UNBOUNDED_V1_BLOCK_GAS_LIMIT,
            UNBOUNDED_V1_TX_GAS_LIMIT,
            Some(&other),
        );
        assert_ne!(v2, v2b, "different bytes must change the commitment");

        // Gas tiers commit differently through the bound limit values alone —
        // no new domain tag needed for `SimProfile::UnboundedV1Xl`.
        let v1_xl = env_commitment(
            UNBOUNDED_V1_XL_BLOCK_GAS_LIMIT,
            UNBOUNDED_V1_XL_TX_GAS_LIMIT,
            None,
        );
        assert_ne!(v1, v1_xl, "the XL gas tier must change the commitment");
    }
}

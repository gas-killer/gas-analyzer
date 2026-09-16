//! Artifact manifest v3: paged Merkle commitments over guest artifacts.
//!
//! V2's manifest is a flat streaming keccak over whole blobs — perfect for
//! mount-time verification, unusable in a dispute, where a guest must
//! authenticate *one page* without hashing 597MB. V3 commits to a
//! domain-separated binary Merkle tree over 4,096-byte pages:
//!
//! ```text
//! leaf_i = keccak256(0x00 || page_i)            page zero-padded to 4,096
//! node   = keccak256(0x01 || left || right)
//! root_k = the file's tree root  (a level with odd width promotes its last
//!          node unchanged — "the lonely node rises" — so branch length can
//!          vary by index and verification is width-aware)
//! artifactRoot = keccak256(DOMAIN_V3 || fileCount (u32 BE)
//!                          || (len_0 (u64 BE) || root_0) || … )
//! ```
//!
//! One deliberate deviation from UNBOUNDED_V3_NATIVE.md, flagged for
//! ratification in the campaign state file: the design doc's outer preimage is
//! `DOMAIN || fileCount || root_0 || …` *without* the byte lengths. The page
//! tree covers zero-padded content, so without a committed length the exact
//! byte size of the last page is malleable — `gk_artifact_len` would return a
//! host-trusted value a dispute cannot check. Binding `len_i` into the outer
//! preimage closes that hole; the domain string and the leaf/node formulas are
//! untouched.

use crate::constants::{ARTIFACT_MANIFEST_DOMAIN_V3, GKVM_ARTIFACT_PAGE_SIZE};
use alloy_primitives::{B256, Keccak256};

/// Domain-separation prefix of a leaf hash.
pub const MANIFEST_LEAF_PREFIX: u8 = 0x00;
/// Domain-separation prefix of an interior node hash.
pub const MANIFEST_NODE_PREFIX: u8 = 0x01;

/// The paged Merkle commitment of one artifact file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactFileManifest {
    /// Exact byte length of the file — bound into the artifact root, so the
    /// intra-page tail size is not host-malleable.
    pub len: u64,
    /// Root of the page tree; [`B256::ZERO`] for an empty file (zero pages).
    pub root: B256,
    /// Leaf hashes, kept so branches can be served without re-reading pages.
    leaves: Vec<B256>,
}

impl ArtifactFileManifest {
    /// Commit to `bytes` as pages of [`GKVM_ARTIFACT_PAGE_SIZE`].
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let leaves: Vec<B256> = bytes
            .chunks(GKVM_ARTIFACT_PAGE_SIZE)
            .map(|chunk| {
                let mut hasher = Keccak256::new();
                hasher.update([MANIFEST_LEAF_PREFIX]);
                hasher.update(chunk);
                // The tail page is committed zero-padded to the full page
                // size, matching what `gk_artifact_read` serves.
                hasher.update(&ZERO_PAGE[chunk.len()..]);
                hasher.finalize()
            })
            .collect();
        let root = root_from_leaves(&leaves);
        Self {
            len: bytes.len() as u64,
            root,
            leaves,
        }
    }

    /// Number of pages (`ceil(len / page size)`), the quantity a verifier
    /// derives from the committed length to interpret promotion widths.
    pub fn page_count(&self) -> u64 {
        self.len.div_ceil(GKVM_ARTIFACT_PAGE_SIZE as u64)
    }

    /// The sibling path authenticating `page_idx`, bottom-up. Levels where the
    /// node is promoted (odd width, last index) contribute no sibling, so the
    /// path can be shorter than `ceil(log2(pages))`.
    pub fn branch(&self, page_idx: u64) -> Option<Vec<B256>> {
        let mut idx = usize::try_from(page_idx).ok()?;
        if idx >= self.leaves.len() {
            return None;
        }
        let mut branch = Vec::new();
        let mut level: Vec<B256> = self.leaves.clone();
        while level.len() > 1 {
            if idx == level.len() - 1 && level.len() % 2 == 1 {
                // Promoted: no sibling at this level.
            } else {
                branch.push(level[idx ^ 1]);
            }
            level = parent_level(&level);
            idx /= 2;
        }
        Some(branch)
    }
}

/// Verify one zero-padded page against a file root, walking the same
/// promotion-aware widths the builder used. `page_count` comes from the
/// committed file length, so a verifier holds every input it needs from the
/// artifact root alone.
pub fn verify_page(
    root: B256,
    page_count: u64,
    page_idx: u64,
    page: &[u8; GKVM_ARTIFACT_PAGE_SIZE],
    branch: &[B256],
) -> bool {
    if page_idx >= page_count || page_count == 0 {
        return false;
    }
    let mut hasher = Keccak256::new();
    hasher.update([MANIFEST_LEAF_PREFIX]);
    hasher.update(page);
    let mut node = hasher.finalize();

    let mut idx = page_idx;
    let mut width = page_count;
    let mut consumed = 0usize;
    while width > 1 {
        if idx == width - 1 && width % 2 == 1 {
            // Promoted unchanged.
        } else {
            let Some(sibling) = branch.get(consumed) else {
                return false;
            };
            consumed += 1;
            let (left, right) = if idx.is_multiple_of(2) {
                (node, *sibling)
            } else {
                (*sibling, node)
            };
            let mut hasher = Keccak256::new();
            hasher.update([MANIFEST_NODE_PREFIX]);
            hasher.update(left);
            hasher.update(right);
            node = hasher.finalize();
        }
        idx /= 2;
        width = width.div_ceil(2);
    }
    consumed == branch.len() && node == root
}

/// The artifact-level root binding every file's length and page-tree root
/// under the v3 domain.
pub fn artifact_root(files: &[ArtifactFileManifest]) -> B256 {
    let mut hasher = Keccak256::new();
    hasher.update(ARTIFACT_MANIFEST_DOMAIN_V3);
    hasher.update((files.len() as u32).to_be_bytes());
    for file in files {
        hasher.update(file.len.to_be_bytes());
        hasher.update(file.root);
    }
    hasher.finalize()
}

/// The manifest blob served to the guest as its third input buffer:
/// `fileCount (u32 BE) || (len_i (u64 BE) || root_i)*` — exactly the outer
/// preimage minus the domain, so the guest re-derives the artifact root from
/// it in one hash and thereafter trusts the per-file roots and lengths.
pub fn manifest_blob(files: &[ArtifactFileManifest]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(4 + files.len() * 40);
    blob.extend_from_slice(&(files.len() as u32).to_be_bytes());
    for file in files {
        blob.extend_from_slice(&file.len.to_be_bytes());
        blob.extend_from_slice(file.root.as_slice());
    }
    blob
}

const ZERO_PAGE: [u8; GKVM_ARTIFACT_PAGE_SIZE] = [0u8; GKVM_ARTIFACT_PAGE_SIZE];

fn parent_level(level: &[B256]) -> Vec<B256> {
    let mut parents = Vec::with_capacity(level.len().div_ceil(2));
    let mut chunks = level.chunks_exact(2);
    for pair in &mut chunks {
        let mut hasher = Keccak256::new();
        hasher.update([MANIFEST_NODE_PREFIX]);
        hasher.update(pair[0]);
        hasher.update(pair[1]);
        parents.push(hasher.finalize());
    }
    if let [lonely] = chunks.remainder() {
        parents.push(*lonely);
    }
    parents
}

fn root_from_leaves(leaves: &[B256]) -> B256 {
    match leaves {
        [] => B256::ZERO,
        [only] => *only,
        _ => {
            let mut level = leaves.to_vec();
            while level.len() > 1 {
                level = parent_level(&level);
            }
            level[0]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::GKVM_ARTIFACT_PAGE_SIZE as PAGE;
    use alloy_primitives::keccak256;

    fn page_bytes(fill: u8, len: usize) -> Vec<u8> {
        vec![fill; len]
    }

    fn padded(chunk: &[u8]) -> [u8; PAGE] {
        let mut page = [0u8; PAGE];
        page[..chunk.len()].copy_from_slice(chunk);
        page
    }

    #[test]
    fn empty_file_commits_to_the_zero_root() {
        let manifest = ArtifactFileManifest::from_bytes(&[]);
        assert_eq!(manifest.root, B256::ZERO);
        assert_eq!(manifest.page_count(), 0);
        assert_eq!(manifest.branch(0), None);
    }

    #[test]
    fn single_page_root_is_the_leaf() {
        let bytes = page_bytes(7, 100);
        let manifest = ArtifactFileManifest::from_bytes(&bytes);
        let mut preimage = vec![MANIFEST_LEAF_PREFIX];
        preimage.extend_from_slice(&padded(&bytes));
        assert_eq!(manifest.root, keccak256(&preimage));
        // And the empty branch verifies it.
        assert!(verify_page(manifest.root, 1, 0, &padded(&bytes), &[]));
    }

    #[test]
    fn two_page_root_matches_the_hand_computed_node() {
        let bytes = page_bytes(1, PAGE + 5);
        let manifest = ArtifactFileManifest::from_bytes(&bytes);
        let leaf = |chunk: &[u8]| {
            let mut preimage = vec![MANIFEST_LEAF_PREFIX];
            preimage.extend_from_slice(&padded(chunk));
            keccak256(&preimage)
        };
        let (l0, l1) = (leaf(&bytes[..PAGE]), leaf(&bytes[PAGE..]));
        let mut preimage = vec![MANIFEST_NODE_PREFIX];
        preimage.extend_from_slice(l0.as_slice());
        preimage.extend_from_slice(l1.as_slice());
        assert_eq!(manifest.root, keccak256(&preimage));
    }

    #[test]
    fn every_page_of_an_odd_tree_verifies_and_nothing_else_does() {
        // Five pages: widths 5 → 3 → 2 → 1, with promotions at both odd
        // levels — the shape that catches naive power-of-two verifiers.
        let mut bytes = Vec::new();
        for fill in 1..=5u8 {
            bytes.extend_from_slice(&page_bytes(fill, PAGE));
        }
        let manifest = ArtifactFileManifest::from_bytes(&bytes);
        assert_eq!(manifest.page_count(), 5);

        for idx in 0..5u64 {
            let chunk = &bytes[idx as usize * PAGE..(idx as usize + 1) * PAGE];
            let branch = manifest.branch(idx).expect("page exists");
            assert!(
                verify_page(manifest.root, 5, idx, &padded(chunk), &branch),
                "page {idx} must verify with its own branch"
            );
            // The right data under the wrong index must fail.
            let wrong_idx = (idx + 1) % 5;
            assert!(
                !verify_page(manifest.root, 5, wrong_idx, &padded(chunk), &branch),
                "page {idx} must not verify as page {wrong_idx}"
            );
            // Tampered data must fail.
            let mut tampered = padded(chunk);
            tampered[0] ^= 1;
            assert!(!verify_page(manifest.root, 5, idx, &tampered, &branch));
            // A branch with junk appended must fail: every sibling has to be
            // consumed, or a prover could smuggle unused data past the check.
            let mut fat = branch.clone();
            fat.push(B256::ZERO);
            assert!(!verify_page(manifest.root, 5, idx, &padded(chunk), &fat));
        }
        assert!(!verify_page(manifest.root, 5, 5, &[0; PAGE], &[]));
    }

    #[test]
    fn the_promoted_page_carries_a_shorter_branch() {
        let bytes = page_bytes(9, 3 * PAGE);
        let manifest = ArtifactFileManifest::from_bytes(&bytes);
        // Page 2 is promoted at the leaf level (width 3), then paired once.
        assert_eq!(manifest.branch(2).unwrap().len(), 1);
        assert_eq!(manifest.branch(0).unwrap().len(), 2);
    }

    #[test]
    fn the_tail_length_is_bound_into_the_artifact_root() {
        // Same padded page bytes, different lengths: the page trees agree,
        // the artifact roots must not — this is the deviation from the design
        // doc's outer preimage, and the property it exists to provide.
        let a = ArtifactFileManifest::from_bytes(&page_bytes(3, 100));
        let b = ArtifactFileManifest::from_bytes(&{
            let mut bytes = page_bytes(3, 100);
            bytes.extend_from_slice(&[0; 20]); // explicit zeros the padding hides
            bytes
        });
        assert_eq!(a.root, b.root, "zero-padded page trees agree");
        assert_ne!(
            artifact_root(&[a]),
            artifact_root(&[b]),
            "artifact roots must differ when only the length differs"
        );
    }

    #[test]
    fn the_outer_preimage_is_exactly_the_specified_layout() {
        let file = ArtifactFileManifest::from_bytes(&page_bytes(1, 10));
        let root = artifact_root(std::slice::from_ref(&file));
        let mut preimage = Vec::new();
        preimage.extend_from_slice(b"gaskiller.artifact.v3");
        preimage.extend_from_slice(&1u32.to_be_bytes());
        preimage.extend_from_slice(&10u64.to_be_bytes());
        preimage.extend_from_slice(file.root.as_slice());
        assert_eq!(root, keccak256(&preimage));
        // The guest-facing blob is that preimage minus the domain.
        assert_eq!(
            manifest_blob(&[file]),
            preimage[b"gaskiller.artifact.v3".len()..]
        );
    }

    #[test]
    fn file_order_is_binding() {
        let a = ArtifactFileManifest::from_bytes(&page_bytes(1, 50));
        let b = ArtifactFileManifest::from_bytes(&page_bytes(2, 50));
        assert_ne!(
            artifact_root(&[a.clone(), b.clone()]),
            artifact_root(&[b, a]),
            "artifact kinds are positional; swapping files must change the root"
        );
    }
}

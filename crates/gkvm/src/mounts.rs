//! Loaded guest programs and mounted artifacts, and the `GK_GUEST_*`
//! environment conventions that configure them.
//!
//! Mirrors the overlay mount conventions: an env "slot" is an all-or-nothing
//! pair (`GK_GUEST_PROGRAM` requires `GK_GUEST_PROGRAM_HASH`, likewise the
//! `_1..=_N` suffixed slots, likewise artifacts), everything is verified
//! against its committed hash at load, and a mismatch is an error the service
//! boundary turns into a startup panic. Missing or wrong config must fail
//! loudly before an operator ever analyzes a task — silently degrading would
//! fork the quorum.

use crate::manifest::{ArtifactFileManifest, artifact_root};
use alloy_primitives::{B256, keccak256};
use memmap2::Mmap;
use sp1_core_executor::Program;
use std::{collections::HashMap, fs::File, path::Path, sync::Arc};

/// Why a guest program or artifact failed to load. The service boundary
/// panics on these at startup (the `GK_SIM_EXECUTOR` doctrine); the analyzer
/// surfaces them as executor errors, never as signable results.
#[derive(Debug, thiserror::Error)]
pub enum GkVmMountError {
    /// An env slot named a program or artifact without its hash/root, or vice
    /// versa — the pair is all-or-nothing so a typo cannot half-configure a
    /// consensus input.
    #[error("incomplete env slot: {present} is set but {missing} is not")]
    IncompleteEnvSlot {
        /// The variable that was found.
        present: String,
        /// The variable the slot also requires.
        missing: String,
    },
    /// A hash/root env value did not parse as 32 hex bytes.
    #[error("malformed digest in {var}: {source}")]
    MalformedDigest {
        /// The offending variable.
        var: String,
        /// The parse failure.
        source: hex::FromHexError,
    },
    /// The file could not be read or mapped.
    #[error("failed to read {path}: {source}")]
    Io {
        /// The path that failed.
        path: String,
        /// The underlying error.
        source: std::io::Error,
    },
    /// The bytes on disk do not hash to the committed value. Refusing to
    /// serve is the only sound response: an operator running different bytes
    /// under the same commitment would sign divergent results.
    #[error("{what} at {path} hashes to {actual} but {expected} was committed")]
    DigestMismatch {
        /// "guest program" or "artifact".
        what: &'static str,
        /// The file that mismatched.
        path: String,
        /// keccak of the bytes found.
        actual: B256,
        /// The committed hash from the environment.
        expected: B256,
    },
    /// The ELF failed SP1's loader (not a RISC-V executable, layout outside
    /// the guest address rules, too many instructions, …).
    #[error("guest ELF {path} rejected by the SP1 loader: {message}")]
    ElfRejected {
        /// The file that failed to parse.
        path: String,
        /// The loader's report.
        message: String,
    },
}

/// A guest ELF, verified against its `programHash` and parsed once.
pub struct LoadedGuestProgram {
    /// `keccak256(elf)` — the identity the wire format carries.
    pub hash: B256,
    /// The raw ELF bytes (kept for re-hashing and diagnostics).
    pub elf: Vec<u8>,
    /// The parsed program, shared with every executor instance.
    pub program: Arc<Program>,
}

impl LoadedGuestProgram {
    /// Load and verify an ELF from bytes.
    pub fn from_bytes(elf: Vec<u8>, expected: B256, origin: &str) -> Result<Self, GkVmMountError> {
        let hash = keccak256(&elf);
        if hash != expected {
            return Err(GkVmMountError::DigestMismatch {
                what: "guest program",
                path: origin.to_string(),
                actual: hash,
                expected,
            });
        }
        let program = Program::from(&elf).map_err(|e| GkVmMountError::ElfRejected {
            path: origin.to_string(),
            message: format!("{e:#}"),
        })?;
        Ok(Self {
            hash,
            elf,
            program: Arc::new(program),
        })
    }

    /// Load and verify an ELF from disk.
    pub fn from_file(path: &Path, expected: B256) -> Result<Self, GkVmMountError> {
        let elf = std::fs::read(path).map_err(|source| GkVmMountError::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_bytes(elf, expected, &path.display().to_string())
    }
}

/// A mounted artifact bundle: memory-mapped file bytes plus their manifest,
/// verified against the committed `artifactRoot` at mount time (one streaming
/// pass, the same cost class as the V2 flat hash).
pub struct ArtifactMountV3 {
    /// The committed root the mount verified against.
    pub root: B256,
    files: Vec<ArtifactFile>,
}

struct ArtifactFile {
    manifest: ArtifactFileManifest,
    bytes: ArtifactBytes,
}

enum ArtifactBytes {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl AsRef<[u8]> for ArtifactBytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            ArtifactBytes::Mapped(map) => map,
            ArtifactBytes::Owned(bytes) => bytes,
        }
    }
}

impl ArtifactMountV3 {
    /// Mount files from disk in kind order, verifying the recomputed root.
    pub fn from_files(paths: &[&Path], expected: B256) -> Result<Self, GkVmMountError> {
        let mut files = Vec::with_capacity(paths.len());
        for path in paths {
            let file = File::open(path).map_err(|source| GkVmMountError::Io {
                path: path.display().to_string(),
                source,
            })?;
            // SAFETY: the mapping is read-only and artifact blobs are
            // deploy-time immutable inputs; mutating them out from under a
            // running operator is outside the threat model, exactly as for
            // the overlay blobs this mirrors.
            let map = unsafe { Mmap::map(&file) }.map_err(|source| GkVmMountError::Io {
                path: path.display().to_string(),
                source,
            })?;
            let manifest = ArtifactFileManifest::from_bytes(&map);
            files.push(ArtifactFile {
                manifest,
                bytes: ArtifactBytes::Mapped(map),
            });
        }
        Self::verified(files, expected, || {
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(",")
        })
    }

    /// Mount in-memory bytes (tests and fixtures).
    pub fn from_bytes(blobs: Vec<Vec<u8>>, expected: B256) -> Result<Self, GkVmMountError> {
        let files = blobs
            .into_iter()
            .map(|bytes| ArtifactFile {
                manifest: ArtifactFileManifest::from_bytes(&bytes),
                bytes: ArtifactBytes::Owned(bytes),
            })
            .collect();
        Self::verified(files, expected, || "<memory>".to_string())
    }

    fn verified(
        files: Vec<ArtifactFile>,
        expected: B256,
        origin: impl FnOnce() -> String,
    ) -> Result<Self, GkVmMountError> {
        let manifests: Vec<_> = files.iter().map(|f| f.manifest.clone()).collect();
        let actual = artifact_root(&manifests);
        if actual != expected {
            return Err(GkVmMountError::DigestMismatch {
                what: "artifact",
                path: origin(),
                actual,
                expected,
            });
        }
        Ok(Self {
            root: expected,
            files,
        })
    }

    /// Per-file manifests, in kind order.
    pub fn manifests(&self) -> Vec<ArtifactFileManifest> {
        self.files.iter().map(|f| f.manifest.clone()).collect()
    }

    /// One zero-padded page plus its Merkle branch, or `None` when the kind
    /// or page index is out of range.
    pub fn page(&self, kind: u32, page_idx: u64) -> Option<(Vec<u8>, Vec<B256>)> {
        let file = self.files.get(kind as usize)?;
        let branch = file.manifest.branch(page_idx)?;
        let bytes = file.bytes.as_ref();
        let start = page_idx as usize * crate::constants::GKVM_ARTIFACT_PAGE_SIZE;
        let end = (start + crate::constants::GKVM_ARTIFACT_PAGE_SIZE).min(bytes.len());
        let mut page = vec![0u8; crate::constants::GKVM_ARTIFACT_PAGE_SIZE];
        page[..end - start].copy_from_slice(&bytes[start..end]);
        Some((page, branch))
    }

    /// The whole bundle, front to back: every page of kind 0 in order, then
    /// kind 1, and so on. The schedule of a guest that loads its artifacts
    /// once, sequentially (the `qwen` guest).
    pub fn sequential_schedule(&self) -> Vec<(u32, u64)> {
        self.files
            .iter()
            .enumerate()
            .flat_map(|(kind, file)| (0..file.manifest.page_count()).map(move |p| (kind as u32, p)))
            .collect()
    }
}

/// Every guest program and artifact an operator has installed, keyed by the
/// commitment the wire format carries.
#[derive(Default)]
pub struct GuestProgramSet {
    programs: HashMap<B256, Arc<LoadedGuestProgram>>,
    artifacts: HashMap<B256, Arc<ArtifactMountV3>>,
}

impl GuestProgramSet {
    /// Load every configured `GK_GUEST_PROGRAM[_N]` / `GK_GUEST_ARTIFACT[_N]`
    /// slot from the process environment. Slots are the bare name plus
    /// `_1..=_N` counting up from 1 without gaps, matching the overlay env
    /// conventions.
    pub fn from_env() -> Result<Self, GkVmMountError> {
        let mut set = Self::default();
        for (path_var, hash_var) in env_slots("GK_GUEST_PROGRAM", "GK_GUEST_PROGRAM_HASH") {
            let Some((path, hash)) = read_slot(&path_var, &hash_var)? else {
                break;
            };
            set.insert_program(LoadedGuestProgram::from_file(Path::new(&path), hash)?);
        }
        for (path_var, root_var) in env_slots("GK_GUEST_ARTIFACT", "GK_GUEST_ARTIFACT_ROOT") {
            let Some((path, root)) = read_slot(&path_var, &root_var)? else {
                break;
            };
            let paths: Vec<&Path> = path.split(':').map(Path::new).collect();
            set.insert_artifact(ArtifactMountV3::from_files(&paths, root)?);
        }
        Ok(set)
    }

    /// Install a verified program.
    pub fn insert_program(&mut self, program: LoadedGuestProgram) {
        self.programs.insert(program.hash, Arc::new(program));
    }

    /// Install a verified artifact mount.
    pub fn insert_artifact(&mut self, artifact: ArtifactMountV3) {
        self.artifacts.insert(artifact.root, Arc::new(artifact));
    }

    /// The program committed to by `hash`, if installed.
    pub fn program(&self, hash: &B256) -> Option<Arc<LoadedGuestProgram>> {
        self.programs.get(hash).cloned()
    }

    /// The artifact bundle committed to by `root`, if mounted.
    pub fn artifact(&self, root: &B256) -> Option<Arc<ArtifactMountV3>> {
        self.artifacts.get(root).cloned()
    }

    /// Number of installed programs.
    pub fn program_count(&self) -> usize {
        self.programs.len()
    }
}

/// The unbounded slot-name sequence `NAME`, `NAME_1`, `NAME_2`, …
fn env_slots(
    path_base: &'static str,
    digest_base: &'static str,
) -> impl Iterator<Item = (String, String)> {
    (0u32..).map(move |i| {
        if i == 0 {
            (path_base.to_string(), digest_base.to_string())
        } else {
            (format!("{path_base}_{i}"), format!("{digest_base}_{i}"))
        }
    })
}

/// One env slot: `Ok(None)` ends the scan, a half-configured slot is an
/// error, never a skip.
fn read_slot(path_var: &str, digest_var: &str) -> Result<Option<(String, B256)>, GkVmMountError> {
    match (std::env::var(path_var).ok(), std::env::var(digest_var).ok()) {
        (None, None) => Ok(None),
        (Some(_), None) => Err(GkVmMountError::IncompleteEnvSlot {
            present: path_var.to_string(),
            missing: digest_var.to_string(),
        }),
        (None, Some(_)) => Err(GkVmMountError::IncompleteEnvSlot {
            present: digest_var.to_string(),
            missing: path_var.to_string(),
        }),
        (Some(path), Some(digest)) => {
            let digest = digest.strip_prefix("0x").unwrap_or(&digest);
            let bytes: [u8; 32] = hex::FromHex::from_hex(digest).map_err(|source| {
                GkVmMountError::MalformedDigest {
                    var: digest_var.to_string(),
                    source,
                }
            })?;
            Ok(Some((path, B256::from(bytes))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::verify_page;

    #[test]
    fn a_wrong_hash_refuses_to_load() {
        let err = LoadedGuestProgram::from_bytes(vec![1, 2, 3], B256::ZERO, "<test>")
            .err()
            .expect("wrong hash must fail");
        assert!(matches!(
            err,
            GkVmMountError::DigestMismatch {
                what: "guest program",
                ..
            }
        ));
    }

    #[test]
    fn garbage_bytes_with_the_right_hash_are_rejected_by_the_loader() {
        let bytes = b"not an elf".to_vec();
        let err = LoadedGuestProgram::from_bytes(bytes.clone(), keccak256(&bytes), "<test>")
            .err()
            .expect("garbage bytes must fail the loader");
        assert!(matches!(err, GkVmMountError::ElfRejected { .. }));
    }

    #[test]
    fn a_mounted_artifact_serves_verifiable_pages() {
        let blob = vec![0xabu8; crate::constants::GKVM_ARTIFACT_PAGE_SIZE * 2 + 100];
        let manifest = ArtifactFileManifest::from_bytes(&blob);
        let root = artifact_root(std::slice::from_ref(&manifest));
        let mount = ArtifactMountV3::from_bytes(vec![blob], root).expect("root matches");

        let (page, branch) = mount.page(0, 2).expect("tail page exists");
        assert!(verify_page(
            manifest.root,
            manifest.page_count(),
            2,
            page.as_slice().try_into().unwrap(),
            &branch
        ));
        assert!(
            mount.page(0, 3).is_none(),
            "past-the-end page must not be served"
        );
        assert!(
            mount.page(1, 0).is_none(),
            "unknown kind must not be served"
        );
    }

    #[test]
    fn a_mount_against_the_wrong_root_is_refused() {
        let err = ArtifactMountV3::from_bytes(vec![vec![1u8; 10]], B256::ZERO)
            .err()
            .expect("wrong root must fail");
        assert!(matches!(
            err,
            GkVmMountError::DigestMismatch {
                what: "artifact",
                ..
            }
        ));
    }
}

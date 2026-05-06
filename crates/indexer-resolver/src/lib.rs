//! Address → project resolver.
//!
//! Two sources, in priority order:
//!   1. Curated YAML overlay (e.g. `data/overlay.yaml`). Always wins.
//!   2. DefiLlama protocol list — populates the project metadata table; address
//!      mappings from DefiLlama are best-effort (their per-chain address data
//!      is sparse).
//!
//! Lookups are O(1), synchronous, and cheap. The internal map is rebuilt by
//! the refresher worker (typically every 24h) and atomically swapped via
//! `ArcSwap`, so reads never block.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum ResolverError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("yaml parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("invalid address `{0}`: {1}")]
    Address(String, String),
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub slug: String,
    pub name: String,
    pub category: Option<String>,
    pub contact_email: Option<String>,
    pub contact_url: Option<String>,
}

impl ProjectInfo {
    pub fn unknown(address: [u8; 20]) -> Self {
        ProjectInfo {
            slug: format!("unknown:0x{}", hex::encode(address)),
            name: format!("Unknown (0x{})", hex::encode(address)),
            category: None,
            contact_email: None,
            contact_url: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct OverlayContact {
    primary: Option<String>,
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OverlayEntry {
    chain_id: u64,
    address: String,
    project_slug: String,
    project_name: String,
    category: Option<String>,
    #[serde(default)]
    contact: Option<OverlayContact>,
}

/// Snapshot held by `Resolver`. Cheap to clone (Arc'd internally).
#[derive(Default)]
pub struct ResolverSnapshot {
    addresses: HashMap<(u64, [u8; 20]), ProjectInfo>,
    projects: HashMap<String, ProjectInfo>,
}

impl ResolverSnapshot {
    pub fn projects(&self) -> impl Iterator<Item = &ProjectInfo> {
        self.projects.values()
    }
}

pub struct Resolver {
    snapshot: ArcSwap<ResolverSnapshot>,
}

impl Resolver {
    pub fn new() -> Self {
        Self {
            snapshot: ArcSwap::new(Arc::new(ResolverSnapshot::default())),
        }
    }

    /// Resolve an address. Falls back to a synthetic `unknown:0xADDR` project
    /// — never returns an error or drops a row, so unmapped contracts still
    /// count toward BD totals.
    pub fn resolve(&self, chain_id: u64, address: [u8; 20]) -> ProjectInfo {
        self.snapshot
            .load()
            .addresses
            .get(&(chain_id, address))
            .cloned()
            .unwrap_or_else(|| ProjectInfo::unknown(address))
    }

    pub fn snapshot(&self) -> Arc<ResolverSnapshot> {
        self.snapshot.load_full()
    }

    /// Rebuild the in-memory map from the overlay file (and optionally
    /// DefiLlama). Atomically swaps the snapshot when done.
    pub async fn refresh(
        &self,
        overlay_path: Option<&Path>,
        defillama_endpoint: Option<&str>,
    ) -> Result<(), ResolverError> {
        let mut new_snapshot = ResolverSnapshot::default();

        if let Some(path) = overlay_path {
            load_overlay(path, &mut new_snapshot).await?;
        }

        if let Some(url) = defillama_endpoint {
            // Best-effort: failures don't poison the snapshot — we still have
            // the overlay. Log and continue.
            if let Err(e) = load_defillama(url, &mut new_snapshot).await {
                tracing::warn!(error = %e, "defillama refresh failed; using overlay only");
            }
        }

        self.snapshot.store(Arc::new(new_snapshot));
        Ok(())
    }
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

async fn load_overlay(path: &Path, into: &mut ResolverSnapshot) -> Result<(), ResolverError> {
    let bytes = tokio::fs::read(path).await?;
    let entries: Vec<OverlayEntry> = serde_yaml::from_slice(&bytes)?;

    for entry in entries {
        let address = parse_address(&entry.address)?;
        let (email, url) = entry
            .contact
            .map(|c| (c.primary, c.url))
            .unwrap_or((None, None));
        let info = ProjectInfo {
            slug: entry.project_slug.clone(),
            name: entry.project_name,
            category: entry.category,
            contact_email: email,
            contact_url: url,
        };
        into.addresses.insert((entry.chain_id, address), info.clone());
        into.projects.entry(entry.project_slug).or_insert(info);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct DefiLlamaProtocol {
    slug: String,
    name: String,
    category: Option<String>,
}

async fn load_defillama(url: &str, into: &mut ResolverSnapshot) -> Result<(), ResolverError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let protocols: Vec<DefiLlamaProtocol> = client.get(url).send().await?.json().await?;

    for p in protocols {
        // Don't overwrite overlay project metadata.
        into.projects.entry(p.slug.clone()).or_insert(ProjectInfo {
            slug: p.slug,
            name: p.name,
            category: p.category,
            contact_email: None,
            contact_url: None,
        });
        // DefiLlama address-to-protocol mapping is not in this endpoint —
        // would require fetching `/protocol/{slug}` for each. Deferred to a
        // follow-up so we don't burn rate on every refresh.
    }

    Ok(())
}

fn parse_address(s: &str) -> Result<[u8; 20], ResolverError> {
    let stripped = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(stripped)
        .map_err(|e| ResolverError::Address(s.to_string(), e.to_string()))?;
    if bytes.len() != 20 {
        return Err(ResolverError::Address(
            s.to_string(),
            format!("expected 20 bytes, got {}", bytes.len()),
        ));
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_address_with_and_without_prefix() {
        let with = parse_address("0x1234567890abcdef1234567890abcdef12345678").unwrap();
        let without = parse_address("1234567890abcdef1234567890abcdef12345678").unwrap();
        assert_eq!(with, without);
    }

    #[test]
    fn unknown_fallback() {
        let r = Resolver::new();
        let info = r.resolve(1, [0xaa; 20]);
        assert!(info.slug.starts_with("unknown:"));
    }

    #[tokio::test]
    async fn overlay_round_trip() {
        let tmp = std::env::temp_dir().join("indexer-resolver-test.yaml");
        std::fs::write(
            &tmp,
            "- chain_id: 1\n  address: \"0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n  project_slug: x\n  project_name: X\n",
        )
        .unwrap();
        let r = Resolver::new();
        r.refresh(Some(&tmp), None).await.unwrap();
        let info = r.resolve(1, [0xaa; 20]);
        assert_eq!(info.slug, "x");
    }
}

//! 4byte.directory client: resolves 4-byte function selectors to their
//! canonical signature (e.g. `0xa9059cbb` → `transfer(address,uint256)`).
//!
//! Free public endpoint, no auth. Their signatures DB is community-curated
//! and frequently has multiple text signatures hashing to the same selector
//! — we pick the lowest-id entry, which 4byte's docs describe as the most
//! widely-used canonical form.

use std::time::Duration;

use serde::Deserialize;

use crate::ResolverError;

#[derive(Debug, Clone)]
pub struct ResolvedSelector {
    pub selector: [u8; 4],
    pub primary_name: String,
    pub primary_sig: String,
    pub all_signatures: Vec<String>,
}

#[derive(Clone)]
pub struct FourByteClient {
    http: reqwest::Client,
    base_url: String,
}

impl FourByteClient {
    pub fn new() -> Result<Self, ResolverError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base_url: "https://www.4byte.directory/api/v1/signatures/".to_string(),
        })
    }

    /// Look up signatures for a single selector. Returns `None` when 4byte
    /// has no entries; transport errors propagate so the caller can decide
    /// to retry vs. mark unresolved.
    pub async fn lookup(
        &self,
        selector: &[u8; 4],
    ) -> Result<Option<ResolvedSelector>, ResolverError> {
        let hex = format!("0x{}", hex::encode(selector));
        let resp: ApiPage = self
            .http
            .get(&self.base_url)
            .query(&[("hex_signature", hex.as_str())])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        if resp.results.is_empty() {
            return Ok(None);
        }

        // Lowest id = earliest-added canonical form per 4byte's convention.
        let mut sorted = resp.results;
        sorted.sort_by_key(|s| s.id);
        let primary_sig = sorted[0].text_signature.clone();
        let primary_name = primary_sig
            .split('(')
            .next()
            .unwrap_or(&primary_sig)
            .to_string();
        let all_signatures = sorted.into_iter().map(|s| s.text_signature).collect();

        Ok(Some(ResolvedSelector {
            selector: *selector,
            primary_name,
            primary_sig,
            all_signatures,
        }))
    }
}

#[derive(Debug, Deserialize)]
struct ApiPage {
    results: Vec<Signature>,
}

#[derive(Debug, Deserialize)]
struct Signature {
    id: i64,
    text_signature: String,
}

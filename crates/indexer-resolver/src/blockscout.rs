//! Blockscout v2 API fallback. When Etherscan returns Unverified or rate
//! limits us, we try the chain's Blockscout instance. Most public chains
//! have a Blockscout deployment with verified-contract metadata covering
//! addresses Etherscan doesn't.
//!
//! Free, no auth. The endpoints differ per host but the response shape
//! is consistent enough that we only need `/api/v2/addresses/{addr}`.

use std::time::Duration;

use serde::Deserialize;

use crate::{ResolverError, etherscan::ContractMeta};

#[derive(Clone)]
pub struct BlockscoutClient {
    http: reqwest::Client,
    /// Base URL up to and including `/api/v2`. Trailing slash optional.
    base_url: String,
}

impl BlockscoutClient {
    pub fn new(base_url: String) -> Result<Self, ResolverError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;
        let base_url = base_url.trim_end_matches('/').to_string();
        Ok(Self { http, base_url })
    }

    /// Returns `Verified { name, .. }` when Blockscout knows the contract,
    /// `Unverified` otherwise. Mirrors `EtherscanClient::get_contract_name`
    /// so the labeler can swap callsites without rewrites.
    pub async fn get_contract_name(
        &self,
        address: &[u8; 20],
    ) -> Result<ContractMeta, ResolverError> {
        let addr_hex = format!("0x{}", hex::encode(address));
        let url = format!("{}/addresses/{}", self.base_url, addr_hex);
        let resp = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json::<AddressResp>()
            .await?;

        // Multiple shapes — "name" alone (legacy), "contract_name" via the
        // smart_contract nested object, or an explicit "is_contract" + name
        // field. We accept any non-empty name we can find.
        let name = resp
            .smart_contract
            .as_ref()
            .and_then(|s| s.name.clone())
            .or(resp.name.clone())
            .or_else(|| {
                resp.implementations
                    .as_ref()
                    .and_then(|v| v.first())
                    .and_then(|i| i.name.clone())
            });

        let Some(name) = name.filter(|n| !n.trim().is_empty()) else {
            return Ok(ContractMeta::Unverified);
        };

        let implementation = resp
            .implementations
            .as_ref()
            .and_then(|v| v.first())
            .and_then(|i| i.address.as_deref())
            .and_then(parse_address_lowercase);

        Ok(ContractMeta::Verified {
            name,
            is_proxy: implementation.is_some(),
            implementation,
        })
    }
}

fn parse_address_lowercase(s: &str) -> Option<[u8; 20]> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 20 {
        return None;
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Some(out)
}

#[derive(Debug, Deserialize)]
struct AddressResp {
    name: Option<String>,
    smart_contract: Option<SmartContract>,
    implementations: Option<Vec<Implementation>>,
}

#[derive(Debug, Deserialize)]
struct SmartContract {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Implementation {
    address: Option<String>,
    name: Option<String>,
}

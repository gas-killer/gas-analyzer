//! Etherscan v2 API client. Used by the auto-labeler to fetch a verified
//! contract's `ContractName`, which we then heuristically map to a project
//! slug.
//!
//! Free-tier limit is 5 req/s. The labeler caller is responsible for pacing;
//! this client just performs single requests and returns parsed responses.
//!
//! Only the `getsourcecode` endpoint is wired — that's the one that surfaces
//! the human-readable name.
//!
//! Etherscan API v2 (https://docs.etherscan.io/etherscan-v2) takes a
//! `chainid` query param so the same key works across chains. We default to
//! Ethereum mainnet (chainid=1).

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::ResolverError;

/// Result of a single Etherscan lookup. `Unverified` means the API responded
/// but the contract has no verified source — distinct from `Error` so the
/// caller can decide whether to retry.
#[derive(Debug, Clone)]
pub enum ContractMeta {
    Verified {
        name: String,
        is_proxy: bool,
        implementation: Option<[u8; 20]>,
    },
    Unverified,
}

#[derive(Clone)]
pub struct EtherscanClient {
    api_key: String,
    base_url: String,
    chain_id: u64,
    http: reqwest::Client,
}

impl EtherscanClient {
    pub fn new(api_key: String, chain_id: u64) -> Result<Self, ResolverError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            api_key,
            base_url: "https://api.etherscan.io/v2/api".to_string(),
            chain_id,
            http,
        })
    }

    /// Fetch the verified contract metadata for an address. Returns
    /// `Ok(Unverified)` when the contract has no verified source — that's a
    /// data signal, not a transport failure.
    pub async fn get_contract_name(
        &self,
        address: &[u8; 20],
    ) -> Result<ContractMeta, ResolverError> {
        let addr_hex = format!("0x{}", hex::encode(address));
        let resp: ApiResponse<Vec<SourceCode>> = self
            .http
            .get(&self.base_url)
            .query(&[
                ("chainid", self.chain_id.to_string().as_str()),
                ("module", "contract"),
                ("action", "getsourcecode"),
                ("address", addr_hex.as_str()),
                ("apikey", self.api_key.as_str()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        // Status "1" = success; "0" with NOTOK message can happen even for
        // valid lookups when the contract is unverified. We rely on the
        // response payload rather than the status flag.
        let entry = match resp.result {
            ApiResult::Ok(rows) => rows.into_iter().next(),
            ApiResult::Err(msg) => {
                return Err(ResolverError::Etherscan(format!("api error: {msg}")));
            }
        };
        let Some(entry) = entry else {
            return Ok(ContractMeta::Unverified);
        };

        if entry.contract_name.trim().is_empty() {
            return Ok(ContractMeta::Unverified);
        }

        let implementation = entry
            .implementation
            .as_deref()
            .and_then(parse_address_lowercase);
        Ok(ContractMeta::Verified {
            name: entry.contract_name,
            is_proxy: entry.proxy.as_deref() == Some("1"),
            implementation,
        })
    }
}

/// Etherscan returns `result` as either an array (success) or a string
/// (error). This enum decodes both shapes.
#[derive(Debug)]
enum ApiResult<T> {
    Ok(T),
    Err(String),
}

#[derive(Debug, Deserialize)]
#[serde(bound = "T: serde::de::DeserializeOwned")]
struct ApiResponse<T> {
    #[serde(rename = "result")]
    #[serde(deserialize_with = "deserialize_api_result")]
    result: ApiResult<T>,
}

fn deserialize_api_result<'de, D, T>(d: D) -> Result<ApiResult<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = serde_json::Value::deserialize(d)?;
    if let Some(s) = v.as_str() {
        return Ok(ApiResult::Err(s.to_string()));
    }
    let parsed = serde_json::from_value::<T>(v).map_err(serde::de::Error::custom)?;
    Ok(ApiResult::Ok(parsed))
}

#[derive(Debug, Deserialize)]
struct SourceCode {
    #[serde(rename = "ContractName")]
    contract_name: String,
    #[serde(rename = "Proxy")]
    proxy: Option<String>,
    #[serde(rename = "Implementation")]
    implementation: Option<String>,
}

fn parse_address_lowercase(s: &str) -> Option<[u8; 20]> {
    let stripped = s.trim().strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(stripped).ok()?;
    if bytes.len() != 20 {
        return None;
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Some(out)
}

// ---------- name → slug resolver ----------

/// Compiled name→slug dictionary loaded from a YAML file. Names are stored
/// pre-normalized; lookups also normalize, so suffixes like `Router02`,
/// `V2`, `Token`, `Proxy` don't break matches.
#[derive(Debug, Clone, Default)]
pub struct NameDict {
    map: std::collections::HashMap<String, String>,
}

impl NameDict {
    /// Load a name dictionary from a YAML file. Missing file → empty dict
    /// (caller can still fall back to slug-existence checks against the
    /// projects table).
    pub async fn load(path: &Path) -> Result<Self, ResolverError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = tokio::fs::read(path).await?;
        let entries: Vec<NameEntry> = serde_yaml::from_slice(&bytes)?;
        let mut map = std::collections::HashMap::new();
        for e in entries {
            map.insert(normalize_name(&e.name), e.slug);
        }
        Ok(Self { map })
    }

    /// Look up a contract name. Tries the exact normalized form first, then
    /// successively-stripped forms so `UniswapV2Router02` falls back to
    /// `uniswap-v2-router02` → `uniswap-v2-router` → `uniswap-v2` → ...
    pub fn lookup(&self, contract_name: &str) -> Option<&str> {
        let normalized = normalize_name(contract_name);
        // Try the full normalized name first.
        if let Some(slug) = self.map.get(&normalized) {
            return Some(slug);
        }
        // Then try progressively stripping known suffixes.
        let mut s = normalized.as_str();
        for _ in 0..4 {
            let trimmed = strip_one_suffix(s);
            if trimmed == s {
                break;
            }
            s = trimmed;
            if let Some(slug) = self.map.get(s) {
                return Some(slug);
            }
        }
        None
    }
}

#[derive(Debug, Deserialize)]
struct NameEntry {
    name: String,
    slug: String,
}

/// Normalize a contract name for fuzzy matching. Lowercases and replaces
/// internal capital-boundaries with dashes so `UniswapV2Router02` becomes
/// `uniswap-v2-router02`. CamelCase input matches dash-cased dict entries.
fn normalize_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            let prev = s.chars().nth(i - 1).unwrap_or(' ');
            if prev.is_lowercase() || prev.is_ascii_digit() {
                out.push('-');
            }
        }
        out.extend(c.to_lowercase());
    }
    out.replace(['_', ' '], "-")
}

/// Strip one trailing "version/role" segment from a normalized dash-cased
/// name. Matched suffixes: `-router02`, `-router`, `-routerv2`, `-token`,
/// `-proxy`, `-implementation`, `-v2`, `-v3`, `-v4`. Returns input unchanged
/// if no suffix matches.
fn strip_one_suffix(s: &str) -> &str {
    const SUFFIXES: &[&str] = &[
        "-router02",
        "-routerv2",
        "-router",
        "-implementation",
        "-proxy",
        "-token",
        "-v4",
        "-v3",
        "-v2",
        "-v1",
    ];
    for suf in SUFFIXES {
        if let Some(stripped) = s.strip_suffix(suf) {
            return stripped;
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_camel_case_to_dashed() {
        assert_eq!(normalize_name("UniswapV2Router02"), "uniswap-v2-router02");
        assert_eq!(normalize_name("TetherToken"), "tether-token");
        assert_eq!(normalize_name("FiatTokenProxy"), "fiat-token-proxy");
    }

    #[test]
    fn strip_suffix_progression() {
        assert_eq!(strip_one_suffix("uniswap-v2-router02"), "uniswap-v2");
        assert_eq!(strip_one_suffix("tether-token"), "tether");
        assert_eq!(strip_one_suffix("fiat-token-proxy"), "fiat-token");
    }
}

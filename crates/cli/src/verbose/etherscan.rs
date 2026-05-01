//! Etherscan v2 ABI fetcher with proxy resolution.
//!
//! Fetches verified ABIs by address, transparently merging in the
//! implementation contract's ABI when the address is a verified proxy.
//! Caches per-address (including misses) so a single CLI run hits the API
//! at most once per address.

use alloy_json_abi::JsonAbi;
use alloy_primitives::Address;
use anyhow::{Context, Result, anyhow};
use std::collections::HashMap;
use std::sync::Mutex;

const ETHERSCAN_V2: &str = "https://api.etherscan.io/v2/api";

/// Etherscan v2 client. Construct with [`EtherscanClient::new`] and call
/// [`fetch_abi`](Self::fetch_abi).
pub struct EtherscanClient {
    api_key: String,
    chain_id: u64,
    http: reqwest::Client,
    cache: Mutex<HashMap<Address, Option<JsonAbi>>>,
}

impl EtherscanClient {
    pub fn new(api_key: String, chain_id: u64) -> Self {
        Self {
            api_key,
            chain_id,
            http: reqwest::Client::new(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the ABI for `address`. If the address is a verified proxy with
    /// a known implementation, the implementation's ABI is merged in so that
    /// proxied selectors are decodable. Returns `None` if the address itself
    /// is not verified on Etherscan.
    pub async fn fetch_abi(&self, address: Address) -> Result<Option<JsonAbi>> {
        if let Some(cached) = self.cache.lock().unwrap().get(&address).cloned() {
            return Ok(cached);
        }

        let (mut abi, impl_addr) = self.fetch_source(address).await?;

        if let Some(impl_addr) = impl_addr
            && impl_addr != Address::ZERO
            && impl_addr != address
        {
            let (impl_abi, _) = self.fetch_source(impl_addr).await?;
            match (abi.as_mut(), impl_abi) {
                (Some(base), Some(extra)) => {
                    base.functions.extend(extra.functions);
                    base.errors.extend(extra.errors);
                    base.events.extend(extra.events);
                }
                (None, Some(extra)) => abi = Some(extra),
                _ => {}
            }
        }

        self.cache.lock().unwrap().insert(address, abi.clone());
        Ok(abi)
    }

    /// Calls `getsourcecode` and returns `(abi, implementation_address)`.
    /// `implementation_address` is `Some` for verified proxies.
    async fn fetch_source(
        &self,
        address: Address,
    ) -> Result<(Option<JsonAbi>, Option<Address>)> {
        let url = format!(
            "{ETHERSCAN_V2}?chainid={}&module=contract&action=getsourcecode&address=0x{:x}&apikey={}",
            self.chain_id, address, self.api_key
        );
        let resp: serde_json::Value = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("etherscan request failed for {address}"))?
            .json()
            .await
            .with_context(|| format!("etherscan json parse failed for {address}"))?;

        let status = resp.get("status").and_then(|v| v.as_str()).unwrap_or("0");
        if status != "1" {
            return Ok((None, None));
        }

        let entry = resp
            .get("result")
            .and_then(|v| v.as_array())
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("etherscan getsourcecode missing result[0]"))?;

        let abi_str = entry.get("ABI").and_then(|v| v.as_str()).unwrap_or("");
        let abi = if abi_str.is_empty()
            || abi_str.starts_with("Contract source code not verified")
        {
            None
        } else {
            Some(
                serde_json::from_str::<JsonAbi>(abi_str)
                    .with_context(|| format!("ABI parse failed for {address}"))?,
            )
        };

        let impl_str = entry
            .get("Implementation")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let impl_addr = if impl_str.is_empty() {
            None
        } else {
            impl_str.parse::<Address>().ok()
        };

        Ok((abi, impl_addr))
    }
}

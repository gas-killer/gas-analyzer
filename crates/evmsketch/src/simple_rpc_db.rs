use std::sync::atomic::{AtomicBool, Ordering};

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::providers::Provider;
use alloy_provider::RootProvider;
use alloy_provider::network::AnyNetwork;
use revm::database_interface::{DBErrorMarker, DatabaseRef};
use revm::primitives::KECCAK_EMPTY;
use revm::state::{AccountInfo, Bytecode};

/// Error type for [`SimpleRpcDb`].
#[derive(Debug)]
pub struct SimpleRpcDbError(String);

impl std::fmt::Display for SimpleRpcDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for SimpleRpcDbError {}
impl DBErrorMarker for SimpleRpcDbError {}

impl From<String> for SimpleRpcDbError {
    fn from(s: String) -> Self {
        SimpleRpcDbError(s)
    }
}

/// Outcome of an `eth_getProof` attempt. Kept private to this module.
enum GetProofOutcome {
    Ok(AccountInfo),
    /// The RPC endpoint explicitly does not support `eth_getProof`
    /// (JSON-RPC error code -32601 "Method not found", or provider-specific
    /// equivalents). Transient errors (network, timeout) do NOT produce this
    /// variant — only an explicit unsupported-method response does.
    MethodNotSupported,
    /// A real error that should be surfaced to the caller.
    Err(SimpleRpcDbError),
}

/// Returns `true` only when an error is an explicit JSON-RPC "Method not found"
/// response (-32601). Transient failures (connection reset, timeout, etc.)
/// are left to propagate normally.
fn is_method_not_found<E: std::fmt::Display>(err: &E) -> bool {
    let s = err.to_string();
    // JSON-RPC standard code for "Method not found".
    // All compliant providers embed -32601 in the error response.
    // Transient errors (IO, timeout, HTTP 5xx) never contain this code.
    s.contains("-32601")
        || s.to_lowercase().contains("method not found")
        || s.to_lowercase().contains("eth_getproof is not supported")
}

/// A minimal revm database backed by standard RPC calls.
///
/// `basic_ref` attempts `eth_getProof` + `eth_getCode` concurrently (≤2 RPCs per
/// account). When the RPC endpoint explicitly rejects `eth_getProof` with a
/// "method not found" response, the `proof_supported` flag is cleared and all
/// subsequent calls use concurrent `eth_getBalance` / `eth_getTransactionCount` /
/// `eth_getCode` via `tokio::join!` (3 RPCs in parallel, not in series). Transient
/// errors are propagated to the caller and do not affect the fallback state.
pub struct SimpleRpcDb {
    pub provider: RootProvider<AnyNetwork>,
    pub block_number: u64,
    /// Cleared after the first explicit "method not found" response from
    /// `eth_getProof`. Subsequent calls skip the attempt entirely.
    proof_supported: AtomicBool,
}

impl SimpleRpcDb {
    pub fn new(provider: RootProvider<AnyNetwork>, block_number: u64) -> Self {
        Self {
            provider,
            block_number,
            proof_supported: AtomicBool::new(true),
        }
    }
}

impl DatabaseRef for SimpleRpcDb {
    type Error = SimpleRpcDbError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let handle =
            tokio::runtime::Handle::try_current().map_err(|e| format!("no tokio runtime: {e}"))?;
        tokio::task::block_in_place(|| handle.block_on(self.fetch_account(address)))
    }

    fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, Self::Error> {
        // Code is always inlined in basic_ref; this should never be called.
        Err("code_by_hash_ref not supported by SimpleRpcDb"
            .to_string()
            .into())
    }

    #[tracing::instrument(name = "evmsketch.rpc.storage", skip_all, level = "debug", fields(block_number = self.block_number, rpc_call_count = 1u32))]
    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        let handle =
            tokio::runtime::Handle::try_current().map_err(|e| format!("no tokio runtime: {e}"))?;
        tokio::task::block_in_place(|| {
            handle.block_on(async {
                self.provider
                    .get_storage_at(address, index)
                    .number(self.block_number)
                    .await
                    .map_err(|e| format!("get_storage_at failed for {address}[{index}]: {e}"))
            })
        })
        .map_err(Into::into)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        let handle =
            tokio::runtime::Handle::try_current().map_err(|e| format!("no tokio runtime: {e}"))?;
        tokio::task::block_in_place(|| {
            handle.block_on(async {
                self.provider
                    .get_block_by_number(number.into())
                    .await
                    .map_err(|e| format!("get_block_by_number failed for {number}: {e}"))
                    .map(|b| b.map(|b| b.header.hash).unwrap_or_default())
            })
        })
        .map_err(Into::into)
    }
}

impl SimpleRpcDb {
    /// Dispatch to the proof path or parallel fallback.
    ///
    /// Only clears `proof_supported` when the RPC explicitly rejects the method.
    /// All other errors (including transient failures) are returned to the caller.
    async fn fetch_account(
        &self,
        address: Address,
    ) -> Result<Option<AccountInfo>, SimpleRpcDbError> {
        if self.proof_supported.load(Ordering::Relaxed) {
            match self.try_fetch_with_proof(address).await {
                GetProofOutcome::Ok(info) => return Ok(Some(info)),
                GetProofOutcome::MethodNotSupported => {
                    self.proof_supported.store(false, Ordering::Relaxed);
                    tracing::debug!(
                        address = %address,
                        "eth_getProof is not supported by this RPC endpoint; \
                         switching permanently to parallel fallback"
                    );
                }
                GetProofOutcome::Err(e) => return Err(e),
            }
        }
        self.fetch_account_parallel(address).await.map(Some)
    }

    /// Attempt to fetch account metadata via `eth_getProof`.
    ///
    /// `eth_getProof` and `eth_getCode` are issued concurrently. The code result
    /// is discarded for EOAs (code_hash == KECCAK_EMPTY).
    ///
    /// Returns [`GetProofOutcome::MethodNotSupported`] when the RPC responds with
    /// a "method not found" error (-32601). All other errors become
    /// [`GetProofOutcome::Err`] and are surfaced to the caller unchanged.
    #[tracing::instrument(name = "evmsketch.rpc.account.proof", skip_all, level = "debug", fields(block_number = self.block_number))]
    async fn try_fetch_with_proof(&self, address: Address) -> GetProofOutcome {
        let block = self.block_number;

        let (proof_res, code_res) = tokio::join!(
            self.provider.get_proof(address, vec![]).number(block),
            self.provider.get_code_at(address).number(block),
        );

        let proof = match proof_res {
            Ok(p) => p,
            Err(e) if is_method_not_found(&e) => return GetProofOutcome::MethodNotSupported,
            Err(e) => {
                return GetProofOutcome::Err(format!("get_proof failed for {address}: {e}").into());
            }
        };

        let bytecode = if proof.code_hash != KECCAK_EMPTY {
            match code_res {
                Ok(code_bytes) => Bytecode::new_raw(code_bytes),
                Err(e) => {
                    return GetProofOutcome::Err(
                        format!("get_code_at failed for {address}: {e}").into(),
                    );
                }
            }
        } else {
            Bytecode::new_raw(Bytes::new())
        };

        GetProofOutcome::Ok(AccountInfo {
            balance: proof.balance,
            nonce: proof.nonce,
            code_hash: proof.code_hash,
            code: Some(bytecode),
        })
    }

    /// Fetch account metadata via 3 concurrent RPCs when `eth_getProof` is unavailable.
    ///
    /// All three calls are issued simultaneously; total latency is bounded by
    /// the slowest call rather than the sum of all three.
    #[tracing::instrument(name = "evmsketch.rpc.account.parallel", skip_all, level = "debug", fields(block_number = self.block_number, rpc_call_count = 3u32))]
    async fn fetch_account_parallel(
        &self,
        address: Address,
    ) -> Result<AccountInfo, SimpleRpcDbError> {
        let block = self.block_number;

        let (balance_res, nonce_res, code_res) = tokio::join!(
            self.provider.get_balance(address).number(block),
            self.provider.get_transaction_count(address).number(block),
            self.provider.get_code_at(address).number(block),
        );

        let balance = balance_res.map_err(|e| format!("get_balance failed for {address}: {e}"))?;
        let nonce = nonce_res.map_err(|e| format!("get_nonce failed for {address}: {e}"))?;
        let code_bytes = code_res.map_err(|e| format!("get_code_at failed for {address}: {e}"))?;
        let bytecode = Bytecode::new_raw(code_bytes);
        let code_hash = bytecode.hash_slow();

        Ok(AccountInfo {
            balance,
            nonce,
            code_hash,
            code: Some(bytecode),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `is_method_not_found` must accept the canonical JSON-RPC -32601 error
    /// string and common provider-specific variants, while rejecting transient
    /// failures that should propagate to the caller.
    #[test]
    fn test_is_method_not_found_detection() {
        // Standard alloy error display for -32601
        assert!(is_method_not_found(
            &"server returned an error response: error code -32601: Method not found"
        ));
        // Short form that some providers emit
        assert!(is_method_not_found(&"error code -32601: method not found"));
        // Provider that spells out the method name
        assert!(is_method_not_found(
            &"eth_getProof is not supported on this endpoint"
        ));
        // Generic "method not found" without an error code
        assert!(is_method_not_found(&"Method not found"));

        // Transient errors — must NOT trigger fallback
        assert!(!is_method_not_found(&"connection reset by peer"));
        assert!(!is_method_not_found(&"request timed out"));
        assert!(!is_method_not_found(
            &"server returned an error response: error code -32000: execution reverted"
        ));
        assert!(!is_method_not_found(
            &"server returned an error response: error code -32603: internal error"
        ));
    }
}

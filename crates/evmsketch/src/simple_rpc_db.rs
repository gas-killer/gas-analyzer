use alloy::primitives::{Address, B256, U256};
use alloy::providers::Provider;
use alloy_provider::RootProvider;
use alloy_provider::network::AnyNetwork;
use revm::database_interface::{DBErrorMarker, DatabaseRef};
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

/// A minimal revm database backed by standard RPC calls.
///
/// Unlike sp1-cc's `BasicRpcDb`, this avoids `eth_getProof` entirely and uses
/// `eth_getBalance` / `eth_getCode` / `eth_getStorageAt` instead. These methods
/// work on any full or archive node without requiring a proof window configuration.
pub struct SimpleRpcDb {
    pub provider: RootProvider<AnyNetwork>,
    pub block_number: u64,
}

impl DatabaseRef for SimpleRpcDb {
    type Error = SimpleRpcDbError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        let handle =
            tokio::runtime::Handle::try_current().map_err(|e| format!("no tokio runtime: {e}"))?;
        tokio::task::block_in_place(|| {
            handle.block_on(async {
                let block = self.block_number;
                let balance = self
                    .provider
                    .get_balance(address)
                    .number(block)
                    .await
                    .map_err(|e| format!("get_balance failed for {address}: {e}"))?;
                let nonce = self
                    .provider
                    .get_transaction_count(address)
                    .number(block)
                    .await
                    .map_err(|e| format!("get_nonce failed for {address}: {e}"))?;
                let code_bytes = self
                    .provider
                    .get_code_at(address)
                    .number(block)
                    .await
                    .map_err(|e| format!("get_code_at failed for {address}: {e}"))?;
                let bytecode = Bytecode::new_raw(code_bytes);
                let code_hash = bytecode.hash_slow();
                Ok(Some(AccountInfo {
                    balance,
                    nonce,
                    code_hash,
                    code: Some(bytecode),
                }))
            })
        })
    }

    fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, Self::Error> {
        // Code is always inlined in basic_ref; this should never be called.
        Err("code_by_hash_ref not supported by SimpleRpcDb"
            .to_string()
            .into())
    }

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

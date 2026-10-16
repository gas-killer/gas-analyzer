//! RPC-based trace fetching functions.
//!
//! This module contains the async functions that require network I/O
//! (alloy-provider, DebugApi). These are separated from the core crate
//! because they are not WASM-compatible.

use alloy::primitives::{Address, FixedBytes};
use alloy::rpc::types::BlockOverrides;
use alloy::rpc::types::trace::geth::{
    CallConfig, CallFrame, DefaultFrame, DiffMode, GethDebugTracingCallOptions,
    GethDebugTracingOptions, GethDefaultTracingOptions, GethTrace, PreStateConfig, PreStateFrame,
};
use alloy::transports::http::{Http, reqwest};
use alloy_eips::BlockId;
use alloy_provider::Provider;
use alloy_provider::ext::DebugApi;
use alloy_rpc_types::TransactionTrait;
use anyhow::{Result, anyhow, bail};
use bytes::Bytes;

use gas_analyzer_core::SimProfile;
use gas_analyzer_core::trace::{TraceExtract, compute_state_updates};
use gas_analyzer_estimator::PrecedingTx;
use gas_analyzer_lean_trace::{LeanFrame, TraceResponse};

/// Apply a [`SimProfile`]'s environment overrides to a `debug_traceCall`
/// request: the transaction-level gas limit goes on the request itself, the
/// block-level gas limit rides in `blockOverrides`.
///
/// Under [`SimProfile::Unbounded`] the overrides are pinned protocol
/// constants (see `gas_analyzer_core::sim_profile`), so they are applied
/// unconditionally — a caller-supplied `gas` field reflects real-chain limits
/// that this profile deliberately lifts. Note the node has the last word:
/// geth/reth clamp `debug_traceCall` gas to `--rpc.gascap` and anvil to its
/// block gas limit, so unbounded extraction requires a node started with the
/// cap lifted (`anvil --disable-block-gas-limit`, `geth --rpc.gascap=0`).
/// A silently-clamping node makes heavy calls OOG, which surfaces as a revert
/// classified for fallback rather than an unsound diff — but extraction still
/// returns `Ok`, with an empty payload, or a partial one if writes landed before
/// the halt. Nothing downstream can distinguish that from a real result, so
/// cap-lifted nodes are a consensus requirement across the operator set, not a
/// per-node performance choice. See `docs/UNBOUNDED_MODE.md`.
fn apply_sim_profile(
    tx_request: &mut alloy::rpc::types::eth::TransactionRequest,
    options: &mut GethDebugTracingCallOptions,
    profile: SimProfile,
) {
    if let Some(tx_gas) = profile.tx_gas_limit_override() {
        tx_request.gas = Some(tx_gas);
    }
    if let Some(block_gas) = profile.block_gas_limit_override() {
        options
            .block_overrides
            .get_or_insert_with(BlockOverrides::default)
            .gas_limit = Some(block_gas);
    }
}

/// Get transaction trace from a provider using debug_traceTransaction.
///
/// `status` must be the receipt status for `tx_hash` (`receipt.status()`).
/// Pass the already-fetched receipt's status to avoid a redundant RPC call.
/// Returns an error immediately if `status` is `false` (transaction reverted).
///
/// The trace keeps memory only on the steps extraction reads it from (see
/// [`gas_analyzer_lean_trace`]), and nothing reads `storage`, so the node isn't asked for it. Over
/// a plain HTTP provider the response is parsed as it arrives rather than buffered whole: a
/// computation-heavy transaction's memory snapshots can run to many gigabytes of JSON.
pub async fn get_tx_trace<P: Provider + DebugApi>(
    provider: &P,
    tx_hash: FixedBytes<32>,
    status: bool,
) -> Result<DefaultFrame> {
    if !status {
        bail!("transaction failed");
    }

    let options = GethDebugTracingOptions {
        config: GethDefaultTracingOptions {
            enable_memory: Some(true),
            disable_storage: Some(true),
            ..Default::default()
        },
        ..Default::default()
    };

    if let Some((client, url)) = http_transport(provider) {
        return stream_tx_trace(&client, &url, tx_hash, &options)
            .await
            .map(Into::into);
    }

    // Any other transport (a layered client, IPC, WebSocket) gets the response whole; parsing it
    // lean still drops most of it.
    let trace: LeanFrame = provider
        .raw_request("debug_traceTransaction".into(), (tx_hash, options))
        .await?;
    Ok(trace.into())
}

/// The HTTP client and URL under `provider`, if its transport is plain reqwest HTTP (as
/// `ProviderBuilder::connect_http` builds), so a request can bypass the transport's buffering.
fn http_transport<P: Provider>(provider: &P) -> Option<(reqwest::Client, String)> {
    let client = provider.client();
    let http = client
        .transport()
        .as_any()
        .downcast_ref::<Http<reqwest::Client>>()?;
    Some((http.client().clone(), http.url().to_string()))
}

/// `debug_traceTransaction` over HTTP, parsed from the response stream as it arrives.
async fn stream_tx_trace(
    client: &reqwest::Client,
    url: &str,
    tx_hash: FixedBytes<32>,
    options: &GethDebugTracingOptions,
) -> Result<LeanFrame> {
    let body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "debug_traceTransaction",
        "params": [tx_hash, options],
    }))?;
    let mut response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        bail!("debug_traceTransaction failed: HTTP {status}: {text}");
    }

    // A few chunks in flight: enough to keep the connection busy while the parser catches up.
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let parse = tokio::task::spawn_blocking(move || {
        serde_json::from_reader::<_, TraceResponse>(std::io::BufReader::new(ChunkReader {
            rx,
            chunk: Bytes::new(),
        }))
    });
    let read = async move {
        loop {
            let chunk = response
                .chunk()
                .await
                .map_err(|e| std::io::Error::other(e.to_string()));
            let done = matches!(chunk, Ok(None) | Err(_));
            // A send fails once the parser has stopped, done or failed; its result says which.
            if tx.send(chunk.transpose()).await.is_err() || done {
                return;
            }
        }
    };
    let ((), parsed) = tokio::join!(read, parse);
    let response = parsed?.map_err(|e| anyhow!("failed to parse debug_traceTransaction: {e}"))?;
    match (response.result, response.error) {
        (_, Some(error)) => bail!("debug_traceTransaction failed: {}", error.message),
        (Some(trace), None) => Ok(trace),
        (None, None) => bail!("debug_traceTransaction response has neither a result nor an error"),
    }
}

/// The response body's chunks, as a blocking [`std::io::Read`] for `serde_json::from_reader`.
struct ChunkReader {
    rx: tokio::sync::mpsc::Receiver<Option<std::io::Result<Bytes>>>,
    chunk: Bytes,
}

impl std::io::Read for ChunkReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        while self.chunk.is_empty() {
            match self.rx.blocking_recv().flatten() {
                Some(chunk) => self.chunk = chunk?,
                None => return Ok(0),
            }
        }
        let n = buf.len().min(self.chunk.len());
        buf[..n].copy_from_slice(&self.chunk.split_to(n));
        Ok(n)
    }
}

/// Get trace from a simulated call using debug_traceCall.
///
/// This simulates the call at the given block and returns the default-format trace.
/// Requires an RPC that supports debug_traceCall (e.g. Geth, Erigon).
pub async fn get_trace_from_call<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
) -> Result<DefaultFrame>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    get_trace_from_call_with_profile(provider, tx_request, block, SimProfile::Chain).await
}

/// [`get_trace_from_call`] with an explicit [`SimProfile`] controlling the
/// simulated environment (gas limits). `SimProfile::Chain` is identical to
/// the plain function.
pub async fn get_trace_from_call_with_profile<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
    profile: SimProfile,
) -> Result<DefaultFrame>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    let mut tx_request = tx_request.into();
    let mut options = GethDebugTracingCallOptions {
        tracing_options: GethDebugTracingOptions {
            config: GethDefaultTracingOptions {
                enable_memory: Some(true),
                disable_storage: Some(true),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    apply_sim_profile(&mut tx_request, &mut options, profile);

    let GethTrace::Default(trace) = provider
        .debug_trace_call(tx_request, block, options)
        .await
        .map_err(|e| anyhow!("debug_trace_call failed: {}", e))?
    else {
        return Err(anyhow!("Expected default trace from debug_trace_call"));
    };

    Ok(trace)
}

/// Simulate a call and return the `prestateTracer` diff (`diffMode`): per-account pre/post storage.
///
/// Cost is `O(changed slots)` rather than `O(execution steps)`, so this succeeds on heavy-compute
/// tracked functions whose struct-log trace would time out the node. Used together with
/// [`get_call_frame_from_call`] to build the prestate net form (see `gas_analyzer_core::prestate`).
pub async fn get_prestate_diff_from_call<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
) -> Result<DiffMode>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    get_prestate_diff_from_call_with_profile(provider, tx_request, block, SimProfile::Chain).await
}

/// [`get_prestate_diff_from_call`] with an explicit [`SimProfile`] controlling
/// the simulated environment (gas limits). `SimProfile::Chain` is identical to
/// the plain function.
pub async fn get_prestate_diff_from_call_with_profile<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
    profile: SimProfile,
) -> Result<DiffMode>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    let mut tx_request = tx_request.into();
    let mut options = GethDebugTracingCallOptions {
        tracing_options: GethDebugTracingOptions::prestate_tracer(PreStateConfig {
            diff_mode: Some(true),
            disable_code: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    apply_sim_profile(&mut tx_request, &mut options, profile);
    match provider
        .debug_trace_call(tx_request, block, options)
        .await
        .map_err(|e| anyhow!("prestate debug_trace_call failed: {}", e))?
    {
        GethTrace::PreStateTracer(PreStateFrame::Diff(diff)) => Ok(diff),
        other => Err(anyhow!("expected prestate diff frame, got {other:?}")),
    }
}

/// Simulate a call and return the `callTracer` frame (with logs): the call tree + emitted events.
///
/// Used together with [`get_prestate_diff_from_call`] to recover events for the prestate net form and
/// to classify whether that form exists (no regular CALL / CREATE / cross-contract storage).
pub async fn get_call_frame_from_call<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
) -> Result<CallFrame>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    get_call_frame_from_call_with_profile(provider, tx_request, block, SimProfile::Chain).await
}

/// [`get_call_frame_from_call`] with an explicit [`SimProfile`] controlling
/// the simulated environment (gas limits). `SimProfile::Chain` is identical to
/// the plain function.
pub async fn get_call_frame_from_call_with_profile<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
    profile: SimProfile,
) -> Result<CallFrame>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    let mut tx_request = tx_request.into();
    let mut options = GethDebugTracingCallOptions {
        tracing_options: GethDebugTracingOptions::call_tracer(CallConfig {
            with_log: Some(true),
            ..Default::default()
        }),
        ..Default::default()
    };
    apply_sim_profile(&mut tx_request, &mut options, profile);
    match provider
        .debug_trace_call(tx_request, block, options)
        .await
        .map_err(|e| anyhow!("callTracer debug_trace_call failed: {}", e))?
    {
        GethTrace::CallTracer(frame) => Ok(frame),
        other => Err(anyhow!("expected call frame, got {other:?}")),
    }
}

/// Compute state updates from an existing transaction using its actual trace.
///
/// `status` must be `receipt.status()` for `tx_hash`. Pass the already-fetched
/// receipt's status to avoid an extra `eth_getTransactionReceipt` round-trip.
///
/// `origin` is the traced transaction's target contract, used for re-entry
/// detection (see [`TraceExtract::reentered`]); pass `None` to skip it.
pub async fn compute_state_updates_from_tx<P: Provider + DebugApi>(
    provider: &P,
    tx_hash: FixedBytes<32>,
    status: bool,
    origin: Option<Address>,
) -> Result<TraceExtract> {
    // Primary path: use the historical trace via debug_traceTransaction.
    let trace = get_tx_trace(provider, tx_hash, status).await?;
    let struct_logs_len = trace.struct_logs.len();
    if struct_logs_len > 0 {
        return compute_state_updates(trace, origin);
    }

    bail!(
        "debug_traceTransaction returned an empty trace for tx {}. \
         Some RPCs (notably Anvil) omit step-level tracing by default. \
         When using Anvil, start it with --steps-tracing to enable debug_traceTransaction support.",
        tx_hash
    )
}

// ============================================================================
// Block Transaction Fetching
// ============================================================================

/// Fetch preceding transactions from a block for replay.
///
/// Calls `eth_getBlockByNumber(block_number, true)` to get the block with
/// full transaction objects, then converts transactions at indices `0..tx_index`
/// into `PrecedingTx` structs suitable for replay in revm.
///
/// Returns an empty vec if `tx_index` is 0 (first in block).
pub async fn get_preceding_transactions<P: Provider>(
    provider: &P,
    block_number: u64,
    tx_index: u64,
) -> Result<Vec<PrecedingTx>> {
    if tx_index == 0 {
        return Ok(Vec::new());
    }

    let block = provider
        .get_block_by_number(block_number.into())
        .full()
        .await?
        .ok_or_else(|| anyhow!("Block {} not found", block_number))?;

    let base_fee = block.header.base_fee_per_gas;
    let txs: Vec<_> = block.transactions.into_transactions().collect();

    if (tx_index as usize) >= txs.len() {
        bail!(
            "Transaction index {} exceeds block transaction count {}",
            tx_index,
            txs.len()
        );
    }

    let preceding: Vec<PrecedingTx> = txs[..tx_index as usize]
        .iter()
        .map(|tx| {
            let kind = match tx.inner.to() {
                Some(addr) => revm::primitives::TxKind::Call(addr),
                None => revm::primitives::TxKind::Create,
            };
            // EIP-2930 access list pre-warms slots/addresses; without it
            // replay sees higher gas costs than the original tx and can
            // OOG-halt, dropping storage writes from the CacheDB.
            let access_list = tx.inner.access_list().cloned().unwrap_or_default();
            // EIP-7702 set-code authorizations; only present on type-4 txs.
            let authorization_list = tx
                .inner
                .authorization_list()
                .map(<[_]>::to_vec)
                .unwrap_or_default();
            PrecedingTx {
                from: tx.inner.signer(),
                kind,
                input: tx.inner.input().clone(),
                value: tx.inner.value(),
                gas_limit: tx.inner.gas_limit(),
                nonce: tx.inner.nonce(),
                gas_price: tx.inner.effective_gas_price(base_fee),
                access_list,
                authorization_list,
            }
        })
        .collect();

    Ok(preceding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_provider::ProviderBuilder;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const MEMORY: &str = "00000000000000000000000000000000000000000000000000000000deadbeef";

    /// A `debug_traceTransaction` response: an SSTORE whose memory is dropped, then a LOG0 that
    /// keeps its memory.
    fn trace_response() -> String {
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "failed": false, "gas": 21000, "returnValue": "0x",
                "structLogs": [
                    {"pc": 0, "op": "SSTORE", "gas": 100, "gasCost": 20, "depth": 1,
                     "stack": ["0x2", "0x1"], "memory": [MEMORY, MEMORY], "storage": {"00": "01"}},
                    {"pc": 1, "op": "LOG0", "gas": 80, "gasCost": 8, "depth": 1,
                     "stack": ["0x20", "0x0"], "memory": [MEMORY]},
                ],
            },
        })
        .to_string()
    }

    /// Serve one HTTP request with `body`, written a few bytes at a time, and hand back the
    /// request's JSON body.
    async fn serve_once(body: String) -> (url::Url, tokio::task::JoinHandle<serde_json::Value>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            let request_body = loop {
                let n = socket.read(&mut buf).await.unwrap();
                request.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length: usize = text[..end]
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        break serde_json::from_slice(&request[end + 4..end + 4 + length]).unwrap();
                    }
                }
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                body.len()
            );
            socket.write_all(head.as_bytes()).await.unwrap();
            for piece in body.as_bytes().chunks(7) {
                socket.write_all(piece).await.unwrap();
                socket.flush().await.unwrap();
            }
            request_body
        });
        (url, server)
    }

    #[tokio::test]
    async fn http_provider_streams_a_lean_trace() {
        let (url, server) = serve_once(trace_response()).await;
        let provider = ProviderBuilder::new().connect_http(url);
        assert!(http_transport(&provider).is_some());

        let trace = get_tx_trace(&provider, FixedBytes::ZERO, true)
            .await
            .unwrap();
        let request = server.await.unwrap();
        assert_eq!(request["method"], "debug_traceTransaction");
        assert_eq!(request["params"][1]["enableMemory"], true);
        assert_eq!(request["params"][1]["disableStorage"], true);

        assert_eq!(trace.struct_logs.len(), 2);
        assert_eq!(trace.struct_logs[0].memory, Some(vec![]));
        assert_eq!(trace.struct_logs[0].storage, None);
        assert_eq!(trace.struct_logs[1].memory, Some(vec![MEMORY.to_string()]));
        let extract = compute_state_updates(trace, None).unwrap();
        assert_eq!(extract.state_updates.len(), 2);
        assert_eq!(extract.sstore_gas_total, 20);
    }

    #[tokio::test]
    async fn streamed_rpc_error_is_reported() {
        let body = r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"tracing failed"}}"#;
        let (url, _server) = serve_once(body.to_string()).await;
        let provider = ProviderBuilder::new().connect_http(url);
        let err = get_tx_trace(&provider, FixedBytes::ZERO, true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("tracing failed"), "{err}");
    }

    #[tokio::test]
    async fn truncated_stream_is_an_error() {
        let full = trace_response();
        let (url, _server) = serve_once(full[..full.len() / 2].to_string()).await;
        let provider = ProviderBuilder::new().connect_http(url);
        assert!(
            get_tx_trace(&provider, FixedBytes::ZERO, true)
                .await
                .is_err()
        );
    }
}

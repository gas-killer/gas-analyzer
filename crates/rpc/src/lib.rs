//! RPC-based trace fetching functions.
//!
//! This module contains the async functions that require network I/O
//! (alloy-provider, DebugApi). These are separated from the core crate
//! because they are not WASM-compatible.

use alloy::primitives::{Address, FixedBytes};
use alloy::rpc::types::BlockOverrides;
use alloy::rpc::types::trace::geth::mux::{MuxConfig, MuxFrame};
use alloy::rpc::types::trace::geth::{
    CallConfig, CallFrame, DefaultFrame, DiffMode, GethDebugBuiltInTracerType,
    GethDebugTracingCallOptions, GethDebugTracingOptions, GethDefaultTracingOptions, GethTrace,
    PreStateConfig, PreStateFrame,
};
use alloy::transports::TransportError;
use alloy_eips::BlockId;
use alloy_json_rpc::ErrorPayload;
use alloy_provider::Provider;
use alloy_provider::ext::DebugApi;
use alloy_rpc_types::TransactionTrait;
use anyhow::{Result, anyhow, bail};

use gas_analyzer_core::SimProfile;
use gas_analyzer_core::trace::{TraceExtract, compute_state_updates};
use gas_analyzer_estimator::PrecedingTx;

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
            ..Default::default()
        },
        ..Default::default()
    };

    let GethTrace::Default(trace) = provider.debug_trace_transaction(tx_hash, options).await?
    else {
        return Err(anyhow!("Expected default trace"));
    };

    Ok(trace)
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
        tracing_options: GethDebugTracingOptions::prestate_tracer(prestate_diff_config()),
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
        tracing_options: GethDebugTracingOptions::call_tracer(call_with_log_config()),
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

/// Simulate a call once and return both the prestate diff of
/// [`get_prestate_diff_from_call`] and the `callTracer` frame of
/// [`get_call_frame_from_call`], configured identically, from one `muxTracer` request.
///
/// Each tracer request re-executes the call, so asking for both separately runs it twice; for the
/// heavy-compute calls the prestate net form exists to serve, execution is nearly all of the cost.
///
/// Any answer other than the two frames is reported rather than returned as an error, so the
/// caller can request the tracers separately: endpoints that lack or allow-list `muxTracer` word
/// the rejection differently, and an error that came from executing the call will recur on the
/// separate requests anyway. Only a transport failure, where nothing was answered, is an error.
pub async fn get_prestate_diff_and_call_frame_with_profile<P, Req>(
    provider: &P,
    tx_request: Req,
    block: BlockId,
    profile: SimProfile,
) -> Result<MuxedTraces>
where
    P: Provider + DebugApi,
    Req: Into<alloy::rpc::types::eth::TransactionRequest>,
{
    let mut tx_request = tx_request.into();
    let mut options = GethDebugTracingCallOptions {
        tracing_options: GethDebugTracingOptions::mux_tracer(MuxConfig(
            [
                (
                    GethDebugBuiltInTracerType::PreStateTracer,
                    Some(prestate_diff_config().into()),
                ),
                (
                    GethDebugBuiltInTracerType::CallTracer,
                    Some(call_with_log_config().into()),
                ),
            ]
            .into_iter()
            .collect(),
        )),
        ..Default::default()
    };
    apply_sim_profile(&mut tx_request, &mut options, profile);
    match provider.debug_trace_call(tx_request, block, options).await {
        Ok(trace) => Ok(match split_prestate_and_call(trace) {
            Ok(frames) => MuxedTraces::Frames(Box::new(frames)),
            Err(e) => MuxedTraces::Malformed(e.to_string()),
        }),
        Err(e) => classify_mux_error(&e),
    }
}

/// How a failed `muxTracer` request was answered, if it was.
///
/// An endpoint can refuse a tracer in the JSON-RPC body or, at a gateway, with a 4xx status before
/// any JSON-RPC is parsed; both are answers. A connection failure, a 5xx, or a rate limit or
/// request timeout (HTTP 429 or 408, or their JSON-RPC equivalents) is not: it says the endpoint is
/// busy, not what it supports, and requesting the separate tracers at once would only add load.
/// Those stay errors, for the caller to retry with backoff as it would without `muxTracer`.
fn classify_mux_error(error: &TransportError) -> Result<MuxedTraces> {
    let (reason, payload) = if let Some(resp) = error
        .as_error_resp()
        .filter(|resp| !is_rate_limited(resp.code, &resp.message))
    {
        (resp.to_string(), Some(resp.clone()))
    } else if let Some(http) = error
        .as_transport_err()
        .and_then(|kind| kind.as_http_error())
        .filter(|http| (400..500).contains(&http.status) && ![408, 429].contains(&http.status))
    {
        (format!("HTTP {}: {}", http.status, http.body), None)
    } else {
        bail!("muxTracer debug_trace_call failed: {error}");
    };
    Ok(if is_unsupported_tracer(&reason) {
        MuxedTraces::Unsupported
    } else {
        MuxedTraces::Rejected { reason, payload }
    })
}

/// Whether a JSON-RPC error says the endpoint is throttling requests: code 429, or -32005 as geth,
/// Infura and Alchemy use it, or the wording of either.
fn is_rate_limited(code: i64, message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    code == 429
        || code == -32005
        || message.contains("rate limit")
        || message.contains("too many requests")
}

/// What a `muxTracer` request for the prestate diff and call frame produced.
#[derive(Debug)]
pub enum MuxedTraces {
    /// Both frames, from one execution of the call.
    Frames(Box<(DiffMode, CallFrame)>),
    /// The endpoint said it has no `muxTracer`.
    Unsupported,
    /// The endpoint answered with an error, which may be about the call or about `muxTracer`.
    /// `payload` is the JSON-RPC error when there was one, rather than a bare HTTP status.
    Rejected {
        reason: String,
        payload: Option<ErrorPayload>,
    },
    /// The endpoint ran the request but answered in a shape other than the two frames.
    Malformed(String),
}

/// The prestate diff and call frame out of a `muxTracer` response.
fn split_prestate_and_call(trace: GethTrace) -> Result<(DiffMode, CallFrame)> {
    let GethTrace::MuxTracer(MuxFrame(mut frames)) = trace else {
        bail!("expected mux frame, got {trace:?}");
    };
    let diff = match frames.remove(&GethDebugBuiltInTracerType::PreStateTracer) {
        Some(GethTrace::PreStateTracer(PreStateFrame::Diff(diff))) => diff,
        other => bail!("expected prestate diff frame in mux response, got {other:?}"),
    };
    let frame = match frames.remove(&GethDebugBuiltInTracerType::CallTracer) {
        Some(GethTrace::CallTracer(frame)) => frame,
        other => bail!("expected call frame in mux response, got {other:?}"),
    };
    Ok((diff, frame))
}

/// The `prestateTracer` config the prestate net form reads: the net diff, without contract code.
fn prestate_diff_config() -> PreStateConfig {
    PreStateConfig {
        diff_mode: Some(true),
        disable_code: Some(true),
        ..Default::default()
    }
}

/// The `callTracer` config the prestate net form reads: the call tree with its logs.
fn call_with_log_config() -> CallConfig {
    CallConfig {
        with_log: Some(true),
        ..Default::default()
    }
}

/// Whether an RPC error message says the endpoint does not offer the requested tracer, as opposed
/// to the call itself failing. Matched on the message because codes differ: anvil answers `-32602
/// "unsupported tracer type"`, geth reports the tracer as not found, and hosted providers that
/// allow-list tracers call it not available or not allowed.
fn is_unsupported_tracer(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("tracer")
        && [
            "unsupported",
            "unknown",
            "not supported",
            "not found",
            "not available",
            "not allowed",
            "disabled",
        ]
        .iter()
        .any(|reason| message.contains(reason))
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

    const CALL_FRAME: &str = r#"{
        "from": "0x004260f8179001930baf7d65a96ae9647ea1dc1d",
        "gas": "0x1c9c380",
        "gasUsed": "0x177a6",
        "to": "0x974554de55ac3e837c3502a11c27eab2993c761f",
        "input": "0x8344f7710000000000000000000000000000000000000000000000000000000000000001",
        "logs": [{
            "address": "0x974554de55ac3e837c3502a11c27eab2993c761f",
            "topics": ["0x5c1b3e2a1f8d7e0c4b9a6f3d2e1c0b9a8f7e6d5c4b3a2f1e0d9c8b7a6f5e4d3c"],
            "data": "0x0000000000000000000000000000000000000000000000000000000000000002",
            "position": "0x0"
        }],
        "value": "0x0",
        "type": "CALL"
    }"#;

    const PRESTATE_DIFF: &str = r#"{
        "pre": {
            "0x974554de55ac3e837c3502a11c27eab2993c761f": {
                "balance": "0x0",
                "nonce": 1,
                "storage": {
                    "0x0000000000000000000000000000000000000000000000000000000000000003": "0x0000000000000000000000000000000000000000000000000000000000000001"
                }
            }
        },
        "post": {
            "0x974554de55ac3e837c3502a11c27eab2993c761f": {
                "storage": {
                    "0x0000000000000000000000000000000000000000000000000000000000000003": "0x0000000000000000000000000000000000000000000000000000000000000002"
                }
            }
        }
    }"#;

    #[test]
    fn a_mux_response_splits_into_the_frames_the_separate_tracers_return() {
        let response =
            format!(r#"{{"callTracer": {CALL_FRAME}, "prestateTracer": {PRESTATE_DIFF}}}"#);
        let trace: GethTrace = serde_json::from_str(&response).unwrap();

        let (diff, frame) = split_prestate_and_call(trace).unwrap();

        assert_eq!(
            diff,
            serde_json::from_str::<DiffMode>(PRESTATE_DIFF).unwrap()
        );
        assert_eq!(
            frame,
            serde_json::from_str::<CallFrame>(CALL_FRAME).unwrap()
        );
    }

    #[test]
    fn a_mux_response_missing_a_tracer_is_an_error() {
        let response = format!(r#"{{"callTracer": {CALL_FRAME}}}"#);
        let trace: GethTrace = serde_json::from_str(&response).unwrap();

        assert!(split_prestate_and_call(trace).is_err());
    }

    /// A provider that answers each request with the next queued response.
    fn mocked() -> (impl Provider + DebugApi, alloy_transport::mock::Asserter) {
        let asserter = alloy_transport::mock::Asserter::new();
        let provider =
            alloy_provider::ProviderBuilder::new().connect_mocked_client(asserter.clone());
        (provider, asserter)
    }

    async fn muxed(provider: &(impl Provider + DebugApi)) -> MuxedTraces {
        get_prestate_diff_and_call_frame_with_profile(
            provider,
            alloy::rpc::types::eth::TransactionRequest::default(),
            BlockId::latest(),
            SimProfile::Chain,
        )
        .await
        .expect("an answered request is never an error")
    }

    #[tokio::test]
    async fn both_frames_come_back_from_one_request() {
        let (provider, asserter) = mocked();
        let response: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"callTracer": {CALL_FRAME}, "prestateTracer": {PRESTATE_DIFF}}}"#
        ))
        .unwrap();
        asserter.push_success(&response);

        assert!(matches!(muxed(&provider).await, MuxedTraces::Frames(..)));
    }

    #[tokio::test]
    async fn a_rejection_in_any_wording_is_reported_rather_than_raised() {
        let (provider, asserter) = mocked();
        asserter.push_failure_msg("method not allowed on this plan");

        let MuxedTraces::Rejected { reason, .. } = muxed(&provider).await else {
            panic!("an unrecognised rejection must still be reported as one");
        };
        assert!(reason.contains("method not allowed"), "{reason}");
    }

    #[tokio::test]
    async fn a_known_missing_tracer_is_reported_as_unsupported() {
        let (provider, asserter) = mocked();
        asserter.push_failure_msg("unsupported tracer type");

        assert!(matches!(muxed(&provider).await, MuxedTraces::Unsupported));
    }

    /// An endpoint that runs the mux but ignores `diffMode` answers with the full prestate.
    #[tokio::test]
    async fn an_answer_in_the_wrong_shape_is_reported_as_malformed() {
        let (provider, asserter) = mocked();
        let response: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"callTracer": {CALL_FRAME}, "prestateTracer": {{"0x974554de55ac3e837c3502a11c27eab2993c761f": {{"balance": "0x0"}}}}}}"#
        ))
        .unwrap();
        asserter.push_success(&response);

        assert!(matches!(muxed(&provider).await, MuxedTraces::Malformed(_)));
    }

    /// A gateway can refuse a tracer with a 4xx before any JSON-RPC is parsed; that is still an
    /// answer. A server error or a dropped connection is not.
    #[test]
    fn a_4xx_status_is_an_answer_and_a_server_or_connection_failure_is_not() {
        use alloy::transports::TransportErrorKind;

        let plan = TransportErrorKind::http_error(
            403,
            "tracer muxTracer is not available on this plan".into(),
        );
        assert!(matches!(
            classify_mux_error(&plan),
            Ok(MuxedTraces::Unsupported)
        ));

        let forbidden = TransportErrorKind::http_error(403, "Forbidden".into());
        let Ok(MuxedTraces::Rejected { reason, .. }) = classify_mux_error(&forbidden) else {
            panic!("a 4xx in any wording is an answer");
        };
        assert!(reason.contains("HTTP 403"), "{reason}");

        let refused = TransportErrorKind::http_error(
            400,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"unsupported tracer type"}}"#.into(),
        );
        assert!(matches!(
            classify_mux_error(&refused),
            Ok(MuxedTraces::Unsupported)
        ));

        let bad_gateway = TransportErrorKind::http_error(502, "bad gateway".into());
        assert!(classify_mux_error(&bad_gateway).is_err());

        let dropped = TransportErrorKind::custom_str("connection refused");
        assert!(classify_mux_error(&dropped).is_err());

        for status in [408, 429] {
            let busy = TransportErrorKind::http_error(status, "Too Many Requests".into());
            assert!(
                classify_mux_error(&busy).is_err(),
                "HTTP {status} is not an answer"
            );
        }
    }

    /// A rate limit in the JSON-RPC body says the endpoint is busy, not what it supports, so it
    /// neither falls back nor counts toward pausing `muxTracer`.
    #[test]
    fn a_json_rpc_rate_limit_is_an_error_not_a_rejection() {
        for (code, message) in [
            (429, "Too Many Requests"),
            (-32005, "limit exceeded"),
            (
                -32000,
                "Your app has exceeded its compute units per second capacity, rate limit",
            ),
        ] {
            let error = alloy::transports::RpcError::ErrorResp(ErrorPayload {
                code,
                message: message.to_string().into(),
                data: None,
            });
            assert!(classify_mux_error(&error).is_err(), "{code} {message}");
        }
        let reverted = alloy::transports::RpcError::ErrorResp(ErrorPayload {
            code: 3,
            message: "execution reverted".into(),
            data: None,
        });
        assert!(matches!(
            classify_mux_error(&reverted),
            Ok(MuxedTraces::Rejected { .. })
        ));
    }

    #[test]
    fn only_a_missing_tracer_counts_as_unsupported() {
        assert!(is_unsupported_tracer("unsupported tracer type"));
        assert!(is_unsupported_tracer("tracer not found"));
        assert!(is_unsupported_tracer("Unknown tracer: muxTracer"));
        assert!(is_unsupported_tracer(
            "tracer muxTracer is not available on this plan"
        ));
        assert!(!is_unsupported_tracer("execution reverted"));
        assert!(!is_unsupported_tracer("out of gas"));
        assert!(!is_unsupported_tracer("request timed out"));
    }
}

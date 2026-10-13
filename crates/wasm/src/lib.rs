// The exports take the JS API's positional arguments, and the `_inner` functions and their helpers
// mirror them.
#![allow(clippy::too_many_arguments)]

use std::collections::{BTreeMap, BTreeSet, HashSet};

use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_rpc_types::trace::geth::{AccountState, CallFrame, DefaultFrame, DiffMode};
use gas_analyzer_core::nested::{
    FrameProgram, NESTING_COST_MODEL_V1, compute_frame_tree_hypothetical, frame_tree_total,
    unnested_owned,
};
use gas_analyzer_core::{
    PrestateEligibility, StateUpdate, TURETZKY_UPPER_GAS_LIMIT_SCHNORR, TraceExtract,
    build_state_updates_from_prestate, classify_prestate_eligibility, compute_state_updates,
    encode_state_updates_to_abi, estimate_gas_from_state_updates,
};
use gas_analyzer_estimator::{SimEnvOpts, estimate_frame_tree_gas, estimate_state_changes_gas};
use revm::database::{CacheDB, EmptyDB};
use revm::primitives::KECCAK_EMPTY;
use revm::primitives::hardfork::SpecId;
use revm::state::{AccountInfo, Bytecode};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

mod lean_trace;
use lean_trace::LeanFrame;

/// Initialize panic hook for better error messages in browser console.
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
pub struct AnalyzeTraceResult {
    pub encoded_updates: String,
    pub gas_estimate: u64,
    pub is_heuristic: bool,
    pub state_update_count: usize,
    pub skipped_opcodes: Vec<String>,
    /// A callee called back into the origin contract during the traced
    /// execution; the estimate counts that callback gas as external and may
    /// overshoot. Only detected when an origin address was supplied.
    pub reentered: bool,
    /// What the callees of the `CALL`s the program replays spent in the trace. Included in
    /// `gas_estimate` unless `callees_executed`, since the callees then have no code to run.
    pub native_call_gas: u64,
    /// The callees ran their own code over the state from `prestate`, so `gas_estimate` measures
    /// what they cost in the settlement instead of taking it from the trace.
    pub callees_executed: bool,
    /// Why a replay over `prestate` failed and the estimate fell back, so a missing `timestamp` or
    /// block can be told from a callee that reverts in the settlement.
    pub replay_error: Option<String>,
    /// See [`SIGNATURE_VERIFICATION_GAS`]; not included in `gas_estimate`.
    pub signature_verification_gas: u64,
}

/// The outcome of [`analyze_prestate`]: a result when the call has a prestate net form, otherwise
/// the reason it doesn't, so the caller can fall back to [`analyze_trace`].
#[derive(Debug, serde::Serialize)]
pub struct PrestateAnalysis {
    pub eligible: bool,
    pub reason: Option<String>,
    pub result: Option<AnalyzeTraceResult>,
}

#[derive(Debug, serde::Serialize)]
pub struct EncodeTraceResult {
    pub encoded_updates: String,
    pub state_update_count: usize,
    pub skipped_opcodes: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct EstimateGasResult {
    pub gas_estimate: u64,
    pub is_heuristic: bool,
    pub state_update_count: usize,
    pub skipped_opcodes: Vec<String>,
    /// See [`AnalyzeTraceResult::reentered`].
    pub reentered: bool,
}

/// What a transaction would have cost settled as a nested tree, had its target and the `owned`
/// contracts integrated the SDK.
#[derive(Debug, serde::Serialize)]
pub struct FrameTreeAnalysis {
    /// The whole call settled by its target alone, split by the same walker, so the difference
    /// from `nested_gas` is the nesting alone.
    pub root_only_gas: u64,
    pub nested_gas: u64,
    /// What the callees of the `CALL`s each settlement replays natively spent in the trace.
    /// Included in its total unless `callees_executed`: against an empty state those callees have
    /// no code, so the simulation charges only the `CALL` opcodes.
    pub root_only_native_call_gas: u64,
    pub nested_native_call_gas: u64,
    /// The callees ran their own code over the state from `prestate`, so both totals measure what
    /// they cost in the settlement instead of taking it from the trace.
    pub callees_executed: bool,
    /// See [`AnalyzeTraceResult::replay_error`]; names the settlement that failed.
    pub replay_error: Option<String>,
    /// The cost model's fixed charge per nested frame, included in `nested_gas`.
    pub frame_overhead_gas: u64,
    /// See [`SIGNATURE_VERIFICATION_GAS`]; one signature covers the whole tree, and neither total
    /// includes it.
    pub signature_verification_gas: u64,
    /// Root first, in execution order.
    pub frames: Vec<FrameGas>,
    pub not_nested: Vec<NotNested>,
    pub skipped_opcodes: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct FrameGas {
    pub target: String,
    /// Zero for the root.
    pub caller: String,
    pub op_count: usize,
    /// Measured with one base transaction cost, as if the frame were its own transaction.
    pub gas: u64,
}

/// An owned contract that got no frame.
#[derive(Debug, serde::Serialize)]
pub struct NotNested {
    pub address: String,
    pub reason: String,
}

/// Gas to verify the operators' aggregate signature, which every settlement pays on top of its
/// program. The service signs with Schnorr only, so this is its upper limit; add it to an estimate
/// for the transaction's total with Gas Killer.
pub const SIGNATURE_VERIFICATION_GAS: u64 = TURETZKY_UPPER_GAS_LIMIT_SCHNORR;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn parse_and_compute(
    trace_json: &str,
    origin: Option<Address>,
) -> Result<(TraceExtract, BTreeSet<Address>), String> {
    let trace: LeanFrame =
        serde_json::from_str(trace_json).map_err(|e| format!("Failed to parse trace: {}", e))?;
    compute(trace, origin)
}

/// The trace's state updates, and [`callees_with_code`].
fn compute(
    trace: LeanFrame,
    origin: Option<Address>,
) -> Result<(TraceExtract, BTreeSet<Address>), String> {
    let trace = DefaultFrame::from(trace);
    let with_code = callees_with_code(&trace);
    let extract = compute_state_updates(trace, origin)
        .map_err(|e| format!("Failed to compute state updates: {}", e))?;
    Ok((extract, with_code))
}

/// Every account a `CALL` or `CALLCODE` in `trace` entered a frame in. Only code runs a frame, so
/// each had code when called and wasn't empty (EIP-161).
fn callees_with_code(trace: &DefaultFrame) -> BTreeSet<Address> {
    trace
        .struct_logs
        .windows(2)
        .filter_map(|pair| {
            let [call, next] = pair else { return None };
            if !matches!(call.op.as_ref(), "CALL" | "CALLCODE")
                || call.error.is_some()
                || next.depth != call.depth + 1
            {
                return None;
            }
            let stack = call.stack.as_ref()?;
            let target = stack.get(stack.len().checked_sub(2)?)?;
            Some(Address::from_word((*target).into()))
        })
        .collect()
}

/// A `debug_traceTransaction` JSON-RPC response; `jsonrpc` and `id` are ignored.
#[derive(Deserialize)]
struct TraceResponse {
    result: Option<LeanFrame>,
    error: Option<RpcErrorBody>,
}

#[derive(Deserialize)]
struct RpcErrorBody {
    #[serde(default)]
    message: String,
}

/// Why [`analyze_trace_bytes_inner`] produced no result.
#[derive(Debug, PartialEq)]
pub enum AnalyzeTraceBytesError {
    /// The node answered with an error instead of a trace. `too_large` marks a provider refusing
    /// to send a trace over its response size limit.
    Rpc { message: String, too_large: bool },
    /// The response couldn't be parsed or analyzed.
    Analysis(String),
}

/// Whether an RPC error message is a provider's response size limit, as worded by geth, Erigon,
/// Alchemy, QuickNode and others.
fn is_too_large(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("too big")
        || m.contains("too large")
        || m.contains("size exceeds")
        || m.contains("larger than")
        || m.find("exceeds").is_some_and(|i| m[i..].contains("limit"))
}

/// Parse an optional hex origin address for re-entry detection.
fn parse_origin(origin_address: Option<&str>) -> Result<Option<Address>, String> {
    origin_address
        .map(|s| {
            s.parse()
                .map_err(|e| format!("Invalid origin address: {}", e))
        })
        .transpose()
}

/// What lets the callees of a replayed `CALL` run their own code: the state the transaction started
/// from, and the gas price and timestamp they may check.
pub struct Replay {
    /// `prestateTracer` in its default mode: every account the transaction touched, with its code,
    /// balance, nonce and every slot it touched, as they were before the transaction.
    state: BTreeMap<Address, AccountState>,
    gas_price: u128,
    timestamp: u64,
}

impl Replay {
    pub fn parse(
        prestate_json: &str,
        gas_price: Option<&str>,
        timestamp: Option<u64>,
    ) -> Result<Replay, String> {
        let state = serde_json::from_str(prestate_json)
            .map_err(|e| format!("Failed to parse prestate: {}", e))?;
        let gas_price = parse_value(gas_price)?
            .try_into()
            .map_err(|_| "gas price is out of range".to_string())?;
        Ok(Replay {
            state,
            gas_price,
            timestamp: timestamp.unwrap_or_default(),
        })
    }

    /// Every touched account as it was before the transaction, code included.
    fn seeds(&self) -> Seeds {
        let mut seeds = Seeds::default();
        for (address, account) in &self.state {
            let code = account.code.clone().filter(|code| !code.is_empty());
            seeds.accounts.insert(
                *address,
                AccountInfo {
                    balance: account.balance.unwrap_or_default(),
                    nonce: account.nonce.unwrap_or_default(),
                    code_hash: code.as_ref().map_or(KECCAK_EMPTY, keccak256),
                    code: code.map(Bytecode::new_raw),
                },
            );
            seeds.storage.insert(*address, account.storage.clone());
        }
        seeds
    }

    fn sim_env(&self, block_number: Option<u64>, value: U256) -> SimEnvOpts {
        SimEnvOpts {
            gas_price: self.gas_price,
            timestamp: self.timestamp,
            ..sim_env(block_number, value)
        }
    }
}

fn parse_diff(diff_json: &str) -> Result<DiffMode, String> {
    serde_json::from_str(diff_json).map_err(|e| format!("Failed to parse prestate diff: {}", e))
}

/// What the empty state holds before simulating, from the transaction's prestate diff.
#[derive(Default)]
struct Seeds {
    /// Slots at their values before the transaction, by account.
    storage: BTreeMap<Address, BTreeMap<B256, B256>>,
    /// The accounts the program calls that existed before the transaction.
    accounts: BTreeMap<Address, AccountInfo>,
}

impl Seeds {
    /// Seeds for a program running at `at` whose writes and calls stand in for `consumer`'s.
    fn for_program<'a>(
        diff: Option<(&DiffMode, Address)>,
        at: Address,
        updates: impl IntoIterator<Item = &'a StateUpdate> + Clone,
        with_code: &BTreeSet<Address>,
    ) -> Seeds {
        Seeds {
            storage: diff
                .map(|(diff, consumer)| {
                    BTreeMap::from([(at, original_storage(diff, consumer, updates.clone()))])
                })
                .unwrap_or_default(),
            accounts: called_accounts(diff.map(|(diff, _)| diff), updates, with_code),
        }
    }
}

/// Each account `updates` calls that wasn't empty (EIP-161) when called.
///
/// Every callee is missing from the empty state, so a `CALL` that sends it ETH pays the 25,000-gas
/// new-account charge, which on chain only an empty recipient pays. A recipient whose balance the
/// transaction changed is in the prestate diff's `pre`, where an empty one has no nonce or code and
/// a zero or absent balance. One whose balance ended unchanged sent the ETH on, which takes code,
/// so the trace shows it in `with_code`; the diff leaves such an account out.
fn called_accounts<'a>(
    diff: Option<&DiffMode>,
    updates: impl IntoIterator<Item = &'a StateUpdate>,
    with_code: &BTreeSet<Address>,
) -> BTreeMap<Address, AccountInfo> {
    updates
        .into_iter()
        .filter_map(|update| {
            let StateUpdate::Call(call) = update else {
                return None;
            };
            let existing = AccountInfo {
                nonce: 1,
                ..AccountInfo::default()
            };
            let Some(pre) = diff.and_then(|diff| diff.pre.get(&call.target)) else {
                return with_code
                    .contains(&call.target)
                    .then_some((call.target, existing));
            };
            let balance = pre.balance.unwrap_or_default();
            let nonce = pre.nonce.unwrap_or_default();
            let has_code = pre.code.as_ref().is_some_and(|code| !code.is_empty())
                || with_code.contains(&call.target);
            if balance.is_zero() && nonce == 0 && !has_code {
                return None;
            }
            Some((
                call.target,
                AccountInfo {
                    balance,
                    // Callees run without code here, so a contract deployed before Spurious Dragon
                    // with nonce 0 needs one to not be empty.
                    nonce: if has_code { nonce.max(1) } else { nonce },
                    ..AccountInfo::default()
                },
            ))
        })
        .collect()
}

/// The value before the transaction of each slot `updates` stores to in `account`, from its
/// prestate diff. Without it every slot starts at zero, and each write is priced as a fresh 22.1k
/// one where the chain charged ~2.9k to change a set slot or ~100 to rewrite it.
///
/// A slot only in `post` started at zero. A slot in neither half of the diff ended where it
/// started, so the value last stored to it is its original.
fn original_storage<'a>(
    diff: &DiffMode,
    account: Address,
    updates: impl IntoIterator<Item = &'a StateUpdate>,
) -> BTreeMap<B256, B256> {
    let pre = diff.pre.get(&account).map(|a| &a.storage);
    let post = diff.post.get(&account).map(|a| &a.storage);
    let mut last = BTreeMap::new();
    for update in updates {
        if let StateUpdate::Store(store) = update {
            last.insert(store.slot, store.value);
        }
    }
    last.into_iter()
        .map(|(slot, last)| {
            let original = match pre.and_then(|p| p.get(&slot)) {
                Some(value) => *value,
                None if post.is_some_and(|p| p.contains_key(&slot)) => B256::ZERO,
                None => last,
            };
            (slot, original)
        })
        .collect()
}

/// A fresh empty state holding `seeds`. The estimator injected into a seeded account later keeps
/// its storage.
fn seeded_db(seeds: &Seeds) -> Result<CacheDB<EmptyDB>, String> {
    let mut cache_db = CacheDB::new(EmptyDB::default());
    for (account, info) in &seeds.accounts {
        cache_db.insert_account_info(*account, info.clone());
    }
    for (account, slots) in &seeds.storage {
        for (slot, value) in slots.iter().filter(|(_, v)| !v.is_zero()) {
            cache_db
                .insert_account_storage(
                    *account,
                    U256::from_be_bytes(slot.0),
                    U256::from_be_bytes(value.0),
                )
                .map_err(|e| format!("Failed to seed storage: {:?}", e))?;
        }
    }
    Ok(cache_db)
}

fn to_js<T: Serialize>(value: &T) -> Result<JsValue, JsError> {
    let serializer = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
    value
        .serialize(&serializer)
        .map_err(|e| JsError::new(&e.to_string()))
}

// ---------------------------------------------------------------------------
// Inner functions (testable without wasm-bindgen)
// ---------------------------------------------------------------------------

pub fn analyze_trace_inner(
    trace_json: &str,
    estimator_address: &str,
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    origin_address: Option<&str>,
    value: Option<&str>,
    prestate_diff_json: Option<&str>,
    replay: Option<&Replay>,
) -> Result<AnalyzeTraceResult, String> {
    let origin = parse_origin(origin_address)?;
    let (extract, with_code) = parse_and_compute(trace_json, origin)?;
    let diff = trace_diff(origin, prestate_diff_json)?;
    analyze_extract(
        &extract,
        estimator_address,
        caller_address,
        estimate_state_changes_block_number,
        value,
        diff.as_ref().map(|(diff, origin)| (diff, *origin)),
        &with_code,
        trace_replay(origin, replay)?,
    )
}

/// The replay for a struct-log extraction, whose program stands in for `origin`.
fn trace_replay(
    origin: Option<Address>,
    replay: Option<&Replay>,
) -> Result<Option<(&Replay, Address)>, String> {
    replay
        .map(|replay| {
            let origin = origin.ok_or("a prestate needs origin_address, the account it settles")?;
            Ok((replay, origin))
        })
        .transpose()
}

/// The prestate diff for a struct-log extraction, whose program stands in for `origin`.
fn trace_diff(
    origin: Option<Address>,
    prestate_diff_json: Option<&str>,
) -> Result<Option<(DiffMode, Address)>, String> {
    let Some(diff_json) = prestate_diff_json else {
        return Ok(None);
    };
    let origin =
        origin.ok_or("a prestate diff needs origin_address, the account whose slots it seeds")?;
    Ok(Some((parse_diff(diff_json)?, origin)))
}

/// [`analyze_trace_inner`] on a whole `debug_traceTransaction` JSON-RPC response, undecoded.
pub fn analyze_trace_bytes_inner(
    response: &[u8],
    estimator_address: &str,
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    origin_address: Option<&str>,
    value: Option<&str>,
    prestate_diff_json: Option<&str>,
    replay: Option<&Replay>,
) -> Result<AnalyzeTraceResult, AnalyzeTraceBytesError> {
    use AnalyzeTraceBytesError::Analysis;
    let origin = parse_origin(origin_address).map_err(Analysis)?;
    let trace = parse_trace_response(response)?;
    let (extract, with_code) = compute(trace, origin).map_err(Analysis)?;
    let diff = trace_diff(origin, prestate_diff_json).map_err(Analysis)?;
    analyze_extract(
        &extract,
        estimator_address,
        caller_address,
        estimate_state_changes_block_number,
        value,
        diff.as_ref().map(|(diff, origin)| (diff, *origin)),
        &with_code,
        trace_replay(origin, replay).map_err(Analysis)?,
    )
    .map_err(Analysis)
}

/// The trace in a `debug_traceTransaction` JSON-RPC response, or the node's error.
fn parse_trace_response(response: &[u8]) -> Result<LeanFrame, AnalyzeTraceBytesError> {
    use AnalyzeTraceBytesError::{Analysis, Rpc};
    let response: TraceResponse = serde_json::from_slice(response)
        .map_err(|e| Analysis(format!("Failed to parse trace: {}", e)))?;
    match (response.result, response.error) {
        (_, Some(error)) => {
            let too_large = is_too_large(&error.message);
            Err(Rpc {
                message: error.message,
                too_large,
            })
        }
        (Some(trace), None) => Ok(trace),
        (None, None) => Err(Analysis(
            "RPC response has neither a result nor an error".to_string(),
        )),
    }
}

/// Analyze a call from its `prestateTracer` (`diffMode`) diff and `callTracer` (`withLog`) frame,
/// which stay small however much computation the call does, unlike its struct-log trace.
///
/// Returns `eligible: false` with a reason when the net form can't represent the call (see
/// [`classify_prestate_eligibility`]); its state updates then have to come from the struct-log trace.
/// For the same call the net form can differ from [`analyze_trace_inner`]'s program: repeated writes
/// to a slot collapse to one, and a slot written back to its original value produces none.
pub fn analyze_prestate_inner(
    diff_json: &str,
    call_frame_json: &str,
    consumer_address: &str,
    estimator_address: &str,
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    value: Option<&str>,
) -> Result<PrestateAnalysis, String> {
    let diff = parse_diff(diff_json)?;
    let frame: CallFrame = serde_json::from_str(call_frame_json)
        .map_err(|e| format!("Failed to parse call frame: {}", e))?;
    let consumer: Address = consumer_address
        .parse()
        .map_err(|e| format!("Invalid consumer address: {}", e))?;

    let ineligible = |reason: String| PrestateAnalysis {
        eligible: false,
        reason: Some(reason),
        result: None,
    };
    // A creation's storage comes from its constructor, which the net form can't stand in for.
    if frame.typ != "CALL" {
        return Ok(ineligible(format!(
            "top-level frame is {}, not CALL",
            frame.typ
        )));
    }
    if let PrestateEligibility::Fallback(reason) =
        classify_prestate_eligibility(&frame, &diff, consumer)
    {
        return Ok(ineligible(reason));
    }

    let (sstore_gas_total, refund_counter) = net_sstore_costs(&diff, consumer);
    let extract = TraceExtract {
        state_updates: build_state_updates_from_prestate(consumer, &diff, &frame),
        skipped_opcodes: HashSet::new(),
        // Eligibility rules out regular CALLs at target depth, and with them re-entry.
        call_gas_total: 0,
        callee_gas_total: 0,
        sstore_gas_total,
        refund_counter,
        reentered: false,
    };
    Ok(PrestateAnalysis {
        eligible: true,
        reason: None,
        result: Some(analyze_extract(
            &extract,
            estimator_address,
            caller_address,
            estimate_state_changes_block_number,
            value,
            Some((&diff, consumer)),
            &BTreeSet::new(),
            None,
        )?),
    })
}

/// EIP-2929/2200 cold SSTORE charges.
const SSTORE_COLD_SET_COST: u64 = 22_100;
const SSTORE_COLD_RESET_COST: u64 = 5_000;
/// EIP-3529 refund for clearing a slot.
const SSTORE_CLEARS_REFUND: u64 = 4_800;

/// SSTORE gas and refunds for the heuristic, priced as one cold write per changed slot: a diff has
/// no per-write `gasCost` the way a struct-log trace does.
fn net_sstore_costs(diff: &DiffMode, consumer: Address) -> (u64, u64) {
    let empty = BTreeMap::new();
    let pre = diff
        .pre
        .get(&consumer)
        .map(|a| &a.storage)
        .unwrap_or(&empty);
    let post = diff
        .post
        .get(&consumer)
        .map(|a| &a.storage)
        .unwrap_or(&empty);
    let slots: BTreeSet<&B256> = pre.keys().chain(post.keys()).collect();
    let (mut gas, mut refund) = (0, 0);
    for slot in slots {
        let old = pre.get(slot).copied().unwrap_or(B256::ZERO);
        let new = post.get(slot).copied().unwrap_or(B256::ZERO);
        if old == new {
            continue;
        }
        gas += if old == B256::ZERO {
            SSTORE_COLD_SET_COST
        } else {
            SSTORE_COLD_RESET_COST
        };
        if old != B256::ZERO && new == B256::ZERO {
            refund += SSTORE_CLEARS_REFUND;
        }
    }
    (gas, refund)
}

/// Encode an extraction's state updates and estimate their gas with revm, falling back to the
/// heuristic when the simulation fails.
fn analyze_extract(
    extract: &TraceExtract,
    estimator_address: &str,
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    value: Option<&str>,
    diff: Option<(&DiffMode, Address)>,
    with_code: &BTreeSet<Address>,
    replay: Option<(&Replay, Address)>,
) -> Result<AnalyzeTraceResult, String> {
    let state_updates: &[StateUpdate] = &extract.state_updates;

    let encoded = encode_state_updates_to_abi(state_updates);

    let addr: Address = estimator_address
        .parse()
        .map_err(|e| format!("Invalid estimator address: {}", e))?;

    let caller = parse_caller(caller_address)?;

    let value = parse_value(value)?;

    // Injected at the consumer itself, the estimator runs the program over the state the
    // transaction started from, so each callee runs its own code, as the consumer's callee.
    let replayed = replay.map(|(replay, consumer)| {
        let mut cache_db = seeded_db(&replay.seeds())?;
        let sim_env = replay.sim_env(estimate_state_changes_block_number, value);
        estimate_state_changes_gas(&mut cache_db, consumer, caller, state_updates, &sim_env)
            .map_err(|e| e.to_string())
    });
    let replay_error = replayed.as_ref().and_then(|r| r.as_ref().err().cloned());

    let (gas_estimate, is_heuristic, callees_executed) = match replayed {
        Some(Ok(gas)) => (gas, false, true),
        _ => {
            // The program writes the consumer's slots, here into the estimator's storage.
            let seeds = Seeds::for_program(diff, addr, state_updates, with_code);
            let mut cache_db = seeded_db(&seeds)?;
            let sim_env = sim_env(estimate_state_changes_block_number, value);
            match estimate_state_changes_gas(&mut cache_db, addr, caller, state_updates, &sim_env) {
                // The callees have no code here, so the simulation charges only the `CALL`s.
                Ok(gas) => (gas.saturating_add(extract.callee_gas_total), false, false),
                Err(_) => (estimate_gas_from_state_updates(extract), true, false),
            }
        }
    };

    let mut skipped = extract.skipped_opcodes.iter().cloned().collect::<Vec<_>>();
    skipped.sort();

    Ok(AnalyzeTraceResult {
        encoded_updates: format!("0x{}", hex::encode(&encoded)),
        gas_estimate,
        is_heuristic,
        state_update_count: state_updates.len(),
        skipped_opcodes: skipped,
        reentered: extract.reentered,
        native_call_gas: extract.callee_gas_total,
        callees_executed,
        replay_error,
        signature_verification_gas: SIGNATURE_VERIFICATION_GAS,
    })
}

fn parse_caller(caller_address: &str) -> Result<Address, String> {
    caller_address
        .parse()
        .map_err(|e| format!("Invalid caller address: {}", e))
}

/// A `msg.value` in wei, decimal or `0x` hex; zero when absent.
fn parse_value(value: Option<&str>) -> Result<U256, String> {
    value.map_or(Ok(U256::ZERO), |v| {
        v.parse().map_err(|e| format!("Invalid value {v:?}: {e}"))
    })
}

/// The simulation environment for an analysis against `EmptyDB`. `value` is the transaction's
/// `msg.value`: a payload that forwards it in a `CALL` halts unfunded without it.
fn sim_env(block_number: Option<u64>, value: U256) -> SimEnvOpts {
    SimEnvOpts {
        number: block_number.unwrap_or(0),
        timestamp: 0,
        gas_limit: 30_000_000,
        coinbase: Address::ZERO,
        prevrandao: B256::ZERO,
        // No contract code exists in `EmptyDB`, so nothing can read `tx.gasprice`.
        gas_price: 0,
        basefee: 0,
        difficulty: U256::ZERO,
        // WASM runs against EmptyDB with no real chain state; pick the newest
        // spec so post-Pectra opcodes don't halt with `NotActivated`.
        spec: SpecId::OSAKA,
        value,
    }
}

/// [`original_storage`] for every contract a frame tree writes, where the estimator is injected
/// at each frame's own target, and [`called_accounts`] for every frame.
///
/// A contract with several frames (sibling calls, or a cycle) has its writes ordered by when each
/// frame returns, since a frame's final slice is its last write to its target.
fn frame_tree_seeds(
    diff: Option<&DiffMode>,
    frames: &[FrameProgram],
    with_code: &BTreeSet<Address>,
) -> Seeds {
    let accounts = called_accounts(diff, frames.iter().flat_map(|f| &f.updates), with_code);
    let Some(diff) = diff else {
        return Seeds {
            accounts,
            ..Seeds::default()
        };
    };
    let mut by_target: BTreeMap<Address, Vec<&StateUpdate>> = BTreeMap::new();
    for id in return_order(frames) {
        let frame = &frames[id];
        by_target
            .entry(frame.target)
            .or_default()
            .extend(&frame.updates);
    }
    Seeds {
        storage: by_target
            .into_iter()
            .map(|(target, updates)| (target, original_storage(diff, target, updates)))
            .collect(),
        accounts,
    }
}

/// Frame ids in the order the frames return: each one after its children, siblings in call order.
fn return_order(frames: &[FrameProgram]) -> Vec<usize> {
    fn visit(frames: &[FrameProgram], id: usize, order: &mut Vec<usize>) {
        for &child in &frames[id].children {
            visit(frames, child, order);
        }
        order.push(id);
    }
    let mut order = Vec::with_capacity(frames.len());
    if !frames.is_empty() {
        visit(frames, 0, &mut order);
    }
    order
}

/// Price `trace` as a nested settlement rooted at `root` with `owned` as consumers, against the
/// root-only settlement of the same trace.
fn analyze_frame_tree_trace(
    trace: DefaultFrame,
    root_address: &str,
    owned: &[String],
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    value: Option<&str>,
    prestate_diff_json: Option<&str>,
    replay: Option<&Replay>,
) -> Result<FrameTreeAnalysis, String> {
    let diff = prestate_diff_json.map(parse_diff).transpose()?;
    let root: Address = root_address
        .parse()
        .map_err(|e| format!("Invalid root address: {}", e))?;
    let owned = owned
        .iter()
        .map(|a| {
            a.parse()
                .map_err(|e| format!("Invalid owned address {a:?}: {e}"))
        })
        .filter(|a| a.as_ref() != Ok(&root))
        .collect::<Result<BTreeSet<Address>, String>>()?;
    let caller = parse_caller(caller_address)?;
    let value = parse_value(value)?;
    let sim_env = sim_env(estimate_state_changes_block_number, value);
    let cost = NESTING_COST_MODEL_V1;

    let with_code = callees_with_code(&trace);
    let root_only = compute_frame_tree_hypothetical(trace.clone(), root, &BTreeSet::new(), &cost)
        .map_err(|e| format!("Failed to split trace: {}", e))?;
    let nested = compute_frame_tree_hypothetical(trace, root, &owned, &cost)
        .map_err(|e| format!("Failed to split trace: {}", e))?;
    let price = |frames: &[FrameProgram]| {
        let seeds = frame_tree_seeds(diff.as_ref(), frames, &with_code);
        estimate_frame_tree_gas(&mut seeded_db(&seeds)?, frames, caller, value, &sim_env)
            .map_err(|e| format!("Gas estimation failed: {}", e))
    };
    // With the starting state, each frame's target keeps its storage and balance under the injected
    // estimator, and the callees outside the tree run their own code. Both settlements are priced
    // the same way, so their difference stays the nesting alone.
    let replayed = replay.map(|replay| {
        let env = replay.sim_env(estimate_state_changes_block_number, value);
        let price = |settlement: &str, frames: &[FrameProgram]| {
            let mut cache_db = seeded_db(&replay.seeds())?;
            estimate_frame_tree_gas(&mut cache_db, frames, caller, value, &env)
                .map_err(|e| format!("{settlement}: {e}"))
        };
        Ok::<_, String>((
            price("root-only", &root_only.frames)?,
            price("nested", &nested.frames)?,
        ))
    });
    let replay_error = replayed.as_ref().and_then(|r| r.as_ref().err().cloned());
    let (root_only_gas, frame_gas, root_only_callees, nested_callees, callees_executed) =
        match replayed {
            Some(Ok((root_only_gas, frame_gas))) => (root_only_gas, frame_gas, 0, 0, true),
            _ => (
                price(&root_only.frames)?,
                price(&nested.frames)?,
                root_only.callee_gas_total,
                nested.callee_gas_total,
                false,
            ),
        };
    let root_only_gas = frame_tree_total(&root_only_gas, &cost).saturating_add(root_only_callees);

    let mut skipped = nested.skipped_opcodes.into_iter().collect::<Vec<_>>();
    skipped.sort();
    Ok(FrameTreeAnalysis {
        root_only_gas,
        nested_gas: frame_tree_total(&frame_gas, &cost).saturating_add(nested_callees),
        callees_executed,
        replay_error,
        root_only_native_call_gas: root_only.callee_gas_total,
        nested_native_call_gas: nested.callee_gas_total,
        frame_overhead_gas: cost.frame_overhead_gas,
        signature_verification_gas: SIGNATURE_VERIFICATION_GAS,
        frames: nested
            .frames
            .iter()
            .zip(frame_gas)
            .map(|(frame, gas)| FrameGas {
                target: frame.target.to_string(),
                caller: frame.caller.to_string(),
                op_count: frame.updates.len(),
                gas,
            })
            .collect(),
        not_nested: unnested_owned(&owned, &nested.frames)
            .into_iter()
            .map(|(address, reason)| NotNested {
                address: address.to_string(),
                reason: reason.describe().to_string(),
            })
            .collect(),
        skipped_opcodes: skipped,
    })
}

/// What `trace_json`, a struct-log trace, would have cost as a nested settlement; see
/// [`analyze_frame_tree`].
pub fn analyze_frame_tree_inner(
    trace_json: &str,
    root_address: &str,
    owned: &[String],
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    value: Option<&str>,
    prestate_diff_json: Option<&str>,
    replay: Option<&Replay>,
) -> Result<FrameTreeAnalysis, String> {
    let trace: LeanFrame =
        serde_json::from_str(trace_json).map_err(|e| format!("Failed to parse trace: {}", e))?;
    analyze_frame_tree_trace(
        trace.into(),
        root_address,
        owned,
        caller_address,
        estimate_state_changes_block_number,
        value,
        prestate_diff_json,
        replay,
    )
}

/// [`analyze_frame_tree_inner`] on a whole `debug_traceTransaction` JSON-RPC response, undecoded.
pub fn analyze_frame_tree_bytes_inner(
    response: &[u8],
    root_address: &str,
    owned: &[String],
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    value: Option<&str>,
    prestate_diff_json: Option<&str>,
    replay: Option<&Replay>,
) -> Result<FrameTreeAnalysis, AnalyzeTraceBytesError> {
    let trace = parse_trace_response(response)?;
    analyze_frame_tree_trace(
        trace.into(),
        root_address,
        owned,
        caller_address,
        estimate_state_changes_block_number,
        value,
        prestate_diff_json,
        replay,
    )
    .map_err(AnalyzeTraceBytesError::Analysis)
}

pub fn estimate_gas_heuristic_inner(
    trace_json: &str,
    origin_address: Option<&str>,
) -> Result<EstimateGasResult, String> {
    let (extract, _) = parse_and_compute(trace_json, parse_origin(origin_address)?)?;

    let gas = estimate_gas_from_state_updates(&extract);

    let mut skipped = extract.skipped_opcodes.into_iter().collect::<Vec<_>>();
    skipped.sort();

    Ok(EstimateGasResult {
        gas_estimate: gas,
        is_heuristic: true,
        state_update_count: extract.state_updates.len(),
        skipped_opcodes: skipped,
        reentered: extract.reentered,
    })
}

pub fn encode_trace_inner(trace_json: &str) -> Result<EncodeTraceResult, String> {
    let TraceExtract {
        state_updates,
        skipped_opcodes,
        ..
    } = parse_and_compute(trace_json, None)?.0;

    let encoded = encode_state_updates_to_abi(&state_updates);
    let mut skipped = skipped_opcodes.into_iter().collect::<Vec<_>>();
    skipped.sort();

    Ok(EncodeTraceResult {
        encoded_updates: format!("0x{}", hex::encode(&encoded)),
        state_update_count: state_updates.len(),
        skipped_opcodes: skipped,
    })
}

// ---------------------------------------------------------------------------
// wasm-bindgen exports
// ---------------------------------------------------------------------------

/// Analyze a Geth trace: parse state updates, ABI-encode them, and estimate gas.
///
/// `trace_json` is the JSON body of a debug_traceTransaction/debug_traceCall response
/// (the `result` field, not the full JSON-RPC envelope).
///
/// `estimator_address` is the hex address where the gas estimator contract will be
/// deployed in the empty CacheDB, e.g. `"0x1234..."`.
///
/// `caller_address` is the hex address of the original transaction sender, used as
/// `tx.origin` during gas simulation.
///
/// `origin_address` is the traced transaction's target contract; when supplied,
/// callbacks re-entering it are detected and reported via `reentered`.
///
/// `value` is the transaction's `msg.value` in wei, as a decimal or `0x` hex string. A payload
/// whose `CALL`s forward that ETH is otherwise unfunded, and falls back to the heuristic.
///
/// `prestate_diff_json` is the transaction's `prestateTracer` diff (see [`analyze_prestate`]). It
/// seeds the slots the program writes in `origin_address`, which it requires, with their values
/// before the transaction, and creates the called accounts that already existed. Without it every
/// write is priced as one to an empty slot, and every ETH transfer as one to a new account.
///
/// `prestate_json` is the transaction's `prestateTracer` output in its default mode, with
/// `gas_price` (wei, as for `value`) and `block_timestamp`. With it the program runs at
/// `origin_address` over the state the transaction started from, so each callee runs its own code;
/// see [`Replay`]. When that reverts, the estimate falls back to the empty state.
///
/// Returns a JS object with: `encoded_updates`, `gas_estimate`, `is_heuristic`,
/// `state_update_count`, `skipped_opcodes`, `reentered`, `native_call_gas`, `callees_executed`,
/// `replay_error`.
#[wasm_bindgen]
pub fn analyze_trace(
    trace_json: &str,
    estimator_address: &str,
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    origin_address: Option<String>,
    value: Option<String>,
    prestate_diff_json: Option<String>,
    prestate_json: Option<String>,
    gas_price: Option<String>,
    block_timestamp: Option<u64>,
) -> Result<JsValue, JsError> {
    let replay = prestate_json
        .map(|json| Replay::parse(&json, gas_price.as_deref(), block_timestamp))
        .transpose()
        .map_err(|e| JsError::new(&e))?;
    let result = analyze_trace_inner(
        trace_json,
        estimator_address,
        caller_address,
        estimate_state_changes_block_number,
        origin_address.as_deref(),
        value.as_deref(),
        prestate_diff_json.as_deref(),
        replay.as_ref(),
    )
    .map_err(|e| JsError::new(&e))?;
    to_js(&result)
}

/// [`analyze_trace`] on the raw bytes of a whole `debug_traceTransaction` JSON-RPC response, so a
/// large trace never has to be decoded into a JS string or unwrapped from its envelope in JS.
///
/// When the node answered with an error, throws an `Error` named `RpcError` with the node's
/// message, or `TraceTooLargeError` when the message is the provider's response size limit.
/// Other failures throw a plain `Error`, as [`analyze_trace`] does.
#[wasm_bindgen]
pub fn analyze_trace_bytes(
    response: &[u8],
    estimator_address: &str,
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    origin_address: Option<String>,
    value: Option<String>,
    prestate_diff_json: Option<String>,
    prestate_json: Option<String>,
    gas_price: Option<String>,
    block_timestamp: Option<u64>,
) -> Result<JsValue, JsValue> {
    let replay = prestate_json
        .map(|json| Replay::parse(&json, gas_price.as_deref(), block_timestamp))
        .transpose()
        .map_err(|e| JsValue::from(JsError::new(&e)))?;
    let result = analyze_trace_bytes_inner(
        response,
        estimator_address,
        caller_address,
        estimate_state_changes_block_number,
        origin_address.as_deref(),
        value.as_deref(),
        prestate_diff_json.as_deref(),
        replay.as_ref(),
    )
    .map_err(bytes_error_to_js)?;
    Ok(to_js(&result)?)
}

/// A JS `Error` named for what went wrong; see [`analyze_trace_bytes`].
fn bytes_error_to_js(e: AnalyzeTraceBytesError) -> JsValue {
    let (name, message) = match e {
        AnalyzeTraceBytesError::Rpc { message, too_large } => (
            if too_large {
                "TraceTooLargeError"
            } else {
                "RpcError"
            },
            message,
        ),
        AnalyzeTraceBytesError::Analysis(message) => ("Error", message),
    };
    let error = js_sys::Error::new(&message);
    error.set_name(name);
    JsValue::from(error)
}

/// Analyze a transaction from its `prestateTracer` diff and `callTracer` frame instead of its
/// struct-log trace, for calls that admit the prestate net form.
///
/// `diff_json` is the `result` of `debug_traceTransaction` with
/// `{"tracer":"prestateTracer","tracerConfig":{"diffMode":true}}`, and `call_frame_json` the
/// `result` with `{"tracer":"callTracer","tracerConfig":{"withLog":true}}`.
///
/// `consumer_address` is the transaction's target contract (its `to`). The other arguments are as
/// for [`analyze_trace`].
///
/// Returns a JS object with `eligible`, and either `result` (shaped like [`analyze_trace`]'s) or
/// `reason`, why the call has no net form; analyze its struct-log trace with [`analyze_trace`] then.
#[wasm_bindgen]
pub fn analyze_prestate(
    diff_json: &str,
    call_frame_json: &str,
    consumer_address: &str,
    estimator_address: &str,
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    value: Option<String>,
) -> Result<JsValue, JsError> {
    let result = analyze_prestate_inner(
        diff_json,
        call_frame_json,
        consumer_address,
        estimator_address,
        caller_address,
        estimate_state_changes_block_number,
        value.as_deref(),
    )
    .map_err(|e| JsError::new(&e))?;
    to_js(&result)
}

/// What a transaction would have cost settled as a nested tree, had its target `root_address`
/// and the contracts in `owned` integrated the SDK: the browser counterpart of the CLI's
/// `t <tx> --owned`.
///
/// `trace_json` is the struct-log trace, as for [`analyze_trace`]. A `CALL` from a frame into an
/// owned contract becomes that contract's own frame where nesting is cheaper under the cost model.
/// Both the nested tree and the root-only settlement are split from the same trace and priced in
/// execution order on one state, so their difference is the nesting alone. With `prestate_json`
/// that is the transaction's starting state and the callees outside the tree run their own code;
/// otherwise it's empty but for what `prestate_diff_json` supplies, and a `CALL` either settlement
/// replays natively is charged what its callee spent in the trace. There is no heuristic fallback:
/// a simulation failure throws.
///
/// `caller_address`, `estimate_state_changes_block_number`, `value`, `prestate_diff_json`,
/// `prestate_json`, `gas_price` and `block_timestamp` are as for [`analyze_trace`].
///
/// Returns a JS object with `root_only_gas`, `nested_gas`, `frame_overhead_gas`, `frames`,
/// `not_nested` and `skipped_opcodes`.
#[wasm_bindgen]
pub fn analyze_frame_tree(
    trace_json: &str,
    root_address: &str,
    owned: Vec<String>,
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    value: Option<String>,
    prestate_diff_json: Option<String>,
    prestate_json: Option<String>,
    gas_price: Option<String>,
    block_timestamp: Option<u64>,
) -> Result<JsValue, JsError> {
    let replay = prestate_json
        .map(|json| Replay::parse(&json, gas_price.as_deref(), block_timestamp))
        .transpose()
        .map_err(|e| JsError::new(&e))?;
    let result = analyze_frame_tree_inner(
        trace_json,
        root_address,
        &owned,
        caller_address,
        estimate_state_changes_block_number,
        value.as_deref(),
        prestate_diff_json.as_deref(),
        replay.as_ref(),
    )
    .map_err(|e| JsError::new(&e))?;
    to_js(&result)
}

/// [`analyze_frame_tree`] on the raw bytes of a whole `debug_traceTransaction` JSON-RPC response,
/// throwing as [`analyze_trace_bytes`] does.
#[wasm_bindgen]
pub fn analyze_frame_tree_bytes(
    response: &[u8],
    root_address: &str,
    owned: Vec<String>,
    caller_address: &str,
    estimate_state_changes_block_number: Option<u64>,
    value: Option<String>,
    prestate_diff_json: Option<String>,
    prestate_json: Option<String>,
    gas_price: Option<String>,
    block_timestamp: Option<u64>,
) -> Result<JsValue, JsValue> {
    let replay = prestate_json
        .map(|json| Replay::parse(&json, gas_price.as_deref(), block_timestamp))
        .transpose()
        .map_err(|e| JsValue::from(JsError::new(&e)))?;
    let result = analyze_frame_tree_bytes_inner(
        response,
        root_address,
        &owned,
        caller_address,
        estimate_state_changes_block_number,
        value.as_deref(),
        prestate_diff_json.as_deref(),
        replay.as_ref(),
    )
    .map_err(bytes_error_to_js)?;
    Ok(to_js(&result)?)
}

/// Heuristic-only gas estimation (no revm, faster, less accurate).
///
/// `origin_address` enables re-entry detection — see [`analyze_trace`].
#[wasm_bindgen]
pub fn estimate_gas_heuristic(
    trace_json: &str,
    origin_address: Option<String>,
) -> Result<JsValue, JsError> {
    let result = estimate_gas_heuristic_inner(trace_json, origin_address.as_deref())
        .map_err(|e| JsError::new(&e))?;
    to_js(&result)
}

/// Encode state updates only (no gas estimation).
#[wasm_bindgen]
pub fn encode_trace(trace_json: &str) -> Result<JsValue, JsError> {
    let result = encode_trace_inner(trace_json).map_err(|e| JsError::new(&e))?;
    to_js(&result)
}

// ---------------------------------------------------------------------------
// TypeScript type definitions
// ---------------------------------------------------------------------------

#[wasm_bindgen(typescript_custom_section)]
const TS_TYPES: &str = r#"
export interface AnalyzeTraceResult {
    encoded_updates: string;
    gas_estimate: number;
    is_heuristic: boolean;
    state_update_count: number;
    skipped_opcodes: string[];
    reentered: boolean;
    native_call_gas: number;
    callees_executed: boolean;
    replay_error: string | null;
    signature_verification_gas: number;
}

export interface PrestateAnalysis {
    eligible: boolean;
    reason: string | null;
    result: AnalyzeTraceResult | null;
}

export interface EncodeTraceResult {
    encoded_updates: string;
    state_update_count: number;
    skipped_opcodes: string[];
}

export interface EstimateGasResult {
    gas_estimate: number;
    is_heuristic: boolean;
    state_update_count: number;
    skipped_opcodes: string[];
    reentered: boolean;
}

export interface FrameTreeAnalysis {
    root_only_gas: number;
    nested_gas: number;
    root_only_native_call_gas: number;
    nested_native_call_gas: number;
    callees_executed: boolean;
    replay_error: string | null;
    frame_overhead_gas: number;
    signature_verification_gas: number;
    frames: FrameGas[];
    not_nested: NotNested[];
    skipped_opcodes: string[];
}

export interface FrameGas {
    target: string;
    caller: string;
    op_count: number;
    gas: number;
}

export interface NotNested {
    address: string;
    reason: string;
}
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ===== Fixture builders =====

    /// Build a DefaultFrame JSON string from struct log entries.
    fn make_trace(struct_logs: Vec<serde_json::Value>) -> String {
        serde_json::json!({
            "failed": false,
            "gas": 100000,
            "returnValue": "0x",
            "structLogs": struct_logs,
        })
        .to_string()
    }

    /// SSTORE structlog. Geth stack is bottom-to-top: [value, slot].
    /// After reverse in compute_state_updates: stack[0]=slot, stack[1]=value.
    fn make_sstore_log(slot: &str, value: &str, gas: u64) -> serde_json::Value {
        serde_json::json!({
            "pc": 100,
            "op": "SSTORE",
            "gas": gas,
            "gasCost": 5000,
            "depth": 1,
            "stack": [
                format!("0x{:0>64}", value.trim_start_matches("0x")),
                format!("0x{:0>64}", slot.trim_start_matches("0x")),
            ],
            "memory": [],
        })
    }

    /// LOG1 structlog. Geth stack bottom-to-top: [topic1, length, offset].
    /// After reverse: stack[0]=offset, stack[1]=length, stack[2]=topic1.
    fn make_log1_log(data_hex: &str, topic1: &str, gas: u64) -> serde_json::Value {
        let data_bytes = hex::decode(data_hex.trim_start_matches("0x")).unwrap();
        let data_len = data_bytes.len();
        // Place data at memory offset 0
        let mem_word_count = data_len.div_ceil(32).max(1);
        let mut memory_hex = hex::encode(&data_bytes);
        // Pad to 32-byte words (Geth memory is returned as 32-byte hex chunks without 0x)
        let target_len = mem_word_count * 64;
        while memory_hex.len() < target_len {
            memory_hex.push('0');
        }
        let memory: Vec<String> = memory_hex
            .as_bytes()
            .chunks(64)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();

        serde_json::json!({
            "pc": 300,
            "op": "LOG1",
            "gas": gas,
            "gasCost": 375,
            "depth": 1,
            "stack": [
                format!("0x{:0>64}", topic1.trim_start_matches("0x")),
                format!("0x{:0>64}", format!("{:x}", data_len)),
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ],
            "memory": memory,
        })
    }

    /// CALL structlog at depth 1 with memory for args.
    /// Geth stack bottom-to-top: [retLen, retOff, argsLen, argsOff, value, addr, gas].
    /// After reverse: [gas, addr, value, argsOff, argsLen, retOff, retLen].
    fn make_call_log(
        target: &str,
        call_value: &str,
        args_hex: &str,
        gas: u64,
    ) -> serde_json::Value {
        let args_bytes = hex::decode(args_hex.trim_start_matches("0x")).unwrap_or_default();
        let args_len = args_bytes.len();
        let mem_word_count = args_len.div_ceil(32).max(1);
        let mut memory_hex = hex::encode(&args_bytes);
        let target_len = mem_word_count * 64;
        while memory_hex.len() < target_len {
            memory_hex.push('0');
        }
        let memory: Vec<String> = memory_hex
            .as_bytes()
            .chunks(64)
            .map(|c| String::from_utf8(c.to_vec()).unwrap())
            .collect();

        serde_json::json!({
            "pc": 200,
            "op": "CALL",
            "gas": gas,
            "gasCost": 100,
            "depth": 1,
            "stack": [
                "0x0000000000000000000000000000000000000000000000000000000000000020",
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                format!("0x{:0>64}", format!("{:x}", args_len)),
                "0x0000000000000000000000000000000000000000000000000000000000000000",
                format!("0x{:0>64}", call_value.trim_start_matches("0x")),
                format!("0x{:0>64}", target.trim_start_matches("0x")),
                format!("0x{:0>64}", format!("{:x}", gas)),
            ],
            "memory": memory,
        })
    }

    /// Build a CALL trace with the depth transitions needed for gas tracking:
    /// CALL at depth 1 → entry at depth 2 → return to depth 1.
    fn make_call_trace_with_depth(target: &str, args_hex: &str) -> String {
        let call = make_call_log(target, "0", args_hex, 50000);
        let subcall_entry = serde_json::json!({
            "pc": 0, "op": "STOP", "gas": 49000, "gasCost": 0, "depth": 2,
            "stack": [], "memory": [],
        });
        let return_to_depth1 = serde_json::json!({
            "pc": 201, "op": "POP", "gas": 48000, "gasCost": 2, "depth": 1,
            "stack": [
                "0x0000000000000000000000000000000000000000000000000000000000000001",
            ],
            "memory": [],
        });
        make_trace(vec![call, subcall_entry, return_to_depth1])
    }

    fn test_estimator_address() -> String {
        "0xd682Fe2ee8bdd59fdcCc5a4962FD98c20Ef47290".to_string()
    }

    fn test_caller_address() -> String {
        "0x0000000000000000000000000000000000000001".to_string()
    }

    // ===== Parsing & deserialization tests =====

    #[test]
    fn test_parse_valid_empty_trace() {
        let trace = make_trace(vec![]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 0);
    }

    #[test]
    fn test_parse_invalid_json() {
        let result = encode_trace_inner("not json at all");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to parse trace"));
    }

    #[test]
    fn test_parse_wrong_json_shape() {
        let result = encode_trace_inner(r#"{"foo": "bar"}"#);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to parse trace"));
    }

    #[test]
    fn test_parse_empty_string() {
        let result = encode_trace_inner("");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to parse trace"));
    }

    #[test]
    fn test_parse_valid_address() {
        let trace = make_trace(vec![]);
        let result = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_parse_invalid_address() {
        let trace = make_trace(vec![]);
        let result = analyze_trace_inner(
            &trace,
            "not-an-address",
            &test_caller_address(),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid estimator address"));
    }

    #[test]
    fn test_parse_address_too_short() {
        let trace = make_trace(vec![]);
        let result = analyze_trace_inner(
            &trace,
            "0x1234",
            &test_caller_address(),
            None,
            None,
            None,
            None,
            None,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid estimator address"));
    }

    // ===== State update extraction tests =====

    #[test]
    fn test_single_sstore() {
        let trace = make_trace(vec![make_sstore_log("1", "2a", 90000)]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 1);
        assert!(result.skipped_opcodes.is_empty());
    }

    #[test]
    fn test_single_log1() {
        let data_hex = "00000000000000000000000000000000000000000000000000000000000000ff";
        let topic = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        let trace = make_trace(vec![make_log1_log(data_hex, topic, 80000)]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 1);
    }

    #[test]
    fn test_multiple_mixed_ops() {
        let data_hex = "00000000000000000000000000000000000000000000000000000000000000ff";
        let topic = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        let trace = make_trace(vec![
            make_sstore_log("1", "2a", 90000),
            make_log1_log(data_hex, topic, 80000),
            make_sstore_log("2", "3b", 70000),
        ]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 3);
    }

    #[test]
    fn test_depth_gt_1_only_produces_no_updates() {
        let deep_sstore = serde_json::json!({
            "pc": 100, "op": "SSTORE", "gas": 90000, "gasCost": 5000, "depth": 2,
            "stack": [
                "0x0000000000000000000000000000000000000000000000000000000000000001",
                "0x0000000000000000000000000000000000000000000000000000000000000002",
            ],
            "memory": [],
        });
        let trace = make_trace(vec![deep_sstore]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 0);
    }

    // ===== Encoding tests =====

    #[test]
    fn test_encode_single_store_produces_hex() {
        let trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let result = encode_trace_inner(&trace).unwrap();
        assert!(result.encoded_updates.starts_with("0x"));
        assert!(result.encoded_updates.len() > 2);
    }

    #[test]
    fn test_encode_empty_trace_produces_valid_output() {
        let trace = make_trace(vec![]);
        let result = encode_trace_inner(&trace).unwrap();
        assert!(result.encoded_updates.starts_with("0x"));
    }

    // ===== revm/EmptyDB gas estimation tests =====

    #[test]
    fn test_revm_sstore_only_succeeds() {
        let trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let result = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!result.is_heuristic);
        assert!(result.gas_estimate > 0);
    }

    #[test]
    fn test_revm_multiple_sstores() {
        let trace = make_trace(vec![
            make_sstore_log("1", "aa", 90000),
            make_sstore_log("2", "bb", 85000),
            make_sstore_log("3", "cc", 80000),
        ]);
        let result = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!result.is_heuristic);
        // Gas for 3 stores should be meaningfully more than for 1
        let single_trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let single_result = analyze_trace_inner(
            &single_trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(result.gas_estimate > single_result.gas_estimate);
    }

    #[test]
    fn test_revm_call_to_empty_address_succeeds() {
        // In the EVM, a CALL to an address with no code succeeds (like sending to an EOA).
        // The StateChangeHandlerGasEstimator's CALL to an empty address returns success,
        // so revm doesn't revert and is_heuristic stays false.
        let trace =
            make_call_trace_with_depth("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "aabbccdd");
        let result = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert!(!result.is_heuristic);
        assert!(result.gas_estimate > 0);
    }

    #[test]
    fn test_revm_gas_estimate_is_reasonable() {
        let trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let result = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        // Should be between 21k (base) and 500k for a single SSTORE
        assert!(
            result.gas_estimate > 21_000 && result.gas_estimate < 500_000,
            "gas was {}",
            result.gas_estimate
        );
    }

    // ===== Heuristic gas estimation tests =====

    /// Calldata cost of the ABI-encoded state updates the replay would ship —
    /// part of every heuristic estimate since the calldata term landed.
    fn trace_calldata_gas(trace: &str) -> u64 {
        let encoded = encode_trace_inner(trace).unwrap().encoded_updates;
        let bytes = hex::decode(encoded.trim_start_matches("0x")).unwrap();
        gas_analyzer_core::calldata_gas(&bytes)
    }

    #[test]
    fn test_heuristic_empty_trace() {
        let trace = make_trace(vec![]);
        let result = estimate_gas_heuristic_inner(&trace, None).unwrap();
        // BASE_TX_COST + calldata framing of the empty update set
        assert_eq!(result.gas_estimate, 21_000 + trace_calldata_gas(&trace));
        assert!(result.is_heuristic);
    }

    #[test]
    fn test_heuristic_single_sstore() {
        let trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let result = estimate_gas_heuristic_inner(&trace, None).unwrap();
        // BASE_TX_COST(21000) + actual SSTORE gasCost from the struct log
        // (5000 in make_sstore_log) + calldata for the encoded update
        assert_eq!(
            result.gas_estimate,
            21_000 + 5_000 + trace_calldata_gas(&trace)
        );
    }

    #[test]
    fn test_heuristic_log1_with_32_bytes_data() {
        let data_hex = "00000000000000000000000000000000000000000000000000000000000000ff";
        let topic = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        let trace = make_trace(vec![make_log1_log(data_hex, topic, 80000)]);
        let result = estimate_gas_heuristic_inner(&trace, None).unwrap();
        // BASE_TX_COST(21000) + LOG_BASE_COST(375) + LOG_TOPIC_COST(375)
        // + 32*LOG_DATA_COST_PER_BYTE(8) + calldata for the encoded update
        assert_eq!(result.gas_estimate, 22_006 + trace_calldata_gas(&trace));
    }

    #[test]
    fn test_heuristic_always_is_heuristic() {
        let trace = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let result = estimate_gas_heuristic_inner(&trace, None).unwrap();
        assert!(result.is_heuristic);
    }

    #[test]
    fn test_heuristic_call_includes_gas() {
        let trace =
            make_call_trace_with_depth("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef", "aabbccdd");
        let result = estimate_gas_heuristic_inner(&trace, None).unwrap();
        assert!(
            result.gas_estimate > 21_000,
            "gas was {}",
            result.gas_estimate
        );
    }

    // Response shape is now guaranteed at compile time by the typed structs.
    // AnalyzeTraceResult has 5 fields, EncodeTraceResult has 3, EstimateGasResult has 3.

    // ===== Revm vs heuristic comparison test =====

    #[test]
    fn test_revm_and_heuristic_both_produce_positive_gas() {
        let trace_json = make_trace(vec![make_sstore_log("1", "ff", 90000)]);
        let analyze = analyze_trace_inner(
            &trace_json,
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        let heuristic = estimate_gas_heuristic_inner(&trace_json, None).unwrap();

        assert!(analyze.gas_estimate > 0);
        assert!(heuristic.gas_estimate > 0);
        assert!(!analyze.is_heuristic);
        assert!(heuristic.is_heuristic);
    }

    // ===== Roundtrip encoding test =====

    #[test]
    fn test_encode_decode_roundtrip() {
        // Encode a trace with 2 SSTOREs, then verify the ABI structure directly.
        let trace = make_trace(vec![
            make_sstore_log("1", "aa", 90000),
            make_sstore_log("2", "bb", 85000),
        ]);
        let result = encode_trace_inner(&trace).unwrap();
        let encoded_bytes = hex::decode(result.encoded_updates.trim_start_matches("0x")).unwrap();

        // Should be non-trivial size for 2 SSTOREs
        assert!(
            encoded_bytes.len() > 64,
            "encoding too short: {} bytes",
            encoded_bytes.len()
        );

        // First 32 bytes: offset to types array = 0x40 (64)
        let types_offset = u64::from_be_bytes(encoded_bytes[24..32].try_into().unwrap()) as usize;
        assert_eq!(types_offset, 64);

        // At types_offset: count = 2
        let types_count = u64::from_be_bytes(
            encoded_bytes[types_offset + 24..types_offset + 32]
                .try_into()
                .unwrap(),
        );
        assert_eq!(types_count, 2, "should have 2 type entries");

        // types[0] and types[1] should both be 0 (StateUpdateType::STORE)
        let type0 = encoded_bytes[types_offset + 32 + 31]; // last byte of first 32-byte word
        let type1 = encoded_bytes[types_offset + 64 + 31]; // last byte of second 32-byte word
        assert_eq!(type0, 0, "type[0] should be STORE(0)");
        assert_eq!(type1, 0, "type[1] should be STORE(0)");
    }

    // ===== Failed trace test =====

    #[test]
    fn test_failed_trace_still_extracts_updates() {
        // compute_state_updates processes structLogs regardless of the `failed` field.
        // Document this behavior: a reverted tx's trace still yields state updates
        // (the caller decides whether to use them).
        let trace = serde_json::json!({
            "failed": true,
            "gas": 100000,
            "returnValue": "0x",
            "structLogs": [
                {
                    "pc": 100, "op": "SSTORE", "gas": 90000, "gasCost": 5000, "depth": 1,
                    "stack": [
                        "0x00000000000000000000000000000000000000000000000000000000000000ff",
                        "0x0000000000000000000000000000000000000000000000000000000000000001",
                    ],
                    "memory": [],
                }
            ],
        })
        .to_string();
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 1);
    }

    // ===== LOG0 test =====

    #[test]
    fn test_single_log0() {
        // LOG0 has no topics. Stack (bottom-to-top): [length, offset].
        // After reverse: [offset, length].
        let data_hex = "deadbeef";
        let data_bytes = hex::decode(data_hex).unwrap();
        let data_len = data_bytes.len();
        let mut memory_hex = hex::encode(&data_bytes);
        while memory_hex.len() < 64 {
            memory_hex.push('0');
        }
        let log0 = serde_json::json!({
            "pc": 300, "op": "LOG0", "gas": 80000, "gasCost": 375, "depth": 1,
            "stack": [
                format!("0x{:0>64}", format!("{:x}", data_len)),
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ],
            "memory": [memory_hex],
        });
        let trace = make_trace(vec![log0]);
        let result = encode_trace_inner(&trace).unwrap();
        assert_eq!(result.state_update_count, 1);
    }

    #[test]
    fn test_heuristic_log0() {
        let data_hex = "deadbeef";
        let data_bytes = hex::decode(data_hex).unwrap();
        let data_len = data_bytes.len();
        let mut memory_hex = hex::encode(&data_bytes);
        while memory_hex.len() < 64 {
            memory_hex.push('0');
        }
        let log0 = serde_json::json!({
            "pc": 300, "op": "LOG0", "gas": 80000, "gasCost": 375, "depth": 1,
            "stack": [
                format!("0x{:0>64}", format!("{:x}", data_len)),
                "0x0000000000000000000000000000000000000000000000000000000000000000",
            ],
            "memory": [memory_hex],
        });
        let trace = make_trace(vec![log0]);
        let result = estimate_gas_heuristic_inner(&trace, None).unwrap();
        // BASE_TX_COST(21000) + LOG_BASE_COST(375) + 4*LOG_DATA_COST_PER_BYTE(8)
        // + calldata for the encoded update
        assert_eq!(result.gas_estimate, 21_407 + trace_calldata_gas(&trace));
    }

    // ===== Isolation test =====

    #[test]
    fn test_sequential_calls_dont_leak_state() {
        let trace1 = make_trace(vec![make_sstore_log("1", "aa", 90000)]);
        let trace2 = make_trace(vec![
            make_sstore_log("1", "bb", 90000),
            make_sstore_log("2", "cc", 85000),
        ]);
        let addr = test_estimator_address();
        let caller = test_caller_address();
        let result1 =
            analyze_trace_inner(&trace1, &addr, &caller, None, None, None, None, None).unwrap();
        let result2 =
            analyze_trace_inner(&trace2, &addr, &caller, None, None, None, None, None).unwrap();
        // Each call uses a fresh CacheDB, so results should differ
        assert_eq!(result1.state_update_count, 1);
        assert_eq!(result2.state_update_count, 2);
    }

    // ===== Prestate net form =====

    const CONSUMER: &str = "0x00000000000000000000000000000000000000c0";

    fn slot(n: u64) -> String {
        format!("0x{:064x}", n)
    }

    /// A `callTracer` root frame from the caller to the consumer, with `calls` and `logs` as given.
    fn make_frame(
        typ: &str,
        calls: Vec<serde_json::Value>,
        logs: Vec<serde_json::Value>,
    ) -> serde_json::Value {
        serde_json::json!({
            "type": typ,
            "from": test_caller_address(),
            "to": CONSUMER,
            "gas": "0x100000",
            "gasUsed": "0x5208",
            "input": "0x",
            "calls": calls,
            "logs": logs,
        })
    }

    /// A `prestateTracer` diff of the consumer's storage, as (slot, value) pairs before and after.
    fn make_diff(pre: &[(u64, u64)], post: &[(u64, u64)]) -> String {
        let storage = |pairs: &[(u64, u64)]| {
            pairs
                .iter()
                .map(|&(k, v)| (slot(k), serde_json::Value::String(slot(v))))
                .collect::<serde_json::Map<_, _>>()
        };
        serde_json::json!({
            "pre": { CONSUMER: { "storage": storage(pre) } },
            "post": { CONSUMER: { "storage": storage(post) } },
        })
        .to_string()
    }

    fn analyze_prestate_fixture(diff: &str, frame: &serde_json::Value) -> PrestateAnalysis {
        analyze_prestate_inner(
            diff,
            &frame.to_string(),
            CONSUMER,
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
        )
        .unwrap()
    }

    #[test]
    fn test_prestate_eligible_call_is_analyzed() {
        let log = serde_json::json!({
            "address": CONSUMER,
            "topics": [slot(0xaa)],
            "data": "0x01",
            "position": "0x0",
        });
        // Slot 1 changes, slot 2 is new, slot 3 is written back to its original value.
        let diff = make_diff(&[(1, 1), (3, 7)], &[(1, 2), (2, 3), (3, 7)]);
        let out = analyze_prestate_fixture(&diff, &make_frame("CALL", vec![], vec![log]));
        assert!(out.eligible);
        assert!(out.reason.is_none());
        let result = out.result.unwrap();
        // Two stores (slots 1 and 2) and one log; slot 3 is unchanged so it produces none.
        assert_eq!(result.state_update_count, 3);
        assert!(!result.is_heuristic);
        assert!(result.gas_estimate > 0);
        assert!(!result.reentered);
        assert!(result.skipped_opcodes.is_empty());
    }

    #[test]
    fn test_prestate_external_call_is_ineligible() {
        let call = serde_json::json!({
            "type": "CALL",
            "from": CONSUMER,
            "to": "0x00000000000000000000000000000000000000d0",
            "input": "0x",
        });
        let out = analyze_prestate_fixture(
            &make_diff(&[], &[(1, 1)]),
            &make_frame("CALL", vec![call], vec![]),
        );
        assert!(!out.eligible);
        assert!(out.result.is_none());
        assert!(out.reason.unwrap().contains("regular CALL"));
    }

    #[test]
    fn test_prestate_reverted_call_is_ineligible() {
        let mut frame = make_frame("CALL", vec![], vec![]);
        frame["error"] = "execution reverted".into();
        let out = analyze_prestate_fixture(&make_diff(&[], &[]), &frame);
        assert!(!out.eligible);
        assert!(out.reason.unwrap().contains("reverted"));
    }

    #[test]
    fn test_prestate_contract_creation_is_ineligible() {
        let out = analyze_prestate_fixture(
            &make_diff(&[], &[(1, 1)]),
            &make_frame("CREATE", vec![], vec![]),
        );
        assert!(!out.eligible);
        assert!(out.reason.unwrap().contains("CREATE"));
    }

    #[test]
    fn test_prestate_invalid_json_is_an_error() {
        let frame = make_frame("CALL", vec![], vec![]).to_string();
        let addr = test_estimator_address();
        let caller = test_caller_address();
        assert!(analyze_prestate_inner("{", &frame, CONSUMER, &addr, &caller, None, None).is_err());
        let diff = make_diff(&[], &[]);
        assert!(analyze_prestate_inner(&diff, "[]", CONSUMER, &addr, &caller, None, None).is_err());
        assert!(analyze_prestate_inner(&diff, &frame, "0x12", &addr, &caller, None, None).is_err());
    }

    /// Cold-slot SSTORE prices (EIP-2929/2200) the seeded original turns one write into another.
    const SET_FROM_ZERO: u64 = 22_100;
    const CHANGE_SET_SLOT: u64 = 5_000;
    const REWRITE_SAME_VALUE: u64 = 2_200;

    fn store(slot_n: u64, value: u64) -> StateUpdate {
        StateUpdate::Store(gas_analyzer_core::types::IStateUpdateTypes::Store {
            slot: B256::from(U256::from(slot_n)),
            value: B256::from(U256::from(value)),
        })
    }

    #[test]
    fn test_original_storage_reads_each_slot_from_the_diff_or_its_last_write() {
        // Slot 1 changes, 2 is set from zero, 4 is cleared, and 5 is in neither half: unchanged.
        let diff: DiffMode =
            serde_json::from_str(&make_diff(&[(1, 1), (4, 9)], &[(1, 2), (2, 3)])).unwrap();
        let updates = [
            store(1, 2),
            store(2, 3),
            store(5, 7),
            store(5, 8),
            store(4, 0),
        ];
        let consumer: Address = CONSUMER.parse().unwrap();
        let word = |n: u64| B256::from(U256::from(n));
        assert_eq!(
            original_storage(&diff, consumer, &updates),
            BTreeMap::from([
                (word(1), word(1)),
                (word(2), word(0)),
                (word(4), word(9)),
                (word(5), word(8)),
            ])
        );
        assert!(original_storage(&diff, Address::ZERO, &updates)[&word(1)] == word(2));
    }

    #[test]
    fn test_a_prestate_diff_prices_struct_log_writes_against_their_original_values() {
        let trace = make_trace(vec![
            make_sstore_log("1", "2", 90_000),
            make_sstore_log("5", "7", 80_000),
        ]);
        let analyze = |diff: Option<&str>| {
            let out = analyze_trace_inner(
                &trace,
                &test_estimator_address(),
                &test_caller_address(),
                None,
                Some(CONSUMER),
                None,
                diff,
                None,
            )
            .unwrap();
            assert!(!out.is_heuristic);
            out.gas_estimate
        };
        // Slot 1 goes from 1 to 2; slot 5 isn't in the diff, so it already held 7.
        let diff = make_diff(&[(1, 1)], &[(1, 2)]);
        assert_eq!(
            analyze(None) - analyze(Some(&diff)),
            (SET_FROM_ZERO - CHANGE_SET_SLOT) + (SET_FROM_ZERO - REWRITE_SAME_VALUE)
        );
    }

    #[test]
    fn test_a_prestate_diff_needs_the_origin_it_seeds() {
        let trace = make_trace(vec![make_sstore_log("1", "2", 90_000)]);
        let diff = make_diff(&[(1, 1)], &[(1, 2)]);
        let err = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
            None,
            Some(&diff),
            None,
        )
        .unwrap_err();
        assert!(err.contains("origin_address"), "{err}");
    }

    #[test]
    fn test_prestate_writes_are_priced_against_their_original_values() {
        let frame = make_frame("CALL", vec![], vec![]);
        let gas = |pre: &[(u64, u64)]| {
            analyze_prestate_fixture(&make_diff(pre, &[(1, 2)]), &frame)
                .result
                .unwrap()
                .gas_estimate
        };
        assert_eq!(gas(&[]) - gas(&[(1, 1)]), SET_FROM_ZERO - CHANGE_SET_SLOT);
    }

    #[test]
    fn test_net_sstore_costs_price_each_changed_slot_once() {
        // Set from zero, reset, cleared (refunded), and unchanged.
        let diff: DiffMode = serde_json::from_str(&make_diff(
            &[(2, 5), (3, 6), (4, 9)],
            &[(1, 1), (2, 8), (4, 9)],
        ))
        .unwrap();
        let (gas, refund) = net_sstore_costs(&diff, CONSUMER.parse().unwrap());
        assert_eq!(
            gas,
            SSTORE_COLD_SET_COST + SSTORE_COLD_RESET_COST + SSTORE_COLD_RESET_COST
        );
        assert_eq!(refund, SSTORE_CLEARS_REFUND);
    }

    // ===== analyze_trace_bytes =====

    fn analyze_bytes(response: &str) -> Result<AnalyzeTraceResult, AnalyzeTraceBytesError> {
        analyze_trace_bytes_inner(
            response.as_bytes(),
            &test_estimator_address(),
            &test_caller_address(),
            Some(1),
            None,
            None,
            None,
            None,
        )
    }

    fn mixed_trace() -> String {
        let data_hex = "00000000000000000000000000000000000000000000000000000000000000ff";
        let topic = "ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
        make_trace(vec![
            make_sstore_log("1", "2a", 90000),
            make_log1_log(data_hex, topic, 80000),
        ])
    }

    #[test]
    fn test_bytes_matches_analyze_trace_on_its_result() {
        let trace = mixed_trace();
        let expected = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            Some(1),
            None,
            None,
            None,
            None,
        )
        .unwrap();
        for response in [
            format!(r#"{{"jsonrpc":"2.0","id":1,"result":{trace}}}"#),
            format!(r#"{{"jsonrpc":"2.0","result":{trace},"id":1}}"#),
        ] {
            let got = analyze_bytes(&response).unwrap();
            assert_eq!(got.encoded_updates, expected.encoded_updates);
            assert_eq!(got.gas_estimate, expected.gas_estimate);
            assert_eq!(got.state_update_count, 2);
        }
    }

    #[test]
    fn test_bytes_rpc_error_is_typed() {
        let err = analyze_bytes(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"execution timeout"}}"#,
        )
        .unwrap_err();
        assert_eq!(
            err,
            AnalyzeTraceBytesError::Rpc {
                message: "execution timeout".to_string(),
                too_large: false
            }
        );
    }

    #[test]
    fn test_bytes_size_limit_error_is_too_large() {
        for message in [
            "response too large",
            "Response size exceeds the limit",
            "trace exceeds the 150MB response limit",
            "Response is larger than 10MB",
            "Response body too big",
        ] {
            let response =
                serde_json::json!({ "id": 1, "error": { "code": -32000, "message": message } });
            match analyze_bytes(&response.to_string()) {
                Err(AnalyzeTraceBytesError::Rpc { too_large, .. }) => {
                    assert!(too_large, "{message}")
                }
                other => panic!("{message}: {other:?}"),
            }
        }
    }

    #[test]
    fn test_bytes_malformed_response_is_an_analysis_error() {
        for response in [r#"{"jsonrpc":"2.0","id":1}"#, r#"{"result":"#, "not json"] {
            assert!(
                matches!(
                    analyze_bytes(response),
                    Err(AnalyzeTraceBytesError::Analysis(_))
                ),
                "{response}"
            );
        }
    }

    // ===== Native call gas =====

    /// A root whose callee spends `callee_gas`, with Geth's `gasCost` for the `CALL`: a cold
    /// account access plus the 400,000 gas forwarded.
    fn call_spending(callee_gas: u64) -> String {
        let (gas, forwarded, access) = (500_000, 400_000, 2_600);
        let mut call = make_call_log(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "0",
            "aabbccdd",
            gas,
        );
        call["gasCost"] = (access + forwarded).into();
        let entry = serde_json::json!({
            "pc": 0, "op": "JUMPDEST", "gas": forwarded, "gasCost": 1, "depth": 2,
            "stack": [], "memory": [],
        });
        let stop = serde_json::json!({
            "pc": 1, "op": "STOP", "gas": forwarded - callee_gas, "gasCost": 0, "depth": 2,
            "stack": [], "memory": [],
        });
        let resume = serde_json::json!({
            "pc": 201, "op": "POP", "gas": gas - access - callee_gas, "gasCost": 2, "depth": 1,
            "stack": ["0x0000000000000000000000000000000000000000000000000000000000000001"],
            "memory": [],
        });
        make_trace(vec![call, entry, stop, resume])
    }

    /// The callee has no code in `EmptyDB`, so what it spent comes from the trace; the simulation
    /// already charges the `CALL` opcode itself.
    #[test]
    fn test_measured_estimate_charges_the_callees_native_gas() {
        let analyze = |trace: &str| {
            analyze_trace_inner(
                trace,
                &test_estimator_address(),
                &test_caller_address(),
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        };
        let idle = analyze(&call_spending(0));
        let cheap = analyze(&call_spending(2_000));
        let costly = analyze(&call_spending(300_000));
        assert!(!idle.is_heuristic && !cheap.is_heuristic && !costly.is_heuristic);
        assert_eq!(
            idle.native_call_gas, 0,
            "the access cost is the simulation's"
        );
        assert_eq!(cheap.native_call_gas, 2_000);
        assert_eq!(
            cheap.signature_verification_gas, 50_000,
            "the Schnorr limit"
        );
        assert_eq!(costly.native_call_gas, 300_000);
        assert_eq!(cheap.gas_estimate - idle.gas_estimate, 2_000);
        assert_eq!(costly.gas_estimate - idle.gas_estimate, 300_000);
    }

    // ===== Transaction value =====

    /// A trace whose root forwards `call_value` wei to an empty address.
    fn value_call_trace(call_value: &str) -> String {
        let call = make_call_log(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            call_value,
            "",
            50000,
        );
        let entry = serde_json::json!({
            "pc": 0, "op": "STOP", "gas": 49000, "gasCost": 0, "depth": 2,
            "stack": [], "memory": [],
        });
        let resume = serde_json::json!({
            "pc": 201, "op": "POP", "gas": 40000, "gasCost": 2, "depth": 1,
            "stack": ["0x0000000000000000000000000000000000000000000000000000000000000001"],
            "memory": [],
        });
        make_trace(vec![call, entry, resume])
    }

    /// A trace whose root sends `call_value` wei to an account without code, which runs no frame.
    fn eoa_value_call_trace(call_value: &str) -> String {
        let call = make_call_log(
            "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            call_value,
            "",
            50000,
        );
        let resume = serde_json::json!({
            "pc": 201, "op": "POP", "gas": 40000, "gasCost": 2, "depth": 1,
            "stack": ["0x0000000000000000000000000000000000000000000000000000000000000001"],
            "memory": [],
        });
        make_trace(vec![call, resume])
    }

    #[test]
    fn test_forwarded_value_is_measured_only_when_the_transaction_brings_it() {
        let trace = value_call_trace("de0b6b3a7640000"); // 1 ETH
        let analyze = |value| {
            analyze_trace_inner(
                &trace,
                &test_estimator_address(),
                &test_caller_address(),
                None,
                None,
                value,
                None,
                None,
            )
            .unwrap()
        };
        assert!(
            analyze(None).is_heuristic,
            "the estimator has no ETH to forward"
        );
        assert!(!analyze(Some("1000000000000000000")).is_heuristic);
        assert!(!analyze(Some("0xde0b6b3a7640000")).is_heuristic);
    }

    /// The 25,000-gas charge for a `CALL` that sends ETH to an account that doesn't exist (EIP-161).
    const NEW_ACCOUNT: u64 = 25_000;

    #[test]
    fn test_a_prestate_diff_spares_an_existing_recipient_the_new_account_charge() {
        let trace = eoa_value_call_trace("de0b6b3a7640000"); // 1 ETH
        let recipient = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        let analyze = |diff: Option<&str>| {
            let out = analyze_trace_inner(
                &trace,
                &test_estimator_address(),
                &test_caller_address(),
                None,
                Some(CONSUMER),
                Some("1000000000000000000"),
                diff,
                None,
            )
            .unwrap();
            assert!(!out.is_heuristic);
            out.gas_estimate
        };
        let existing = serde_json::json!({
            "pre": { recipient: { "balance": "0x5" } },
            "post": { recipient: { "balance": "0xde0b6b3a7640005" } },
        })
        .to_string();
        // A recipient that was empty paid the charge on chain too, whether the diff omits it from
        // `pre` or lists it there with a zero balance.
        let created = |pre: serde_json::Value| {
            serde_json::json!({
                "pre": pre,
                "post": { recipient: { "balance": "0xde0b6b3a7640000" } },
            })
            .to_string()
        };
        assert_eq!(analyze(None) - analyze(Some(&existing)), NEW_ACCOUNT);
        assert_eq!(
            analyze(None),
            analyze(Some(&created(serde_json::json!({}))))
        );
        let zero_balance = serde_json::json!({ recipient: { "balance": "0x0" } });
        assert_eq!(analyze(None), analyze(Some(&created(zero_balance))));
    }

    #[test]
    fn test_a_recipient_that_runs_code_is_spared_the_new_account_charge_without_the_diff() {
        // It forwards what it gets, so its balance ends unchanged and the diff leaves it out.
        let trace = value_call_trace("de0b6b3a7640000"); // 1 ETH
        let analyze = |diff: serde_json::Value| {
            analyze_trace_inner(
                &trace,
                &test_estimator_address(),
                &test_caller_address(),
                None,
                Some(CONSUMER),
                Some("1000000000000000000"),
                Some(&diff.to_string()),
                None,
            )
            .unwrap()
            .gas_estimate
        };
        let listed = serde_json::json!({
            "pre": { "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef": { "nonce": 1 } },
            "post": {},
        });
        let left_out = serde_json::json!({ "pre": {}, "post": {} });
        assert_eq!(analyze(left_out), analyze(listed));
    }

    #[test]
    fn test_callees_with_code_are_those_whose_call_entered_a_frame() {
        let recipient: Address = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
            .parse()
            .unwrap();
        let callees = |trace: String| {
            let trace: LeanFrame = serde_json::from_str(&trace).unwrap();
            callees_with_code(&trace.into())
        };
        assert_eq!(callees(value_call_trace("1")), BTreeSet::from([recipient]));
        assert!(callees(eoa_value_call_trace("1")).is_empty());
    }

    #[test]
    fn test_called_accounts_are_only_those_that_were_not_empty() {
        let recipient: Address = "0xdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
            .parse()
            .unwrap();
        let call = StateUpdate::Call(gas_analyzer_core::types::IStateUpdateTypes::Call {
            target: recipient,
            value: U256::from(1),
            callargs: Default::default(),
        });
        let accounts = |pre: serde_json::Value| {
            let diff = serde_json::json!({ "pre": { recipient.to_string(): pre }, "post": {} });
            let diff: DiffMode = serde_json::from_value(diff).unwrap();
            called_accounts(Some(&diff), [&call], &BTreeSet::new())
        };
        assert!(accounts(serde_json::json!({})).is_empty());
        assert!(accounts(serde_json::json!({ "balance": "0x0" })).is_empty());
        assert!(!accounts(serde_json::json!({ "nonce": 3 }))[&recipient].is_empty());
        // A contract from before Spurious Dragon can have nonce 0; its code makes it exist.
        assert!(!accounts(serde_json::json!({ "code": "0x60006000" }))[&recipient].is_empty());
    }

    // ===== Replaying callees =====

    const CALLEE: &str = "0x00000000000000000000000000000000000000ce";

    /// The root calls `CALLEE`, which runs a frame of its own.
    fn callee_call_trace() -> String {
        let call = make_call_log(CALLEE, "0", "", 50000);
        let entry = serde_json::json!({
            "pc": 0, "op": "STOP", "gas": 47000, "gasCost": 0, "depth": 2,
            "stack": [], "memory": [],
        });
        let resume = serde_json::json!({
            "pc": 201, "op": "POP", "gas": 46000, "gasCost": 2, "depth": 1,
            "stack": ["0x0000000000000000000000000000000000000000000000000000000000000001"],
            "memory": [],
        });
        make_trace(vec![call, entry, resume])
    }

    fn replay_with_code(code: &str, gas_price: Option<&str>) -> Replay {
        let prestate = serde_json::json!({ CALLEE: { "code": code, "nonce": 1 } }).to_string();
        Replay::parse(&prestate, gas_price, Some(1_700_000_000)).unwrap()
    }

    fn analyze_with(replay: Option<&Replay>) -> AnalyzeTraceResult {
        analyze_trace_inner(
            &callee_call_trace(),
            &test_estimator_address(),
            &test_caller_address(),
            None,
            Some(CONSUMER),
            None,
            None,
            replay,
        )
        .unwrap()
    }

    #[test]
    fn test_a_prestate_runs_each_callees_own_code() {
        let idle = analyze_with(Some(&replay_with_code("0x00", None)));
        // PUSH1 1 PUSH1 0 SSTORE STOP: a cold write from zero, 22,100, after two pushes.
        let writes = analyze_with(Some(&replay_with_code("0x600160005500", None)));
        assert!(idle.callees_executed && writes.callees_executed);
        assert_eq!(writes.gas_estimate - idle.gas_estimate, 22_100 + 2 * 3);
    }

    #[test]
    fn test_a_callee_sees_the_consumers_own_code_hash() {
        // keccak256(0x00), the consumer's code below.
        let hash = "bc36789e7a1e281436464229828f817d6612f7b477d66591ff96a9e064bcc98a";
        // Reverts unless EXTCODEHASH(CALLER) is that hash, as a codehash allowlist would.
        let gate = format!("0x333f7f{hash}14602b57600080fd5b00");
        let prestate = serde_json::json!({
            CONSUMER: { "code": "0x00", "nonce": 1 },
            CALLEE: { "code": gate, "nonce": 1 },
        });
        let replay = Replay::parse(&prestate.to_string(), None, None).unwrap();
        let result = analyze_with(Some(&replay));
        assert!(result.callees_executed, "{:?}", result.replay_error);
    }

    #[test]
    fn test_a_callee_checking_the_gas_price_runs_at_the_transactions() {
        // Reverts unless GASPRICE is at least 1 gwei, as Railgun's wallet checks its relayer's.
        let gate = "0x633b9aca003a10600b57005b600080fd";
        let paid = analyze_with(Some(&replay_with_code(gate, Some("2000000000"))));
        assert!(paid.callees_executed);
        let free = analyze_with(Some(&replay_with_code(gate, None)));
        assert!(
            !free.callees_executed,
            "the callee reverts, so the replay falls back"
        );
        assert!(paid.replay_error.is_none() && free.replay_error.is_some());
        assert_eq!(free.gas_estimate, analyze_with(None).gas_estimate);
    }

    #[test]
    fn test_a_prestate_needs_the_origin_it_settles() {
        let replay = replay_with_code("0x00", None);
        let err = analyze_trace_inner(
            &callee_call_trace(),
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
            None,
            None,
            Some(&replay),
        )
        .unwrap_err();
        assert!(err.contains("origin_address"), "{err}");
    }

    #[test]
    fn test_invalid_value_is_an_error() {
        let trace = make_trace(vec![]);
        let result = analyze_trace_inner(
            &trace,
            &test_estimator_address(),
            &test_caller_address(),
            None,
            None,
            Some("one ether"),
            None,
            None,
        );
        assert!(result.unwrap_err().contains("Invalid value"));
    }

    // ===== Frame trees =====

    const ROOT: &str = "0x00000000000000000000000000000000000000aa";
    const VAULT: &str = "0x00000000000000000000000000000000000000bb";
    const UNREACHED: &str = "0x00000000000000000000000000000000000000cc";

    fn sstore_at(depth: u64, slot: &str, value: &str) -> serde_json::Value {
        let mut log = make_sstore_log(slot, value, 5_000_000);
        log["depth"] = depth.into();
        log
    }

    /// The root writes a slot and calls the vault, which writes its own; the call costs
    /// `call_gas - resume_gas` natively, of which the cold access to the vault is `VAULT_ACCESS`.
    fn root_calls_vault(call_gas: u64, resume_gas: u64) -> String {
        let forwarded = call_gas - VAULT_ACCESS;
        let mut call = make_call_log(VAULT, "0", "aabbccdd", call_gas);
        call["gasCost"] = (VAULT_ACCESS + forwarded).into();
        let mut entry = sstore_at(2, "5", "32");
        entry["gas"] = forwarded.into();
        let resume = serde_json::json!({
            "pc": 201, "op": "POP", "gas": resume_gas, "gasCost": 2, "depth": 1,
            "stack": ["0x0000000000000000000000000000000000000000000000000000000000000001"],
            "memory": [],
        });
        make_trace(vec![sstore_at(1, "1", "a"), call, entry, resume])
    }

    const VAULT_ACCESS: u64 = 2_600;

    fn frame_tree(trace: &str, owned: &[&str]) -> FrameTreeAnalysis {
        let owned: Vec<String> = owned.iter().map(|a| a.to_string()).collect();
        analyze_frame_tree_inner(
            trace,
            ROOT,
            &owned,
            &test_caller_address(),
            None,
            None,
            None,
            None,
        )
        .expect("frame tree")
    }

    #[test]
    fn test_a_prestate_diff_prices_each_frames_writes_against_their_original_values() {
        let trace = root_calls_vault(1_000_000, 100_000);
        let owned = vec![VAULT.to_string()];
        let word = |n: u64| format!("0x{n:064x}");
        // The root changes slot 1 from 9 to 0xa, the vault slot 5 from 0x31 to 0x32.
        let diff = serde_json::json!({
            "pre": {
                ROOT: { "storage": { word(1): word(9) } },
                VAULT: { "storage": { word(5): word(0x31) } },
            },
            "post": {
                ROOT: { "storage": { word(1): word(0xa) } },
                VAULT: { "storage": { word(5): word(0x32) } },
            },
        })
        .to_string();
        let analyze = |diff: Option<&str>| {
            analyze_frame_tree_inner(
                &trace,
                ROOT,
                &owned,
                &test_caller_address(),
                None,
                None,
                diff,
                None,
            )
            .unwrap()
        };
        let (empty, seeded) = (analyze(None), analyze(Some(&diff)));
        let saved = SET_FROM_ZERO - CHANGE_SET_SLOT;
        // Root-only, the vault's write runs natively inside its CALL, charged from the trace.
        assert_eq!(empty.root_only_gas - seeded.root_only_gas, saved);
        assert_eq!(empty.nested_gas - seeded.nested_gas, 2 * saved);
    }

    /// The root calls the vault twice: the first call sets its slot 5 from 3 to 7, the second back
    /// to 3, so the slot is in neither half of the diff and its original is the last value written.
    #[test]
    fn test_sibling_frames_seed_a_slot_from_the_frame_that_returned_last() {
        let call = |gas: u64| {
            let mut call = make_call_log(VAULT, "0", "aabbccdd", gas);
            call["gasCost"] = gas.into();
            call
        };
        let entry = |value: &str, gas: u64| {
            let mut entry = sstore_at(2, "5", value);
            entry["gas"] = (gas - VAULT_ACCESS).into();
            entry
        };
        let resume = |gas: u64| {
            serde_json::json!({
                "pc": 201, "op": "POP", "gas": gas, "gasCost": 2, "depth": 1,
                "stack": ["0x0000000000000000000000000000000000000000000000000000000000000001"],
                "memory": [],
            })
        };
        let trace: LeanFrame = serde_json::from_str(&make_trace(vec![
            call(1_000_000),
            entry("7", 1_000_000),
            resume(600_000),
            call(600_000),
            entry("3", 600_000),
            resume(100_000),
        ]))
        .unwrap();
        let vault: Address = VAULT.parse().unwrap();
        let tree = compute_frame_tree_hypothetical(
            trace.into(),
            ROOT.parse().unwrap(),
            &BTreeSet::from([vault]),
            &NESTING_COST_MODEL_V1,
        )
        .unwrap();
        assert_eq!(tree.frames.len(), 3, "one frame per call");

        let diff: DiffMode = serde_json::from_str(r#"{"pre":{},"post":{}}"#).unwrap();
        let seeds = frame_tree_seeds(Some(&diff), &tree.frames, &BTreeSet::new());
        let word = |n: u64| B256::from(U256::from(n));
        assert_eq!(seeds.storage[&vault][&word(5)], word(3));
    }

    #[test]
    fn test_a_prestate_replays_both_settlements_or_neither() {
        let trace = root_calls_vault(1_000_000, 100_000);
        let owned = vec![VAULT.to_string()];
        let analyze = |code: Option<&str>| {
            let replay = code.map(|code| {
                let prestate = serde_json::json!({ VAULT: { "code": code, "nonce": 1 } });
                Replay::parse(&prestate.to_string(), None, None).unwrap()
            });
            analyze_frame_tree_inner(
                &trace,
                ROOT,
                &owned,
                &test_caller_address(),
                None,
                None,
                None,
                replay.as_ref(),
            )
            .unwrap()
        };
        let empty = analyze(None);
        // Root-only, the vault's call costs what its code does, not the ~900k the trace charges.
        let replayed = analyze(Some("0x00"));
        assert!(replayed.callees_executed);
        assert!(empty.root_only_gas - replayed.root_only_gas > 800_000);

        // Root-only, the vault runs its code and reverts, so neither settlement is replayed.
        let reverting = analyze(Some("0xfe"));
        assert!(!reverting.callees_executed);
        assert!(replayed.replay_error.is_none());
        assert!(
            reverting
                .replay_error
                .as_deref()
                .is_some_and(|e| e.starts_with("root-only: ")),
            "{:?}",
            reverting.replay_error
        );
        assert_eq!(
            (reverting.root_only_gas, reverting.nested_gas),
            (empty.root_only_gas, empty.nested_gas)
        );
    }

    #[test]
    fn test_a_depth_zero_log_is_an_error_not_a_hang() {
        let trace = make_trace(vec![sstore_at(1, "1", "a"), sstore_at(0, "2", "b")]);
        let owned = vec![VAULT.to_string()];
        let err = analyze_frame_tree_inner(
            &trace,
            ROOT,
            &owned,
            &test_caller_address(),
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(err.contains("depth 0"), "{err}");
    }

    #[test]
    fn test_an_expensive_owned_callee_becomes_its_own_frame() {
        let out = frame_tree(&root_calls_vault(1_000_000, 100_000), &[VAULT, UNREACHED]);

        let targets: Vec<&str> = out.frames.iter().map(|f| f.target.as_str()).collect();
        assert_eq!(targets.len(), 2);
        assert!(targets[0].eq_ignore_ascii_case(ROOT));
        assert!(targets[1].eq_ignore_ascii_case(VAULT));
        assert!(out.frames[1].caller.eq_ignore_ascii_case(ROOT));
        let children: u64 = out.frames[1..].iter().map(|f| f.gas - 21_000).sum();
        assert_eq!(out.nested_native_call_gas, 0, "the vault's call is nested");
        assert_eq!(out.signature_verification_gas, 50_000);
        assert_eq!(
            out.root_only_native_call_gas,
            900_000 - VAULT_ACCESS,
            "the simulation charges the CALL itself"
        );
        assert_eq!(
            out.nested_gas,
            out.frames[0].gas + children + out.frame_overhead_gas,
            "one base cost for the tree, plus the overhead of each nested frame"
        );
        assert!(out.nested_gas < out.root_only_gas);
        assert_eq!(out.not_nested.len(), 1);
        assert!(out.not_nested[0].address.eq_ignore_ascii_case(UNREACHED));
        assert!(out.not_nested[0].reason.starts_with("not called"));
    }

    #[test]
    fn test_a_cheap_owned_callee_stays_a_call() {
        let out = frame_tree(&root_calls_vault(30_000, 25_000), &[VAULT]);

        assert_eq!(out.frames.len(), 1);
        assert_eq!(out.nested_gas, out.root_only_gas);
        assert_eq!(out.not_nested.len(), 1);
        assert!(out.not_nested[0].reason.starts_with("called directly"));
    }

    #[test]
    fn test_with_nothing_owned_the_tree_is_the_root_alone() {
        let out = frame_tree(&root_calls_vault(1_000_000, 100_000), &[]);
        assert_eq!(out.frames.len(), 1);
        assert_eq!(out.nested_gas, out.root_only_gas);
        assert!(out.not_nested.is_empty());
    }

    #[test]
    fn test_frame_tree_bytes_unwraps_the_response_and_reports_rpc_errors() {
        let trace = root_calls_vault(1_000_000, 100_000);
        let response = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{trace}}}"#);
        let owned = vec![VAULT.to_string()];
        let out = analyze_frame_tree_bytes_inner(
            response.as_bytes(),
            ROOT,
            &owned,
            &test_caller_address(),
            None,
            None,
            None,
            None,
        )
        .expect("frame tree");
        assert_eq!(out.frames.len(), 2);

        let error = br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"response size exceeds limit"}}"#;
        assert_eq!(
            analyze_frame_tree_bytes_inner(
                error,
                ROOT,
                &owned,
                &test_caller_address(),
                None,
                None,
                None,
                None
            )
            .unwrap_err(),
            AnalyzeTraceBytesError::Rpc {
                message: "response size exceeds limit".to_string(),
                too_large: true,
            }
        );
    }
}

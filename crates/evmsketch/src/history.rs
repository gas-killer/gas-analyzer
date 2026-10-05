//! Tracked-function execution with access to historical chain state.
//!
//! The default extraction path ([`crate::call_to_encoded_state_updates_with_evmsketch_profiled`])
//! asks an RPC node to simulate the tracked function with `debug_traceCall`. A node can only run
//! code against one block's state and cannot be taught new precompiles, so a tracked function that
//! needs state from earlier blocks cannot be extracted that way.
//!
//! This module runs the tracked function **locally**, in revm over RPC-backed state, with the
//! [`IGasKillerHistory`] precompile installed at [`HISTORY_PRECOMPILE_ADDRESS`]. The precompile
//! answers each query by reading the requested block's state from the same RPC (an archive node is
//! required for anything older than its pruning window) and records every answer in an ordered read
//! log.
//!
//! The local run produces the same geth-format artifacts a node would — a `callTracer` frame and a
//! `prestateTracer` diff, or a struct-log trace — via `revm-inspectors`, and feeds them to the
//! existing extractors unchanged. So for a call that never touches the history precompile, the signed
//! payload is byte-identical to the node path's; the only new behaviour is the precompile itself.
//!
//! Execution semantics mirror `debug_traceCall` at block `N`: the tracked function runs against the
//! state **after** block `N`, under block `N`'s header. History reads may target any block `<= N`.
//!
//! What this does not do yet: prove the reads. The read log and its commitment
//! ([`HistoricalEncodedStateUpdates::reads_commitment`]) are what a slashing guest needs to verify an
//! execution that used history — each entry is a claim checkable against that block's state root —
//! but wiring that verification into the guest is separate work. See `docs/HISTORICAL_STATE.md`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use alloy::primitives::{Address, B256, Bytes, TxKind, U256};
use alloy::rpc::types::eth::TransactionRequest;
use alloy::rpc::types::trace::geth::{
    CallConfig, CallFrame, DefaultFrame, DiffMode, GethDefaultTracingOptions, PreStateConfig,
    PreStateFrame,
};
use alloy_hardforks::EthereumChainHardforks;
use alloy_provider::network::AnyNetwork;
use alloy_provider::{Provider, RootProvider};
use alloy_sol_types::SolError;
use anyhow::{Result, anyhow};
use gas_analyzer_core::{
    HISTORY_CALL_INNER_GAS_LIMIT, HISTORY_PRECOMPILE_ADDRESS, HistoricalRead, HistoryAnswer,
    HistoryQuery, PrestateEligibility, SimProfile, StateUpdate, build_state_updates_from_prestate,
    classify_prestate_eligibility, compute_state_updates, compute_state_updates_canonical,
    encode_history_answer, history_read_record, history_reads_commitment,
};
use gas_analyzer_estimator::SimEnvOpts;
use revm::context::result::{ExecutionResult, ResultAndState};
use revm::context::{Cfg, ContextTr, Evm, LocalContextTr, TxEnv};
use revm::database::CacheDB;
use revm::handler::instructions::EthInstructions;
use revm::handler::{EthFrame, EthPrecompiles, PrecompileProvider};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::{CallInput, CallInputs, Gas, InstructionResult, InterpreterResult};
use revm::primitives::hardfork::SpecId;
use revm::state::AccountInfo;
use revm::{Context, Database, ExecuteEvm, InspectEvm, MainBuilder, MainContext};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};

use crate::simple_rpc_db::SimpleRpcDb;
use crate::{
    EncodedStateUpdates, EvmSketchExecutorCache, Extracted, Extraction, StateEncoding,
    StructLogEncoder, access_list_storage_hints, chain_id_to_genesis_and_spec,
    finish_encoded_state_updates,
};

/// The result of extracting a tracked function that may read historical state.
#[derive(Debug)]
pub struct HistoricalEncodedStateUpdates {
    /// The signed payload and its gas estimate, exactly as the default extraction path reports them.
    pub encoded: EncodedStateUpdates,
    /// Every history query the tracked function issued, in order, with the answer it received.
    /// Empty when the function never touched the history precompile.
    pub reads: Vec<HistoricalRead>,
    /// `keccak256(abi.encode(reads))` — see [`gas_analyzer_core::history_reads_commitment`].
    pub reads_commitment: B256,
}

/// Extract the state-update program for a tracked function that may read historical state.
///
/// Same contract as [`crate::call_to_encoded_state_updates_with_evmsketch_profiled`] — same
/// encodings, same profiles, same payload-budget check, same gas estimate — except that the call is
/// simulated locally with the history precompile available. `block_number` is the block the tracked
/// function executes at; history reads may target it or any earlier block.
///
/// The RPC must serve state for every block the function reads, so reads beyond a full node's
/// pruning window need an archive node. Fetch failures are errors: the function never sees a guessed
/// value.
#[tracing::instrument(name = "evmsketch.encode_history", skip_all, fields(block_number, encoding = ?encoding, profile = ?profile))]
pub async fn call_to_encoded_state_updates_with_history(
    cache: &EvmSketchExecutorCache,
    rpc_url: impl AsRef<str>,
    tx_request: TransactionRequest,
    block_number: u64,
    encoding: StateEncoding,
    profile: SimProfile,
) -> Result<HistoricalEncodedStateUpdates> {
    let rpc_url = rpc_url.as_ref();
    let consumer = tx_request
        .to
        .and_then(|t| match t {
            TxKind::Call(addr) => Some(addr),
            TxKind::Create => None,
        })
        .ok_or_else(|| anyhow!("Transaction must have a 'to' address"))?;
    let caller = tx_request.from.unwrap_or_default();
    let storage_hints = access_list_storage_hints(&tx_request);

    let (executor, executor_lookup) = cache.get_or_build_timed(rpc_url, block_number).await?;
    let (_, hardforks) = chain_id_to_genesis_and_spec(executor.chain_id())?;
    let run = LocalRun {
        provider: executor.sketch.provider.clone(),
        hardforks,
        chain_id: executor.chain_id(),
        env: executor.sim_env(),
        block_number,
    };

    let started = Instant::now();
    let (state_updates, skipped_opcodes, call_gas_total, extraction, reads, parse) =
        extract_locally(&run, &tx_request, consumer, encoding, profile)?;
    let trace_fetch = started.elapsed().saturating_sub(parse);

    let extracted = Extracted {
        state_updates,
        skipped_opcodes,
        call_gas_total,
        extraction,
        trace_fetch,
        parse,
    };
    let encoded = finish_encoded_state_updates(
        &executor,
        executor_lookup,
        consumer,
        caller,
        storage_hints,
        extracted,
        profile,
    )
    .await?;
    let reads_commitment = history_reads_commitment(&reads);
    Ok(HistoricalEncodedStateUpdates {
        encoded,
        reads,
        reads_commitment,
    })
}

type LocalExtract = (
    Vec<StateUpdate>,
    std::collections::HashSet<String>,
    u64,
    Extraction,
    Vec<HistoricalRead>,
    Duration,
);

/// Run the local simulation(s) the encoding needs and extract the program from them.
///
/// Mirrors `extract_state_updates_hybrid`: under [`StateEncoding::PrestateNet`] the cheap
/// call-frame + diff run is tried first and the struct-log run happens only for calls with no net
/// form. A fallback re-executes the call, which re-issues the same history queries against the same
/// blocks, so the read log reported is the one from the run that produced the payload.
fn extract_locally(
    run: &LocalRun,
    tx_request: &TransactionRequest,
    consumer: Address,
    encoding: StateEncoding,
    profile: SimProfile,
) -> Result<LocalExtract> {
    let mut extraction = Extraction::StructLog;
    if encoding.signs_prestate_net() {
        let cheap = run.execute(tx_request, profile, TraceDepth::CallsAndDiff)?;
        let Traces::CallsAndDiff(boxed) = cheap.traces else {
            unreachable!("requested a calls-and-diff run")
        };
        let (frame, diff) = *boxed;
        match classify_prestate_eligibility(&frame, &diff, consumer) {
            PrestateEligibility::Eligible => {
                let updates = build_state_updates_from_prestate(consumer, &diff, &frame);
                return Ok((
                    updates,
                    Default::default(),
                    0,
                    Extraction::PrestateNet,
                    cheap.reads,
                    Duration::ZERO,
                ));
            }
            PrestateEligibility::Fallback(reason) => {
                tracing::debug!(reason = %reason, "call has no net form; using struct-log encoder");
                extraction = Extraction::PrestateFallback;
            }
        }
    }

    let full = run.execute(tx_request, profile, TraceDepth::StructLogs)?;
    let Traces::StructLogs(trace) = full.traces else {
        unreachable!("requested a struct-log run")
    };
    let started = Instant::now();
    let (updates, skipped, call_gas_total) = match encoding.struct_log_encoder() {
        StructLogEncoder::Legacy => {
            let extract = compute_state_updates(trace, None)?;
            (
                extract.state_updates,
                extract.skipped_opcodes,
                extract.call_gas_total,
            )
        }
        StructLogEncoder::Canonical => compute_state_updates_canonical(trace, consumer)?,
    };
    reject_history_call_ops(&updates)?;
    Ok((
        updates,
        skipped,
        call_gas_total,
        extraction,
        full.reads,
        started.elapsed(),
    ))
}

/// A `Call` op targeting the history precompile would land on-chain as a call to an address with no
/// code. The precompile already refuses non-static calls, so this cannot arise from a working tracked
/// function; refusing the payload makes sure it never gets signed if it somehow does.
fn reject_history_call_ops(updates: &[StateUpdate]) -> Result<()> {
    for u in updates {
        if let StateUpdate::Call(c) = u
            && c.target == HISTORY_PRECOMPILE_ADDRESS
        {
            return Err(anyhow!(
                "payload contains a CALL to the history precompile; history reads must use STATICCALL"
            ));
        }
    }
    Ok(())
}

/// Everything needed to simulate the tracked function at one block.
struct LocalRun {
    provider: RootProvider<AnyNetwork>,
    hardforks: EthereumChainHardforks,
    chain_id: u64,
    /// Block `N`'s environment (header fields and spec).
    env: SimEnvOpts,
    block_number: u64,
}

#[derive(Clone, Copy)]
enum TraceDepth {
    /// `callTracer` (with logs) + `prestateTracer` diff — what the net form needs.
    CallsAndDiff,
    /// A full struct-log trace with memory — what the struct-log encoders need.
    StructLogs,
}

enum Traces {
    /// Boxed: the frame and diff together dwarf a struct-log frame's handle.
    CallsAndDiff(Box<(CallFrame, DiffMode)>),
    StructLogs(DefaultFrame),
}

struct LocalExecution {
    traces: Traces,
    reads: Vec<HistoricalRead>,
}

fn call_config() -> CallConfig {
    CallConfig {
        only_top_call: Some(false),
        with_log: Some(true),
    }
}

fn prestate_config() -> PreStateConfig {
    PreStateConfig {
        diff_mode: Some(true),
        ..Default::default()
    }
}

/// Matches the options `get_trace_from_call_with_profile` sends a node.
fn struct_log_options() -> GethDefaultTracingOptions {
    GethDefaultTracingOptions {
        enable_memory: Some(true),
        disable_storage: Some(true),
        ..Default::default()
    }
}

impl LocalRun {
    fn execute(
        &self,
        tx_request: &TransactionRequest,
        profile: SimProfile,
        depth: TraceDepth,
    ) -> Result<LocalExecution> {
        let consumer = match tx_request.to {
            Some(TxKind::Call(a)) => a,
            _ => return Err(anyhow!("Transaction must have a 'to' address")),
        };
        let caller = tx_request.from.unwrap_or_default();
        let value = tx_request.value.unwrap_or_default();
        let data = tx_request.input.input().cloned().unwrap_or_default();

        let mut db = CacheDB::new(SimpleRpcDb::new(self.provider.clone(), self.block_number));
        if !value.is_zero() {
            // Fee and balance checks are off, but the value transfer itself still debits the
            // caller; make sure it can cover it, as `debug_traceCall` against a funded account does.
            let info = db
                .basic(caller)
                .map_err(|e| anyhow!("failed to load caller {caller}: {e}"))?
                .unwrap_or_default();
            db.insert_account_info(
                caller,
                AccountInfo {
                    balance: info.balance.saturating_add(value),
                    ..info
                },
            );
        }

        let block_gas_limit = profile
            .block_gas_limit_override()
            .unwrap_or(self.env.gas_limit);
        let tx_gas_limit = profile
            .tx_gas_limit_override()
            .or(tx_request.gas)
            .unwrap_or(block_gas_limit);
        let unbounded = profile.tx_gas_limit_override().is_some();

        let inspector = TracingInspector::new(match depth {
            TraceDepth::CallsAndDiff => {
                TracingInspectorConfig::from_geth_call_config(&call_config())
            }
            TraceDepth::StructLogs => {
                TracingInspectorConfig::from_geth_config(&struct_log_options())
            }
        });
        let precompiles = HistoryPrecompiles::new(HistoryBackend::new(
            self.provider.clone(),
            self.hardforks.clone(),
            self.chain_id,
            self.block_number,
        ));

        let env = self.env.clone();
        let chain_id = self.chain_id;
        let ctx = Context::mainnet()
            .with_db(&mut db)
            .modify_cfg_chained(|cfg| {
                cfg.chain_id = chain_id;
                cfg.spec = env.spec;
                cfg.disable_nonce_check = true;
                cfg.disable_balance_check = true;
                cfg.disable_base_fee = true;
                cfg.disable_fee_charge = true;
                if unbounded {
                    // The unbounded profile deliberately lifts EIP-7825 for the simulation.
                    cfg.tx_gas_limit_cap = Some(u64::MAX);
                }
            })
            .modify_block_chained(|block| {
                block.number = U256::from(env.number);
                block.timestamp = U256::from(env.timestamp);
                block.gas_limit = block_gas_limit;
                block.beneficiary = env.coinbase;
                block.prevrandao = Some(env.prevrandao);
                block.basefee = env.basefee;
                block.difficulty = env.difficulty;
            });
        let mut evm: Evm<_, _, _, HistoryPrecompiles, EthFrame<EthInterpreter>> =
            Evm::new_with_inspector(ctx, inspector, EthInstructions::default(), precompiles);

        let tx = TxEnv::builder()
            .caller(caller)
            .kind(TxKind::Call(consumer))
            .data(data)
            .value(value)
            .gas_limit(tx_gas_limit)
            .gas_price(0)
            .chain_id(Some(chain_id))
            .build()
            .map_err(|e| anyhow!("failed to build tx env: {e:?}"))?;
        let res: ResultAndState = evm
            .inspect_tx(tx)
            .map_err(|e| anyhow!("local simulation of the tracked function failed: {e:?}"))?;

        let Evm {
            inspector,
            precompiles,
            ..
        } = evm;
        let reads = precompiles.backend.reads;
        let gas_used = res.result.gas_used();
        let builder = inspector.into_geth_builder();
        let traces = match depth {
            TraceDepth::CallsAndDiff => {
                let frame = builder.geth_call_traces(call_config(), gas_used);
                let diff = match builder
                    .geth_prestate_traces(&res, &prestate_config(), &db)
                    .map_err(|e| anyhow!("prestate diff failed: {e}"))?
                {
                    PreStateFrame::Diff(d) => d,
                    PreStateFrame::Default(_) => {
                        return Err(anyhow!(
                            "prestate tracer returned prestate, expected a diff"
                        ));
                    }
                };
                Traces::CallsAndDiff(Box::new((frame, diff)))
            }
            TraceDepth::StructLogs => {
                let output = match &res.result {
                    ExecutionResult::Success { output, .. } => output.data().clone(),
                    ExecutionResult::Revert { output, .. } => output.clone(),
                    ExecutionResult::Halt { .. } => Bytes::new(),
                };
                Traces::StructLogs(builder.geth_traces(gas_used, output, struct_log_options()))
            }
        };
        Ok(LocalExecution { traces, reads })
    }
}

// ============================================================================
// The history precompile
// ============================================================================

/// Header fields a history query or historical call needs.
#[derive(Clone)]
struct HistoricalHeader {
    number: u64,
    timestamp: u64,
    gas_limit: u64,
    beneficiary: Address,
    mix_hash: B256,
    base_fee: u64,
    difficulty: U256,
    hash: B256,
}

/// Fetches and caches historical state, and keeps the read log.
pub struct HistoryBackend {
    provider: RootProvider<AnyNetwork>,
    hardforks: EthereumChainHardforks,
    chain_id: u64,
    execution_block: u64,
    /// One cache per historical block, so repeated reads at a block cost one fetch.
    dbs: HashMap<u64, CacheDB<SimpleRpcDb>>,
    headers: HashMap<u64, HistoricalHeader>,
    /// Every answered query, in issue order.
    pub reads: Vec<HistoricalRead>,
}

impl HistoryBackend {
    /// A backend answering queries for a tracked function executing at `execution_block`.
    pub fn new(
        provider: RootProvider<AnyNetwork>,
        hardforks: EthereumChainHardforks,
        chain_id: u64,
        execution_block: u64,
    ) -> Self {
        Self {
            provider,
            hardforks,
            chain_id,
            execution_block,
            dbs: HashMap::new(),
            headers: HashMap::new(),
            reads: Vec::new(),
        }
    }

    fn db(&mut self, block: u64) -> &mut CacheDB<SimpleRpcDb> {
        let provider = &self.provider;
        self.dbs
            .entry(block)
            .or_insert_with(|| CacheDB::new(SimpleRpcDb::new(provider.clone(), block)))
    }

    fn header(&mut self, block: u64) -> Result<HistoricalHeader, String> {
        if let Some(h) = self.headers.get(&block) {
            return Ok(h.clone());
        }
        let handle =
            tokio::runtime::Handle::try_current().map_err(|e| format!("no tokio runtime: {e}"))?;
        let provider = self.provider.clone();
        let fetched = tokio::task::block_in_place(|| {
            handle.block_on(async move { provider.get_block_by_number(block.into()).await })
        })
        .map_err(|e| format!("history: failed to fetch block {block}: {e}"))?
        .ok_or_else(|| format!("history: block {block} not found"))?;
        let h = &fetched.header;
        let header = HistoricalHeader {
            number: h.number,
            timestamp: h.timestamp,
            gas_limit: h.gas_limit,
            beneficiary: h.beneficiary,
            mix_hash: h.mix_hash.unwrap_or_default(),
            base_fee: h.base_fee_per_gas.unwrap_or(0),
            difficulty: h.difficulty,
            hash: h.hash,
        };
        self.headers.insert(block, header.clone());
        Ok(header)
    }

    fn account(&mut self, account: Address, block: u64) -> Result<Option<AccountInfo>, String> {
        self.db(block)
            .basic(account)
            .map_err(|e| format!("history: failed to load {account} at block {block}: {e}"))
    }

    /// Answer one validated query. An `Err` is an I/O failure and aborts the simulation: a tracked
    /// function must never receive a value nobody fetched.
    fn answer(&mut self, query: &HistoryQuery, caller: Address) -> Result<HistoryAnswer, String> {
        Ok(match query {
            HistoryQuery::Storage {
                account,
                slot,
                block,
            } => {
                let v = self
                    .db(*block)
                    .storage(*account, U256::from_be_bytes(slot.0))
                    .map_err(|e| {
                        format!("history: failed to read {account}[{slot}] at block {block}: {e}")
                    })?;
                HistoryAnswer::Word(B256::from(v))
            }
            HistoryQuery::Balance { account, block } => {
                let balance = self
                    .account(*account, *block)?
                    .map(|a| a.balance)
                    .unwrap_or_default();
                HistoryAnswer::Word(B256::from(balance))
            }
            HistoryQuery::CodeHash { account, block } => {
                // EXTCODEHASH semantics: zero for an empty (EIP-161) account.
                let hash = match self.account(*account, *block)? {
                    Some(info) if !info.is_empty() => info.code_hash,
                    _ => B256::ZERO,
                };
                HistoryAnswer::Word(hash)
            }
            HistoryQuery::BlockHash { block } => HistoryAnswer::Word(self.header(*block)?.hash),
            HistoryQuery::BlockTimestamp { block } => {
                HistoryAnswer::Word(B256::from(U256::from(self.header(*block)?.timestamp)))
            }
            HistoryQuery::Call {
                target,
                data,
                block,
            } => self.historical_call(*block, caller, *target, data.clone())?,
        })
    }

    /// Run `data` against `target` on the state after `block`, under `block`'s environment.
    ///
    /// The call runs in its own EVM with the standard precompiles only, so a historical call cannot
    /// itself issue history queries. Its state changes are never committed.
    fn historical_call(
        &mut self,
        block: u64,
        caller: Address,
        target: Address,
        data: Bytes,
    ) -> Result<HistoryAnswer, String> {
        let header = self.header(block)?;
        let spec: SpecId = alloy_evm::spec_by_timestamp_and_block_number(
            &self.hardforks,
            header.timestamp,
            header.number,
        );
        let chain_id = self.chain_id;
        let db = self.db(block);
        let ctx = Context::mainnet()
            .with_db(db)
            .modify_cfg_chained(|cfg| {
                cfg.chain_id = chain_id;
                cfg.spec = spec;
                cfg.disable_nonce_check = true;
                cfg.disable_balance_check = true;
                cfg.disable_base_fee = true;
                cfg.disable_fee_charge = true;
                cfg.disable_block_gas_limit = true;
                // The caller is the tracked contract, which has code.
                cfg.disable_eip3607 = true;
                cfg.tx_gas_limit_cap = Some(u64::MAX);
            })
            .modify_block_chained(|b| {
                b.number = U256::from(header.number);
                b.timestamp = U256::from(header.timestamp);
                b.gas_limit = header.gas_limit;
                b.beneficiary = header.beneficiary;
                b.prevrandao = Some(header.mix_hash);
                b.basefee = header.base_fee;
                b.difficulty = header.difficulty;
            });
        let mut evm = ctx.build_mainnet();
        let tx = TxEnv::builder()
            .caller(caller)
            .kind(TxKind::Call(target))
            .data(data)
            .gas_limit(HISTORY_CALL_INNER_GAS_LIMIT)
            .gas_price(0)
            .chain_id(Some(chain_id))
            .build()
            .map_err(|e| format!("history: failed to build historical call: {e:?}"))?;
        let out = evm
            .transact(tx)
            .map_err(|e| format!("history: historical call at block {block} failed: {e:?}"))?;
        Ok(match out.result {
            ExecutionResult::Success { output, .. } => HistoryAnswer::Call {
                success: true,
                output: output.into_data(),
            },
            ExecutionResult::Revert { output, .. } => HistoryAnswer::Call {
                success: false,
                output,
            },
            ExecutionResult::Halt { .. } => HistoryAnswer::Call {
                success: false,
                output: Bytes::new(),
            },
        })
    }
}

/// The standard precompiles plus the history precompile.
pub struct HistoryPrecompiles {
    eth: EthPrecompiles,
    /// Exposed so the caller can collect the read log after execution.
    pub backend: HistoryBackend,
}

impl HistoryPrecompiles {
    pub fn new(backend: HistoryBackend) -> Self {
        Self {
            eth: EthPrecompiles::default(),
            backend,
        }
    }
}

impl<CTX: ContextTr> PrecompileProvider<CTX> for HistoryPrecompiles {
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        <EthPrecompiles as PrecompileProvider<CTX>>::set_spec(&mut self.eth, spec)
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        if inputs.bytecode_address != HISTORY_PRECOMPILE_ADDRESS {
            return <EthPrecompiles as PrecompileProvider<CTX>>::run(
                &mut self.eth,
                context,
                inputs,
            );
        }

        let input: Vec<u8> = match &inputs.input {
            CallInput::SharedBuffer(range) => context
                .local()
                .shared_memory_buffer_slice(range.clone())
                .map(|s| s.to_vec())
                .unwrap_or_default(),
            CallInput::Bytes(bytes) => bytes.to_vec(),
        };

        let mut result = InterpreterResult {
            result: InstructionResult::Return,
            gas: Gas::new(inputs.gas_limit),
            output: Bytes::new(),
        };
        match HistoryQuery::decode(
            &input,
            self.backend.execution_block,
            inputs.is_static,
            inputs.transfers_value(),
        ) {
            Err(refusal) => {
                result.result = InstructionResult::Revert;
                result.output = alloy_sol_types::Revert::from(refusal.revert_reason())
                    .abi_encode()
                    .into();
            }
            Ok(query) => {
                if !result.gas.record_cost(query.gas_cost()) {
                    result.result = InstructionResult::PrecompileOOG;
                    return Ok(Some(result));
                }
                // An I/O failure is fatal for the whole simulation, never a revert the tracked
                // function could catch and carry on from.
                let answer = self.backend.answer(&query, inputs.caller)?;
                result.output = encode_history_answer(&answer);
                self.backend
                    .reads
                    .push(history_read_record(&query, &answer));
            }
        }
        Ok(Some(result))
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        Box::new(
            self.eth
                .warm_addresses()
                .chain(std::iter::once(HISTORY_PRECOMPILE_ADDRESS)),
        )
    }

    fn contains(&self, address: &Address) -> bool {
        *address == HISTORY_PRECOMPILE_ADDRESS || self.eth.contains(address)
    }
}

#[cfg(test)]
mod anvil_integration {
    //! Anvil integration tests, `#[ignore]`d like the hybrid-extraction ones so a `cargo test`
    //! without foundry skips them; CI runs them with `anvil_integration -- --ignored`. They spawn a
    //! local `anvil`, present it as
    //! Sepolia so the executor can derive a spec, and use hand-assembled bytecode set with
    //! `anvil_setCode` — no Solidity build step, same approach as the hybrid-extraction tests.

    use super::*;
    use crate::{SEPOLIA_CHAIN_ID, call_to_encoded_state_updates_with_evmsketch_profiled};
    use alloy::primitives::address;
    use alloy::providers::Provider;
    use alloy_provider::network::Ethereum;
    use alloy_sol_types::SolCall;
    use gas_analyzer_core::{HistoryReadKind, IGasKillerHistory};
    use url::Url;

    struct LocalAnvil {
        child: std::process::Child,
        url: String,
    }

    impl LocalAnvil {
        async fn spawn() -> LocalAnvil {
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("bind")
                .local_addr()
                .expect("local_addr")
                .port();
            let child = std::process::Command::new("anvil")
                .args(["--port", &port.to_string(), "--silent"])
                .args(["--chain-id", &SEPOLIA_CHAIN_ID.to_string()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("these tests need foundry's `anvil` on PATH");
            let anvil = LocalAnvil {
                child,
                url: format!("http://127.0.0.1:{port}"),
            };
            let p = anvil.provider();
            for _ in 0..100 {
                if p.get_chain_id().await.is_ok() {
                    return anvil;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            panic!("anvil did not become ready");
        }

        fn provider(&self) -> RootProvider<Ethereum> {
            RootProvider::<Ethereum>::new_http(Url::parse(&self.url).unwrap())
        }
    }

    impl Drop for LocalAnvil {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    async fn set_code(p: &RootProvider<Ethereum>, a: Address, code: Bytes) {
        let _: serde_json::Value = p
            .raw_request("anvil_setCode".into(), (a, code))
            .await
            .unwrap();
    }

    async fn set_storage(p: &RootProvider<Ethereum>, a: Address, slot: u64, v: u64) {
        let _: serde_json::Value = p
            .raw_request(
                "anvil_setStorageAt".into(),
                (a, B256::from(U256::from(slot)), B256::from(U256::from(v))),
            )
            .await
            .unwrap();
    }

    async fn mine(p: &RootProvider<Ethereum>) -> u64 {
        let _: serde_json::Value = p.raw_request("evm_mine".into(), ()).await.unwrap();
        p.get_block_number().await.unwrap()
    }

    fn request(to: Address) -> TransactionRequest {
        TransactionRequest::default()
            .from(address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266"))
            .to(to)
            .gas_limit(3_000_000)
    }

    fn hex_code(spaced: &str) -> Bytes {
        Bytes::from(alloy::hex::decode(spaced.replace(' ', "")).unwrap())
    }

    fn addr_hex(a: Address) -> String {
        alloy::hex::encode(a.as_slice())
    }

    fn word(v: U256) -> String {
        alloy::hex::encode(v.to_be_bytes::<32>())
    }

    fn topic_hex(last: u8) -> String {
        format!("{}{last:02x}", "00".repeat(31))
    }

    /// Bytecode that builds `selector ‖ args` in memory, calls the history precompile with
    /// `opcode` (`fa` STATICCALL or `f1` CALL), and leaves the success flag on the stack. The return
    /// data lands at memory 0x100.
    fn history_query(selector: [u8; 4], args: &[U256], ret_len: u8, opcode: &str) -> String {
        let mut c = format!(
            "7f{}{} 6000 52 ",
            alloy::hex::encode(selector),
            "00".repeat(28)
        );
        for (i, a) in args.iter().enumerate() {
            c += &format!("7f{} 60{:02x} 52 ", word(*a), 4 + 32 * i);
        }
        let args_len = 4 + 32 * args.len();
        let target = addr_hex(HISTORY_PRECOMPILE_ADDRESS);
        if opcode == "f1" {
            c += &format!("60{ret_len:02x} 610100 60{args_len:02x} 6000 6000 73{target} 5a f1 ");
        } else {
            c += &format!("60{ret_len:02x} 610100 60{args_len:02x} 6000 73{target} 5a fa ");
        }
        c
    }

    fn storage_at(account: Address, slot: u64, block: u64) -> String {
        history_query(
            IGasKillerHistory::storageAtCall::SELECTOR,
            &[
                U256::from_be_slice(account.as_slice()),
                U256::from(slot),
                U256::from(block),
            ],
            0x20,
            "fa",
        )
    }

    /// With calldata: store its first word in slot 0. Without: return slot 0.
    fn getter_code() -> Bytes {
        hex_code("36 15 600c 57 600035 600055 00 5b 600054 600052 6020 6000 f3")
    }

    /// Set the source's slot 0 to `v` with a real transaction and return the block it landed in.
    /// (`anvil_setStorageAt` rewrites the current block's state in place, so it cannot give each
    /// value its own block.)
    async fn store_via_tx(p: &RootProvider<Ethereum>, source: Address, v: u64) -> u64 {
        let tx = serde_json::json!({
            "from": "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
            "to": source,
            "data": format!("0x{}", word(U256::from(v))),
            "gas": "0x30000",
        });
        let hash: B256 = p
            .raw_request("eth_sendTransaction".into(), (tx,))
            .await
            .unwrap();
        for _ in 0..100 {
            if let Some(r) = p.get_transaction_receipt(hash).await.unwrap() {
                return r.block_number.unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("transaction {hash} was not mined");
    }

    async fn encode_with_history(
        anvil: &LocalAnvil,
        to: Address,
        block: u64,
        encoding: StateEncoding,
    ) -> Result<HistoricalEncodedStateUpdates> {
        call_to_encoded_state_updates_with_history(
            &EvmSketchExecutorCache::new(4),
            &anvil.url,
            request(to),
            block,
            encoding,
            SimProfile::Chain,
        )
        .await
    }

    async fn encode_with_node(
        anvil: &LocalAnvil,
        to: Address,
        block: u64,
        encoding: StateEncoding,
    ) -> EncodedStateUpdates {
        call_to_encoded_state_updates_with_evmsketch_profiled(
            &EvmSketchExecutorCache::new(4),
            &anvil.url,
            request(to),
            block,
            encoding,
            SimProfile::Chain,
        )
        .await
        .expect("node-path encode failed")
    }

    const ALL_ENCODINGS: [StateEncoding; 3] = [
        StateEncoding::Legacy,
        StateEncoding::Canonical,
        StateEncoding::PrestateNet,
    ];

    /// The core guarantee: for a call that never touches the history precompile, local execution
    /// signs exactly the payload a node-traced extraction signs, under every encoding — including
    /// the cases each extractor is most sensitive to (delegatecall log order, a regular CALL that
    /// forces the struct-log path, and a swallowed revert).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "spawns a local anvil; requires foundry on PATH"]
    async fn matches_the_node_path_when_history_is_unused() {
        let anvil = LocalAnvil::spawn().await;
        let p = anvil.provider();

        let plain = address!("0x00000000000000000000000000000000000a0001");
        let root = address!("0x00000000000000000000000000000000000a0002");
        let lib = address!("0x00000000000000000000000000000000000a0003");
        let caller = address!("0x00000000000000000000000000000000000a0004");
        let callee = address!("0x00000000000000000000000000000000000a0005");
        let catcher = address!("0x00000000000000000000000000000000000a0006");
        let reverter = address!("0x00000000000000000000000000000000000a0007");

        set_code(
            &p,
            plain,
            hex_code(&format!(
                "6042600155 60ab600052 7f{}60206000a1 6005600255 6000600255 6000600555 00",
                topic_hex(0xaa)
            )),
        )
        .await;
        set_storage(&p, plain, 5, 0x77).await;
        set_code(
            &p,
            root,
            hex_code(&format!(
                "7f{}60006000a1 6000600060006000 73{} 5af450 7f{}60006000a1 00",
                topic_hex(0xaa),
                addr_hex(lib),
                topic_hex(0xbb)
            )),
        )
        .await;
        set_code(
            &p,
            lib,
            hex_code(&format!("7f{}60006000a1 6007600355 00", topic_hex(0xcc))),
        )
        .await;
        set_code(
            &p,
            caller,
            hex_code(&format!(
                "60006000600060006000 73{} 5af150 6001600455 00",
                addr_hex(callee)
            )),
        )
        .await;
        set_code(&p, callee, hex_code("6009600155 00")).await;
        set_code(
            &p,
            catcher,
            hex_code(&format!(
                "6000600060006000 73{} 5af450 6022600255 00",
                addr_hex(reverter)
            )),
        )
        .await;
        set_code(
            &p,
            reverter,
            hex_code(&format!(
                "6042600155 7f{}60006000a1 60006000fd",
                topic_hex(0xdd)
            )),
        )
        .await;
        let block = mine(&p).await;

        for to in [plain, root, caller, catcher] {
            for encoding in ALL_ENCODINGS {
                let node = encode_with_node(&anvil, to, block, encoding).await;
                let local = encode_with_history(&anvil, to, block, encoding)
                    .await
                    .expect("local encode failed");
                assert_eq!(
                    local.encoded.storage_updates, node.storage_updates,
                    "payload differs for {to} under {encoding:?}"
                );
                assert_eq!(local.encoded.update_count, node.update_count);
                assert_eq!(
                    local.encoded.extraction, node.extraction,
                    "{to} {encoding:?}"
                );
                assert_eq!(
                    local.encoded.gas_estimate, node.gas_estimate,
                    "{to} {encoding:?}"
                );
                assert!(local.reads.is_empty());
            }
        }
    }

    /// Sets slot 0 of a source contract to 10, 20 and 40 in three successive blocks, then runs a
    /// tracked function that reads slot 0 at each of those blocks and stores the sum.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "spawns a local anvil; requires foundry on PATH"]
    async fn reads_storage_at_earlier_blocks() {
        let anvil = LocalAnvil::spawn().await;
        let p = anvil.provider();
        let source = address!("0x00000000000000000000000000000000000b0001");
        let consumer = address!("0x00000000000000000000000000000000000b0002");
        set_code(&p, source, getter_code()).await;
        mine(&p).await;

        let mut blocks = Vec::new();
        for v in [10u64, 20, 40] {
            blocks.push(store_via_tx(&p, source, v).await);
        }
        // Sanity: the node itself reports those values at those blocks.
        for (b, v) in blocks.iter().zip([10u64, 20, 40]) {
            let got = p
                .get_storage_at(source, U256::ZERO)
                .number(*b)
                .await
                .unwrap();
            assert_eq!(got, U256::from(v), "anvil state at block {b}");
        }
        // Change it again after the last read block so a read of "latest" would be caught.
        store_via_tx(&p, source, 999).await;

        // read b0; record ok-flag in slot 0x10; read b1 (0x11); add; read b2 (0x12); add; SSTORE 0.
        let mut code = String::new();
        for (i, b) in blocks.iter().enumerate() {
            code += &storage_at(source, 0, *b);
            code += &format!("60{:02x} 55 610100 51 ", 0x10 + i);
            if i > 0 {
                code += "01 ";
            }
        }
        code += "6000 55 00";
        set_code(&p, consumer, hex_code(&code)).await;
        let exec_block = mine(&p).await;

        for encoding in ALL_ENCODINGS {
            let out = encode_with_history(&anvil, consumer, exec_block, encoding)
                .await
                .expect("encode failed");
            assert_eq!(out.reads.len(), 3, "{encoding:?}");
            for ((r, b), v) in out.reads.iter().zip(&blocks).zip([10u64, 20, 40]) {
                assert_eq!(r.kind, HistoryReadKind::Storage as u8);
                assert_eq!(r.blockNumber, *b);
                assert_eq!(r.account, source);
                assert_eq!(r.output.as_ref(), &B256::from(U256::from(v)).0[..]);
            }
            assert_eq!(out.reads_commitment, history_reads_commitment(&out.reads));
        }

        // Under the net form the payload is exactly: ok flags (1) in 0x10..0x12, and 70 in slot 0.
        let out = encode_with_history(&anvil, consumer, exec_block, StateEncoding::PrestateNet)
            .await
            .unwrap();
        assert_eq!(out.encoded.extraction, Extraction::PrestateNet);
        let stores: Vec<(B256, B256)> = net_stores(&out.encoded.storage_updates);
        assert_eq!(
            stores,
            vec![
                (B256::ZERO, B256::from(U256::from(70))),
                (B256::from(U256::from(0x10)), B256::from(U256::from(1))),
                (B256::from(U256::from(0x11)), B256::from(U256::from(1))),
                (B256::from(U256::from(0x12)), B256::from(U256::from(1))),
            ]
        );
    }

    /// `callAt` runs a getter against a past block; `blockTimestampAt` returns that block's
    /// timestamp.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "spawns a local anvil; requires foundry on PATH"]
    async fn calls_and_timestamps_at_earlier_blocks() {
        let anvil = LocalAnvil::spawn().await;
        let p = anvil.provider();
        let source = address!("0x00000000000000000000000000000000000c0001");
        let consumer = address!("0x00000000000000000000000000000000000c0002");
        set_code(&p, source, getter_code()).await;
        mine(&p).await;
        let past = store_via_tx(&p, source, 1234).await;
        store_via_tx(&p, source, 5).await;

        // callAt(source, "", past): returns (bool, bytes) = 4 words; value at 0x100 + 0x60.
        let mut code = history_query(
            IGasKillerHistory::callAtCall::SELECTOR,
            &[
                U256::from_be_slice(source.as_slice()),
                U256::from(0x60),
                U256::from(past),
                U256::ZERO,
            ],
            0x80,
            "fa",
        );
        code += "6010 55 610160 51 6001 55 "; // ok → 0x10, returned value → slot 1
        code += &history_query(
            IGasKillerHistory::blockTimestampAtCall::SELECTOR,
            &[U256::from(past)],
            0x20,
            "fa",
        );
        code += "6011 55 610100 51 6002 55 00"; // ok → 0x11, timestamp → slot 2
        set_code(&p, consumer, hex_code(&code)).await;
        let exec_block = mine(&p).await;

        let out = encode_with_history(&anvil, consumer, exec_block, StateEncoding::PrestateNet)
            .await
            .unwrap();
        let past_ts = p
            .get_block_by_number(past.into())
            .await
            .unwrap()
            .unwrap()
            .header
            .timestamp;
        let stores = net_stores(&out.encoded.storage_updates);
        assert!(stores.contains(&(B256::from(U256::from(1)), B256::from(U256::from(1234)))));
        assert!(stores.contains(&(B256::from(U256::from(2)), B256::from(U256::from(past_ts)))));
        assert_eq!(out.reads.len(), 2);
        assert_eq!(out.reads[0].kind, HistoryReadKind::Call as u8);
        assert!(out.reads[0].success);
        assert_eq!(out.reads[1].kind, HistoryReadKind::BlockTimestamp as u8);
    }

    /// A read of a block after the execution block reverts inside the tracked function (which can
    /// observe the failure) and is not logged.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "spawns a local anvil; requires foundry on PATH"]
    async fn refuses_future_blocks() {
        let anvil = LocalAnvil::spawn().await;
        let p = anvil.provider();
        let consumer = address!("0x00000000000000000000000000000000000d0001");
        let mut code = storage_at(consumer, 0, 1_000_000);
        code += "6001 01 6010 55 00"; // store success+1: 1 means the call failed
        set_code(&p, consumer, hex_code(&code)).await;
        let exec_block = mine(&p).await;

        let out = encode_with_history(&anvil, consumer, exec_block, StateEncoding::PrestateNet)
            .await
            .unwrap();
        assert!(out.reads.is_empty());
        assert_eq!(
            net_stores(&out.encoded.storage_updates),
            vec![(B256::from(U256::from(0x10)), B256::from(U256::from(1)))]
        );
    }

    /// A regular CALL to the history precompile is refused by the precompile, and because it
    /// would surface as a `Call` op in the struct-log encoders, the payload is rejected outright.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "spawns a local anvil; requires foundry on PATH"]
    async fn rejects_non_static_history_calls() {
        let anvil = LocalAnvil::spawn().await;
        let p = anvil.provider();
        let consumer = address!("0x00000000000000000000000000000000000e0001");
        let mut code = history_query(
            IGasKillerHistory::blockTimestampAtCall::SELECTOR,
            &[U256::from(1)],
            0x20,
            "f1",
        );
        code += "6010 55 00";
        set_code(&p, consumer, hex_code(&code)).await;
        let exec_block = mine(&p).await;

        for encoding in ALL_ENCODINGS {
            let err = encode_with_history(&anvil, consumer, exec_block, encoding)
                .await
                .expect_err("a CALL to the history precompile must not produce a payload");
            assert!(
                err.to_string().contains("STATICCALL"),
                "{encoding:?}: unexpected error {err}"
            );
        }
    }

    fn fixture(hex: &str) -> Bytes {
        Bytes::from(alloy::hex::decode(hex.trim()).expect("fixture is hex"))
    }

    /// End to end with real compiled Solidity: `VaultLossOracle` uses the `GasKillerHistory`
    /// library to read a vault's share price at two past blocks and stores the loss. Proves the
    /// library, the ABI and the precompile agree, and that a direct on-chain call fails loudly.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "spawns a local anvil; requires foundry on PATH"]
    async fn vault_loss_oracle_end_to_end() {
        use alloy_sol_types::{SolValue, sol};
        sol! {
            function assessLoss(address vault, uint256 fromBlock, uint256 toBlock) external returns (uint256 bps);
            function setPrice(uint256 newPrice) external;
            error HistoryUnavailable();
        }

        let anvil = LocalAnvil::spawn().await;
        let p = anvil.provider();
        let vault = address!("0x00000000000000000000000000000000000f0001");
        let oracle = address!("0x00000000000000000000000000000000000f0002");
        set_code(
            &p,
            vault,
            fixture(include_str!("fixtures/MockSharePriceVault.runtime.hex")),
        )
        .await;
        set_code(
            &p,
            oracle,
            fixture(include_str!("fixtures/VaultLossOracle.runtime.hex")),
        )
        .await;
        mine(&p).await;

        let send = |price: u64| {
            let p = p.clone();
            async move {
                let tx = serde_json::json!({
                    "from": "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266",
                    "to": vault,
                    "data": Bytes::from(setPriceCall { newPrice: U256::from(price) }.abi_encode()),
                    "gas": "0x30000",
                });
                let hash: B256 = p
                    .raw_request("eth_sendTransaction".into(), (tx,))
                    .await
                    .unwrap();
                for _ in 0..100 {
                    if let Some(r) = p.get_transaction_receipt(hash).await.unwrap() {
                        return r.block_number.unwrap();
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                panic!("setPrice not mined");
            }
        };
        let from_block = send(1_000_000_000_000_000_000).await; // 1.00
        let to_block = send(900_000_000_000_000_000).await; // 0.90 — a 10% loss
        send(2_000_000_000_000_000_000).await; // later recovery the assessment must not see
        let exec_block = mine(&p).await;

        let calldata = assessLossCall {
            vault,
            fromBlock: U256::from(from_block),
            toBlock: U256::from(to_block),
        }
        .abi_encode();
        let req = request(oracle).input(Bytes::from(calldata.clone()).into());

        // On the node there is no precompile: the library must refuse rather than read zeros.
        let direct = p
            .call(req.clone())
            .await
            .expect_err("a direct call must revert");
        let selector = alloy::hex::encode(HistoryUnavailable::SELECTOR);
        assert!(
            direct.to_string().contains(&selector),
            "expected HistoryUnavailable ({selector}), got {direct}"
        );

        let out = call_to_encoded_state_updates_with_history(
            &EvmSketchExecutorCache::new(4),
            &anvil.url,
            req,
            exec_block,
            StateEncoding::PrestateNet,
            SimProfile::Chain,
        )
        .await
        .expect("encode failed");

        assert_eq!(out.encoded.extraction, Extraction::PrestateNet);
        assert_eq!(out.reads.len(), 2);
        assert!(
            out.reads
                .iter()
                .all(|r| r.kind == HistoryReadKind::Call as u8 && r.success)
        );
        assert_eq!(out.reads[0].blockNumber, from_block);
        assert_eq!(out.reads[1].blockNumber, to_block);

        // lossBps[keccak256(abi.encode(vault, fromBlock, toBlock))] lives at
        // keccak256(abi.encode(key, 0)) — mapping at slot 0.
        let key = alloy::primitives::keccak256(
            (vault, U256::from(from_block), U256::from(to_block)).abi_encode(),
        );
        let slot = alloy::primitives::keccak256((key, U256::ZERO).abi_encode());
        assert_eq!(
            net_stores(&out.encoded.storage_updates),
            vec![(slot, B256::from(U256::from(1000)))],
            "expected a 1000 bps loss"
        );
        // One store plus the LossAssessed event.
        assert_eq!(out.encoded.update_count, 2);
    }

    /// Decode a net-form payload's stores as `(slot, value)` pairs.
    fn net_stores(payload: &Bytes) -> Vec<(B256, B256)> {
        use alloy_sol_types::{SolType, sol_data};
        type Payload = (
            sol_data::Array<sol_data::Uint<8>>,
            sol_data::Array<sol_data::Bytes>,
        );
        let (types, data) = Payload::abi_decode_params(payload).expect("payload decodes");
        types
            .iter()
            .zip(data)
            .filter(|(t, _)| **t == 0)
            .map(|(_, d)| {
                let s = <gas_analyzer_core::IStateUpdateTypes::Store as SolType>::abi_decode(&d)
                    .expect("store decodes");
                (s.slot, s.value)
            })
            .collect()
    }
}

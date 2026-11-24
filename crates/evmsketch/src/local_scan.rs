//! The prestate net form's observation scan, run in-process.
//!
//! The net form can't carry the check ops a program pins its reads with, so a call whose
//! target-depth context ran a `BALANCE`, `SELFBALANCE`, `EXTCODEHASH` or `EXTCODESIZE` has to go
//! to the struct-log encoder. The node can find those with a JavaScript tracer, but that tracer
//! runs on every step, and on a heavy call it takes longer than a signing round. Here the call is
//! replayed in revm instead, on the state the node's default-mode `prestateTracer` reports, under
//! an inspector that makes the same scan at native speed.
//!
//! The replay is trusted only when it ran the call the node ran: its outcome, output, calls (in
//! order, precompiles aside), logs and storage writes must all match the node's `callTracer`
//! frame and prestate diff. Otherwise the scan is [`LocalScan::Diverged`], and the caller asks the
//! node instead.

use std::collections::BTreeMap;

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::rpc::types::trace::geth::{CallFrame, DiffMode, PreStateMode};
use anyhow::{Result, anyhow};
use gas_analyzer_core::SimProfile;
use gas_analyzer_core::observe::is_precompile;
use revm::bytecode::opcode;
use revm::context::TxEnv;
use revm::context::result::ExecutionResult;
use revm::context_interface::ContextTr;
use revm::database::{CacheDB, EmptyDB};
use revm::interpreter::interpreter_types::Jumps;
use revm::interpreter::{
    CallInputs, CallOutcome, CallScheme, CreateInputs, CreateOutcome, CreateScheme, Interpreter,
};
use revm::primitives::TxKind;
use revm::primitives::hardfork::SpecId;
use revm::state::{AccountInfo, Bytecode};
use revm::{Context, InspectEvm, Inspector, MainBuilder, MainContext};

/// The block a call is replayed in: the header fields a call can read, and the fork it runs
/// under.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScanEnv {
    pub chain_id: u64,
    pub spec: SpecId,
    pub number: u64,
    pub timestamp: u64,
    pub gas_limit: u64,
    pub coinbase: Address,
    pub prevrandao: B256,
    pub basefee: u64,
    pub difficulty: U256,
}

/// What an in-process scan found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LocalScan {
    /// The replay matched the node's traces. Holds why the call needs the struct-log encoder, or
    /// `None` if its target-depth context read nothing a program would pin.
    Observed(Option<String>),
    /// The replay didn't reproduce the node's run, so its scan says nothing; why.
    Diverged(String),
}

/// Replays the call `tx` (already shaped by `profile`'s gas) on `pre`, the state the node's
/// default-mode `prestateTracer` reported for it, and scans its target-depth context. `frame` and
/// `diff` are the node's `callTracer` frame and prestate diff for the same call, which the replay
/// must reproduce to be trusted.
pub(crate) fn scan_observations(
    pre: &PreStateMode,
    frame: &CallFrame,
    diff: &DiffMode,
    tx: &TransactionRequest,
    env: &ScanEnv,
    profile: SimProfile,
) -> Result<LocalScan> {
    let mut db = CacheDB::new(EmptyDB::default());
    for (address, account) in &pre.0 {
        let code = account
            .code
            .as_ref()
            .filter(|code| !code.is_empty())
            .map(|code| Bytecode::new_raw(code.clone()));
        let info = AccountInfo {
            balance: account.balance.unwrap_or_default(),
            nonce: account.nonce.unwrap_or_default(),
            code_hash: code
                .as_ref()
                .map_or(revm::primitives::KECCAK_EMPTY, Bytecode::hash_slow),
            code,
        };
        db.insert_account_info(*address, info);
        for (slot, value) in &account.storage {
            db.insert_account_storage(*address, (*slot).into(), (*value).into())
                .map_err(|e| anyhow!("seeding storage: {e:?}"))?;
        }
    }

    let to = match tx.to {
        Some(TxKind::Call(to)) => to,
        _ => return Ok(LocalScan::Diverged("the call has no target".into())),
    };
    let gas_limit = profile
        .tx_gas_limit_override()
        .or(tx.gas)
        .unwrap_or(env.gas_limit);
    let tx_env = TxEnv::builder()
        .caller(tx.from.unwrap_or_default())
        .kind(TxKind::Call(to))
        .data(tx.input.input().cloned().unwrap_or_default())
        .value(tx.value.unwrap_or_default())
        .gas_limit(gas_limit)
        .gas_price(0)
        .chain_id(Some(env.chain_id))
        .build()
        .map_err(|e| anyhow!("building the replay transaction: {e:?}"))?;
    let block_gas_limit = profile
        .block_gas_limit_override()
        .unwrap_or(env.gas_limit)
        .max(gas_limit);

    let context = Context::mainnet()
        .with_db(&mut db)
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = env.chain_id;
            cfg.spec = env.spec;
            cfg.disable_nonce_check = true;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
            cfg.disable_fee_charge = true;
            cfg.disable_block_gas_limit = true;
            // A call simulated from a contract's address, as the analyzer's are.
            cfg.disable_eip3607 = true;
            cfg.tx_gas_limit_cap = Some(u64::MAX);
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
    let mut evm = context.build_mainnet_with_inspector(ObservationInspector::default());
    let outcome = evm
        .inspect_tx(tx_env)
        .map_err(|e| anyhow!("replaying the call: {e:?}"))?;
    let inspector = evm.inspector;

    if let Some(reason) = divergence(&outcome.result, &outcome.state, &inspector, frame, diff) {
        return Ok(LocalScan::Diverged(reason));
    }
    Ok(LocalScan::Observed(inspector.found.map(|op| {
        format!("{op} at target depth (needs a check op)")
    })))
}

/// Why the replay isn't the run the node traced, or `None` if it is.
fn divergence(
    result: &ExecutionResult,
    state: &revm::state::EvmState,
    inspector: &ObservationInspector,
    frame: &CallFrame,
    diff: &DiffMode,
) -> Option<String> {
    let ExecutionResult::Success { output, logs, .. } = result else {
        return Some(format!(
            "the replay failed where the node's run succeeded: {result:?}"
        ));
    };
    if frame.output.as_ref().unwrap_or(&Bytes::new()) != output.data() {
        return Some("the replay returned different output".into());
    }

    let mut node_calls = Vec::new();
    collect_calls(frame, &mut node_calls);
    if node_calls != inspector.calls {
        return Some(format!(
            "the replay made different calls ({} against the node's {})",
            inspector.calls.len(),
            node_calls.len()
        ));
    }

    let mut node_logs = Vec::new();
    collect_logs(frame, &mut node_logs);
    let mut replay_logs: Vec<_> = logs
        .iter()
        .map(|log| (log.address, log.topics().to_vec(), log.data.data.clone()))
        .collect();
    node_logs.sort();
    replay_logs.sort();
    if node_logs != replay_logs {
        return Some("the replay emitted different logs".into());
    }

    let replay_writes: BTreeMap<(Address, U256), U256> = state
        .iter()
        .flat_map(|(address, account)| {
            account
                .storage
                .iter()
                .filter(|(_, slot)| slot.is_changed())
                .map(move |(key, slot)| ((*address, *key), slot.present_value))
        })
        .collect();
    if storage_writes(diff) != replay_writes {
        return Some("the replay wrote different storage".into());
    }
    None
}

/// The storage the node's run changed, from its prestate diff: every slot whose value before
/// differs from its value after. A slot missing from `pre` was zero before, and one missing from
/// `post` is zero after.
fn storage_writes(diff: &DiffMode) -> BTreeMap<(Address, U256), U256> {
    let mut writes = BTreeMap::new();
    let accounts = diff.pre.keys().chain(diff.post.keys());
    for address in accounts {
        let before = diff.pre.get(address).map(|a| &a.storage);
        let after = diff.post.get(address).map(|a| &a.storage);
        let slots = before
            .into_iter()
            .flat_map(|s| s.keys())
            .chain(after.into_iter().flat_map(|s| s.keys()));
        for slot in slots {
            let was = before
                .and_then(|s| s.get(slot))
                .copied()
                .unwrap_or_default();
            let is = after.and_then(|s| s.get(slot)).copied().unwrap_or_default();
            if was != is {
                writes.insert((*address, U256::from_be_bytes(slot.0)), is.into());
            }
        }
    }
    writes
}

/// A call or create the replay or the node saw, as `callTracer` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SeenCall {
    kind: &'static str,
    /// The callee, or the code a `DELEGATECALL`/`CALLCODE` ran; `None` for a create.
    to: Option<Address>,
    input: Bytes,
}

/// The calls under `frame`, in the order they were made, precompiles left out (tracers differ on
/// whether they show them).
fn collect_calls(frame: &CallFrame, out: &mut Vec<SeenCall>) {
    for call in &frame.calls {
        let kind = match call.typ.as_str() {
            "CALL" => "CALL",
            "STATICCALL" => "STATICCALL",
            "DELEGATECALL" => "DELEGATECALL",
            "CALLCODE" => "CALLCODE",
            "CREATE" => "CREATE",
            "CREATE2" => "CREATE2",
            _ => "OTHER",
        };
        let creates = kind.starts_with("CREATE");
        let to = if creates { None } else { call.to };
        if !to.is_some_and(is_precompile) {
            out.push(SeenCall {
                kind,
                to,
                input: call.input.clone(),
            });
        }
        collect_calls(call, out);
    }
}

/// The logs the node's run kept: those of every frame that didn't fail, inside one that didn't.
fn collect_logs(frame: &CallFrame, out: &mut Vec<(Address, Vec<B256>, Bytes)>) {
    if frame.error.is_some() {
        return;
    }
    for log in &frame.logs {
        out.push((
            log.address.unwrap_or_default(),
            log.topics.clone().unwrap_or_default(),
            log.data.clone().unwrap_or_default(),
        ));
    }
    for call in &frame.calls {
        collect_logs(call, out);
    }
}

/// Scans the target-depth context the way `OBSERVATION_TRACER` does, and records every call for
/// [`divergence`].
///
/// The target-depth context is the top frame and the `DELEGATECALL`/`CALLCODE` frames it opens, at
/// any depth, but nothing a `CALL`, `STATICCALL` or create opens.
#[derive(Debug, Default)]
struct ObservationInspector {
    /// Whether each open frame is in the target-depth context, innermost last.
    emitting: Vec<bool>,
    /// The first observation opcode the context ran.
    found: Option<&'static str>,
    calls: Vec<SeenCall>,
}

impl<CTX: ContextTr> Inspector<CTX> for ObservationInspector {
    fn step(&mut self, interp: &mut Interpreter, _context: &mut CTX) {
        if self.found.is_some() || self.emitting.last() != Some(&true) {
            return;
        }
        self.found = match interp.bytecode.opcode() {
            opcode::BALANCE => Some("BALANCE"),
            opcode::SELFBALANCE => Some("SELFBALANCE"),
            opcode::EXTCODEHASH => Some("EXTCODEHASH"),
            opcode::EXTCODESIZE => Some("EXTCODESIZE"),
            _ => None,
        };
    }

    fn call(&mut self, context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        let Some(&parent) = self.emitting.last() else {
            // The transaction's own call.
            self.emitting.push(true);
            return None;
        };
        let (kind, delegated) = match inputs.scheme {
            CallScheme::Call => ("CALL", false),
            CallScheme::StaticCall => ("STATICCALL", false),
            CallScheme::DelegateCall => ("DELEGATECALL", true),
            CallScheme::CallCode => ("CALLCODE", true),
        };
        let to = if delegated {
            inputs.bytecode_address
        } else {
            inputs.target_address
        };
        if !is_precompile(to) {
            self.calls.push(SeenCall {
                kind,
                to: Some(to),
                input: inputs.input.bytes(context),
            });
        }
        self.emitting.push(parent && delegated);
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.emitting.pop();
    }

    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        let kind = match inputs.scheme {
            CreateScheme::Create2 { .. } => "CREATE2",
            _ => "CREATE",
        };
        self.calls.push(SeenCall {
            kind,
            to: None,
            input: inputs.init_code.clone(),
        });
        self.emitting.push(false);
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.emitting.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256};
    use alloy::rpc::types::trace::geth::AccountState;

    const CONSUMER: Address = address!("0x0000000000000000000000000000000000003001");
    const CALLER: Address = address!("0x0000000000000000000000000000000000003002");

    fn env() -> ScanEnv {
        ScanEnv {
            chain_id: 11_155_111,
            spec: SpecId::PRAGUE,
            number: 100,
            timestamp: 1_000_000,
            gas_limit: 30_000_000,
            coinbase: Address::ZERO,
            prevrandao: B256::ZERO,
            basefee: 0,
            difficulty: U256::ZERO,
        }
    }

    fn prestate(code: &str) -> PreStateMode {
        let code = Bytes::from(alloy::hex::decode(code.replace(' ', "")).unwrap());
        PreStateMode(BTreeMap::from([(
            CONSUMER,
            AccountState {
                code: Some(code),
                ..Default::default()
            },
        )]))
    }

    fn frame() -> CallFrame {
        CallFrame {
            typ: "CALL".into(),
            from: CALLER,
            to: Some(CONSUMER),
            ..Default::default()
        }
    }

    fn tx() -> TransactionRequest {
        TransactionRequest::default()
            .from(CALLER)
            .to(CONSUMER)
            .gas_limit(1_000_000)
    }

    fn scan(code: &str, diff: &DiffMode) -> LocalScan {
        scan_observations(
            &prestate(code),
            &frame(),
            diff,
            &tx(),
            &env(),
            SimProfile::Chain,
        )
        .expect("the replay runs")
    }

    #[test]
    fn a_read_at_target_depth_is_found() {
        // ADDRESS; BALANCE; POP; STOP
        let LocalScan::Observed(Some(reason)) = scan("30 31 50 00", &DiffMode::default()) else {
            panic!("expected an observation");
        };
        assert!(reason.starts_with("BALANCE"), "{reason}");
    }

    #[test]
    fn a_call_that_reads_nothing_is_clean() {
        // PUSH1 1; POP; STOP
        assert_eq!(
            scan("6001 50 00", &DiffMode::default()),
            LocalScan::Observed(None)
        );
    }

    #[test]
    fn storage_the_node_didnt_write_is_a_divergence() {
        // SSTORE(0, 7); STOP. The node's diff shows no write.
        let LocalScan::Diverged(reason) = scan("6007 6000 55 00", &DiffMode::default()) else {
            panic!("expected a divergence");
        };
        assert!(reason.contains("storage"), "{reason}");
    }

    #[test]
    fn storage_the_node_wrote_matches() {
        let slot = B256::ZERO;
        let seven = b256!("0x0000000000000000000000000000000000000000000000000000000000000007");
        let diff = DiffMode {
            pre: BTreeMap::from([(CONSUMER, AccountState::default())]),
            post: BTreeMap::from([(
                CONSUMER,
                AccountState {
                    storage: BTreeMap::from([(slot, seven)]),
                    ..Default::default()
                },
            )]),
        };
        assert_eq!(scan("6007 6000 55 00", &diff), LocalScan::Observed(None));
    }

    #[test]
    fn a_call_the_node_didnt_make_is_a_divergence() {
        // CALL(gas, 0x3003, 0, 0, 0, 0, 0); POP; STOP. The node's frame shows no calls.
        let LocalScan::Diverged(reason) = scan(
            "6000 6000 6000 6000 6000 73 0000000000000000000000000000000000003003 5a f1 50 00",
            &DiffMode::default(),
        ) else {
            panic!("expected a divergence");
        };
        assert!(reason.contains("calls"), "{reason}");
    }
}

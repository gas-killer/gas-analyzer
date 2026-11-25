//! The prestate net form's observation scan, run in-process.
//!
//! The net form can't carry the check ops a program pins its reads with, so a call whose
//! target-depth context ran a `BALANCE`, `SELFBALANCE`, `EXTCODEHASH` or `EXTCODESIZE` has to go
//! to the struct-log encoder. The node can find those with a JavaScript tracer, but that tracer
//! runs on every step, and on a heavy call it takes longer than a signing round. Here the call is
//! replayed in revm instead, on the state the node's default-mode `prestateTracer` reports, under
//! an inspector that makes the same scan at native speed.
//!
//! The replay runs in the environment the node gave the call (its gas, and the block values as the
//! node set them, read with a probe), and it is trusted only when it ran the call the node ran: its
//! outcome, gas, output, calls (in order and with their values, precompiles aside), logs, storage
//! writes, balances and nonces must all match the node's `callTracer` frame and prestate diff.
//! Otherwise the scan is [`LocalScan::Diverged`], and the caller asks the node instead.

use std::collections::BTreeMap;

use alloy::primitives::{Address, B256, Bytes, U256};
use alloy::rpc::types::TransactionRequest;
use alloy::rpc::types::trace::geth::{CallFrame, DiffMode, PreStateMode};
use anyhow::{Result, anyhow};
use gas_analyzer_core::observe::is_precompile;
use gas_analyzer_rpc::SimulatedEnv;
use revm::bytecode::opcode;
use revm::context::TxEnv;
use revm::context::result::ExecutionResult;
use revm::context_interface::ContextTr;
use revm::database::{CacheDB, EmptyDB};
use revm::interpreter::interpreter_types::Jumps;
use revm::interpreter::{
    CallInputs, CallOutcome, CallScheme, CallValue, CreateInputs, CreateOutcome, CreateScheme,
    Interpreter,
};
use revm::primitives::TxKind;
use revm::primitives::hardfork::SpecId;
use revm::state::{AccountInfo, Bytecode};
use revm::{Context, InspectEvm, Inspector, MainBuilder, MainContext};

/// The environment a call is replayed in: the values the node gave it, and the fork it runs under.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScanEnv {
    pub spec: SpecId,
    pub node: SimulatedEnv,
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

/// Replays the call `tx` on `pre`, the state the node's default-mode `prestateTracer` reported for
/// it, in `env`, and scans its target-depth context. `frame` and `diff` are the node's
/// `callTracer` frame and prestate diff for the same call, which the replay must reproduce to be
/// trusted.
///
/// The caller checks [`unreplayable_request`] before fetching anything for the replay, and
/// [`unreplayable_env`] once the node's environment is known.
///
/// The replay is cut off once it has run more steps than the node's run could have (see
/// [`step_limit`]), so a replay that leaves the node's path can't run on to the call's whole gas
/// limit, which under the unbounded profile is far more than a signing round allows.
pub(crate) fn scan_observations(
    pre: &PreStateMode,
    frame: &CallFrame,
    diff: &DiffMode,
    tx: &TransactionRequest,
    env: &ScanEnv,
) -> Result<LocalScan> {
    Ok(judge(
        &replay(pre, tx, env, step_limit(frame.gas_used))?,
        frame,
        diff,
    ))
}

/// The most steps a run that used `gas_used` gas can take, with room to spare. Every opcode costs
/// at least 1 gas except the ones that end a frame, which run once per frame, and every frame past
/// the first costs at least 100 gas to open. Refunds take back at most a fifth of the gas, so the
/// gas the run spent is at most 5/4 of `gas_used`. Twice `gas_used`, plus a margin, covers all of
/// that.
fn step_limit(gas_used: U256) -> u64 {
    u64::try_from(gas_used)
        .unwrap_or(u64::MAX)
        .saturating_mul(2)
        .saturating_add(1_024)
}

/// Why the replay can't run in the environment the node gave the call, or `None`. The replay
/// runs at a zero gas price: the revm here charges the caller and pays the coinbase whatever its
/// fee settings, so a nonzero price would move balances the node may not have. That price is the
/// node's choice for every call, so it says the endpoint can't be replayed, not just this call.
pub(crate) fn unreplayable_env(node: &SimulatedEnv) -> Option<String> {
    (node.gas_price != 0).then(|| format!("the node gives calls a gas price of {}", node.gas_price))
}

/// Why the replay can't reproduce `tx` as the node runs it, or `None`. It drops blobs, access
/// lists and EIP-7702 authorizations, each of which changes what the node runs, and runs at a
/// zero gas price, where a node charges the fee a request sets.
pub(crate) fn unreplayable_request(tx: &TransactionRequest) -> Option<String> {
    if !matches!(tx.to, Some(TxKind::Call(_))) {
        return Some("the call has no target".into());
    }
    if tx.gas_price.is_some()
        || tx.max_fee_per_gas.is_some()
        || tx.max_priority_fee_per_gas.is_some()
        || tx.max_fee_per_blob_gas.is_some()
    {
        return Some("the call sets a fee".into());
    }
    if tx
        .blob_versioned_hashes
        .as_ref()
        .is_some_and(|h| !h.is_empty())
    {
        return Some("the call carries blobs".into());
    }
    if tx.access_list.as_ref().is_some_and(|l| !l.0.is_empty()) {
        return Some("the call carries an access list".into());
    }
    if tx
        .authorization_list
        .as_ref()
        .is_some_and(|l| !l.is_empty())
    {
        return Some("the call carries EIP-7702 authorizations".into());
    }
    None
}

/// A replay of the call: how it ended, the state it left and what its inspector saw.
struct Replay {
    result: ExecutionResult,
    state: revm::state::EvmState,
    /// Each account's balance and nonce before the call, from the node's prestate.
    before: BTreeMap<Address, (U256, u64)>,
    inspector: ObservationInspector,
}

/// Whether `replay` ran the call the node traced and, if it did, what its target-depth context
/// read.
fn judge(replay: &Replay, frame: &CallFrame, diff: &DiffMode) -> LocalScan {
    if let Some(reason) = divergence(replay, frame, diff) {
        return LocalScan::Diverged(reason);
    }
    LocalScan::Observed(
        replay
            .inspector
            .found
            .map(|op| format!("{op} at target depth (needs a check op)")),
    )
}

fn replay(
    pre: &PreStateMode,
    tx: &TransactionRequest,
    env: &ScanEnv,
    step_limit: u64,
) -> Result<Replay> {
    let node = &env.node;
    let mut db = CacheDB::new(EmptyDB::default());
    let mut before = BTreeMap::new();
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
        before.insert(*address, (info.balance, info.nonce));
        db.insert_account_info(*address, info);
        for (slot, value) in &account.storage {
            db.insert_account_storage(*address, (*slot).into(), (*value).into())
                .map_err(|e| anyhow!("seeding storage: {e:?}"))?;
        }
    }

    let Some(TxKind::Call(to)) = tx.to else {
        return Err(anyhow!("the call has no target"));
    };
    let tx_env = TxEnv::builder()
        .caller(tx.from.unwrap_or_default())
        .kind(TxKind::Call(to))
        .data(tx.input.input().cloned().unwrap_or_default())
        .value(tx.value.unwrap_or_default())
        .gas_limit(node.tx_gas_limit)
        .gas_price(0)
        .chain_id(Some(node.chain_id))
        .build()
        .map_err(|e| anyhow!("building the replay transaction: {e:?}"))?;
    let context = Context::mainnet()
        .with_db(&mut db)
        .modify_cfg_chained(|cfg| {
            cfg.chain_id = node.chain_id;
            cfg.spec = env.spec;
            cfg.disable_nonce_check = true;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
            cfg.disable_fee_charge = true;
            // The node may run a call with more gas than its block allows.
            cfg.disable_block_gas_limit = true;
            // A call simulated from a contract's address, as the analyzer's are.
            cfg.disable_eip3607 = true;
            cfg.tx_gas_limit_cap = Some(u64::MAX);
        })
        .modify_block_chained(|block| {
            block.number = U256::from(node.number);
            block.timestamp = U256::from(node.timestamp);
            block.gas_limit = node.block_gas_limit;
            block.beneficiary = node.coinbase;
            // One opcode reads either, depending on the fork.
            block.prevrandao = Some(node.prevrandao);
            block.difficulty = node.prevrandao.into();
            block.basefee = node.basefee;
        });
    let mut evm = context.build_mainnet_with_inspector(ObservationInspector {
        step_limit,
        ..Default::default()
    });
    let outcome = evm.inspect_tx(tx_env);
    if evm.inspector.overran {
        return Err(anyhow!(
            "the replay ran past {step_limit} steps, more than the node's run could have taken"
        ));
    }
    let outcome = outcome.map_err(|e| anyhow!("replaying the call: {e:?}"))?;
    Ok(Replay {
        result: outcome.result,
        state: outcome.state,
        before,
        inspector: evm.inspector,
    })
}

/// Why the replay isn't the run the node traced, or `None` if it is.
///
/// Gas used is the broadest check: a different path almost always costs different gas, even one
/// that changes nothing else compared here, such as a branch that only reads a balance.
fn divergence(replay: &Replay, frame: &CallFrame, diff: &DiffMode) -> Option<String> {
    let Replay {
        result,
        state,
        before,
        inspector,
    } = replay;
    if let Some(op) = inspector.unreplayable {
        return Some(format!(
            "the call ran {op}, whose value the replay can't match to the node's"
        ));
    }
    if let Some(error) = &frame.error {
        return Some(format!("the node's run failed: {error}"));
    }
    let ExecutionResult::Success { output, logs, .. } = result else {
        return Some(format!(
            "the replay failed where the node's run succeeded: {result:?}"
        ));
    };
    if U256::from(result.gas_used()) != frame.gas_used {
        return Some(format!(
            "the replay used {} gas against the node's {}",
            result.gas_used(),
            frame.gas_used
        ));
    }
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

    let replay_accounts: BTreeMap<Address, (U256, u64)> = state
        .iter()
        .filter_map(|(address, account)| {
            let was = before.get(address).copied().unwrap_or_default();
            let is = if account.is_selfdestructed() {
                (U256::ZERO, 0)
            } else {
                (account.info.balance, account.info.nonce)
            };
            (is != was).then_some((*address, is))
        })
        .collect();
    if account_changes(diff) != replay_accounts {
        return Some("the replay left different balances or nonces".into());
    }
    None
}

/// The balances and nonces the node's run changed, from its prestate diff, as each account's
/// balance and nonce after the call. `post` holds only the fields that changed, and an account in
/// `pre` but missing from `post` was deleted.
fn account_changes(diff: &DiffMode) -> BTreeMap<Address, (U256, u64)> {
    let mut changes = BTreeMap::new();
    for address in diff.pre.keys().chain(diff.post.keys()) {
        let pre = diff.pre.get(address);
        let was = (
            pre.and_then(|a| a.balance).unwrap_or_default(),
            pre.and_then(|a| a.nonce).unwrap_or_default(),
        );
        let is = match diff.post.get(address) {
            Some(post) => (post.balance.unwrap_or(was.0), post.nonce.unwrap_or(was.1)),
            None => (U256::ZERO, 0),
        };
        if is != was {
            changes.insert(*address, is);
        }
    }
    changes
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
    /// The value a `CALL`, `CALLCODE` or create sends. `None` for a `DELEGATECALL` or
    /// `STATICCALL`, which send none, and whose value field tracers fill in differently.
    value: Option<U256>,
}

/// The calls under `frame`, in the order they were made, precompiles left out (tracers differ on
/// whether they show them).
fn collect_calls(frame: &CallFrame, out: &mut Vec<SeenCall>) {
    for call in &frame.calls {
        // A SELFDESTRUCT is a frame to `callTracer` but not a call to revm, and its effects are
        // in the storage, logs and gas compared anyway.
        if call.typ == "SELFDESTRUCT" {
            continue;
        }
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
        let sends = matches!(kind, "CALL" | "CALLCODE") || creates;
        if !to.is_some_and(is_precompile) {
            out.push(SeenCall {
                kind,
                to,
                input: call.input.clone(),
                value: sends.then(|| call.value.unwrap_or_default()),
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
    /// The first opcode, at any depth, whose value the replay can't match to the node's:
    /// `BLOCKHASH` (the replay has no block hashes) and the blob values, which the environment
    /// probe doesn't read (a chain before Cancun has no opcode for them).
    unreplayable: Option<&'static str>,
    calls: Vec<SeenCall>,
    /// The steps after which the replay is cut off, and how many it has run.
    step_limit: u64,
    steps: u64,
    /// Whether the replay was cut off.
    overran: bool,
}

impl<CTX: ContextTr> Inspector<CTX> for ObservationInspector {
    fn step(&mut self, interp: &mut Interpreter, _context: &mut CTX) {
        self.steps += 1;
        if self.steps > self.step_limit {
            // Out of gas, the halt revm unwinds normally: every frame still open halts the same
            // way on its next step, so the run ends at once.
            self.overran = true;
            interp.halt_oog();
            return;
        }
        let op = interp.bytecode.opcode();
        if self.unreplayable.is_none() {
            self.unreplayable = match op {
                opcode::BLOCKHASH => Some("BLOCKHASH"),
                opcode::BLOBHASH => Some("BLOBHASH"),
                opcode::BLOBBASEFEE => Some("BLOBBASEFEE"),
                _ => None,
            };
        }
        if self.found.is_some() || self.emitting.last() != Some(&true) {
            return;
        }
        self.found = match op {
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
        let value = match inputs.value {
            CallValue::Transfer(value) => Some(value),
            CallValue::Apparent(_) => None,
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
                // A STATICCALL carries a zero transfer.
                value: value.filter(|_| inputs.scheme != CallScheme::StaticCall),
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
            value: Some(inputs.value),
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
            spec: SpecId::PRAGUE,
            node: SimulatedEnv {
                tx_gas_limit: 1_000_000,
                timestamp: 1_000_000,
                number: 100,
                coinbase: Address::ZERO,
                prevrandao: B256::ZERO,
                block_gas_limit: 30_000_000,
                basefee: 7,
                chain_id: 11_155_111,
                gas_price: 0,
            },
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

    fn replayed(code: &str) -> Replay {
        replay(&prestate(code), &tx(), &env(), u64::MAX).expect("the replay runs")
    }

    /// The node's frame for a run that matches the replay's gas.
    fn frame_like(replay: &Replay) -> CallFrame {
        CallFrame {
            gas_used: U256::from(replay.result.gas_used()),
            ..frame()
        }
    }

    /// `diff` with the caller's nonce bumped, as every call bumps it.
    fn bumping_the_caller(diff: &DiffMode) -> DiffMode {
        let mut diff = diff.clone();
        diff.pre.entry(CALLER).or_default().nonce = Some(0);
        diff.post.entry(CALLER).or_default().nonce = Some(1);
        diff
    }

    /// Judges the replay of `code` against a node run with the same gas and `diff`, plus the
    /// caller's nonce bump.
    fn scan(code: &str, diff: &DiffMode) -> LocalScan {
        let replay = replayed(code);
        judge(&replay, &frame_like(&replay), &bumping_the_caller(diff))
    }

    fn diverges(scan: LocalScan, why: &str) {
        let LocalScan::Diverged(reason) = scan else {
            panic!("expected a divergence, got {scan:?}");
        };
        assert!(reason.contains(why), "{reason}");
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

    #[test]
    fn different_gas_is_a_divergence() {
        // ADDRESS; BALANCE; POP; STOP, against a node run that used a unit more gas: a branch
        // that changed nothing else compared.
        let replay = replayed("30 31 50 00");
        let node = CallFrame {
            gas_used: U256::from(replay.result.gas_used() + 1),
            ..frame()
        };
        diverges(judge(&replay, &node, &DiffMode::default()), "gas");
    }

    #[test]
    fn a_blockhash_read_is_a_divergence() {
        // BLOCKHASH(1); POP; STOP. The replay has no block hashes.
        diverges(scan("6001 40 50 00", &DiffMode::default()), "BLOCKHASH");
    }

    /// A diff in which the consumer's slot 0 went from zero to `value`.
    fn wrote_slot_zero(value: u64) -> DiffMode {
        DiffMode {
            pre: BTreeMap::from([(CONSUMER, AccountState::default())]),
            post: BTreeMap::from([(
                CONSUMER,
                AccountState {
                    storage: BTreeMap::from([(B256::ZERO, B256::from(U256::from(value)))]),
                    ..Default::default()
                },
            )]),
        }
    }

    #[test]
    fn the_replay_reads_the_values_the_node_gave_the_call() {
        // SSTORE(0, BASEFEE + GASLIMIT); STOP. The node's run stored its base fee (7) plus its
        // block gas limit, so a replay that used the header's values would write another value.
        let diff = wrote_slot_zero(7 + 30_000_000);
        assert_eq!(
            scan("48 45 01 6000 55 00", &diff),
            LocalScan::Observed(None)
        );
    }

    #[test]
    fn the_replay_runs_with_the_gas_the_node_gave_the_call() {
        // SSTORE(0, GAS); STOP. The 1M-gas call has 978,998 left when GAS runs (after the
        // intrinsic 21,000 and GAS's own 2), whatever gas the request named.
        let diff = wrote_slot_zero(1_000_000 - 21_002);
        let replay = replay(
            &prestate("5a 6000 55 00"),
            &TransactionRequest { gas: None, ..tx() },
            &env(),
            u64::MAX,
        )
        .expect("the replay runs");
        assert_eq!(
            judge(&replay, &frame_like(&replay), &bumping_the_caller(&diff)),
            LocalScan::Observed(None)
        );
    }

    #[test]
    fn a_call_sending_a_different_value_is_a_divergence() {
        // CALL(gas, 0x3003, 0, 0, 0, 0, 0); POP; STOP, against a node run whose call sent 1 wei.
        let replay = replayed(
            "6000 6000 6000 6000 6000 73 0000000000000000000000000000000000003003 5a f1 50 00",
        );
        let mut node = frame_like(&replay);
        node.calls.push(CallFrame {
            typ: "CALL".into(),
            from: CONSUMER,
            to: Some(address!("0x0000000000000000000000000000000000003003")),
            value: Some(U256::from(1)),
            ..Default::default()
        });
        diverges(judge(&replay, &node, &DiffMode::default()), "calls");
    }

    #[test]
    fn a_nonce_the_node_didnt_bump_is_a_divergence() {
        let replay = replayed("6001 50 00");
        diverges(
            judge(&replay, &frame_like(&replay), &DiffMode::default()),
            "nonces",
        );
    }

    #[test]
    fn a_balance_the_node_changed_differently_is_a_divergence() {
        // PUSH1 1; POP; STOP, against a node run that moved a wei out of the caller.
        let diff = DiffMode {
            pre: BTreeMap::from([(
                CALLER,
                AccountState {
                    balance: Some(U256::from(10)),
                    ..Default::default()
                },
            )]),
            post: BTreeMap::from([(
                CALLER,
                AccountState {
                    balance: Some(U256::from(9)),
                    ..Default::default()
                },
            )]),
        };
        diverges(scan("6001 50 00", &diff), "balances");
    }

    #[test]
    fn a_failed_node_run_is_a_divergence() {
        let replay = replayed("6001 50 00");
        let node = CallFrame {
            error: Some("execution reverted".into()),
            ..frame_like(&replay)
        };
        diverges(judge(&replay, &node, &DiffMode::default()), "failed");
    }

    #[test]
    fn a_call_with_blobs_an_access_list_authorizations_or_a_fee_is_not_replayed() {
        let requests = [
            (
                TransactionRequest {
                    blob_versioned_hashes: Some(vec![B256::repeat_byte(1)]),
                    ..tx()
                },
                "blobs",
            ),
            (
                TransactionRequest {
                    access_list: Some(alloy::eips::eip2930::AccessList(vec![
                        alloy::eips::eip2930::AccessListItem {
                            address: CONSUMER,
                            storage_keys: vec![],
                        },
                    ])),
                    ..tx()
                },
                "access list",
            ),
            (
                TransactionRequest {
                    authorization_list: Some(vec![
                        alloy::eips::eip7702::SignedAuthorization::new_unchecked(
                            alloy::eips::eip7702::Authorization {
                                chain_id: U256::from(1),
                                address: CONSUMER,
                                nonce: 0,
                            },
                            0,
                            U256::from(1),
                            U256::from(1),
                        ),
                    ]),
                    ..tx()
                },
                "authorizations",
            ),
            (
                TransactionRequest {
                    gas_price: Some(1),
                    ..tx()
                },
                "fee",
            ),
            (
                TransactionRequest {
                    max_fee_per_gas: Some(1),
                    ..tx()
                },
                "fee",
            ),
        ];
        for (request, why) in requests {
            let reason = unreplayable_request(&request).expect("the request isn't replayed");
            assert!(reason.contains(why), "{reason}");
        }
        assert_eq!(unreplayable_request(&tx()), None);
    }

    #[test]
    fn a_node_that_prices_calls_is_not_replayed() {
        let mut env = env();
        assert_eq!(unreplayable_env(&env.node), None);
        env.node.gas_price = 1;
        let reason = unreplayable_env(&env.node).expect("the price is refused");
        assert!(reason.contains("gas price"), "{reason}");
    }

    #[test]
    fn a_replay_that_runs_past_the_nodes_run_is_cut_off() {
        // JUMPDEST; PUSH1 0; JUMP: a loop that would run until the call's 1M gas ran out, against
        // a node run that used 1,000.
        let node = CallFrame {
            gas_used: U256::from(1_000),
            ..frame()
        };
        let error = scan_observations(
            &prestate("5b 6000 56"),
            &node,
            &DiffMode::default(),
            &tx(),
            &env(),
        )
        .expect_err("the replay is cut off");
        assert!(error.to_string().contains("3024 steps"), "{error:#}");
    }

    #[test]
    fn a_create_is_not_replayed() {
        let create = TransactionRequest {
            to: Some(TxKind::Create),
            ..tx()
        };
        let reason = unreplayable_request(&create).expect("a create isn't replayed");
        assert!(reason.contains("no target"), "{reason}");
    }

    #[test]
    fn a_selfdestruct_frame_isnt_a_call() {
        let mut node = frame();
        node.calls.push(CallFrame {
            typ: "SELFDESTRUCT".into(),
            to: Some(CALLER),
            ..Default::default()
        });
        let mut calls = Vec::new();
        collect_calls(&node, &mut calls);
        assert!(calls.is_empty(), "{calls:?}");
    }
}

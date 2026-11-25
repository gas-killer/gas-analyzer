//! What a solidity-sdk v0.2.0 program pins about the world outside its consumer.
//!
//! A program's writes were computed against external state as of the trace, but settlement runs
//! later. v0.2.0 lets a program pin what its consumer observed: a `CALL`'s result, a `CREATE`'s
//! address, and the reads that changed no state (`STATICCALL`, `BALANCE`, `SELFBALANCE`,
//! `EXTCODEHASH`, `EXTCODESIZE`). Settlement re-checks each pin where it sits in the program and
//! reverts with `ObservationMismatch` if the world moved.
//!
//! The struct-log walks ([`crate::compute_state_updates`] and
//! [`crate::nested::compute_frame_tree_canonical`]) record each observation an emitting frame
//! makes that the program's effects depend on (the `taint` module decides which), and drop
//! repeated reads; this module holds what they share.
//!
//! A `STATICCALL` whose callee ran stateless code (see the `stateless` module) can be pinned by
//! the code it ran instead: a `CODEHASH_CHECK` of each account whose code ran, so settlement
//! checks code hashes rather than running the read again. The walks emit such a read as its
//! `STATICCALL_CHECK` followed by markers for that code; a caller with the code as of the state
//! the trace ran on resolves the markers ([`resolve_code_sizes`]), and one without falls back to
//! the check ([`unpin_stateless_reads`]). A pricer running on state the pins may not
//! hold against leaves them out (see [`crate::heuristic::unpinned_for_pricing`]).

use std::collections::HashMap;

use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rpc_types::trace::geth::StructLog;
use anyhow::{Result, bail};

use crate::types::{IStateUpdateTypes, StateUpdate};

/// `keccak256(abi.encodePacked(success, returndata))`, as solidity-sdk v0.2.0 hashes a call's
/// result.
pub fn result_hash(success: bool, returndata: &[u8]) -> B256 {
    let mut packed = Vec::with_capacity(1 + returndata.len());
    packed.push(success as u8);
    packed.extend_from_slice(returndata);
    keccak256(packed)
}

/// Whether `address` is a precompile: 0x01–0x11 (through Prague's BLS12-381 set) or Osaka's
/// P-256 verifier at 0x100. A precompile's output is a pure function of its input, so a read of
/// one needs no pin, and a call to one opens no frame to read its output from.
pub fn is_precompile(address: Address) -> bool {
    let n = U256::from_be_slice(address.as_slice());
    (n >= U256::from(1) && n <= U256::from(0x11)) || n == U256::from(0x100)
}

/// Bytes `offset..offset + len` of a struct log's memory, decoding only the words they span.
/// Memory past the end reads as zero, as the EVM's does.
pub(crate) fn memory_slice(log: &StructLog, offset: usize, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let Some(words) = log.memory.as_ref() else {
        return out;
    };
    if len == 0 {
        return out;
    }
    let first = offset / 32;
    let last = (offset.saturating_add(len) - 1) / 32;
    for (w, word) in words.iter().enumerate().take(last + 1).skip(first) {
        let hex = word.strip_prefix("0x").unwrap_or(word);
        let mut bytes = [0u8; 32];
        if hex::decode_to_slice(hex, &mut bytes).is_err() {
            continue;
        }
        let word_start = w * 32;
        for (i, byte) in bytes.iter().enumerate() {
            let at = word_start + i;
            if at >= offset && at < offset + len {
                out[at - offset] = *byte;
            }
        }
    }
    out
}

/// The `i`-th item from the top of a struct log's stack.
pub(crate) fn stack_top(log: &StructLog, i: usize) -> Option<U256> {
    let stack = log.stack.as_ref()?;
    stack.len().checked_sub(i + 1).map(|at| stack[at])
}

fn usize_of(value: Option<U256>, what: &str) -> Result<usize> {
    match value.map(usize::try_from) {
        Some(Ok(n)) => Ok(n),
        _ => bail!("{what} is missing or out of range"),
    }
}

/// The data each open depth's last frame handed back to its caller, so a caller resuming at
/// depth `d` can read what its callee at `d + 1` returned or reverted with.
///
/// Feed it every struct log, in order, with [`ExitData::step`] before the walk looks at the log.
#[derive(Default)]
pub(crate) struct ExitData {
    by_depth: HashMap<u64, Bytes>,
    prev_depth: u64,
}

impl ExitData {
    pub(crate) fn step(&mut self, log: &StructLog) -> Result<()> {
        let depth = log.depth;
        // A frame just opened: whatever an earlier frame at this depth returned isn't its.
        if depth > self.prev_depth {
            self.by_depth.insert(depth, Bytes::new());
        }
        self.prev_depth = depth;
        if matches!(log.op.as_ref(), "RETURN" | "REVERT") && log.error.is_none() {
            let len = usize_of(stack_top(log, 1), "RETURN/REVERT size")?;
            // An empty return may name any offset, however large.
            let data = if len == 0 {
                Bytes::new()
            } else {
                let offset = usize_of(stack_top(log, 0), "RETURN/REVERT offset")?;
                memory_slice(log, offset, len).into()
            };
            self.by_depth.insert(depth, data);
        }
        Ok(())
    }

    /// What the frame that just closed at `depth` returned or reverted with: empty for a `STOP`
    /// or an exceptional halt.
    pub(crate) fn take(&mut self, depth: u64) -> Bytes {
        self.by_depth.remove(&depth).unwrap_or_default()
    }
}

/// An op whose pin is known only once the trace resumes after it.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Await {
    /// A `CALL` at this index of the output: pin its result if the program `used` it, or if
    /// the call failed, which settlement otherwise reverts on.
    Call {
        index: usize,
        value_sent: bool,
        used: bool,
    },
    /// A `CREATE` at this index: pin the address it deployed to.
    Create { index: usize },
    /// A `STATICCALL_CHECK` placeholder at this index: fill in its result hash. `stateless`
    /// when code markers follow it (see [`code_check`]), so it may yet be pinned by its code.
    Static { index: usize, stateless: bool },
}

impl Await {
    /// Complete the awaited op in `out` from the resume log: `success` (or, for a `CREATE`, the
    /// deployed address) is on top of its stack, and `returndata` is what the callee handed
    /// back, empty if it opened no frame.
    ///
    /// Fails for a read the program depends on that failed without data (out of gas, a halt,
    /// the call depth): settlement re-runs a check with all its gas and rejects such a failure,
    /// so no check can pin it. A stateless read's such failure is its code's own `REVERT` with
    /// no data (the `stateless` module rules out the EVM's), which its code pins: its check is
    /// marked [`UNCHECKABLE`] and only fails if it has to fall back to running the read.
    pub(crate) fn resolve(
        self,
        out: &mut [StateUpdate],
        resume_top: U256,
        returndata: &[u8],
    ) -> Result<()> {
        let success = !resume_top.is_zero();
        match self {
            Self::Call {
                index,
                value_sent,
                used,
            } => {
                if let Some(StateUpdate::Call(call)) = out.get_mut(index)
                    && (used || !success)
                {
                    call.resultHash = call_pin(success, value_sent, returndata);
                }
            }
            Self::Create { index } => {
                if let Some(StateUpdate::Create(create)) = out.get_mut(index) {
                    create.expected = Address::from_word(resume_top.into());
                }
            }
            Self::Static { index, stateless } => {
                if let Some(StateUpdate::StaticCallCheck(check)) = out.get_mut(index) {
                    if !success && returndata.is_empty() {
                        if stateless {
                            check.resultHash = UNCHECKABLE;
                            return Ok(());
                        }
                        bail!(
                            "the program depends on a STATICCALL to {} that failed without data, \
                             which settlement can't check",
                            check.target
                        );
                    }
                    check.resultHash = result_hash(success, returndata);
                }
            }
        }
        Ok(())
    }
}

/// The pin of a `CALL` the trace saw end with `success` and `returndata`.
///
/// solidity-sdk v0.2.0 lets a pinned failure stand only when the call sent no value and reverted
/// with data: an empty failure can also be the EVM's own (a value shortfall, the call depth),
/// which the trace didn't see. Any other failure stays unpinned, so settlement reverts on it.
pub(crate) fn call_pin(success: bool, value_sent: bool, returndata: &[u8]) -> B256 {
    if success || (!value_sent && !returndata.is_empty()) {
        result_hash(success, returndata)
    } else {
        B256::ZERO
    }
}

/// A `STATICCALL` an emitting frame made, as a check with its result still to fill in. `None`
/// for a read of a precompile, which needs no pin.
pub(crate) fn static_check(log: &StructLog) -> Result<Option<StateUpdate>> {
    // Stack, top first: gas, address, argsOffset, argsSize, retOffset, retSize.
    let target = Address::from_word(
        stack_top(log, 1)
            .ok_or_else(|| anyhow::anyhow!("STATICCALL log has no address"))?
            .into(),
    );
    if is_precompile(target) {
        return Ok(None);
    }
    let offset = usize_of(stack_top(log, 2), "STATICCALL args offset")?;
    let len = usize_of(stack_top(log, 3), "STATICCALL args size")?;
    Ok(Some(StateUpdate::StaticCallCheck(
        IStateUpdateTypes::StaticCallCheck {
            target,
            data: memory_slice(log, offset, len).into(),
            resultHash: B256::ZERO,
        },
    )))
}

/// The marker a `CODEHASH_CHECK` from `EXTCODESIZE` carries until [`resolve_code_sizes`] replaces
/// it: the trace shows the code's size, not its hash. The low 8 bytes hold the size.
const CODE_SIZE_MARKER: [u8; 24] = *b"gaskiller.extcodesize\0\0\0";

/// The marker a `CODEHASH_CHECK` pinning code a stateless `STATICCALL` ran carries until
/// [`resolve_code_sizes`] replaces it: the trace shows neither the code nor its size. The low 8
/// bytes of a read's first marker hold the gas the read cost in the trace (see
/// [`charge_stateless_read`]).
const CODE_MARKER: [u8; 24] = *b"gaskiller.statelesscode\0";

/// The result hash a stateless read's `STATICCALL_CHECK` carries when the read failed without
/// data: settlement rejects such a failure when it runs the read again, so only the read's code
/// can pin it. It's no hash of a real result.
pub(crate) const UNCHECKABLE: B256 = B256::new(*b"gaskiller.uncheckable\0\0\0\0\0\0\0\0\0\0\0");

fn code_size_marker(size: u64) -> B256 {
    let mut word = [0u8; 32];
    word[..24].copy_from_slice(&CODE_SIZE_MARKER);
    word[24..].copy_from_slice(&size.to_be_bytes());
    B256::from(word)
}

fn marked_size(codehash: B256) -> Option<u64> {
    (codehash[..24] == CODE_SIZE_MARKER)
        .then(|| u64::from_be_bytes(codehash[24..].try_into().expect("8 bytes")))
}

/// Whether `codehash` is still a marker waiting for the account's code.
fn is_marker(codehash: B256) -> bool {
    is_code_marker_hash(codehash) || marked_size(codehash).is_some()
}

/// A `CODEHASH_CHECK` marker for `account`'s code, which a stateless `STATICCALL` ran (see the
/// `stateless` module).
///
/// The walks emit a stateless read as its `STATICCALL_CHECK` followed directly by one marker per
/// account whose code ran. Before the program is used, a caller either pins the read by its
/// code ([`resolve_code_sizes`], with the code as of the state the trace ran on), which drops the
/// `STATICCALL_CHECK`, or falls back to running it ([`unpin_stateless_reads`]), which drops the
/// markers. Either way exactly one of the two pins the read.
pub(crate) fn code_check(account: Address) -> StateUpdate {
    StateUpdate::CodehashCheck(IStateUpdateTypes::CodehashCheck {
        account,
        codehash: code_marker(0),
    })
}

fn code_marker(gas: u64) -> B256 {
    let mut word = [0u8; 32];
    word[..24].copy_from_slice(&CODE_MARKER);
    word[24..].copy_from_slice(&gas.to_be_bytes());
    B256::from(word)
}

fn is_code_marker_hash(codehash: B256) -> bool {
    codehash[..24] == CODE_MARKER
}

/// Whether `update` is a stateless read's code marker, still waiting for its hash.
pub fn is_code_marker(update: &StateUpdate) -> bool {
    matches!(update, StateUpdate::CodehashCheck(check) if is_code_marker_hash(check.codehash))
}

/// Record `gas`, what the read whose `STATICCALL_CHECK` is at `check` cost in the trace, on its
/// first code marker, if it has one. The walks count that gas as settlement running the read
/// again; a caller that pins the read by its code instead gets it back from
/// [`resolve_code_sizes`], since settlement then never runs it.
pub(crate) fn charge_stateless_read(updates: &mut [StateUpdate], check: usize, gas: u64) {
    if !matches!(updates.get(check), Some(StateUpdate::StaticCallCheck(_))) {
        return;
    }
    if let Some(StateUpdate::CodehashCheck(marker)) = updates.get_mut(check + 1)
        && is_code_marker_hash(marker.codehash)
    {
        marker.codehash = code_marker(gas);
    }
}

fn read_gas(marker: &StateUpdate) -> u64 {
    match marker {
        StateUpdate::CodehashCheck(check) if is_code_marker_hash(check.codehash) => {
            u64::from_be_bytes(check.codehash[24..].try_into().expect("8 bytes"))
        }
        _ => 0,
    }
}

/// The stateless reads in `updates`: each `STATICCALL_CHECK`'s index, and the range of the code
/// markers right after it.
fn stateless_reads(updates: &[StateUpdate]) -> Vec<(usize, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < updates.len() {
        if matches!(updates[i], StateUpdate::StaticCallCheck(_)) {
            let start = i + 1;
            let mut end = start;
            while updates.get(end).is_some_and(is_code_marker) {
                end += 1;
            }
            if end > start {
                out.push((i, start..end));
            }
            i = end.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

/// Fall back to running every stateless read again at settlement: drop the code markers and
/// keep the `STATICCALL_CHECK`s. For a caller that can't fetch code as of the state the trace
/// ran on. Fails for a read that failed without data, which only its code could pin.
pub fn unpin_stateless_reads(updates: &mut Vec<StateUpdate>) -> Result<()> {
    unpin_reads_where(updates, |_| true)
}

/// Drop the code markers of each stateless read whose markers `fall_back` returns true for.
fn unpin_reads_where(
    updates: &mut Vec<StateUpdate>,
    mut fall_back: impl FnMut(&[StateUpdate]) -> bool,
) -> Result<()> {
    let mut drop = vec![false; updates.len()];
    for (check, markers) in stateless_reads(updates) {
        if !fall_back(&updates[markers.clone()]) {
            continue;
        }
        if let StateUpdate::StaticCallCheck(c) = &updates[check]
            && c.resultHash == UNCHECKABLE
        {
            bail!(
                "the program depends on a STATICCALL to {} that failed without data, which \
                 settlement can't check without pinning its code",
                c.target
            );
        }
        drop[markers].fill(true);
    }
    let mut at = 0;
    updates.retain(|_| {
        at += 1;
        !drop[at - 1]
    });
    Ok(())
}

/// A read with no frame of its own, whose result is on top of the next log's stack.
#[derive(Debug, Clone, Copy)]
pub(crate) enum PendingRead {
    Balance(Address),
    Codehash(Address),
    CodeSize(Address),
}

impl PendingRead {
    /// The read a `BALANCE`, `SELFBALANCE`, `EXTCODEHASH` or `EXTCODESIZE` log makes, if the op
    /// is one. `this` is the address the frame runs as, which `SELFBALANCE` reads; without it
    /// that read isn't pinned.
    pub(crate) fn of(log: &StructLog, this: Option<Address>) -> Option<Self> {
        let account = || stack_top(log, 0).map(|v| Address::from_word(v.into()));
        match log.op.as_ref() {
            "BALANCE" => account().map(Self::Balance),
            "SELFBALANCE" => this.map(Self::Balance),
            "EXTCODEHASH" => account().map(Self::Codehash),
            "EXTCODESIZE" => account().map(Self::CodeSize),
            _ => None,
        }
    }

    /// The check, from the result on top of the next log's stack. A precompile's code can't
    /// change, so reading it pins nothing.
    pub(crate) fn check(self, result: U256) -> Option<StateUpdate> {
        Some(match self {
            Self::Balance(account) => StateUpdate::BalanceCheck(IStateUpdateTypes::BalanceCheck {
                account,
                balance: result,
            }),
            Self::Codehash(account) if !is_precompile(account) => {
                StateUpdate::CodehashCheck(IStateUpdateTypes::CodehashCheck {
                    account,
                    codehash: result.into(),
                })
            }
            Self::CodeSize(account) if !is_precompile(account) => {
                // No code hashes to zero for the SDK, so a zero size needs no lookup.
                let codehash = if result.is_zero() {
                    B256::ZERO
                } else {
                    code_size_marker(u64::try_from(result).unwrap_or(u64::MAX))
                };
                StateUpdate::CodehashCheck(IStateUpdateTypes::CodehashCheck { account, codehash })
            }
            Self::Codehash(_) | Self::CodeSize(_) => return None,
        })
    }
}

/// Drop every check that repeats an earlier one with nothing between them that could change
/// what it reads. A `STATICCALL`'s callee can read anything, so any op that changes state ends
/// the run; a balance or code hash changes only through a call, a deployment or a nested frame.
///
/// A stateless read's code markers go with its `STATICCALL_CHECK`: dropped with a repeated one,
/// kept with any other, since they don't pin anything until resolved. Run again once they are
/// (as [`resolve_code_sizes`] does), a code hash pinned twice is checked once.
pub(crate) fn dedupe_checks(updates: &mut Vec<StateUpdate>) {
    // Whether each `STATICCALL_CHECK` is a stateless read's, which its code may pin instead. A
    // read repeats another only if both are, or neither: the same call can run pure code once
    // and read storage the next time (a callee branching on its gas), and the stateless one's
    // check may give way to its code, leaving the other's storage unchecked.
    let stateless: Vec<bool> = (0..updates.len())
        .map(|i| {
            matches!(updates[i], StateUpdate::StaticCallCheck(_))
                && updates.get(i + 1).is_some_and(is_code_marker)
        })
        .collect();
    let mut reads: Vec<(StateUpdate, bool)> = Vec::new();
    let mut accounts: Vec<StateUpdate> = Vec::new();
    // Whether the last `STATICCALL_CHECK` was dropped, which its markers follow.
    let mut read_dropped = false;
    let mut at = 0;
    updates.retain(|update| {
        let i = at;
        at += 1;
        if is_code_marker(update) {
            return !read_dropped;
        }
        match update {
            StateUpdate::StaticCallCheck(_) => {
                let repeated = reads
                    .iter()
                    .any(|(earlier, s)| *s == stateless[i] && same_check(earlier, update));
                read_dropped = repeated;
                if !repeated {
                    reads.push((update.clone(), stateless[i]));
                }
                !repeated
            }
            StateUpdate::BalanceCheck(_) | StateUpdate::CodehashCheck(_) => {
                let repeated = accounts.iter().any(|earlier| same_check(earlier, update));
                if !repeated {
                    accounts.push(update.clone());
                }
                !repeated
            }
            StateUpdate::Store(_)
            | StateUpdate::Log0(_)
            | StateUpdate::Log1(_)
            | StateUpdate::Log2(_)
            | StateUpdate::Log3(_)
            | StateUpdate::Log4(_) => {
                reads.clear();
                true
            }
            StateUpdate::Call(_)
            | StateUpdate::Create(_)
            | StateUpdate::Create2(_)
            | StateUpdate::Nested(_) => {
                reads.clear();
                accounts.clear();
                true
            }
        }
    });
}

fn same_check(a: &StateUpdate, b: &StateUpdate) -> bool {
    match (a, b) {
        (StateUpdate::StaticCallCheck(a), StateUpdate::StaticCallCheck(b)) => {
            a.target == b.target && a.data == b.data && a.resultHash == b.resultHash
        }
        (StateUpdate::BalanceCheck(a), StateUpdate::BalanceCheck(b)) => {
            a.account == b.account && a.balance == b.balance
        }
        (StateUpdate::CodehashCheck(a), StateUpdate::CodehashCheck(b)) => {
            a.account == b.account && a.codehash == b.codehash
        }
        _ => false,
    }
}

/// `updates` for a caller that can't fetch code: every stateless read falls back to its
/// `STATICCALL_CHECK` (see [`unpin_stateless_reads`]), and the `EXTCODESIZE` checks still waiting
/// for a code hash are dropped, since a program carrying one could never settle. Returns the
/// accounts whose code size is left unpinned.
pub fn drop_unresolved_code_sizes(updates: &mut Vec<StateUpdate>) -> Result<Vec<Address>> {
    unpin_stateless_reads(updates)?;
    let unresolved = unresolved_code_sizes(updates)
        .into_iter()
        .map(|(account, _)| account)
        .collect();
    updates.retain(
        |update| !matches!(update, StateUpdate::CodehashCheck(check) if is_marker(check.codehash)),
    );
    Ok(unresolved)
}

/// The accounts whose code hash `updates` still need, each with the code size the trace saw:
/// the `EXTCODESIZE` reads of nonzero size. See [`unresolved_code_accounts`] for every account
/// whose code must be fetched.
pub fn unresolved_code_sizes(updates: &[StateUpdate]) -> Vec<(Address, u64)> {
    let mut out: Vec<(Address, u64)> = updates
        .iter()
        .filter_map(|update| match update {
            StateUpdate::CodehashCheck(check) => {
                marked_size(check.codehash).map(|size| (check.account, size))
            }
            _ => None,
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Every account whose code [`resolve_code_sizes`] can use, sorted: those read by `EXTCODESIZE`
/// with a nonzero size, which it needs, and those whose code a stateless `STATICCALL` ran, which
/// pin the read if all of its accounts' code is there.
pub fn unresolved_code_accounts(updates: &[StateUpdate]) -> Vec<Address> {
    let mut out: Vec<Address> = updates
        .iter()
        .filter_map(|update| match update {
            StateUpdate::CodehashCheck(check) if is_marker(check.codehash) => Some(check.account),
            _ => None,
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Replace every code marker in `updates` with the account's code hash, from `code`: each
/// account's code as of the state the trace ran on (the caller vouches for that), for the
/// accounts [`unresolved_code_accounts`] names. Then drop the checks that repeat.
///
/// A stateless read whose accounts all have code in `code` is pinned by those code hashes, and
/// its `STATICCALL_CHECK` dropped; otherwise (code deployed in the trace, an EIP-7702 delegated
/// account, or code not fetched) it falls back to its `STATICCALL_CHECK` (see
/// [`unpin_stateless_reads`]).
///
/// Fails if an `EXTCODESIZE` read's account has no code in `code`, or not of the size the trace
/// saw (code deployed earlier in the same transaction, say), since the program would then pin
/// the wrong code.
///
/// Returns the gas the pinned reads cost in the trace: the walks counted it in the program's
/// call gas as settlement running them again, which it no longer does.
pub fn resolve_code_sizes(
    updates: &mut Vec<StateUpdate>,
    code: &HashMap<Address, Bytes>,
) -> Result<u64> {
    // An EIP-7702 delegation designator (`0xef0100 ‖ delegate`) isn't the code that ran: the
    // delegate's was, which the trace doesn't name, and which the trace may even have deployed.
    // A read that ran one is checked by running it.
    let has_code = |account: &Address| {
        code.get(account)
            .is_some_and(|bytes| !bytes.is_empty() && !bytes.starts_with(&[0xef, 0x01, 0x00]))
    };
    unpin_reads_where(updates, |markers| {
        !markers.iter().all(|marker| match marker {
            StateUpdate::CodehashCheck(check) => has_code(&check.account),
            _ => false,
        })
    })?;
    // The reads left with markers are pinned by them, so their checks go.
    let mut pinned = vec![false; updates.len()];
    let mut pinned_gas = 0u64;
    for (check, markers) in stateless_reads(updates) {
        pinned[check] = true;
        pinned_gas = pinned_gas.saturating_add(read_gas(&updates[markers.start]));
    }
    for update in updates.iter_mut() {
        let StateUpdate::CodehashCheck(check) = update else {
            continue;
        };
        if is_code_marker_hash(check.codehash) {
            // A marker outside a read (a caller moved it from its check) has nothing to stand
            // for; never index on it.
            let Some(bytes) = code.get(&check.account).filter(|bytes| !bytes.is_empty()) else {
                bail!(
                    "no code fetched for {}, whose code a stateless read ran",
                    check.account
                );
            };
            check.codehash = keccak256(bytes);
            continue;
        }
        let Some(size) = marked_size(check.codehash) else {
            continue;
        };
        let Some(bytes) = code.get(&check.account) else {
            bail!("no code fetched for {}, read by EXTCODESIZE", check.account);
        };
        if bytes.len() as u64 != size {
            bail!(
                "{} has {} bytes of code at the traced block, but the trace read a size of {size}",
                check.account,
                bytes.len()
            );
        }
        check.codehash = keccak256(bytes);
    }
    let mut at = 0;
    updates.retain(|_| {
        at += 1;
        !pinned[at - 1]
    });
    dedupe_checks(updates);
    Ok(pinned_gas)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn log(op: &str, depth: u64, stack: Vec<U256>, memory: Vec<&str>) -> StructLog {
        StructLog {
            pc: 0,
            op: op.to_string().into(),
            gas: 0,
            gas_cost: 0,
            depth,
            error: None,
            stack: Some(stack),
            return_data: None,
            memory: Some(memory.iter().map(|s| s.to_string()).collect()),
            memory_size: None,
            storage: None,
            refund_counter: None,
        }
    }

    #[test]
    fn result_hash_packs_the_flag_before_the_data() {
        assert_eq!(result_hash(true, &[]), keccak256([1u8]));
        assert_eq!(result_hash(false, &[0xab]), keccak256([0u8, 0xab]));
    }

    #[test]
    fn precompiles_are_recognized() {
        assert!(is_precompile(address!(
            "0x0000000000000000000000000000000000000001"
        )));
        assert!(is_precompile(address!(
            "0x0000000000000000000000000000000000000011"
        )));
        assert!(is_precompile(address!(
            "0x0000000000000000000000000000000000000100"
        )));
        assert!(!is_precompile(Address::ZERO));
        assert!(!is_precompile(address!(
            "0x0000000000000000000000000000000000000012"
        )));
    }

    #[test]
    fn memory_slice_spans_words_and_pads_past_the_end() {
        let word0 = format!("{}{}", "00".repeat(31), "aa");
        let word1 = format!("bb{}", "00".repeat(31));
        let l = log("RETURN", 1, vec![], vec![&word0, &word1]);
        assert_eq!(memory_slice(&l, 31, 2), vec![0xaa, 0xbb]);
        assert_eq!(memory_slice(&l, 63, 3), vec![0x00, 0x00, 0x00]);
    }

    /// A callee's RETURN data reaches its caller; a later callee that halts gets none of it.
    #[test]
    fn exit_data_is_per_frame() {
        let word = format!("{}{}", "00".repeat(31), "07");
        let mut exits = ExitData::default();
        // Stack bottom→top: size, offset.
        exits.step(&log("CALL", 1, vec![], vec![])).unwrap();
        exits
            .step(&log(
                "RETURN",
                2,
                vec![U256::from(1), U256::from(31)],
                vec![&word],
            ))
            .unwrap();
        exits.step(&log("POP", 1, vec![], vec![])).unwrap();
        assert_eq!(exits.take(2).as_ref(), &[0x07]);
        exits.step(&log("INVALID", 2, vec![], vec![])).unwrap();
        exits.step(&log("POP", 1, vec![], vec![])).unwrap();
        assert!(exits.take(2).is_empty());
    }

    #[test]
    fn a_static_read_failing_without_data_cannot_be_checked() {
        let check = || {
            vec![StateUpdate::StaticCallCheck(
                IStateUpdateTypes::StaticCallCheck {
                    target: Address::repeat_byte(1),
                    data: Bytes::new(),
                    resultHash: B256::ZERO,
                },
            )]
        };
        let pin = Await::Static {
            index: 0,
            stateless: false,
        };

        let mut out = check();
        pin.resolve(&mut out, U256::ZERO, &[7]).unwrap();
        let StateUpdate::StaticCallCheck(c) = &out[0] else {
            unreachable!()
        };
        assert_eq!(
            c.resultHash,
            result_hash(false, &[7]),
            "a revert with data is pinned"
        );

        assert!(pin.resolve(&mut check(), U256::ZERO, &[]).is_err());
        pin.resolve(&mut check(), U256::from(1), &[]).unwrap();

        // A stateless read's empty failure is its code's own: marked, for its code to pin.
        let stateless = Await::Static {
            index: 0,
            stateless: true,
        };
        let mut out = check();
        stateless.resolve(&mut out, U256::ZERO, &[]).unwrap();
        assert!(matches!(&out[0], StateUpdate::StaticCallCheck(c) if c.resultHash == UNCHECKABLE));
    }

    #[test]
    fn call_pins_follow_the_sdks_failure_rule() {
        assert_eq!(call_pin(true, true, &[]), result_hash(true, &[]));
        assert_eq!(call_pin(false, false, &[1]), result_hash(false, &[1]));
        assert_eq!(
            call_pin(false, true, &[1]),
            B256::ZERO,
            "a failure sending value"
        );
        assert_eq!(
            call_pin(false, false, &[]),
            B256::ZERO,
            "a failure without data"
        );
    }

    #[test]
    fn unresolved_code_sizes_can_be_dropped() {
        let account = Address::repeat_byte(3);
        let mut updates = vec![
            PendingRead::CodeSize(account).check(U256::from(3)).unwrap(),
            PendingRead::CodeSize(Address::repeat_byte(1))
                .check(U256::ZERO)
                .unwrap(),
        ];
        assert_eq!(
            drop_unresolved_code_sizes(&mut updates).unwrap(),
            vec![account]
        );
        assert_eq!(updates.len(), 1, "the zero size is already a hash");
        assert!(unresolved_code_sizes(&updates).is_empty());
    }

    #[test]
    fn code_size_markers_resolve_to_the_code_hash() {
        let account = address!("0x00000000000000000000000000000000000c0de0");
        let mut updates = vec![
            PendingRead::CodeSize(account).check(U256::from(3)).unwrap(),
            PendingRead::CodeSize(Address::repeat_byte(1))
                .check(U256::ZERO)
                .unwrap(),
        ];
        assert_eq!(unresolved_code_sizes(&updates), vec![(account, 3)]);

        let wrong = HashMap::from([(account, Bytes::from_static(&[1, 2]))]);
        assert!(resolve_code_sizes(&mut updates.clone(), &wrong).is_err());

        let code = HashMap::from([(account, Bytes::from_static(&[1, 2, 3]))]);
        resolve_code_sizes(&mut updates, &code).unwrap();
        let StateUpdate::CodehashCheck(check) = &updates[0] else {
            panic!("a codehash check");
        };
        assert_eq!(check.codehash, keccak256([1u8, 2, 3]));
        assert!(unresolved_code_sizes(&updates).is_empty());
    }

    fn read_of(target: Address, result: B256) -> StateUpdate {
        StateUpdate::StaticCallCheck(IStateUpdateTypes::StaticCallCheck {
            target,
            data: Bytes::new(),
            resultHash: result,
        })
    }

    fn store() -> StateUpdate {
        StateUpdate::Store(IStateUpdateTypes::Store {
            slot: B256::ZERO,
            value: B256::ZERO,
        })
    }

    fn pinned_code(updates: &[StateUpdate]) -> Vec<(Address, B256)> {
        updates
            .iter()
            .filter_map(|u| match u {
                StateUpdate::CodehashCheck(c) => Some((c.account, c.codehash)),
                _ => None,
            })
            .collect()
    }

    fn reads(updates: &[StateUpdate]) -> Vec<Address> {
        updates
            .iter()
            .filter_map(|u| match u {
                StateUpdate::StaticCallCheck(c) => Some(c.target),
                _ => None,
            })
            .collect()
    }

    /// A stateless read is its check and its code markers. With all its code it's pinned by the
    /// code's hashes alone; without, by its check alone. Never by neither or both.
    #[test]
    fn a_stateless_read_is_pinned_by_its_code_or_falls_back_to_its_check() {
        let verifier = Address::repeat_byte(5);
        let hasher = Address::repeat_byte(6);
        let sized = Address::repeat_byte(3);
        let ok = result_hash(true, &[1]);
        let updates = vec![
            read_of(verifier, ok),
            code_check(verifier),
            code_check(hasher),
            store(),
            PendingRead::CodeSize(sized).check(U256::from(2)).unwrap(),
        ];
        assert_eq!(
            unresolved_code_accounts(&updates),
            vec![sized, verifier, hasher]
        );
        assert_eq!(unresolved_code_sizes(&updates), vec![(sized, 2)]);

        let sized_code = (sized, Bytes::from_static(&[1, 2]));
        let all = HashMap::from([
            sized_code.clone(),
            (verifier, Bytes::from_static(&[0xfe])),
            (hasher, Bytes::from_static(&[0xfd])),
        ]);
        let mut resolved = updates.clone();
        resolve_code_sizes(&mut resolved, &all).unwrap();
        assert!(reads(&resolved).is_empty(), "{resolved:?}");
        assert_eq!(
            pinned_code(&resolved),
            [
                (verifier, keccak256([0xfeu8])),
                (hasher, keccak256([0xfdu8])),
                (sized, keccak256([1u8, 2])),
            ]
        );
        assert!(
            matches!(resolved[0], StateUpdate::CodehashCheck(_)),
            "pinned where it was read"
        );

        // One account's code missing (deployed in the trace, say), or empty: the read falls back.
        for code in [
            HashMap::from([sized_code.clone(), (verifier, Bytes::from_static(&[0xfe]))]),
            HashMap::from([
                sized_code.clone(),
                (verifier, Bytes::from_static(&[0xfe])),
                (hasher, Bytes::new()),
            ]),
        ] {
            let mut fell_back = updates.clone();
            resolve_code_sizes(&mut fell_back, &code).unwrap();
            assert_eq!(reads(&fell_back), [verifier]);
            assert_eq!(pinned_code(&fell_back), [(sized, keccak256([1u8, 2]))]);
        }

        // An EXTCODESIZE read still needs its code.
        let no_size = HashMap::from([(verifier, Bytes::from_static(&[0xfe]))]);
        assert!(resolve_code_sizes(&mut updates.clone(), &no_size).is_err());

        // A caller that can't fetch code keeps every read's check.
        let mut dropped = updates.clone();
        assert_eq!(
            drop_unresolved_code_sizes(&mut dropped).unwrap(),
            vec![sized]
        );
        assert_eq!(reads(&dropped), [verifier]);
        assert!(pinned_code(&dropped).is_empty());
        let mut unpinned = updates;
        unpin_stateless_reads(&mut unpinned).unwrap();
        assert_eq!(reads(&unpinned), [verifier]);
        assert_eq!(unresolved_code_accounts(&unpinned), [sized]);
    }

    /// A stateless read that failed without data can only be pinned by its code.
    #[test]
    fn an_uncheckable_read_must_be_pinned_by_its_code() {
        let verifier = Address::repeat_byte(5);
        let updates = vec![read_of(verifier, UNCHECKABLE), code_check(verifier)];
        assert!(unpin_stateless_reads(&mut updates.clone()).is_err());
        assert!(resolve_code_sizes(&mut updates.clone(), &HashMap::new()).is_err());
        let mut resolved = updates;
        let code = HashMap::from([(verifier, Bytes::from_static(&[0xfe]))]);
        resolve_code_sizes(&mut resolved, &code).unwrap();
        assert_eq!(pinned_code(&resolved), [(verifier, keccak256([0xfeu8]))]);
        assert!(reads(&resolved).is_empty());
    }

    /// A repeated stateless read goes with its markers; once resolved, a code hash pinned twice
    /// (by two reads, or a read and an `EXTCODESIZE`) is checked once, unless a call between
    /// could change it.
    #[test]
    fn code_pins_dedupe_per_account() {
        let call = StateUpdate::Call(IStateUpdateTypes::Call {
            target: Address::repeat_byte(9),
            value: U256::ZERO,
            callargs: Bytes::new(),
            resultHash: B256::ZERO,
            gasLimit: U256::MAX,
        });
        let a = Address::repeat_byte(5);
        let b = Address::repeat_byte(6);
        let ok = result_hash(true, &[]);
        let mut updates = vec![
            read_of(a, ok),
            code_check(a),
            code_check(b),
            read_of(a, ok),
            code_check(a),
            code_check(b),
            read_of(b, ok),
            code_check(b),
            PendingRead::CodeSize(a).check(U256::from(1)).unwrap(),
            store(),
            call,
            read_of(a, ok),
            code_check(a),
        ];
        dedupe_checks(&mut updates);
        assert_eq!(
            reads(&updates),
            [a, b, a],
            "the repeated read and its markers go"
        );
        assert_eq!(updates.len(), 10, "{updates:?}");

        let code = HashMap::from([
            (a, Bytes::from_static(&[0xfe])),
            (b, Bytes::from_static(&[0xfd])),
        ]);
        resolve_code_sizes(&mut updates, &code).unwrap();
        let (ha, hb) = (keccak256([0xfeu8]), keccak256([0xfdu8]));
        assert_eq!(pinned_code(&updates), [(a, ha), (b, hb), (a, ha)]);

        let (priced, gas) = crate::heuristic::unpinned_for_pricing(&updates);
        assert!(priced.iter().all(|u| !u.is_check()), "left out for pricing");
        assert_eq!(gas, 3 * crate::heuristic::COLD_ACCOUNT_ACCESS_COST);
    }

    /// Unresolved, a stateless read is priced as the check it may fall back to.
    #[test]
    fn an_unresolved_stateless_read_is_priced_as_its_check() {
        let a = Address::repeat_byte(5);
        let updates = vec![
            read_of(a, result_hash(true, &[])),
            code_check(a),
            code_check(a),
        ];
        let (priced, gas) = crate::heuristic::unpinned_for_pricing(&updates);
        assert!(priced.is_empty());
        assert_eq!(gas, crate::heuristic::COLD_ACCOUNT_ACCESS_COST);
    }

    #[test]
    fn reads_of_precompiles_pin_nothing() {
        let ecrecover = address!("0x0000000000000000000000000000000000000001");
        assert!(PendingRead::Codehash(ecrecover).check(U256::ZERO).is_none());
        assert!(PendingRead::Balance(ecrecover).check(U256::ZERO).is_some());
    }

    /// A marker a caller moved away from its read names code that wasn't fetched: an error, not
    /// a panic.
    #[test]
    fn an_orphaned_code_marker_is_an_error() {
        let mut updates = vec![code_check(Address::repeat_byte(0xab))];
        assert!(resolve_code_sizes(&mut updates, &HashMap::new()).is_err());
    }

    /// The same call, once stateless and once not, is two reads: the stateless one may be pinned
    /// by its code, which says nothing of the storage the other read.
    #[test]
    fn a_stateless_read_does_not_stand_in_for_a_stateful_one() {
        let check = || {
            StateUpdate::StaticCallCheck(IStateUpdateTypes::StaticCallCheck {
                target: Address::repeat_byte(0xcc),
                data: Bytes::from_static(&[1, 2, 3, 4]),
                resultHash: B256::repeat_byte(0x11),
            })
        };
        let marker = code_check(Address::repeat_byte(0xcc));
        let mut updates = vec![check(), marker.clone(), check()];
        dedupe_checks(&mut updates);
        assert_eq!(updates.len(), 3, "both reads stay: {updates:?}");

        let mut updates = vec![check(), marker.clone(), check(), marker];
        dedupe_checks(&mut updates);
        assert_eq!(
            updates.len(),
            2,
            "a repeated stateless read goes, markers and all"
        );
    }

    /// An EIP-7702 delegated account's code is a designator, not the code that ran: its read is
    /// checked by running it.
    #[test]
    fn a_read_of_a_delegated_account_falls_back_to_its_check() {
        let eoa = Address::repeat_byte(0xe0);
        let read = StateUpdate::StaticCallCheck(IStateUpdateTypes::StaticCallCheck {
            target: eoa,
            data: Bytes::new(),
            resultHash: B256::repeat_byte(0x11),
        });
        let mut updates = vec![read, code_check(eoa)];
        let mut designator = vec![0xef, 0x01, 0x00];
        designator.extend_from_slice(Address::repeat_byte(0xde).as_slice());
        let code = HashMap::from([(eoa, Bytes::from(designator))]);
        assert_eq!(resolve_code_sizes(&mut updates, &code).unwrap(), 0);
        assert!(
            matches!(updates.as_slice(), [StateUpdate::StaticCallCheck(_)]),
            "{updates:?}"
        );
    }
}

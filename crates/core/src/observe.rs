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
//! repeated reads; this module holds what they share. A pricer running on state the pins may not
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
    /// A `STATICCALL_CHECK` placeholder at this index: fill in its result hash.
    Static { index: usize },
}

impl Await {
    /// Complete the awaited op in `out` from the resume log: `success` (or, for a `CREATE`, the
    /// deployed address) is on top of its stack, and `returndata` is what the callee handed
    /// back, empty if it opened no frame.
    ///
    /// Fails for a read the program depends on that failed without data (out of gas, a halt,
    /// the call depth): settlement re-runs a check with all its gas and rejects such a failure,
    /// so no check can pin it.
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
            Self::Static { index } => {
                if let Some(StateUpdate::StaticCallCheck(check)) = out.get_mut(index) {
                    if !success && returndata.is_empty() {
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
pub(crate) fn dedupe_checks(updates: &mut Vec<StateUpdate>) {
    let mut reads: Vec<StateUpdate> = Vec::new();
    let mut accounts: Vec<StateUpdate> = Vec::new();
    updates.retain(|update| {
        let seen = match update {
            StateUpdate::StaticCallCheck(_) => &mut reads,
            StateUpdate::BalanceCheck(_) | StateUpdate::CodehashCheck(_) => &mut accounts,
            StateUpdate::Store(_)
            | StateUpdate::Log0(_)
            | StateUpdate::Log1(_)
            | StateUpdate::Log2(_)
            | StateUpdate::Log3(_)
            | StateUpdate::Log4(_) => {
                reads.clear();
                return true;
            }
            StateUpdate::Call(_)
            | StateUpdate::Create(_)
            | StateUpdate::Create2(_)
            | StateUpdate::Nested(_) => {
                reads.clear();
                accounts.clear();
                return true;
            }
        };
        if seen.iter().any(|earlier| same_check(earlier, update)) {
            return false;
        }
        seen.push(update.clone());
        true
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

/// `updates` without the code-size checks still waiting for a code hash, for a caller that can't
/// fetch code: a program carrying one could never settle. Returns the accounts left unpinned.
pub fn drop_unresolved_code_sizes(updates: &mut Vec<StateUpdate>) -> Vec<Address> {
    let unresolved = unresolved_code_sizes(updates)
        .into_iter()
        .map(|(account, _)| account)
        .collect();
    updates.retain(|update| {
        !matches!(update, StateUpdate::CodehashCheck(check) if marked_size(check.codehash).is_some())
    });
    unresolved
}

/// The accounts whose code hash `updates` still need, each with the code size the trace saw:
/// the `EXTCODESIZE` reads of nonzero size.
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

/// Replace every `EXTCODESIZE` marker in `updates` with the account's code hash, from `code`:
/// each account's code at the traced block. Fails if an account's code is missing or isn't the
/// size the trace saw (code deployed earlier in the same transaction, say), since the program
/// would then pin the wrong code.
pub fn resolve_code_sizes(
    updates: &mut [StateUpdate],
    code: &HashMap<Address, Bytes>,
) -> Result<()> {
    for update in updates {
        let StateUpdate::CodehashCheck(check) = update else {
            continue;
        };
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
    Ok(())
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
        let pin = Await::Static { index: 0 };

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
        assert_eq!(drop_unresolved_code_sizes(&mut updates), vec![account]);
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

    #[test]
    fn reads_of_precompiles_pin_nothing() {
        let ecrecover = address!("0x0000000000000000000000000000000000000001");
        assert!(PendingRead::Codehash(ecrecover).check(U256::ZERO).is_none());
        assert!(PendingRead::Balance(ecrecover).check(U256::ZERO).is_some());
    }
}

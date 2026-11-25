//! Which `STATICCALL`s ran code whose result is a function of that code and its input alone.
//!
//! A `STATICCALL_CHECK` makes settlement run its read again, which for heavy pure code (a Groth16
//! verifier's pairings, a Poseidon hash) costs about what the program saves. When every frame
//! under the read is stateless, nothing settlement could see differently can change its result
//! but the code itself, so the walks pin the code instead: a `CODEHASH_CHECK` of each account
//! whose code ran (see `observe::code_check`).
//!
//! A frame is stateless when every op it runs is on [`pure`]'s list, and every call it makes is
//! either into a precompile every chain has (0x01 to 0x0a), or a `STATICCALL` or `DELEGATECALL`
//! into code that is itself stateless, and succeeded. Everything else, from an `SLOAD` or
//! `TIMESTAMP` to an op this module doesn't know, leaves the read checked by running it again.
//!
//! A read of an EIP-7702 delegated account runs its delegate's code, which the trace doesn't
//! name (and may have deployed earlier on). The account's own code is just the designator, so
//! such a read is never pinned by code: resolving it falls back to its check (see
//! `observe::resolve_code_sizes`).
//!
//! Code deployed in the trace itself can't be pinned by its hash: the code a caller fetches
//! from the state the trace ran on isn't there yet. A read that ran such code is left checked.
//!
//! Like `taint`'s, the verdict is keyed by the `STATICCALL`'s struct-log index, which the walks
//! also count, and depends only on the trace, so every operator reaches the same one.

use std::collections::{HashMap, HashSet};

use alloy_primitives::Address;
use alloy_rpc_types::trace::geth::StructLog;

use crate::observe::{is_precompile, stack_top};
use crate::taint::Usage;

/// The stateless `STATICCALL`s of a trace, by struct-log index, each with the accounts whose
/// code ran under it: the target first, then each account a nested call ran code from, in trace
/// order and without repeats. Precompiles run no code and aren't listed.
#[derive(Debug, Clone, Default)]
pub(crate) struct StatelessReads(HashMap<usize, Vec<Address>>);

impl StatelessReads {
    pub(crate) fn of(logs: &[StructLog]) -> Self {
        Self(follow(logs))
    }

    /// [`Self::of`], for a trace whose observations `usage` tracked. Only a `STATICCALL` the
    /// program depends on is ever looked up, so a trace without one skips the walk: most traces
    /// make no such read, and the walk costs a pass over every step.
    pub(crate) fn of_used(logs: &[StructLog], usage: &Usage) -> Self {
        if usage.any_used_staticcall(logs) {
            Self::of(logs)
        } else {
            Self::default()
        }
    }

    /// The accounts to pin by code hash if the `STATICCALL` at struct-log `step` ran stateless
    /// code; `None` if it must be checked by running it again.
    pub(crate) fn code_of(&self, step: usize) -> Option<&[Address]> {
        self.0.get(&step).map(Vec::as_slice)
    }
}

/// A `STATICCALL` whose callee is still running.
struct Read {
    step: usize,
    depth: u64,
    accounts: Vec<Address>,
    stateless: bool,
}

impl Read {
    fn ran(&mut self, account: Address) {
        if !self.accounts.contains(&account) {
            self.accounts.push(account);
        }
    }
}

fn follow(logs: &[StructLog]) -> HashMap<usize, Vec<Address>> {
    let mut out = HashMap::new();
    // The reads whose callees are running, innermost last. Each log belongs to the innermost.
    let mut open: Vec<Read> = Vec::new();
    // Calls made under an open read, whose success flag tops the log resuming their caller:
    // (depth of the call, index of the read in `open`).
    let mut outcomes: Vec<(u64, usize)> = Vec::new();
    // A call that must open a frame for its read to stay stateless: (its depth, the read).
    let mut expect_frame: Option<(u64, usize)> = None;
    // Every account the trace deploys code to, and the depths of the `CREATE`s and `CREATE2`s
    // whose resume log (carrying the address) is still to come.
    let mut created: HashSet<Address> = HashSet::new();
    let mut creating: Vec<u64> = Vec::new();

    for (step, log) in logs.iter().enumerate() {
        let depth = log.depth;
        while let Some(&create_depth) = creating.last()
            && create_depth >= depth
        {
            creating.pop();
            if create_depth == depth
                && let Some(address) = stack_top(log, 0).filter(|a| !a.is_zero())
            {
                created.insert(Address::from_word(address.into()));
            }
        }
        if matches!(log.op.as_ref(), "CREATE" | "CREATE2") && log.error.is_none() {
            creating.push(depth);
        }
        if let Some((call_depth, at)) = expect_frame.take()
            && depth != call_depth + 1
        {
            // A call into an account without code (or that failed to start) ran nothing to pin.
            open[at].stateless = false;
        }
        while let Some(&(call_depth, at)) = outcomes.last()
            && call_depth >= depth
        {
            outcomes.pop();
            // A failure could be the EVM's own (out of gas, the call depth), which depends on
            // more than code and input; and a trace that skips its resume log can't be followed.
            let succeeded = call_depth == depth && stack_top(log, 0).is_some_and(|s| !s.is_zero());
            if !succeeded {
                open[at].stateless = false;
            }
        }
        while open.last().is_some_and(|read| read.depth >= depth) {
            let read = open.pop().expect("checked");
            if let Some(outer) = open.last_mut() {
                // The inner read's frames are the outer read's too.
                outer.stateless &= read.stateless;
                for account in &read.accounts {
                    outer.ran(*account);
                }
            }
            if read.stateless {
                out.insert(read.step, read.accounts);
            }
        }

        let op = log.op.as_ref();
        let inside = open.len().checked_sub(1);
        // Clients differ on whether a `REVERT` step carries an error (reth's tracer sets one,
        // geth's doesn't), and every operator must reach the same verdict: a revert is the
        // callee's own result, which the call's success flag already records.
        let failed = log.error.is_some() && op != "REVERT";
        if let Some(at) = inside
            && (failed || !(pure(op) || is_call(op)))
        {
            open[at].stateless = false;
        }
        if !is_call(op) || log.error.is_some() {
            continue;
        }
        let Some(target) = stack_top(log, 1).map(|v| Address::from_word(v.into())) else {
            if let Some(at) = inside {
                open[at].stateless = false;
            }
            continue;
        };
        if let Some(at) = inside {
            outcomes.push((depth, at));
            if is_precompile(target) {
                if !always_active(target) {
                    // On a chain or fork without it, the address is an empty account and the
                    // call "succeeds" with nothing: its result is the fork's, not the code's.
                    open[at].stateless = false;
                }
            } else {
                match op {
                    "DELEGATECALL" => {
                        // The code runs in the caller's context; it's the code address's code.
                        open[at].ran(target);
                        expect_frame = Some((depth, at));
                    }
                    // A `STATICCALL` opens its own read below, which folds into this one.
                    "STATICCALL" => {}
                    // A `CALL` (or `CALLCODE`) into code: if in doubt, it's stateful.
                    _ => open[at].stateless = false,
                }
            }
        }
        if op == "STATICCALL" && !is_precompile(target) {
            open.push(Read {
                step,
                depth,
                accounts: vec![target],
                stateless: true,
            });
            expect_frame = Some((depth, open.len() - 1));
        }
    }
    // Reads still open when the trace ends can't be followed to their outcome: none qualify.
    // Nor can a read whose code was deployed by the trace (before it, since its code ran) be
    // pinned by code fetched from the state the trace started on.
    out.retain(|_, accounts| !accounts.iter().any(|a| created.contains(a)));
    out
}

/// Whether `precompile` is one every EVM chain this runs on has had since Cancun: `ECRECOVER`
/// (0x01) through the point evaluation precompile (0x0a). Later ones (the BLS12-381 precompiles,
/// `P256VERIFY`) came with later forks, and some chains still lack them.
fn always_active(precompile: Address) -> bool {
    let bytes = precompile.0.0;
    bytes[..19].iter().all(|&b| b == 0) && (1..=0x0a).contains(&bytes[19])
}

fn is_call(op: &str) -> bool {
    matches!(op, "STATICCALL" | "CALL" | "DELEGATECALL" | "CALLCODE")
}

/// Whether `op` reads nothing but its own frame (stack, memory, calldata, return data, its
/// code, and its call's context, which the caller fixes), so its result is the same at
/// settlement as in the trace.
///
/// `GAS` counts, as a deliberate trade-off: snarkjs verifiers pass `gas()` to the pairing
/// precompile, and without it no such verifier could be pinned by its code. The cost is that a
/// read whose result does depend on its gas (a callee branching on `gasleft()`) is settled on
/// the result the trace saw. A `STATICCALL_CHECK` would have run it again with settlement's gas
/// and reverted on a different result; pinned by code, the trace's result stands. A transition
/// is defined by the trace at its reference block, gas included, so that result is the one the
/// operators signed. That covers a read that fails on purpose (`revert()` with no data after a
/// `gasleft()` check), which the walks pin by its code too; a failure the EVM itself causes (out
/// of gas, the call depth) shows as an error on the log and leaves the read checked.
///
/// Not listed, and so stateful: storage and transient storage (`SLOAD`, `TLOAD`), other
/// accounts (`BALANCE`, `SELFBALANCE`, `EXTCODESIZE`, `EXTCODEHASH`, `EXTCODECOPY`), the block and
/// transaction (`BLOCKHASH`, `BLOBHASH`, `COINBASE`, `TIMESTAMP`, `NUMBER`, `PREVRANDAO`/
/// `DIFFICULTY`, `GASLIMIT`, `BASEFEE`, `BLOBBASEFEE`, `GASPRICE`, `ORIGIN`), every write (which a
/// static context rejects anyway), and any op this list doesn't name.
fn pure(op: &str) -> bool {
    matches!(
        op,
        "STOP"
            | "ADD"
            | "MUL"
            | "SUB"
            | "DIV"
            | "SDIV"
            | "MOD"
            | "SMOD"
            | "ADDMOD"
            | "MULMOD"
            | "EXP"
            | "SIGNEXTEND"
            | "LT"
            | "GT"
            | "SLT"
            | "SGT"
            | "EQ"
            | "ISZERO"
            | "AND"
            | "OR"
            | "XOR"
            | "NOT"
            | "BYTE"
            | "SHL"
            | "SHR"
            | "SAR"
            | "CLZ"
            | "KECCAK256"
            | "SHA3"
            | "ADDRESS"
            | "CALLER"
            | "CALLVALUE"
            | "CALLDATALOAD"
            | "CALLDATASIZE"
            | "CALLDATACOPY"
            | "CODESIZE"
            | "CODECOPY"
            | "RETURNDATASIZE"
            | "RETURNDATACOPY"
            | "CHAINID"
            | "GAS"
            | "POP"
            | "MLOAD"
            | "MSTORE"
            | "MSTORE8"
            | "MCOPY"
            | "JUMP"
            | "JUMPI"
            | "PC"
            | "MSIZE"
            | "JUMPDEST"
            | "RETURN"
            | "REVERT"
    ) || numbered(op, "PUSH", 0..=32)
        || numbered(op, "DUP", 1..=16)
        || numbered(op, "SWAP", 1..=16)
}

fn numbered(op: &str, prefix: &str, range: std::ops::RangeInclusive<u8>) -> bool {
    op.strip_prefix(prefix)
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| range.contains(&n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;

    const A: Address = Address::new([0xaa; 20]);
    const B: Address = Address::new([0xbb; 20]);
    const C: Address = Address::new([0xcc; 20]);
    const SHA256: Address =
        Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2]);

    fn log(op: &str, depth: u64, top_first: &[U256]) -> StructLog {
        let mut stack = top_first.to_vec();
        stack.reverse();
        StructLog {
            pc: 0,
            op: op.to_string().into(),
            gas: 100_000,
            gas_cost: 0,
            depth,
            error: None,
            stack: Some(stack),
            return_data: None,
            memory: Some(vec![]),
            memory_size: None,
            storage: None,
            refund_counter: None,
        }
    }

    fn call(op: &str, depth: u64, to: Address) -> StructLog {
        let to = U256::from_be_slice(to.as_slice());
        log(op, depth, &[U256::from(50_000), to, U256::ZERO, U256::ZERO])
    }

    /// The log resuming a caller at `depth`, with a call's success flag on top.
    fn resume(depth: u64, success: bool) -> StructLog {
        log("ISZERO", depth, &[U256::from(success as u8)])
    }

    fn verdict(logs: &[StructLog]) -> Option<Vec<Address>> {
        StatelessReads::of(logs).code_of(0).map(<[_]>::to_vec)
    }

    #[test]
    fn arithmetic_and_precompiles_are_stateless() {
        let logs = [
            call("STATICCALL", 1, A),
            log("CALLDATALOAD", 2, &[U256::ZERO]),
            log("MULMOD", 2, &[]),
            call("STATICCALL", 2, SHA256),
            resume(2, true),
            log("RETURN", 2, &[U256::ZERO, U256::ZERO]),
            resume(1, true),
        ];
        assert_eq!(verdict(&logs), Some(vec![A]));
    }

    #[test]
    fn a_storage_read_is_stateful() {
        let logs = [
            call("STATICCALL", 1, A),
            log("SLOAD", 2, &[U256::ZERO]),
            log("RETURN", 2, &[U256::ZERO, U256::ZERO]),
            resume(1, true),
        ];
        assert_eq!(verdict(&logs), None);
    }

    #[test]
    fn nested_code_is_pinned_and_its_state_reads_count() {
        let clean = [
            call("STATICCALL", 1, A),
            call("STATICCALL", 2, B),
            log("RETURN", 3, &[U256::ZERO, U256::ZERO]),
            resume(2, true),
            call("DELEGATECALL", 2, C),
            log("RETURN", 3, &[U256::ZERO, U256::ZERO]),
            resume(2, true),
            call("STATICCALL", 2, B),
            log("RETURN", 3, &[U256::ZERO, U256::ZERO]),
            resume(2, true),
            log("RETURN", 2, &[U256::ZERO, U256::ZERO]),
            resume(1, true),
        ];
        assert_eq!(verdict(&clean), Some(vec![A, B, C]));

        let mut dirty = clean.to_vec();
        dirty.insert(5, log("TIMESTAMP", 3, &[]));
        assert_eq!(verdict(&dirty), None, "a delegated block read");
        let mut dirty = clean.to_vec();
        dirty.insert(2, log("SLOAD", 3, &[U256::ZERO]));
        assert_eq!(verdict(&dirty), None, "a nested storage read");
    }

    #[test]
    fn failures_and_calls_that_run_nothing_are_stateful() {
        let failed_precompile = [
            call("STATICCALL", 1, A),
            call("STATICCALL", 2, SHA256),
            resume(2, false),
            log("RETURN", 2, &[U256::ZERO, U256::ZERO]),
            resume(1, true),
        ];
        assert_eq!(verdict(&failed_precompile), None);

        let into_an_account_without_code = [
            call("STATICCALL", 1, A),
            call("STATICCALL", 2, B),
            resume(2, true),
            log("RETURN", 2, &[U256::ZERO, U256::ZERO]),
            resume(1, true),
        ];
        assert_eq!(verdict(&into_an_account_without_code), None);

        let frameless = [call("STATICCALL", 1, A), resume(1, true)];
        assert_eq!(verdict(&frameless), None);

        let a_call_into_code = [
            call("STATICCALL", 1, A),
            call("CALL", 2, B),
            log("RETURN", 3, &[U256::ZERO, U256::ZERO]),
            resume(2, true),
            log("RETURN", 2, &[U256::ZERO, U256::ZERO]),
            resume(1, true),
        ];
        assert_eq!(verdict(&a_call_into_code), None);

        let mut halted = log("ADD", 2, &[]);
        halted.error = Some("out of gas".into());
        assert_eq!(
            verdict(&[call("STATICCALL", 1, A), halted, resume(1, false)]),
            None
        );

        let unfinished = [call("STATICCALL", 1, A), log("ADD", 2, &[])];
        assert_eq!(verdict(&unfinished), None);
    }

    /// The walk runs only for a trace with a `STATICCALL` the program depends on.
    #[test]
    fn only_a_used_read_is_followed() {
        let logs = [
            log("PUSH1", 1, &[]),
            call("STATICCALL", 1, A),
            log("ADD", 2, &[]),
            log("RETURN", 2, &[]),
            resume(1, true),
        ];
        assert!(
            StatelessReads::of_used(&logs, &Usage::only(&[]))
                .code_of(1)
                .is_none()
        );
        assert!(
            StatelessReads::of_used(&logs, &Usage::only(&[0]))
                .code_of(1)
                .is_none()
        );
        assert_eq!(
            StatelessReads::of_used(&logs, &Usage::only(&[1])).code_of(1),
            Some(&[A][..])
        );
    }

    /// A precompile a chain may not have yet answers by fork, not by code: the read stays checked.
    #[test]
    fn later_precompiles_are_stateful() {
        const P256VERIFY: Address =
            Address::new([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
        const BLS12_G1ADD: Address = Address::new([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0b,
        ]);
        for precompile in [P256VERIFY, BLS12_G1ADD] {
            let logs = [
                call("STATICCALL", 1, A),
                call("STATICCALL", 2, precompile),
                resume(2, true),
                log("RETURN", 2, &[]),
                resume(1, true),
            ];
            assert_eq!(verdict(&logs), None, "{precompile}");
        }
    }

    /// A tracer that marks the `REVERT` step with an error (as reth's does) gets the same verdict
    /// as one that doesn't.
    #[test]
    fn a_revert_marked_as_an_error_is_still_stateless() {
        let mut revert = log("REVERT", 2, &[U256::ZERO, U256::from(32)]);
        revert.error = Some("Reverted".into());
        let logs = [call("STATICCALL", 1, A), revert, resume(1, false)];
        assert_eq!(verdict(&logs), Some(vec![A]));
    }

    /// The callee's own revert is its result, which the walks pin like a success.
    #[test]
    fn a_revert_with_data_is_still_stateless() {
        let logs = [
            call("STATICCALL", 1, A),
            log("REVERT", 2, &[U256::ZERO, U256::from(32)]),
            resume(1, false),
        ];
        assert_eq!(verdict(&logs), Some(vec![A]));
    }

    #[test]
    fn unknown_ops_are_stateful() {
        assert!(pure("PUSH0") && pure("PUSH32") && pure("DUP16") && pure("SWAP1"));
        assert!(!pure("PUSH33") && !pure("DUP17") && !pure("SWAP0"));
        for op in [
            "SLOAD",
            "TLOAD",
            "BALANCE",
            "SELFBALANCE",
            "EXTCODESIZE",
            "EXTCODEHASH",
            "EXTCODECOPY",
            "BLOCKHASH",
            "BLOBHASH",
            "COINBASE",
            "TIMESTAMP",
            "NUMBER",
            "PREVRANDAO",
            "DIFFICULTY",
            "GASLIMIT",
            "BASEFEE",
            "BLOBBASEFEE",
            "GASPRICE",
            "ORIGIN",
            "SSTORE",
            "TSTORE",
            "LOG0",
            "CREATE",
            "SELFDESTRUCT",
            "INVALID",
            "opcode 0xef not defined",
        ] {
            assert!(!pure(op), "{op}");
        }
    }
}

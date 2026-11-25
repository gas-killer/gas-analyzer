//! Which `STATICCALL`s ran code whose result is a function of that code and its input alone.
//!
//! A `STATICCALL_CHECK` makes settlement run its read again, which for heavy pure code (a Groth16
//! verifier's pairings, a Poseidon hash) costs about what the program saves. When every frame
//! under the read is stateless, nothing settlement could see differently can change its result
//! but the code itself, so the walks pin the code instead: a `CODEHASH_CHECK` of each account
//! whose code ran (see `observe::code_check`).
//!
//! A frame is stateless when every op it runs is on [`pure`]'s list, and every call it makes is
//! either into a precompile, or a `STATICCALL` or `DELEGATECALL` into code that is itself
//! stateless, and succeeded. Everything else, from an `SLOAD` or `TIMESTAMP` to an op this module
//! doesn't know, leaves the read checked by running it again.
//!
//! Like `taint`'s, the verdict is keyed by the `STATICCALL`'s struct-log index, which the walks
//! also count, and depends only on the trace, so every operator reaches the same one.

use std::collections::HashMap;

use alloy_primitives::Address;
use alloy_rpc_types::trace::geth::StructLog;

use crate::observe::{is_precompile, stack_top};

/// The stateless `STATICCALL`s of a trace, by struct-log index, each with the accounts whose
/// code ran under it: the target first, then each account a nested call ran code from, in trace
/// order and without repeats. Precompiles run no code and aren't listed.
#[derive(Debug, Clone, Default)]
pub(crate) struct StatelessReads(HashMap<usize, Vec<Address>>);

impl StatelessReads {
    pub(crate) fn of(logs: &[StructLog]) -> Self {
        Self(follow(logs))
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

    for (step, log) in logs.iter().enumerate() {
        let depth = log.depth;
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
        if let Some(at) = inside
            && (log.error.is_some() || !(pure(op) || is_call(op)))
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
            if !is_precompile(target) {
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
    out
}

fn is_call(op: &str) -> bool {
    matches!(op, "STATICCALL" | "CALL" | "DELEGATECALL" | "CALLCODE")
}

/// Whether `op` reads nothing but its own frame (stack, memory, calldata, return data, its
/// code, and its call's context, which the caller fixes), so its result is the same at
/// settlement as in the trace. `GAS` counts: a read's gas is part of how the trace ran it.
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

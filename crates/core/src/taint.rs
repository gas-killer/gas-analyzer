//! Which observations a program depends on.
//!
//! A pin costs settlement gas, and it makes settlement revert when the observed state moves,
//! which anyone can cause: anyone can send ETH to any account. So a solidity-sdk v0.2.0 program
//! pins only what its effects depend on. [`Usage::of`] follows every value an observation
//! produces through the stack, memory, calldata and return data of every frame. An observation
//! is used when one of its values reaches:
//!
//! - an effect a program replays with the traced arguments: `SSTORE`, `TSTORE`, a `LOG`, a
//!   `CALL`'s or `CREATE`'s operands or input, `SELFDESTRUCT`;
//! - a branch (`JUMP`, `JUMPI`), since a different value could take a different path;
//! - a memory offset or size an op writes to, since the bytes it overwrites would differ.
//!
//! A `CALL`'s success flag is clean: settlement requires success unless the call is pinned, and
//! a caught failure is always pinned. A `STATICCALL`'s result also carries what fed its
//! arguments, since its check replays the traced arguments.
//!
//! Every frame is followed, including those of callees whose code settlement re-runs natively.
//! A value a callee hands back carries what it was computed from, so a nested SDK frame's or a
//! library's observation counts as used when its caller uses the value. Following a native
//! callee can only add uses, never lose one.
//!
//! An observation's id is the index of its struct log in the trace, which the walks also count.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

use alloy_rpc_types::trace::geth::StructLog;

/// The observations that reach an effect, by struct-log index.
#[derive(Debug, Clone)]
pub(crate) struct Usage(Option<HashSet<usize>>);

impl Usage {
    /// Follow `logs`. A trace the tracker can't follow (no stack, an opcode it doesn't know, a
    /// stack that doesn't line up with its model) leaves every observation used.
    pub(crate) fn of(logs: &[StructLog]) -> Self {
        match follow(logs) {
            Ok(used) => Self(Some(used)),
            Err(why) => {
                tracing::debug!(why, "taint tracking gave up; pinning every observation");
                Self(None)
            }
        }
    }

    /// Whether the observation at struct-log `step` reaches an effect.
    pub(crate) fn used(&self, step: usize) -> bool {
        self.0.as_ref().is_none_or(|used| used.contains(&step))
    }

    /// Whether the tracker followed the trace at all.
    #[cfg(test)]
    pub(crate) fn followed(&self) -> bool {
        self.0.is_some()
    }
}

/// The observations a value was computed from, as sorted struct-log indices.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Taint(Option<Rc<[usize]>>);

impl Taint {
    fn of(step: usize) -> Self {
        Self(Some(Rc::from([step].as_slice())))
    }

    fn is_clean(&self) -> bool {
        self.0.is_none()
    }

    fn ids(&self) -> &[usize] {
        self.0.as_deref().unwrap_or(&[])
    }

    fn union(&self, other: &Self) -> Self {
        let (a, b) = match (&self.0, &other.0) {
            (None, _) => return other.clone(),
            (_, None) => return self.clone(),
            (Some(a), Some(b)) if Rc::ptr_eq(a, b) => return self.clone(),
            (Some(a), Some(b)) => (a, b),
        };
        let mut merged = Vec::with_capacity(a.len() + b.len());
        let (mut i, mut j) = (0, 0);
        while i < a.len() && j < b.len() {
            match a[i].cmp(&b[j]) {
                Ordering::Less => {
                    merged.push(a[i]);
                    i += 1;
                }
                Ordering::Greater => {
                    merged.push(b[j]);
                    j += 1;
                }
                Ordering::Equal => {
                    merged.push(a[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        merged.extend_from_slice(&a[i..]);
        merged.extend_from_slice(&b[j..]);
        if merged.len() == a.len() {
            self.clone()
        } else if merged.len() == b.len() {
            other.clone()
        } else {
            Self(Some(merged.into()))
        }
    }
}

/// A run of bytes `start..end` and what they were computed from.
type Piece = (u64, u64, Taint);

/// The tainted byte ranges of a memory, calldata or return data buffer. Bytes outside every
/// range are clean.
#[derive(Debug, Clone, Default)]
struct Regions(BTreeMap<u64, (u64, Taint)>);

impl Regions {
    /// The tainted pieces of `start..start + len`, relative to `start`.
    fn slice(&self, start: u64, len: u64) -> Vec<Piece> {
        let end = start.saturating_add(len);
        let mut out = Vec::new();
        if len == 0 {
            return out;
        }
        if let Some((_, (e, t))) = self.0.range(..start).next_back()
            && *e > start
        {
            out.push((0, (*e).min(end) - start, t.clone()));
        }
        for (s, (e, t)) in self.0.range(start..end) {
            out.push((s - start, (*e).min(end) - start, t.clone()));
        }
        out
    }

    fn get(&self, start: u64, len: u64) -> Taint {
        self.slice(start, len)
            .iter()
            .fold(Taint::default(), |acc, (_, _, t)| acc.union(t))
    }

    fn clear(&mut self, start: u64, end: u64) {
        if let Some((&s, (e, t))) = self.0.range(..start).next_back()
            && *e > start
        {
            let (e, t) = (*e, t.clone());
            self.0.insert(s, (start, t.clone()));
            if e > end {
                self.0.insert(end, (e, t));
            }
        }
        let inside: Vec<u64> = self.0.range(start..end).map(|(s, _)| *s).collect();
        for s in inside {
            let (e, t) = self.0.remove(&s).expect("listed");
            if e > end {
                self.0.insert(end, (e, t));
            }
        }
    }

    /// Overwrite `to..to + len` with `pieces` (relative to `to`), adding `extra` to every byte.
    fn write(&mut self, to: u64, len: u64, pieces: Vec<Piece>, extra: &Taint) {
        if len == 0 {
            return;
        }
        let end = to.saturating_add(len);
        self.clear(to, end);
        let mut cursor = 0;
        let mut put = |s: u64, e: u64, t: Taint| {
            if s < e && !t.is_clean() {
                self.0
                    .insert(to.saturating_add(s), (to.saturating_add(e), t));
            }
        };
        for (s, e, t) in pieces {
            put(cursor, s, extra.clone());
            put(s, e, t.union(extra));
            cursor = e;
        }
        put(cursor, len, extra.clone());
    }

    fn set(&mut self, to: u64, len: u64, taint: &Taint) {
        self.write(to, len, Vec::new(), taint);
    }

    /// Add `taint` to every byte of `to..to + len`, keeping what they carry.
    fn add(&mut self, to: u64, len: u64, taint: &Taint) {
        let pieces = self.slice(to, len);
        self.write(to, len, pieces, taint);
    }

    fn of(pieces: Vec<Piece>, len: u64) -> Self {
        let mut out = Self::default();
        out.write(0, len, pieces, &Taint::default());
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallKind {
    Call,
    Static,
    Delegate,
    Create,
}

/// A call or create a frame is suspended on.
#[derive(Debug)]
struct Pending {
    kind: CallKind,
    /// What the success flag (or a `CREATE`'s address) carries.
    result: Taint,
    /// What every byte the callee returns carries.
    returned: Taint,
    /// What a callee that opens no frame (a precompile) computes its output from.
    input_all: Taint,
    /// Where the caller wants the output.
    out: (u64, u64),
    /// The callee's calldata.
    input: Regions,
    input_extra: Taint,
}

/// How a frame ended.
#[derive(Debug, Default)]
enum Exit {
    /// `STOP`, `SELFDESTRUCT` or an exceptional halt: nothing returned.
    #[default]
    Halt,
    /// `RETURN` or `REVERT` of `len` bytes; `extra` is what the offset and size carry.
    Data {
        data: Regions,
        len: u64,
        extra: Taint,
    },
}

#[derive(Debug, Default)]
struct Frame {
    stack: Vec<Taint>,
    memory: Regions,
    calldata: Regions,
    calldata_extra: Taint,
    returndata: Regions,
    returndata_extra: Taint,
    pending: Option<Pending>,
    exit: Exit,
}

impl Frame {
    fn pop(&mut self) -> Result<Taint, String> {
        self.stack
            .pop()
            .ok_or_else(|| "stack underflow".to_string())
    }

    fn pops<const N: usize>(&mut self) -> Result<[Taint; N], String> {
        let mut out: [Taint; N] = std::array::from_fn(|_| Taint::default());
        for slot in &mut out {
            *slot = self.pop()?;
        }
        Ok(out)
    }

    /// Resume after the pending call: `exit` is how its frame ended, `None` if it opened none.
    fn resume(&mut self, exit: Option<Exit>) -> Result<(), String> {
        let call = self
            .pending
            .take()
            .ok_or_else(|| "resumed with no call pending".to_string())?;
        let (out_offset, out_len) = call.out;
        match exit {
            None => {
                // A precompile's output is a function of its input; an account without code
                // returns nothing. The size isn't known, so the output region keeps what it had.
                let returned = call.returned.union(&call.input_all);
                if call.kind != CallKind::Create {
                    self.memory.add(out_offset, out_len, &returned);
                }
                self.returndata = Regions::default();
                self.returndata_extra = returned;
            }
            Some(Exit::Halt) => {
                self.returndata = Regions::default();
                self.returndata_extra = call.returned;
            }
            Some(Exit::Data { data, len, extra }) => {
                let returned = call.returned.union(&extra);
                if call.kind != CallKind::Create {
                    let copied = out_len.min(len);
                    self.memory
                        .write(out_offset, copied, data.slice(0, copied), &returned);
                }
                self.returndata = data;
                self.returndata_extra = returned;
            }
        }
        self.stack.push(call.result);
        Ok(())
    }
}

fn word(log: &StructLog, i: usize) -> Result<u64, String> {
    let stack = log.stack.as_ref().ok_or("no stack")?;
    let at = stack
        .len()
        .checked_sub(i + 1)
        .ok_or_else(|| format!("{} reads past its stack", log.op))?;
    Ok(u64::try_from(stack[at]).unwrap_or(u64::MAX))
}

/// `(pops, pushes)` of an opcode whose result carries everything it popped.
fn arity(op: &str) -> Option<(usize, usize)> {
    Some(match op {
        "STOP" | "JUMPDEST" | "INVALID" => (0, 0),
        "ADDRESS" | "ORIGIN" | "CALLER" | "CALLVALUE" | "CODESIZE" | "GASPRICE" | "COINBASE"
        | "TIMESTAMP" | "NUMBER" | "DIFFICULTY" | "PREVRANDAO" | "RANDOM" | "GASLIMIT"
        | "CHAINID" | "BASEFEE" | "BLOBBASEFEE" | "PC" | "MSIZE" | "GAS" => (0, 1),
        "ISZERO" | "NOT" | "BLOCKHASH" | "BLOBHASH" | "CLZ" => (1, 1),
        "ADD" | "MUL" | "SUB" | "DIV" | "SDIV" | "MOD" | "SMOD" | "EXP" | "SIGNEXTEND" | "LT"
        | "GT" | "SLT" | "SGT" | "EQ" | "AND" | "OR" | "XOR" | "BYTE" | "SHL" | "SHR" | "SAR" => {
            (2, 1)
        }
        "ADDMOD" | "MULMOD" => (3, 1),
        _ => return None,
    })
}

fn numbered(op: &str, prefix: &str) -> Option<usize> {
    op.strip_prefix(prefix)?.parse().ok()
}

fn follow(logs: &[StructLog]) -> Result<HashSet<usize>, String> {
    let mut used = HashSet::new();
    let mut sink = |t: &Taint| used.extend(t.ids().iter().copied());
    // One frame per open depth.
    let mut frames: Vec<Frame> = Vec::new();

    for (step, log) in logs.iter().enumerate() {
        let depth = usize::try_from(log.depth).map_err(|_| "depth out of range")?;
        if frames.is_empty() {
            if depth != 1 {
                return Err(format!("trace starts at depth {depth}"));
            }
            frames.push(Frame::default());
        } else if depth == frames.len() + 1 {
            let caller = frames.last().expect("nonempty");
            let call = caller
                .pending
                .as_ref()
                .ok_or_else(|| format!("step {step} opened a frame no call made"))?;
            frames.push(Frame {
                calldata: call.input.clone(),
                calldata_extra: call.input_extra.clone(),
                ..Frame::default()
            });
        } else if depth > frames.len() {
            return Err(format!("depth jumped to {depth} at step {step}"));
        } else {
            while frames.len() > depth {
                let closed = frames.pop().expect("nonempty");
                frames
                    .last_mut()
                    .expect("depth >= 1")
                    .resume(Some(closed.exit))?;
            }
            // Still pending: the call at the previous step opened no frame.
            let frame = frames.last_mut().expect("depth >= 1");
            if frame.pending.is_some() {
                frame.resume(None)?;
            }
        }

        let frame = frames.last_mut().expect("depth >= 1");
        let stack_len = log.stack.as_ref().ok_or("no stack")?.len();
        if stack_len != frame.stack.len() {
            return Err(format!(
                "step {step} ({}) has {stack_len} stack items, the model {}",
                log.op,
                frame.stack.len()
            ));
        }
        // An op that fails halts its frame without effect.
        if log.error.is_some() {
            continue;
        }

        let op = log.op.as_ref();
        let w = |i| word(log, i);
        match op {
            _ if op.starts_with("PUSH") => frame.stack.push(Taint::default()),
            _ if op.starts_with("DUP") => {
                let n = numbered(op, "DUP").ok_or("bad DUP")?;
                let at = frame.stack.len().checked_sub(n).ok_or("DUP underflow")?;
                frame.stack.push(frame.stack[at].clone());
            }
            _ if op.starts_with("SWAP") => {
                let n = numbered(op, "SWAP").ok_or("bad SWAP")?;
                let top = frame.stack.len().checked_sub(1).ok_or("SWAP underflow")?;
                let other = top.checked_sub(n).ok_or("SWAP underflow")?;
                frame.stack.swap(top, other);
            }
            "POP" => {
                frame.pop()?;
            }
            "MLOAD" => {
                let offset = w(0)?;
                let [t] = frame.pops()?;
                let loaded = frame.memory.get(offset, 32).union(&t);
                frame.stack.push(loaded);
            }
            "MSTORE" | "MSTORE8" => {
                let offset = w(0)?;
                let [at, value] = frame.pops()?;
                sink(&at);
                frame
                    .memory
                    .set(offset, if op == "MSTORE" { 32 } else { 1 }, &value);
            }
            "MCOPY" => {
                let (to, from, len) = (w(0)?, w(1)?, w(2)?);
                let [t_to, t_from, t_len] = frame.pops()?;
                sink(&t_to.union(&t_len));
                let pieces = frame.memory.slice(from, len);
                frame.memory.write(to, len, pieces, &t_from);
            }
            "CALLDATALOAD" => {
                let offset = w(0)?;
                let [t] = frame.pops()?;
                let loaded = frame
                    .calldata
                    .get(offset, 32)
                    .union(&t)
                    .union(&frame.calldata_extra);
                frame.stack.push(loaded);
            }
            "CALLDATASIZE" => frame.stack.push(frame.calldata_extra.clone()),
            "RETURNDATASIZE" => frame.stack.push(frame.returndata_extra.clone()),
            "CALLDATACOPY" | "RETURNDATACOPY" => {
                let (to, from, len) = (w(0)?, w(1)?, w(2)?);
                let [t_to, t_from, t_len] = frame.pops()?;
                sink(&t_to.union(&t_len));
                let (source, extra) = if op == "CALLDATACOPY" {
                    (&frame.calldata, &frame.calldata_extra)
                } else {
                    (&frame.returndata, &frame.returndata_extra)
                };
                let pieces = source.slice(from, len);
                let extra = extra.union(&t_from);
                frame.memory.write(to, len, pieces, &extra);
            }
            "CODECOPY" => {
                let (to, len) = (w(0)?, w(2)?);
                let [t_to, t_from, t_len] = frame.pops()?;
                sink(&t_to.union(&t_len));
                frame.memory.set(to, len, &t_from);
            }
            "EXTCODECOPY" => {
                let (to, len) = (w(1)?, w(3)?);
                let [t_account, t_to, t_from, t_len] = frame.pops()?;
                sink(&t_to.union(&t_len));
                frame.memory.set(to, len, &t_account.union(&t_from));
            }
            "KECCAK256" | "SHA3" => {
                let (offset, len) = (w(0)?, w(1)?);
                let [t_offset, t_len] = frame.pops()?;
                let hashed = frame.memory.get(offset, len).union(&t_offset).union(&t_len);
                frame.stack.push(hashed);
            }
            // The program replays its own storage, so a loaded value carries only its key.
            "SLOAD" | "TLOAD" => {
                let [key] = frame.pops()?;
                frame.stack.push(key);
            }
            "SSTORE" | "TSTORE" => {
                let [key, value] = frame.pops()?;
                sink(&key.union(&value));
            }
            "JUMP" | "SELFDESTRUCT" => {
                let [t] = frame.pops()?;
                sink(&t);
            }
            "JUMPI" => {
                let [dest, cond] = frame.pops()?;
                sink(&dest.union(&cond));
            }
            _ if op.starts_with("LOG") => {
                let topics = numbered(op, "LOG").ok_or("bad LOG")?;
                let (offset, len) = (w(0)?, w(1)?);
                let mut all = frame.memory.get(offset, len);
                for _ in 0..topics + 2 {
                    all = all.union(&frame.pop()?);
                }
                sink(&all);
            }
            "BALANCE" | "EXTCODESIZE" | "EXTCODEHASH" => {
                let [account] = frame.pops()?;
                frame.stack.push(account.union(&Taint::of(step)));
            }
            "SELFBALANCE" => frame.stack.push(Taint::of(step)),
            "RETURN" | "REVERT" => {
                let (offset, len) = (w(0)?, w(1)?);
                let [t_offset, t_len] = frame.pops()?;
                let data = Regions::of(frame.memory.slice(offset, len), len);
                frame.exit = Exit::Data {
                    data,
                    len,
                    extra: t_offset.union(&t_len),
                };
            }
            "CALL" | "CALLCODE" | "DELEGATECALL" | "STATICCALL" => {
                let with_value = matches!(op, "CALL" | "CALLCODE");
                let at = if with_value { 3 } else { 2 };
                let (in_offset, in_len) = (w(at)?, w(at + 1)?);
                let (out_offset, out_len) = (w(at + 2)?, w(at + 3)?);
                let [gas, target] = frame.pops()?;
                let value = if with_value {
                    frame.pop()?
                } else {
                    Taint::default()
                };
                let [t_in_offset, t_in_len, t_out_offset, t_out_len] = frame.pops()?;
                sink(&t_out_offset.union(&t_out_len));
                let input_extra = t_in_offset.union(&t_in_len);
                let input_all = frame.memory.get(in_offset, in_len).union(&input_extra);
                let args = gas.union(&target).union(&value);
                let (kind, result, returned) = match op {
                    // Replayed with the traced arguments, so they're effects. Settlement requires
                    // success unless the call is pinned.
                    "CALL" | "CALLCODE" => {
                        sink(&args.union(&input_all));
                        (CallKind::Call, Taint::default(), Taint::of(step))
                    }
                    // Runs in this frame's context; its calldata carries the input.
                    "DELEGATECALL" => {
                        sink(&args);
                        (CallKind::Delegate, Taint::default(), Taint::default())
                    }
                    // The check replays the traced arguments, so the result carries them.
                    _ => {
                        let read = Taint::of(step).union(&args).union(&input_all);
                        (CallKind::Static, read.clone(), read)
                    }
                };
                frame.pending = Some(Pending {
                    kind,
                    result,
                    returned,
                    input_all,
                    out: (out_offset, out_len),
                    input: Regions::of(frame.memory.slice(in_offset, in_len), in_len),
                    input_extra,
                });
            }
            "CREATE" | "CREATE2" => {
                let (offset, len) = (w(1)?, w(2)?);
                let mut all = frame.memory.get(offset, len);
                for _ in 0..if op == "CREATE" { 3 } else { 4 } {
                    all = all.union(&frame.pop()?);
                }
                sink(&all);
                // A `CREATE`'s address depends on the deployer's nonce; a `CREATE2`'s only on
                // what the program carries.
                let result = if op == "CREATE" {
                    Taint::of(step)
                } else {
                    Taint::default()
                };
                frame.pending = Some(Pending {
                    kind: CallKind::Create,
                    result,
                    returned: Taint::default(),
                    input_all: Taint::default(),
                    out: (0, 0),
                    input: Regions::default(),
                    input_extra: Taint::default(),
                });
            }
            _ => {
                let (pops, pushes) = arity(op).ok_or_else(|| format!("unknown opcode {op}"))?;
                let mut carried = Taint::default();
                for _ in 0..pops {
                    carried = carried.union(&frame.pop()?);
                }
                for _ in 0..pushes {
                    frame.stack.push(carried.clone());
                }
            }
        }
    }
    Ok(used)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloy_primitives::U256;

    /// Build struct logs from `(depth, op, stack top-first)`, with stacks the caller keeps
    /// consistent.
    pub(crate) fn logs(steps: &[(u64, &str, &[u64])]) -> Vec<StructLog> {
        steps
            .iter()
            .map(|(depth, op, top_first)| StructLog {
                pc: 0,
                op: op.to_string().into(),
                gas: 100_000,
                gas_cost: 0,
                depth: *depth,
                error: None,
                stack: Some(top_first.iter().rev().map(|v| U256::from(*v)).collect()),
                return_data: None,
                memory: Some(Vec::new()),
                memory_size: None,
                storage: None,
                refund_counter: None,
            })
            .collect()
    }

    fn used(steps: &[(u64, &str, &[u64])]) -> Vec<usize> {
        let usage = Usage::of(&logs(steps));
        assert!(usage.followed(), "the model must follow the trace");
        let mut out: Vec<usize> = usage.0.expect("followed").into_iter().collect();
        out.sort();
        out
    }

    #[test]
    fn a_dropped_read_is_unused() {
        assert_eq!(
            used(&[
                (1, "PUSH1", &[]),
                (1, "BALANCE", &[7]),
                (1, "POP", &[0]),
                (1, "STOP", &[])
            ]),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn a_stored_read_is_used() {
        assert_eq!(
            used(&[
                (1, "PUSH1", &[]),
                (1, "BALANCE", &[7]),
                (1, "PUSH1", &[0]),
                (1, "SSTORE", &[5, 0]),
                (1, "STOP", &[]),
            ]),
            vec![1]
        );
    }

    #[test]
    fn a_branch_on_a_read_uses_it() {
        assert_eq!(
            used(&[
                (1, "SELFBALANCE", &[]),
                (1, "PUSH1", &[0]),
                (1, "JUMPI", &[9, 0]),
                (1, "STOP", &[]),
            ]),
            vec![0]
        );
    }

    #[test]
    fn a_read_flows_through_arithmetic_and_memory() {
        assert_eq!(
            used(&[
                (1, "PUSH1", &[]),
                (1, "BALANCE", &[7]),
                (1, "PUSH1", &[0]),
                (1, "ADD", &[1, 0]),
                (1, "PUSH1", &[1]),
                (1, "MSTORE", &[0x40, 1]),
                (1, "PUSH1", &[]),
                (1, "MLOAD", &[0x40]),
                (1, "PUSH1", &[0]),
                (1, "PUSH1", &[0, 0]),
                (1, "LOG1", &[0, 0, 0]),
                (1, "STOP", &[]),
            ]),
            // LOG1's topic is the loaded word.
            vec![1]
        );
    }

    #[test]
    fn an_overwritten_word_forgets_its_read() {
        assert_eq!(
            used(&[
                (1, "PUSH1", &[]),
                (1, "BALANCE", &[7]),
                (1, "PUSH1", &[0]),
                (1, "MSTORE", &[0x40, 0]),
                (1, "PUSH1", &[]),
                (1, "PUSH1", &[3]),
                (1, "MSTORE", &[0x40, 3]),
                (1, "PUSH1", &[]),
                (1, "MLOAD", &[0x40]),
                (1, "PUSH1", &[0]),
                (1, "SSTORE", &[0, 0]),
                (1, "STOP", &[]),
            ]),
            Vec::<usize>::new()
        );
    }

    /// `CALL(gas, to, 0, 0, 0, 0, 0x20)`, pushed then called.
    fn call_steps(depth: u64, op: &'static str) -> Vec<(u64, &'static str, &'static [u64])> {
        vec![
            (depth, "PUSH1", &[]),
            (depth, "PUSH1", &[0x20]),
            (depth, "PUSH1", &[0, 0x20]),
            (depth, "PUSH1", &[0, 0, 0x20]),
            (depth, "PUSH1", &[0, 0, 0, 0x20]),
            (depth, "PUSH1", &[0, 0, 0, 0, 0x20]),
            (depth, "GAS", &[0x1234, 0, 0, 0, 0, 0x20]),
            (depth, op, &[9, 0x1234, 0, 0, 0, 0, 0x20]),
        ]
    }

    /// `op(gas, to, 0, 0, 0, 0x20)` for a call without a value operand, pushed then called.
    fn call6_steps(depth: u64, op: &'static str) -> Vec<(u64, &'static str, &'static [u64])> {
        vec![
            (depth, "PUSH1", &[]),
            (depth, "PUSH1", &[0x20]),
            (depth, "PUSH1", &[0, 0x20]),
            (depth, "PUSH1", &[0, 0, 0x20]),
            (depth, "PUSH1", &[0, 0, 0, 0x20]),
            (depth, "GAS", &[0x1234, 0, 0, 0, 0x20]),
            (depth, op, &[9, 0x1234, 0, 0, 0, 0x20]),
        ]
    }

    /// A callee at `depth` that returns one stored word.
    fn returning(depth: u64) -> Vec<(u64, &'static str, &'static [u64])> {
        vec![
            (depth, "PUSH1", &[]),
            (depth, "PUSH1", &[0x2a]),
            (depth, "MSTORE", &[0, 0x2a]),
            (depth, "PUSH1", &[]),
            (depth, "PUSH1", &[0x20]),
            (depth, "RETURN", &[0, 0x20]),
        ]
    }

    #[test]
    fn a_calls_checked_success_leaves_it_unused() {
        let mut steps = call_steps(1, "CALL");
        steps.extend(returning(2));
        steps.extend([
            (1, "ISZERO", &[1][..]),
            (1, "PUSH1", &[0]),
            (1, "JUMPI", &[9, 0]),
            (1, "STOP", &[]),
        ]);
        assert_eq!(used(&steps), Vec::<usize>::new());
    }

    #[test]
    fn a_calls_stored_output_is_used() {
        let mut steps = call_steps(1, "CALL");
        steps.extend(returning(2));
        steps.extend([
            (1, "POP", &[1][..]),
            (1, "PUSH1", &[]),
            (1, "MLOAD", &[0]),
            (1, "PUSH1", &[0x2a]),
            (1, "SSTORE", &[0, 0x2a]),
            (1, "STOP", &[]),
        ]);
        assert_eq!(used(&steps), vec![7]);
    }

    #[test]
    fn returndatacopy_carries_the_calls_result() {
        let mut steps = call_steps(1, "CALL");
        steps.extend(returning(2));
        steps.extend([
            (1, "POP", &[1][..]),
            (1, "PUSH1", &[]),
            (1, "PUSH1", &[0x20]),
            (1, "PUSH1", &[0, 0x20]),
            (1, "RETURNDATACOPY", &[0x80, 0, 0x20]),
            (1, "PUSH1", &[]),
            (1, "PUSH1", &[0x20]),
            (1, "LOG0", &[0x80, 0x20]),
            (1, "STOP", &[]),
        ]);
        assert_eq!(used(&steps), vec![7]);
    }

    #[test]
    fn a_staticcalls_checked_success_uses_it() {
        let mut steps = call6_steps(1, "STATICCALL");
        steps.extend(returning(2));
        steps.extend([
            (1, "PUSH1", &[1][..]),
            (1, "JUMPI", &[9, 1]),
            (1, "STOP", &[]),
        ]);
        assert_eq!(used(&steps), vec![6]);
    }

    #[test]
    fn a_delegated_read_returned_and_stored_is_used() {
        let mut steps = call6_steps(1, "DELEGATECALL");
        // The library reads a balance and returns it.
        steps.extend([
            (2, "PUSH1", &[][..]),
            (2, "BALANCE", &[7]),
            (2, "PUSH1", &[0]),
            (2, "MSTORE", &[0, 0]),
            (2, "PUSH1", &[]),
            (2, "PUSH1", &[0x20]),
            (2, "RETURN", &[0, 0x20]),
            (1, "POP", &[1]),
            (1, "PUSH1", &[]),
            (1, "MLOAD", &[0]),
            (1, "PUSH1", &[0]),
            (1, "SSTORE", &[0, 0]),
            (1, "STOP", &[]),
        ]);
        assert_eq!(used(&steps), vec![8]);
    }

    #[test]
    fn a_precompiles_output_carries_its_input() {
        // MSTORE(0, BALANCE(7)); STATICCALL(gas, 2, 0, 0x20, 0x40, 0x20) opens no frame;
        // SSTORE(0, MLOAD(0x40)) stores the hash, which carries the read.
        let steps: Vec<(u64, &str, &[u64])> = vec![
            (1, "PUSH1", &[]),
            (1, "BALANCE", &[7]),
            (1, "PUSH1", &[0]),
            (1, "MSTORE", &[0, 0]),
            (1, "PUSH1", &[]),
            (1, "PUSH1", &[0x20]),
            (1, "PUSH1", &[0x40, 0x20]),
            (1, "PUSH1", &[0x20, 0x40, 0x20]),
            (1, "PUSH1", &[0, 0x20, 0x40, 0x20]),
            (1, "GAS", &[2, 0, 0x20, 0x40, 0x20]),
            (1, "STATICCALL", &[9, 2, 0, 0x20, 0x40, 0x20]),
            (1, "POP", &[1]),
            (1, "PUSH1", &[]),
            (1, "MLOAD", &[0x40]),
            (1, "PUSH1", &[0]),
            (1, "SSTORE", &[0, 0]),
            (1, "STOP", &[]),
        ];
        assert_eq!(used(&steps), vec![1, 10]);
    }

    #[test]
    fn a_stack_that_does_not_line_up_pins_everything() {
        let usage = Usage::of(&logs(&[(1, "PUSH1", &[]), (1, "POP", &[])]));
        assert!(!usage.followed());
        assert!(usage.used(0));
    }

    #[test]
    fn an_unknown_opcode_pins_everything() {
        let usage = Usage::of(&logs(&[(1, "EOFCREATE", &[])]));
        assert!(!usage.followed());
    }

    mod walks {
        use super::*;
        use crate::observe::result_hash;
        use crate::types::{IStateUpdateTypes, StateUpdate};
        use alloy_primitives::{Address, B256, address};
        use alloy_rpc_types::trace::geth::DefaultFrame;

        const TARGET: Address = address!("0x00000000000000000000000000000000000000aa");

        fn trace(steps: &[(u64, &str, &[u64])]) -> DefaultFrame {
            DefaultFrame {
                failed: false,
                gas: 0,
                return_value: Default::default(),
                struct_logs: logs(steps),
            }
        }

        fn journal(steps: &[(u64, &str, &[u64])]) -> Vec<StateUpdate> {
            crate::compute_state_updates(trace(steps), Some(TARGET))
                .expect("journal walk")
                .state_updates
        }

        fn canonical(steps: &[(u64, &str, &[u64])]) -> Vec<StateUpdate> {
            crate::compute_state_updates_canonical(trace(steps), TARGET)
                .expect("canonical walk")
                .0
        }

        fn both(steps: &[(u64, &str, &[u64])]) -> [Vec<StateUpdate>; 2] {
            [journal(steps), canonical(steps)]
        }

        fn checks(updates: &[StateUpdate]) -> usize {
            updates.iter().filter(|u| u.is_check()).count()
        }

        fn call_hash(updates: &[StateUpdate]) -> B256 {
            updates
                .iter()
                .find_map(|u| match u {
                    StateUpdate::Call(call) => Some(call.resultHash),
                    _ => None,
                })
                .expect("a CALL")
        }

        #[test]
        fn reads_nothing_depends_on_are_not_checked() {
            for updates in both(&[
                (1, "PUSH1", &[]),
                (1, "BALANCE", &[7]),
                (1, "POP", &[100]),
                (1, "PUSH1", &[]),
                (1, "EXTCODESIZE", &[7]),
                (1, "POP", &[3]),
                (1, "SELFBALANCE", &[]),
                (1, "POP", &[100]),
                (1, "STOP", &[]),
            ]) {
                assert_eq!(checks(&updates), 0, "{updates:?}");
            }
        }

        #[test]
        fn a_repeated_read_is_checked_once() {
            // SSTORE(0, SELFBALANCE); SSTORE(1, SELFBALANCE): a write to the consumer's storage
            // can't move its balance.
            for updates in both(&[
                (1, "SELFBALANCE", &[]),
                (1, "PUSH1", &[100]),
                (1, "SSTORE", &[0, 100]),
                (1, "SELFBALANCE", &[]),
                (1, "PUSH1", &[100]),
                (1, "SSTORE", &[1, 100]),
                (1, "STOP", &[]),
            ]) {
                assert_eq!(checks(&updates), 1, "{updates:?}");
                assert!(matches!(
                    &updates[0],
                    StateUpdate::BalanceCheck(IStateUpdateTypes::BalanceCheck { account, .. })
                        if *account == TARGET
                ));
            }
        }

        #[test]
        fn a_read_after_a_call_is_checked_again() {
            let mut steps: Vec<(u64, &str, &[u64])> = vec![
                (1, "SELFBALANCE", &[]),
                (1, "PUSH1", &[100]),
                (1, "SSTORE", &[0, 100]),
            ];
            steps.extend(call_steps(1, "CALL"));
            steps.extend(returning(2));
            steps.extend([
                (1, "POP", &[1][..]),
                (1, "SELFBALANCE", &[]),
                (1, "PUSH1", &[100]),
                (1, "SSTORE", &[1, 100]),
                (1, "STOP", &[]),
            ]);
            for updates in both(&steps) {
                assert_eq!(checks(&updates), 2, "{updates:?}");
            }
        }

        #[test]
        fn a_call_whose_output_is_unused_requires_only_success() {
            let mut steps = call_steps(1, "CALL");
            steps.extend(returning(2));
            steps.extend([
                (1, "ISZERO", &[1][..]),
                (1, "PUSH1", &[0]),
                (1, "JUMPI", &[9, 0]),
                (1, "STOP", &[]),
            ]);
            for updates in both(&steps) {
                assert_eq!(call_hash(&updates), B256::ZERO, "{updates:?}");
            }
        }

        #[test]
        fn a_call_whose_output_is_stored_is_pinned() {
            let mut steps = call_steps(1, "CALL");
            steps.extend(returning(2));
            steps.extend([
                (1, "POP", &[1][..]),
                (1, "PUSH1", &[]),
                (1, "MLOAD", &[0]),
                (1, "PUSH1", &[0x2a]),
                (1, "SSTORE", &[0, 0x2a]),
                (1, "STOP", &[]),
            ]);
            // The test logs carry no memory, so the callee returned zeros.
            let pinned = result_hash(true, &[0u8; 32]);
            for updates in both(&steps) {
                assert_eq!(call_hash(&updates), pinned, "{updates:?}");
            }
        }

        #[test]
        fn a_caught_revert_is_pinned_even_unused() {
            let mut steps = call_steps(1, "CALL");
            steps.extend(returning(2));
            let last = steps.len() - 1;
            steps[last].1 = "REVERT";
            steps.extend([(1, "POP", &[0][..]), (1, "STOP", &[])]);
            let pinned = result_hash(false, &[0u8; 32]);
            for updates in both(&steps) {
                assert_eq!(call_hash(&updates), pinned, "{updates:?}");
            }
        }

        /// The callee only writes memory and returns, which is stateless: unused, it pins
        /// nothing, not even its code.
        #[test]
        fn a_staticcall_whose_result_is_dropped_is_not_checked() {
            let mut steps = call6_steps(1, "STATICCALL");
            steps.extend(returning(2));
            steps.extend([(1, "POP", &[1][..]), (1, "STOP", &[])]);
            for updates in both(&steps) {
                assert_eq!(checks(&updates), 0, "{updates:?}");
            }
        }

        /// The same stateless read, branched on: its check, with its callee's code marked to pin
        /// it instead once a caller fetches the code.
        #[test]
        fn a_used_stateless_staticcall_is_pinned_by_its_code() {
            let mut steps = call6_steps(1, "STATICCALL");
            steps.extend(returning(2));
            steps.extend([
                (1, "PUSH1", &[1][..]),
                (1, "JUMPI", &[9, 1]),
                (1, "STOP", &[]),
            ]);
            let callee = Address::left_padding_from(&[0x12, 0x34]);
            for updates in both(&steps) {
                assert!(
                    matches!(
                        updates.as_slice(),
                        [StateUpdate::StaticCallCheck(read), StateUpdate::CodehashCheck(c)]
                            if read.target == callee && c.account == callee
                    ),
                    "the check, and the marker of the code that may pin it instead: {updates:?}"
                );
            }
        }
    }
}

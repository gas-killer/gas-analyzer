//! Differential-equality harness: **interpreted revm** vs **revmc-compiled
//! (JIT)**, on synthetic bytecode fixtures modelled on the on-chain-LLM engine
//! workload (tight packed-integer MLOAD/MSTORE/arith loops, EXTCODECOPY of a
//! ~24 KiB "weight" chunk, a REVERT path, and a near-gas-cap / OOG case).
//!
//! The consensus requirement for wiring revmc into the analyzer's local
//! executor is *EVM-exactness*: the compiled path must produce **byte-identical
//! returndata, identical `gas_used`, and identical success/revert/halt
//! classification** to the interpreter. This crate exercises exactly that
//! property, running each fixture through BOTH paths and letting the caller
//! (see `tests/differential.rs`) assert equality.
//!
//! Both paths share one code path for everything except the frame executor:
//! [`run_interpreted`] runs a plain `MainnetEvm`, [`run_compiled`] wraps the
//! *same* context in [`revmc_context::JitEvm`] with a codehash -> compiled-fn
//! map for the fixture's bytecode (interpreter fallback for every other
//! codehash — precisely the wiring intended for `call_view_local_blocking`).

// Force-link the builtin symbols the JIT-compiled bytecode calls into
// (EXTCODECOPY, KECCAK256, etc. lower to calls into revmc_builtins).
use revmc_builtins as _;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use revm_bytecode::Bytecode;
use revm_context::{BlockEnv, CfgEnv, Context, Journal, TxEnv};
use revm_context_interface::result::{ExecutionResult, ResultGas};
use revm_database::{CacheDB, EmptyDB};
use revm_database_interface::Database;
use revm_handler::{ExecuteEvm, MainBuilder};
use revm_primitives::{
    Address, Bytes, TxKind, U256, address, hardfork::SpecId, keccak256, map::B256Map,
};
use revm_state::AccountInfo;
use revmc::EvmCompiler;
use revmc_context::JitEvm;

/// Hardfork the fixtures execute under. CANCUN predates EIP-7825's per-tx gas
/// cap, so the big compute fixtures can carry a multi-Ggas gas limit.
pub const SPEC: SpecId = SpecId::CANCUN;

/// Contract whose bytecode is the fixture under test (and the one JIT-compiled).
pub const TARGET: Address = address!("0000000000000000000000000000000000001234");
/// Contract holding the ~24 KiB "weight" blob EXTCODECOPY reads.
pub const WEIGHTS: Address = address!("00000000000000000000000000000000000000cc");
/// Transaction sender.
pub const CALLER: Address = address!("1000000000000000000000000000000000000001");

/// Mainnet context with an in-memory DB — the exact shape revmc's `JitEvm`
/// wraps, so `MainnetEvm` and `JitEvm<MainnetEvm<_>>` unify.
pub type MainnetContext<DB> = Context<BlockEnv, TxEnv, CfgEnv, DB, Journal<DB>, ()>;

// ============================================================================
// Comparable outcome
// ============================================================================

/// The three things operators would sign over, normalized for comparison.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Outcome {
    /// `"success"`, `"revert"`, or `"halt:<HaltReason debug>"`.
    pub class: String,
    /// The FULL gas accounting (EIP-8037 split: total/state/refund/floor), so
    /// equality here is stricter than just the receipt's `tx_gas_used`.
    pub gas: ResultGas,
    /// Returned / reverted data (empty for a halt).
    pub output: Vec<u8>,
}

impl Outcome {
    /// Receipt gas (the value that would go into a receipt / be signed).
    pub fn tx_gas_used(&self) -> u64 {
        self.gas.tx_gas_used()
    }
}

fn classify(r: &ExecutionResult) -> Outcome {
    let (class, gas, output) = match r {
        ExecutionResult::Success { gas, output, .. } => {
            ("success".to_string(), *gas, output.data().to_vec())
        }
        ExecutionResult::Revert { gas, output, .. } => ("revert".to_string(), *gas, output.to_vec()),
        ExecutionResult::Halt { gas, reason, .. } => {
            (format!("halt:{reason:?}"), *gas, Vec::new())
        }
    };
    Outcome { class, gas, output }
}

// ============================================================================
// Fixtures
// ============================================================================

/// A synthetic call: bytecode at [`TARGET`], an optional weight blob at
/// [`WEIGHTS`], calldata, and a gas limit.
#[derive(Clone)]
pub struct Fixture {
    pub name: &'static str,
    pub code: Vec<u8>,
    pub weights: Option<Vec<u8>>,
    pub calldata: Vec<u8>,
    pub gas: u64,
}

/// Minimal two-pass EVM assembler with labels, so jump destinations are never
/// hand-counted.
#[derive(Default)]
struct Asm {
    code: Vec<u8>,
    labels: HashMap<&'static str, usize>,
    fixups: Vec<(usize, &'static str)>,
}

impl Asm {
    fn op(&mut self, b: u8) -> &mut Self {
        self.code.push(b);
        self
    }
    fn push0(&mut self) -> &mut Self {
        self.op(0x5f)
    }
    fn push1(&mut self, x: u8) -> &mut Self {
        self.op(0x60);
        self.code.push(x);
        self
    }
    /// PUSH<n> of `bytes` (1..=32). Opcode 0x5f + n (PUSH1 = 0x60).
    fn push_bytes(&mut self, bytes: &[u8]) -> &mut Self {
        assert!(!bytes.is_empty() && bytes.len() <= 32);
        self.op(0x5f + bytes.len() as u8);
        self.code.extend_from_slice(bytes);
        self
    }
    /// A jump target: emits JUMPDEST and records its PC.
    fn label(&mut self, name: &'static str) -> &mut Self {
        self.op(0x5b);
        self.labels.insert(name, self.code.len() - 1);
        self
    }
    /// PUSH2 <pc of label> (resolved in [`Self::finish`]).
    fn push_label(&mut self, name: &'static str) -> &mut Self {
        self.op(0x61);
        let pos = self.code.len();
        self.code.extend_from_slice(&[0, 0]);
        self.fixups.push((pos, name));
        self
    }
    fn jump_to(&mut self, name: &'static str) -> &mut Self {
        self.push_label(name).op(0x56)
    }
    /// Expects the branch condition already on the stack.
    fn jumpi_to(&mut self, name: &'static str) -> &mut Self {
        self.push_label(name).op(0x57)
    }
    fn finish(mut self) -> Vec<u8> {
        for (pos, name) in &self.fixups {
            let addr = *self.labels.get(name).expect("undefined label") as u16;
            let be = addr.to_be_bytes();
            self.code[*pos] = be[0];
            self.code[*pos + 1] = be[1];
        }
        self.code
    }
}

// opcode aliases used below
const CALLDATALOAD: u8 = 0x35;
const MLOAD: u8 = 0x51;
const MSTORE: u8 = 0x52;
const ADD: u8 = 0x01;
const MUL: u8 = 0x02;
const SUB: u8 = 0x03;
const AND: u8 = 0x16;
const ISZERO: u8 = 0x15;
const DUP1: u8 = 0x80;
const SWAP1: u8 = 0x90;
const POP: u8 = 0x50;
const RETURN: u8 = 0xf3;
const REVERT: u8 = 0xfd;
const EXTCODECOPY: u8 = 0x3c;
const SHA3: u8 = 0x20;

/// Compute-heavy fixture: `iters` iterations of a packed-int32 memory-arith
/// loop — `acc = ((acc*3 + 7)^2) & 0xffffffff`, kept in `mem[0]`, returned as
/// 32 bytes. Models the engine's tight integer matmul inner loop.
pub fn matmul(iters: u64, gas: u64) -> Fixture {
    let mut a = Asm::default();
    a.push0().op(CALLDATALOAD); // n = calldata[0..32]
    a.label("loop");
    a.op(DUP1).op(ISZERO); // n == 0 ?
    a.jumpi_to("end");
    a.push0().op(MLOAD); // acc = mem[0]
    a.push1(3).op(MUL); // acc *= 3
    a.push1(7).op(ADD); // acc += 7
    a.op(DUP1).op(MUL); // acc = acc^2
    a.push_bytes(&[0xff, 0xff, 0xff, 0xff]).op(AND); // acc &= 0xffffffff
    a.push0().op(MSTORE); // mem[0] = acc
    a.push1(1).op(SWAP1).op(SUB); // n -= 1
    a.jump_to("loop");
    a.label("end");
    a.op(POP);
    a.push1(0x20).push0().op(RETURN); // return mem[0..32]
    let mut cd = vec![0u8; 32];
    cd[24..32].copy_from_slice(&iters.to_be_bytes());
    Fixture { name: "matmul", code: a.finish(), weights: None, calldata: cd, gas }
}

/// Deterministic ~24575-byte "weight" blob (one under the EIP-170 cap, matching
/// the engine's 24575-byte weight chunks).
fn weight_blob() -> Vec<u8> {
    let mut v = vec![0u8; 24575];
    // simple LCG so both paths see identical bytes
    let mut s: u32 = 0x1234_5678;
    for b in v.iter_mut() {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *b = (s >> 24) as u8;
    }
    v
}

/// EXTCODECOPY the full 24575-byte weight blob into memory, KECCAK256 it, and
/// return the 32-byte digest. Exercises the external-code + large-memory path.
pub fn extcodecopy() -> Fixture {
    let w = weight_blob();
    let len = w.len() as u16; // 24575 = 0x5fff
    let mut a = Asm::default();
    // EXTCODECOPY(addr, destOffset, offset, size): push size, offset, dest, addr
    a.push_bytes(&len.to_be_bytes()); // size
    a.push0(); // offset
    a.push0(); // destOffset
    a.push_bytes(WEIGHTS.as_slice()); // addr (PUSH20)
    a.op(EXTCODECOPY);
    // KECCAK256(offset, length): push length, then offset
    a.push_bytes(&len.to_be_bytes()); // length
    a.push0(); // offset
    a.op(SHA3); // -> digest
    a.push0().op(MSTORE); // mem[0] = digest
    a.push1(0x20).push0().op(RETURN);
    Fixture {
        name: "extcodecopy",
        code: a.finish(),
        weights: Some(w),
        calldata: Vec::new(),
        gas: 50_000_000,
    }
}

/// Workload-representative fixture: `iters` iterations of {EXTCODECOPY the full
/// 24575-byte weight blob into memory, then fold one word of it into a
/// packed-int32 accumulator}. Unlike [`matmul`], the per-iteration EXTCODECOPY
/// is a host builtin with memory side effects that the compiler cannot hoist,
/// so this is memory-bandwidth / host-call bound — much closer to the engine's
/// real "EXTCODECOPY weight chunk + integer matmul" inner loop than the
/// register-only [`matmul`] microbenchmark.
pub fn extcodecopy_loop(iters: u64, gas: u64) -> Fixture {
    let w = weight_blob();
    let len = w.len() as u16;
    const ACC: u16 = 0x8000; // accumulator slot, past the 24575-byte copy region
    let mut a = Asm::default();
    a.push0().op(CALLDATALOAD); // n
    a.label("loop");
    a.op(DUP1).op(ISZERO);
    a.jumpi_to("end");
    // EXTCODECOPY(WEIGHTS, dest=0, off=0, size=len)
    a.push_bytes(&len.to_be_bytes());
    a.push0();
    a.push0();
    a.push_bytes(WEIGHTS.as_slice());
    a.op(EXTCODECOPY);
    // acc = (acc + mem[0]) & 0xffffffff
    a.push_bytes(&ACC.to_be_bytes()).op(MLOAD); // acc
    a.push0().op(MLOAD).op(ADD); // + mem[0]
    a.push_bytes(&[0xff, 0xff, 0xff, 0xff]).op(AND);
    a.push_bytes(&ACC.to_be_bytes()).op(MSTORE);
    a.push1(1).op(SWAP1).op(SUB); // n -= 1
    a.jump_to("loop");
    a.label("end");
    a.op(POP);
    a.push_bytes(&ACC.to_be_bytes()).op(MLOAD).push0().op(MSTORE); // mem[0] = acc
    a.push1(0x20).push0().op(RETURN);
    let mut cd = vec![0u8; 32];
    cd[24..32].copy_from_slice(&iters.to_be_bytes());
    Fixture { name: "extcodecopy_loop", code: a.finish(), weights: Some(w), calldata: cd, gas }
}

/// Writes `0xAA` to `mem[0]` then REVERTs with `mem[0..32]`.
pub fn revert_fixture() -> Fixture {
    let mut a = Asm::default();
    a.push1(0xAA).push0().op(MSTORE); // mem[0] = 0x..AA
    a.push1(0x20).push0().op(REVERT); // revert mem[0..32]
    Fixture { name: "revert", code: a.finish(), weights: None, calldata: Vec::new(), gas: 1_000_000 }
}

/// Unbounded loop that burns gas forever — hits the gas cap and halts.
pub fn oog_fixture() -> Fixture {
    let mut a = Asm::default();
    a.label("loop");
    a.push0().op(MLOAD).push1(1).op(ADD).push0().op(MSTORE);
    a.jump_to("loop");
    Fixture { name: "oog", code: a.finish(), weights: None, calldata: Vec::new(), gas: 100_000 }
}

// ============================================================================
// Shared EVM setup
// ============================================================================

fn seed_db(fx: &Fixture) -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::new());
    db.insert_account_info(
        TARGET,
        AccountInfo {
            code_hash: keccak256(&fx.code),
            code: Some(Bytecode::new_legacy(Bytes::from(fx.code.clone()))),
            ..Default::default()
        },
    );
    if let Some(w) = &fx.weights {
        db.insert_account_info(
            WEIGHTS,
            AccountInfo {
                code_hash: keccak256(w),
                code: Some(Bytecode::new_legacy(Bytes::from(w.clone()))),
                ..Default::default()
            },
        );
    }
    db.insert_account_info(
        CALLER,
        AccountInfo { balance: U256::from(1u64) << 200, ..Default::default() },
    );
    db
}

fn build_ctx<DB: Database>(db: DB) -> MainnetContext<DB> {
    // No fee/balance/nonce enforcement needed: gas_price = 0, basefee = 0,
    // caller funded + nonce 0, and block gas_limit = u64::MAX (so tx gas can be
    // multi-Ggas without tripping the block cap). This avoids the feature-gated
    // `disable_*` cfg flags entirely while keeping both paths identical.
    Context::new(db, SPEC).modify_block_chained(|b: &mut BlockEnv| {
        b.gas_limit = u64::MAX;
        b.basefee = 0;
    })
}

fn build_tx(fx: &Fixture) -> TxEnv {
    TxEnv::builder()
        .caller(CALLER)
        .gas_limit(fx.gas)
        .gas_price(0)
        .kind(TxKind::Call(TARGET))
        .data(Bytes::from(fx.calldata.clone()))
        .build()
        .expect("build tx")
}

// ============================================================================
// The two execution paths
// ============================================================================

/// Reference path: plain interpreted `MainnetEvm`.
pub fn run_interpreted(fx: &Fixture) -> Outcome {
    let mut evm = build_ctx(seed_db(fx)).build_mainnet();
    let r = evm.transact(build_tx(fx)).expect("interpreted transact");
    classify(&r.result)
}

/// Compiled path: the same context wrapped in `JitEvm`, with the fixture's
/// bytecode JIT-compiled and dispatched by codehash (interpreter fallback for
/// any other code).
pub fn run_compiled(fx: &Fixture) -> Outcome {
    // `compiler` owns the JIT-compiled code; it must outlive every call.
    let mut compiler = EvmCompiler::new_llvm(false).expect("EvmCompiler::new_llvm");
    let f = unsafe { compiler.jit(fx.name, &fx.code[..], SPEC) }.expect("jit-compile fixture");
    let mut functions = B256Map::default();
    functions.insert(keccak256(&fx.code), f.into_inner());

    let inner = build_ctx(seed_db(fx)).build_mainnet();
    let mut evm = JitEvm::new(inner, functions);
    let r = evm.transact(build_tx(fx)).expect("compiled transact");
    let out = classify(&r.result);
    drop(evm); // release borrow of compiled fn before compiler drops
    out
}

// ============================================================================
// Execution-only timing (JIT compiled ONCE, outside the timed region)
// ============================================================================

/// Time a single interpreted execution.
pub fn bench_interpreted(fx: &Fixture) -> (Outcome, Duration) {
    let t0 = Instant::now();
    let out = run_interpreted(fx);
    (out, t0.elapsed())
}

/// JIT-compile once, then time a single compiled execution (optionally after a
/// warmup run). Compile time is deliberately excluded — production AOT-links
/// the artifact, so steady-state execution is the number that matters.
pub fn bench_compiled(fx: &Fixture, warmup: bool) -> (Outcome, Duration) {
    let mut compiler = EvmCompiler::new_llvm(false).expect("EvmCompiler::new_llvm");
    let f = unsafe { compiler.jit(fx.name, &fx.code[..], SPEC) }.expect("jit-compile fixture");
    let mut functions = B256Map::default();
    functions.insert(keccak256(&fx.code), f.into_inner());

    let run = || {
        let inner = build_ctx(seed_db(fx)).build_mainnet();
        let mut evm = JitEvm::new(inner, functions.clone());
        let r = evm.transact(build_tx(fx)).expect("compiled transact");
        classify(&r.result)
    };
    if warmup {
        let _ = run();
    }
    let t0 = Instant::now();
    let out = run();
    (out, t0.elapsed())
}

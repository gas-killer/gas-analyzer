//! Targeted opcode differential: revm-41 interpreter vs revmc-JIT.
//!
//! The seg-engine does signed Q24 fixed-point arithmetic — `SAR`, `SIGNEXTEND`,
//! `SDIV`, `SMOD`, `SGT`/`SLT`, `MULMOD` — on NEGATIVE int256 values. The
//! synthetic consensus fixtures only exercised UNSIGNED ops (`MUL/ADD/AND/SUB`),
//! so a revmc codegen bug on a signed op would pass every fixture yet diverge on
//! the real engine. This test isolates exactly that: for each suspect opcode it
//! runs adversarial (sign-bit-set) operands through BOTH paths and asserts the
//! returndata is byte-identical.

use gk_fast_view::{FastView, Profile, ViewEnv, ViewTx};
use revm_database::{CacheDB, EmptyDB};
use revm_primitives::{Address, U256, address, hardfork::SpecId, keccak256};
use revm_state::{AccountInfo, Bytecode};

const ENGINE: Address = address!("0x00000000000000000000000000000000000000ab");
const CALLER: Address = address!("0x1000000000000000000000000000000000000001");

/// Bytecode: PUSH32 lo; PUSH32 hi; <op>; PUSH0 MSTORE; PUSH1 0x20 PUSH0 RETURN.
/// After the two pushes the stack top is `hi` (pushed last), so a binary opcode
/// sees (top=hi, next=lo) — e.g. `SAR` computes `lo >> hi`, `SIGNEXTEND`
/// computes `signextend(hi, lo)`.
fn snippet(op: u8, lo: U256, hi: U256) -> Vec<u8> {
    let mut c = vec![0x7f]; // PUSH32
    c.extend_from_slice(&lo.to_be_bytes::<32>());
    c.push(0x7f); // PUSH32
    c.extend_from_slice(&hi.to_be_bytes::<32>());
    c.push(op);
    c.push(0x5f); // PUSH0
    c.push(0x52); // MSTORE
    c.push(0x60); // PUSH1
    c.push(0x20);
    c.push(0x5f); // PUSH0
    c.push(0xf3); // RETURN
    c
}

/// Ternary (MULMOD/ADDMOD): PUSH32 m; PUSH32 b; PUSH32 a; <op>. Stack top = a.
fn snippet3(op: u8, a: U256, b: U256, m: U256) -> Vec<u8> {
    let mut c = Vec::new();
    for v in [m, b, a] {
        c.push(0x7f);
        c.extend_from_slice(&v.to_be_bytes::<32>());
    }
    c.push(op);
    c.push(0x5f);
    c.push(0x52);
    c.push(0x60);
    c.push(0x20);
    c.push(0x5f);
    c.push(0xf3);
    c
}

fn run_both(code: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut base = CacheDB::new(EmptyDB::new());
    base.insert_account_info(
        ENGINE,
        AccountInfo {
            balance: U256::from(1u64) << 200,
            code_hash: keccak256(code),
            code: Some(Bytecode::new_raw(code.to_vec().into())),
            ..Default::default()
        },
    );
    base.insert_account_info(
        CALLER,
        AccountInfo {
            balance: U256::from(1u64) << 200,
            ..Default::default()
        },
    );
    let env = ViewEnv {
        spec: SpecId::CANCUN,
        ..ViewEnv::default()
    };
    let tx = ViewTx::call(CALLER, ENGINE, code.to_vec(), 5_000_000);
    let mut fv = FastView::new(SpecId::CANCUN).expect("FastView");
    let interp = fv
        .call_view_interpreted(&base, Default::default(), &env, &tx, Profile::UnboundedV1)
        .expect("interp");
    let jit = fv
        .call_view(&base, Default::default(), &env, &tx, Profile::UnboundedV1)
        .expect("jit");
    (interp.to_vec(), jit.to_vec())
}

/// Adversarial operand corpus: sign-bit-set, min-int, small negatives, mixed.
fn corpus() -> Vec<U256> {
    let min_int = U256::from(1u64) << 255; // int256::MIN
    let neg1 = U256::MAX; // -1
    let neg_small = U256::MAX - U256::from(41u64); // -42
    vec![
        U256::ZERO,
        U256::from(1u64),
        U256::from(16u64),
        U256::from(32u64),
        U256::from(255u64),
        U256::from(0x1234_5678u64),
        min_int,
        neg1,
        neg_small,
        min_int | U256::from(0xABCDu64),
        // a big negative Q24-ish accumulator like `acc` in _attendHead
        U256::MAX - U256::from(0x00ff_ffff_ffff_ffffu64),
    ]
}

fn assert_op_binary(name: &str, op: u8) {
    for &lo in &corpus() {
        for &hi in &corpus() {
            let code = snippet(op, lo, hi);
            let (i, j) = run_both(&code);
            assert_eq!(
                i, j,
                "[{name}] revmc != interpreter for lo=0x{lo:x} hi=0x{hi:x}\n interp=0x{}\n jit   =0x{}",
                hex::encode(&i),
                hex::encode(&j)
            );
        }
    }
    eprintln!("[opcode OK] {name}: revmc == interpreter across adversarial corpus");
}

#[test]
fn sar_matches() {
    assert_op_binary("SAR", 0x1d);
}
#[test]
fn signextend_matches() {
    assert_op_binary("SIGNEXTEND", 0x0b);
}
#[test]
fn sdiv_matches() {
    assert_op_binary("SDIV", 0x05);
}
#[test]
fn smod_matches() {
    assert_op_binary("SMOD", 0x07);
}
#[test]
fn sgt_matches() {
    assert_op_binary("SGT", 0x13);
}
#[test]
fn slt_matches() {
    assert_op_binary("SLT", 0x12);
}
#[test]
fn sar_shl_shr_matches() {
    assert_op_binary("SHR", 0x1c);
    assert_op_binary("SHL", 0x1b);
}

#[test]
fn mulmod_addmod_matches() {
    let c = corpus();
    for &a in &c {
        for &b in &c {
            for &m in &[U256::from(0u64), U256::from(7u64), U256::from(1u64) << 200, U256::MAX] {
                for op in [0x09u8 /*MULMOD*/, 0x08 /*ADDMOD*/] {
                    let code = snippet3(op, a, b, m);
                    let (i, j) = run_both(&code);
                    assert_eq!(
                        i, j,
                        "[MULMOD/ADDMOD op=0x{op:x}] revmc != interpreter a=0x{a:x} b=0x{b:x} m=0x{m:x}"
                    );
                }
            }
        }
    }
    eprintln!("[opcode OK] MULMOD/ADDMOD: revmc == interpreter");
}

//! UNBOUNDED_V3 M3 entry criterion (design-doc risk 8).
//!
//! Before the gkvm precompile provider is built, prove against the PINNED
//! revm (31.0.2) that `PrecompileProvider::run`:
//!
//! 1. sees whether the frame is static (`CallInputs::is_static`), including
//!    inheritance through a nested CALL inside a STATICCALL;
//! 2. can return a REVERT that carries returndata, and that the caller
//!    observes those exact bytes (typed `GkGuest*` errors depend on it);
//! 3. charges exactly the gas the provider records on the revert path and
//!    refunds the rest to the caller;
//! 4. falls through to the stock `EthPrecompiles` for every other address;
//! 5. behaves identically on the inspector build (`inspect_tx`), which is the
//!    only build `local_exec::run_pass` uses.
//!
//! The probe provider here is NOT the gkvm provider — it is the smallest
//! thing that exercises the surface M3 needs.

use alloy::primitives::{Address, Bytes, TxKind, address};
use revm::context::result::{ExecutionResult, Output};
use revm::context::{Cfg, Context, ContextTr, LocalContextTr, TxEnv};
use revm::database::{CacheDB, EmptyDB};
use revm::handler::{EthPrecompiles, PrecompileProvider};
use revm::inspector::NoOpInspector;
use revm::interpreter::{CallInput, CallInputs, Gas, InstructionResult, InterpreterResult};
use revm::primitives::hardfork::SpecId;
use revm::state::{AccountInfo, Bytecode};
use revm::{ExecuteEvm, InspectEvm, MainBuilder, MainContext};

const PROBE: Address = address!("0x00000000000000000000000000000000000c0de5");
const STATIC_FWD: Address = address!("0x1000000000000000000000000000000000000001");
const CALL_FWD: Address = address!("0x1000000000000000000000000000000000000002");
const STATIC_THEN_CALL: Address = address!("0x1000000000000000000000000000000000000003");
const EOA: Address = address!("0x2000000000000000000000000000000000000001");

/// Revert payload prefixes, so the test can tell the two revert reasons apart.
const NOT_STATIC: &[u8] = b"probe:not-static:";
const REVERT_ECHO: &[u8] = b"probe:revert:";

/// Wraps `EthPrecompiles`, intercepting `PROBE`:
/// - non-static frame            → REVERT(`NOT_STATIC || input`)
/// - static, input[0] == 0x00    → REVERT(`REVERT_ECHO || input`)
/// - static, otherwise           → RETURN(`0x01 || input`)
///
/// Every intercepted path records `charge` gas.
struct ProbePrecompiles {
    inner: EthPrecompiles,
    charge: u64,
}

impl<CTX> PrecompileProvider<CTX> for ProbePrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = SpecId>>,
{
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: SpecId) -> bool {
        <EthPrecompiles as PrecompileProvider<CTX>>::set_spec(&mut self.inner, spec)
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        if inputs.bytecode_address != PROBE {
            return self.inner.run(context, inputs);
        }

        let input: Vec<u8> = match &inputs.input {
            CallInput::Bytes(bytes) => bytes.to_vec(),
            CallInput::SharedBuffer(range) => context
                .local()
                .shared_memory_buffer_slice(range.clone())
                .map(|slice| slice.to_vec())
                .unwrap_or_default(),
        };

        let mut gas = Gas::new(inputs.gas_limit);
        if !gas.record_cost(self.charge) {
            return Ok(Some(InterpreterResult {
                result: InstructionResult::PrecompileOOG,
                gas,
                output: Bytes::new(),
            }));
        }

        let (result, output) = if !inputs.is_static {
            (InstructionResult::Revert, [NOT_STATIC, &input].concat())
        } else if input.first() == Some(&0) {
            (InstructionResult::Revert, [REVERT_ECHO, &input].concat())
        } else {
            (InstructionResult::Return, [&[1u8][..], &input].concat())
        };

        Ok(Some(InterpreterResult {
            result,
            gas,
            output: output.into(),
        }))
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        let inner = <EthPrecompiles as PrecompileProvider<CTX>>::warm_addresses(&self.inner);
        Box::new(inner.chain(core::iter::once(PROBE)))
    }

    fn contains(&self, address: &Address) -> bool {
        *address == PROBE
            || <EthPrecompiles as PrecompileProvider<CTX>>::contains(&self.inner, address)
    }
}

/// Forwarder: passes calldata to `target` with all gas (STATICCALL or CALL),
/// then returns the returndata on success / reverts with it on failure.
fn forwarder(target: Address, static_call: bool) -> Bytecode {
    let mut code = vec![
        0x36, 0x5f, 0x5f, 0x37, // CALLDATASIZE PUSH0 PUSH0 CALLDATACOPY
        0x5f, 0x5f, 0x36, 0x5f, // retSize retOffset argsSize argsOffset
    ];
    if !static_call {
        code.push(0x5f); // value
    }
    code.push(0x73); // PUSH20 target
    code.extend_from_slice(target.as_slice());
    code.push(0x5a); // GAS
    code.push(if static_call { 0xfa } else { 0xf1 });
    code.extend_from_slice(&[0x3d, 0x5f, 0x5f, 0x3e]); // RETURNDATACOPY(0, 0, size)
    let jumpdest = (code.len() + 3 + 3) as u8;
    code.extend_from_slice(&[0x60, jumpdest, 0x57]); // PUSH1 dest JUMPI
    code.extend_from_slice(&[0x3d, 0x5f, 0xfd]); // REVERT(0, size)
    code.extend_from_slice(&[0x5b, 0x3d, 0x5f, 0xf3]); // JUMPDEST RETURN(0, size)
    Bytecode::new_raw(code.into())
}

fn seeded_db() -> CacheDB<EmptyDB> {
    let mut db = CacheDB::new(EmptyDB::default());
    for (addr, code) in [
        (STATIC_FWD, forwarder(PROBE, true)),
        (CALL_FWD, forwarder(PROBE, false)),
        (STATIC_THEN_CALL, forwarder(CALL_FWD, true)),
    ] {
        db.insert_account_info(addr, AccountInfo::from_bytecode(code));
    }
    db
}

fn tx(to: Address, data: &[u8]) -> TxEnv {
    TxEnv::builder()
        .caller(EOA)
        .kind(TxKind::Call(to))
        .data(Bytes::copy_from_slice(data))
        .gas_limit(1_000_000)
        .build()
        .expect("tx env")
}

#[derive(Clone, Copy, Debug)]
enum Build {
    Plain,
    Inspector,
}

fn exec(build: Build, charge: u64, to: Address, data: &[u8]) -> ExecutionResult {
    let ctx = Context::mainnet()
        .with_db(seeded_db())
        .modify_cfg_chained(|cfg| {
            cfg.disable_nonce_check = true;
            cfg.disable_balance_check = true;
            cfg.disable_base_fee = true;
        });
    let provider = ProbePrecompiles {
        inner: EthPrecompiles::default(),
        charge,
    };
    match build {
        Build::Plain => {
            let mut evm = ctx.build_mainnet().with_precompiles(provider);
            evm.transact(tx(to, data)).expect("transact").result
        }
        Build::Inspector => {
            let mut evm = ctx
                .build_mainnet_with_inspector(NoOpInspector)
                .with_precompiles(provider);
            evm.inspect_tx(tx(to, data)).expect("inspect_tx").result
        }
    }
}

fn expect_revert(result: &ExecutionResult) -> &[u8] {
    match result {
        ExecutionResult::Revert { output, .. } => output,
        other => panic!("expected revert, got {other:?}"),
    }
}

fn expect_return(result: &ExecutionResult) -> &[u8] {
    match result {
        ExecutionResult::Success {
            output: Output::Call(bytes),
            ..
        } => bytes,
        other => panic!("expected success, got {other:?}"),
    }
}

const BUILDS: [Build; 2] = [Build::Plain, Build::Inspector];

#[test]
fn static_frame_is_visible_and_ok_returndata_round_trips() {
    for build in BUILDS {
        let result = exec(build, 0, STATIC_FWD, &[0xaa, 0xbb]);
        assert_eq!(expect_return(&result), [0x01, 0xaa, 0xbb], "{build:?}");
    }
}

#[test]
fn revert_with_returndata_reaches_the_caller_byte_exact() {
    for build in BUILDS {
        let result = exec(build, 0, STATIC_FWD, &[0x00, 0xde, 0xad]);
        assert_eq!(
            expect_revert(&result),
            [REVERT_ECHO, &[0x00, 0xde, 0xad]].concat(),
            "{build:?}"
        );
    }
}

#[test]
fn non_static_frames_are_distinguishable() {
    for build in BUILDS {
        // CALL from a contract.
        let result = exec(build, 0, CALL_FWD, &[0xaa]);
        assert_eq!(
            expect_revert(&result),
            [NOT_STATIC, &[0xaa]].concat(),
            "{build:?}"
        );
        // Top-level transaction straight at the precompile address.
        let result = exec(build, 0, PROBE, &[0xaa]);
        assert_eq!(
            expect_revert(&result),
            [NOT_STATIC, &[0xaa]].concat(),
            "{build:?}"
        );
    }
}

#[test]
fn static_context_is_inherited_through_a_nested_call() {
    // STATICCALL → forwarder → CALL → probe: the inner frame is a CALL opcode
    // but executes in a static context, and `is_static` reports that.
    for build in BUILDS {
        let result = exec(build, 0, STATIC_THEN_CALL, &[0xaa]);
        assert_eq!(expect_return(&result), [0x01, 0xaa], "{build:?}");
    }
}

#[test]
fn revert_path_charges_exactly_the_recorded_gas() {
    for build in BUILDS {
        let free = exec(build, 0, STATIC_FWD, &[0x00]);
        let charged = exec(build, 5_000, STATIC_FWD, &[0x00]);
        expect_revert(&free);
        expect_revert(&charged);
        assert_eq!(charged.gas_used() - free.gas_used(), 5_000, "{build:?}");
        // …and the unspent remainder came back: nowhere near the 1M limit.
        assert!(
            charged.gas_used() < 100_000,
            "{build:?}: {}",
            charged.gas_used()
        );
    }
}

#[test]
fn other_addresses_fall_through_to_eth_precompiles() {
    // 0x04 = identity.
    let identity = address!("0x0000000000000000000000000000000000000004");
    for build in BUILDS {
        let result = exec(build, 0, identity, b"gkvm");
        assert_eq!(expect_return(&result), b"gkvm", "{build:?}");
    }
}

//! The gkvm precompile (`UNBOUNDED_V3`): native guest execution inside the
//! local simulation env.
//!
//! [`GkvmPrecompiles`] wraps revm's stock [`EthPrecompiles`] and intercepts
//! exactly one address, [`GKVM_ADDRESS`]. A STATICCALL there with the wire
//! input `programHash (32) || artifactRoot (32) || payload` runs the named
//! riscv64im guest under `gas-analyzer-gkvm` and answers
//! `GKVM_OK_TAG || output`. Spec: `UNBOUNDED_V3_NATIVE.md` § The precompile
//! (gas-killer/solidity-sdk).
//!
//! # The determinism split
//!
//! * **Deterministic guest outcomes** — trap, out of cycles, cap breaches, a
//!   non-static invocation — are typed REVERTs
//!   ([`gas_analyzer_core::gkvm::errors`], the twin of `GkVmErrors.sol`).
//!   They bubble up as tracked-function reverts and settle through the
//!   ordinary revert path; every honest operator computes the same bytes.
//! * **Operator-local environment failures** — no [`GkvmHost`] configured, a
//!   `programHash` that is not installed, an `artifactRoot` that is not
//!   mounted, the executor failing on the host — are **not EVM outcomes**.
//!   `run` returns `Err`, revm aborts the transaction with a fatal error,
//!   `run_pass` fails and the operator does not sign. Missing config is a
//!   liveness event, never a divergent signable result. This is also why the
//!   address is intercepted even when no host is configured: falling through
//!   would make the call hit an empty account, `GkVm.sol` would revert
//!   `GkVmUnavailable`, and a misconfigured operator would sign a transition
//!   its correctly configured peers do not produce.
//!
//! # Metering
//!
//! Charged before execution: [`gkvm_intrinsic_gas`] of the payload length. The
//! call's remaining gas becomes the cycle budget ([`gas_to_cycle_limit`]); the
//! guest's instruction count converts back via `ceil(cycles / 4)`. Out of
//! cycles consumes all provided gas. A call that cannot afford the intrinsic
//! charge halts `PrecompileOOG`, like any stock precompile.
//!
//! # Evaluation order (each step is deterministic, so the order is protocol)
//!
//! 1. non-static frame → `GkVmStaticOnly` (nothing charged);
//! 2. intrinsic gas (`PrecompileOOG` when unaffordable);
//! 3. wire input shorter than the 64-byte header → empty REVERT;
//! 4. payload above [`GKVM_INPUT_BYTES_CAP`] → `GkVmInputOverflow`;
//! 5. program / artifact lookup (environment class);
//! 6. guest execution.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alloy::primitives::{Address, B256, Bytes};
use gas_analyzer_core::gkvm::errors::SolError;
use gas_analyzer_core::gkvm::{
    GKVM_ADDRESS, GKVM_INPUT_BYTES_CAP, GKVM_OK_TAG, errors, gas_to_cycle_limit, gkvm_intrinsic_gas,
};
use gas_analyzer_gkvm::{GkVmError, GkVmJob, GkVmMountError, GkVmOutcome, GuestProgramSet};
use revm::context::{Cfg, ContextTr, LocalContextTr};
use revm::handler::{EthPrecompiles, PrecompileProvider};
use revm::interpreter::{CallInput, CallInputs, Gas, InstructionResult, InterpreterResult};
use revm::primitives::hardfork::SpecId;

/// Length of the wire header: `programHash (32) || artifactRoot (32)`.
const WIRE_HEADER_LEN: usize = 64;

/// An operator-local failure to serve a gkvm call — the abstain class. Never
/// an EVM outcome: the provider turns it into a fatal executor error.
#[derive(Debug, thiserror::Error)]
pub enum GkvmHostError {
    /// The wire format named a program this operator has not installed.
    #[error("guest program {0} is not installed (GK_GUEST_PROGRAM[_N])")]
    ProgramNotInstalled(B256),
    /// The wire format named an artifact bundle this operator has not mounted.
    #[error("guest artifact {0} is not mounted (GK_GUEST_ARTIFACT[_N])")]
    ArtifactNotMounted(B256),
    /// The guest runner failed on the host (executor panic, host-pressure
    /// kill) — not a guest verdict.
    #[error(transparent)]
    Runner(#[from] GkVmError),
}

/// Everything the precompile needs to serve guest calls: the installed
/// programs and artifacts, and each artifact's page-serving schedule.
///
/// One instance lives on [`crate::LocalStateCache`] for the life of the
/// process and is shared by every pass of every call.
pub struct GkvmHost {
    programs: GuestProgramSet,
    schedules: HashMap<B256, Vec<(u32, u64)>>,
    guest_runs: AtomicU64,
}

impl GkvmHost {
    /// A host serving `programs`.
    pub fn new(programs: GuestProgramSet) -> Self {
        Self {
            programs,
            schedules: HashMap::new(),
            guest_runs: AtomicU64::new(0),
        }
    }

    /// A host over every `GK_GUEST_PROGRAM[_N]` / `GK_GUEST_ARTIFACT[_N]` slot
    /// in the process environment (verified at load; the service boundary
    /// panics on the error, the `GK_SIM_EXECUTOR` doctrine).
    pub fn from_env() -> Result<Self, GkVmMountError> {
        Ok(Self::new(GuestProgramSet::from_env()?))
    }

    /// Declare the order in which the artifact at `root` serves its pages
    /// `(kind, page_idx)` to a guest. The guest verifies every page against
    /// the manifest, so a wrong schedule is a deterministic guest trap, never
    /// silent corruption. Without a declaration no pages are served — the
    /// same default as `gk-run` without `--schedule`.
    pub fn with_artifact_schedule(mut self, root: B256, schedule: Vec<(u32, u64)>) -> Self {
        self.schedules.insert(root, schedule);
        self
    }

    /// Guest executions actually performed (not calls answered).
    pub fn guest_runs(&self) -> u64 {
        self.guest_runs.load(Ordering::Relaxed)
    }

    /// Run `program_hash` over `payload` under `cycle_limit`, returning the
    /// deterministic outcome and the cycles it took.
    fn exec(
        &self,
        program_hash: B256,
        artifact_root: B256,
        payload: &[u8],
        cycle_limit: u64,
    ) -> Result<(GkVmOutcome, u64), GkvmHostError> {
        let program = self
            .programs
            .program(&program_hash)
            .ok_or(GkvmHostError::ProgramNotInstalled(program_hash))?;
        let artifact = if artifact_root.is_zero() {
            None
        } else {
            Some(
                self.programs
                    .artifact(&artifact_root)
                    .ok_or(GkvmHostError::ArtifactNotMounted(artifact_root))?,
            )
        };
        let schedule = self
            .schedules
            .get(&artifact_root)
            .map_or(&[][..], Vec::as_slice);

        self.guest_runs.fetch_add(1, Ordering::Relaxed);
        let report = gas_analyzer_gkvm::run(&GkVmJob {
            program: program.program.clone(),
            payload,
            artifact: artifact.as_deref(),
            schedule,
            cycle_limit,
        })?;
        tracing::debug!(
            program = %program_hash,
            cycles = report.cycles,
            gas_used = report.gas_used,
            tier = report.tier.as_str(),
            wall_nanos = report.wall_nanos,
            "gkvm guest executed"
        );
        Ok((report.outcome, report.cycles))
    }
}

/// revm's stock precompiles plus the gkvm precompile at [`GKVM_ADDRESS`].
pub struct GkvmPrecompiles {
    inner: EthPrecompiles,
    host: Option<Arc<GkvmHost>>,
}

impl GkvmPrecompiles {
    /// The provider for one execution. `host: None` still intercepts
    /// [`GKVM_ADDRESS`] and fails the execution (see module docs).
    pub fn new(host: Option<Arc<GkvmHost>>) -> Self {
        Self {
            inner: EthPrecompiles::default(),
            host,
        }
    }
}

fn revert(gas: Gas, error: impl SolError) -> InterpreterResult {
    InterpreterResult {
        result: InstructionResult::Revert,
        gas,
        output: error.abi_encode().into(),
    }
}

impl<CTX> PrecompileProvider<CTX> for GkvmPrecompiles
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
        if inputs.bytecode_address != GKVM_ADDRESS {
            return self.inner.run(context, inputs);
        }

        let mut gas = Gas::new(inputs.gas_limit);

        // Keyed on the frame's static context, not the opcode: a CALL nested
        // inside a STATICCALL is static (M3 entry-criterion finding).
        if !inputs.is_static {
            return Ok(Some(revert(gas, errors::GkVmStaticOnly {})));
        }

        let input: Vec<u8> = match &inputs.input {
            CallInput::Bytes(bytes) => bytes.to_vec(),
            CallInput::SharedBuffer(range) => context
                .local()
                .shared_memory_buffer_slice(range.clone())
                .map(|slice| slice.to_vec())
                .unwrap_or_default(),
        };
        let payload_len = input.len().saturating_sub(WIRE_HEADER_LEN);

        if !gas.record_cost(gkvm_intrinsic_gas(payload_len)) {
            return Ok(Some(InterpreterResult {
                result: InstructionResult::PrecompileOOG,
                gas,
                output: Bytes::new(),
            }));
        }
        if input.len() < WIRE_HEADER_LEN {
            return Ok(Some(InterpreterResult {
                result: InstructionResult::Revert,
                gas,
                output: Bytes::new(),
            }));
        }
        if payload_len > GKVM_INPUT_BYTES_CAP {
            return Ok(Some(revert(gas, errors::GkVmInputOverflow {})));
        }

        let program_hash = B256::from_slice(&input[..32]);
        let artifact_root = B256::from_slice(&input[32..WIRE_HEADER_LEN]);
        let payload = &input[WIRE_HEADER_LEN..];

        let host = self.host.as_ref().ok_or_else(|| {
            format!(
                "gkvm precompile called for program {program_hash} but no guest programs are \
                 configured on this executor"
            )
        })?;
        let cycle_limit = gas_to_cycle_limit(gas.remaining());
        let (outcome, cycles) = host
            .exec(program_hash, artifact_root, payload, cycle_limit)
            .map_err(|e| format!("gkvm environment failure: {e}"))?;

        // Within budget `ceil(cycles / 4) <= remaining` always holds; the
        // clamp only keeps the arithmetic total.
        let guest_gas = gas_analyzer_core::gkvm::cycles_to_gas(cycles).min(gas.remaining());
        let result = match outcome {
            GkVmOutcome::Ok { output } => {
                let _ = gas.record_cost(guest_gas);
                let mut tagged = Vec::with_capacity(1 + output.len());
                tagged.push(GKVM_OK_TAG);
                tagged.extend_from_slice(&output);
                InterpreterResult {
                    result: InstructionResult::Return,
                    gas,
                    output: tagged.into(),
                }
            }
            GkVmOutcome::Trap { code, data } => {
                let _ = gas.record_cost(guest_gas);
                revert(
                    gas,
                    errors::GkGuestTrap {
                        code,
                        data: data.into(),
                    },
                )
            }
            GkVmOutcome::OutOfCycles { used, limit } => {
                let _ = gas.record_cost(gas.remaining());
                revert(gas, errors::GkGuestOutOfCycles { used, limit })
            }
            GkVmOutcome::InputOverflow => revert(gas, errors::GkVmInputOverflow {}),
            GkVmOutcome::OutputOverflow => {
                let _ = gas.record_cost(guest_gas);
                revert(gas, errors::GkVmOutputOverflow {})
            }
        };
        Ok(Some(result))
    }

    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        let inner = <EthPrecompiles as PrecompileProvider<CTX>>::warm_addresses(&self.inner);
        Box::new(inner.chain(core::iter::once(GKVM_ADDRESS)))
    }

    fn contains(&self, address: &Address) -> bool {
        *address == GKVM_ADDRESS
            || <EthPrecompiles as PrecompileProvider<CTX>>::contains(&self.inner, address)
    }
}

// ============================================================================
// Tests (pure revm + the M1 guest fixtures — no anvil, no network)
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_exec::{
        ExecMounts, LocalBlockEnv, LocalTxRequest, call_view_local_blocking,
        extract_state_updates_local_blocking,
    };
    use alloy::primitives::{U256, address, keccak256};
    use gas_analyzer_core::{SimProfile, StateUpdate};
    use gas_analyzer_gkvm::LoadedGuestProgram;
    use revm::database::{CacheDB, EmptyDB};
    use revm::state::{AccountInfo, Bytecode};

    const HELLO_C: &[u8] = include_bytes!("../../gkvm/tests/fixtures/hello-c.elf");
    const BENCH_C: &[u8] = include_bytes!("../../gkvm/tests/fixtures/bench-c.elf");

    const CALLER: Address = address!("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    const STATIC_CONSUMER: Address = address!("0x1000000000000000000000000000000000000001");
    const CALL_CONSUMER: Address = address!("0x1000000000000000000000000000000000000002");
    const VIEW_FORWARDER: Address = address!("0x1000000000000000000000000000000000000003");

    /// A tracked-function stand-in: forwards its calldata to the precompile
    /// (STATICCALL, or CALL when `static_call` is false) and records what came
    /// back in storage, so the assertion runs through the real extraction:
    /// slot 0 = success + 1, slot 1 = keccak(returndata), slot 2 =
    /// returndatasize + 1, slot 3 / 4 = returndata words at byte 4 / 36 (the
    /// two arguments of `GkGuestOutOfCycles`), slot 5 = gas spent from entry
    /// to just after the call.
    fn recording_consumer(static_call: bool) -> Bytes {
        let mut code = vec![
            0x5a, // GAS — stays at the bottom of the stack until slot 5
            0x36, 0x5f, 0x5f, 0x37, // CALLDATACOPY(0, 0, calldatasize)
            0x5f, 0x5f, 0x36, 0x5f, // retSize retOffset argsSize argsOffset
        ];
        if !static_call {
            code.push(0x5f); // value
        }
        code.push(0x73); // PUSH20 gkvm
        code.extend_from_slice(GKVM_ADDRESS.as_slice());
        code.push(0x5a); // GAS
        code.push(if static_call { 0xfa } else { 0xf1 });
        code.extend_from_slice(&[0x60, 0x01, 0x01, 0x5f, 0x55]); // slot0 = success + 1
        code.extend_from_slice(&[0x5a, 0x90, 0x03, 0x60, 0x05, 0x55]); // slot5 = gas spent so far
        code.extend_from_slice(&[0x3d, 0x5f, 0x5f, 0x3e]); // RETURNDATACOPY(0, 0, size)
        code.extend_from_slice(&[0x3d, 0x5f, 0x20, 0x60, 0x01, 0x55]); // slot1 = keccak
        code.extend_from_slice(&[0x3d, 0x60, 0x01, 0x01, 0x60, 0x02, 0x55]); // slot2 = size + 1
        code.extend_from_slice(&[0x60, 0x04, 0x51, 0x60, 0x03, 0x55]); // slot3 = mload(4)
        code.extend_from_slice(&[0x60, 0x24, 0x51, 0x60, 0x04, 0x55]); // slot4 = mload(36)
        code.push(0x00);
        code.into()
    }

    /// A view stand-in: STATICCALLs the precompile with its calldata and
    /// returns the returndata (reverting with it on failure).
    fn view_forwarder() -> Bytes {
        let mut code = vec![0x36, 0x5f, 0x5f, 0x37, 0x5f, 0x5f, 0x36, 0x5f, 0x73];
        code.extend_from_slice(GKVM_ADDRESS.as_slice());
        code.extend_from_slice(&[0x5a, 0xfa, 0x3d, 0x5f, 0x5f, 0x3e]);
        let jumpdest = (code.len() + 3 + 3) as u8;
        code.extend_from_slice(&[0x60, jumpdest, 0x57, 0x3d, 0x5f, 0xfd]);
        code.extend_from_slice(&[0x5b, 0x3d, 0x5f, 0xf3]);
        code.into()
    }

    fn seeded_db() -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());
        for (addr, code) in [
            (STATIC_CONSUMER, recording_consumer(true)),
            (CALL_CONSUMER, recording_consumer(false)),
            (VIEW_FORWARDER, view_forwarder()),
        ] {
            db.insert_account_info(addr, AccountInfo::from_bytecode(Bytecode::new_raw(code)));
        }
        db
    }

    fn test_env() -> LocalBlockEnv {
        LocalBlockEnv {
            chain_id: 31_337,
            spec: SpecId::PRAGUE,
            number: 100,
            timestamp: 1_700_000_000,
            gas_limit: 30_000_000,
            coinbase: Address::ZERO,
            prevrandao: B256::ZERO,
            basefee: 0,
            difficulty: U256::ZERO,
        }
    }

    fn test_tx(to: Address, input: Vec<u8>, gas: u64) -> LocalTxRequest {
        LocalTxRequest {
            from: CALLER,
            to,
            input: input.into(),
            value: U256::ZERO,
            gas: Some(gas),
            gas_price: 0,
            nonce: None,
            access_list: Default::default(),
            authorization_list: Default::default(),
        }
    }

    fn host() -> Arc<GkvmHost> {
        let mut programs = GuestProgramSet::default();
        for elf in [HELLO_C, BENCH_C] {
            programs.insert_program(
                LoadedGuestProgram::from_bytes(elf.to_vec(), keccak256(elf), "<fixture>")
                    .expect("fixture loads"),
            );
        }
        Arc::new(GkvmHost::new(programs))
    }

    fn mounts(host: &Arc<GkvmHost>) -> ExecMounts {
        ExecMounts {
            overlay: Default::default(),
            gkvm: Some(host.clone()),
        }
    }

    fn wire(elf: &[u8], payload: &[u8]) -> Vec<u8> {
        [keccak256(elf).as_slice(), B256::ZERO.as_slice(), payload].concat()
    }

    /// Run the recording consumer through the real extraction and return its
    /// recorded slots `[success + 1, keccak, size + 1, word@4, word@36, gas]`.
    fn record(
        mounts: ExecMounts,
        consumer: Address,
        input: Vec<u8>,
        gas: u64,
    ) -> anyhow::Result<[B256; 6]> {
        let (updates, _) = extract_state_updates_local_blocking(
            seeded_db(),
            mounts,
            &test_env(),
            &test_tx(consumer, input, gas),
            SimProfile::Chain,
            consumer,
        )?;
        let mut slots = [B256::ZERO; 6];
        for update in updates {
            if let StateUpdate::Store(store) = update {
                slots[store.slot[31] as usize] = store.value;
            }
        }
        Ok(slots)
    }

    fn word(value: u64) -> B256 {
        B256::from(U256::from(value))
    }

    #[test]
    fn a_static_guest_call_returns_the_tagged_output_in_one_pass() {
        let host = host();
        let slots = record(
            mounts(&host),
            STATIC_CONSUMER,
            wire(HELLO_C, &[0x11, 0x22, 0x33, 0x44]),
            3_000_000,
        )
        .expect("extraction");

        // M1's recorded hello answer: "GKVM-HELLO-V1\n" + the reversed payload.
        let expected = [
            &[GKVM_OK_TAG][..],
            b"GKVM-HELLO-V1\n",
            &[0x44, 0x33, 0x22, 0x11],
        ]
        .concat();
        assert_eq!(slots[0], word(2), "STATICCALL succeeded");
        assert_eq!(slots[1], keccak256(&expected));
        assert_eq!(slots[2], word(expected.len() as u64 + 1));
        // A target-depth STATICCALL keeps the classify fast path: exactly one
        // pass ran the guest, no replay-script fallback.
        assert_eq!(host.guest_runs(), 1);
    }

    #[test]
    fn a_guest_abort_reverts_with_the_typed_trap() {
        let host = host();
        let slots = record(
            mounts(&host),
            STATIC_CONSUMER,
            wire(BENCH_C, &[0x01]),
            3_000_000,
        )
        .expect("extraction");
        let expected = errors::GkGuestTrap {
            code: 1,
            data: Bytes::from_static(b"bench wants a u64 BE iteration count"),
        }
        .abi_encode();
        assert_eq!(slots[0], word(1), "STATICCALL failed");
        assert_eq!(slots[1], keccak256(&expected));
        assert_eq!(slots[2], word(expected.len() as u64 + 1));
    }

    #[test]
    fn an_exhausted_budget_reverts_out_of_cycles_with_the_exact_count() {
        // bench N = 10^7 retires 80,000,157 instructions (M1) = 20,000,040
        // gas; a 12M-gas transaction cannot carry that. Out of cycles burns
        // the whole forwarded 63/64, so the transaction is sized for the
        // consumer to finish recording on the 1/64 it kept.
        let host = host();
        let slots = record(
            mounts(&host),
            STATIC_CONSUMER,
            wire(BENCH_C, &10_000_000u64.to_be_bytes()),
            12_000_000,
        )
        .expect("extraction");
        assert_eq!(slots[0], word(1), "STATICCALL failed");
        assert_eq!(slots[2], word(4 + 64 + 1), "selector + two words");
        assert_eq!(slots[3], word(80_000_157), "used = the guest's exact total");
        let limit = U256::from_be_bytes(slots[4].0);
        assert!(limit < U256::from(48_000_000u64) && limit > U256::from(40_000_000u64));
        assert_eq!(limit % U256::from(4u64), U256::ZERO, "limit = gas × 4");
        // All forwarded gas is gone: the call cost the consumer over 63/64 of
        // what it had.
        assert!(U256::from_be_bytes(slots[5].0) > U256::from(11_000_000u64));
    }

    #[test]
    fn a_non_static_invocation_reverts_static_only_without_running_the_guest() {
        let host = host();
        let slots = record(
            mounts(&host),
            CALL_CONSUMER,
            wire(HELLO_C, &[0x11]),
            3_000_000,
        )
        .expect("extraction");
        assert_eq!(slots[0], word(1), "CALL failed");
        assert_eq!(slots[1], keccak256(errors::GkVmStaticOnly {}.abi_encode()));
        assert_eq!(host.guest_runs(), 0);
    }

    #[test]
    fn a_short_wire_header_is_an_empty_revert() {
        let host = host();
        let slots =
            record(mounts(&host), STATIC_CONSUMER, vec![0xaa; 63], 3_000_000).expect("extraction");
        assert_eq!(slots[0], word(1), "STATICCALL failed");
        assert_eq!(slots[2], word(1), "empty returndata");
        assert_eq!(host.guest_runs(), 0);
    }

    #[test]
    fn an_oversized_payload_overflows_before_any_lookup() {
        // The program hash is not installed — the cap verdict must win, since
        // it is the deterministic one.
        let host = host();
        let input = [
            B256::repeat_byte(0xee).as_slice(),
            B256::ZERO.as_slice(),
            &vec![0u8; GKVM_INPUT_BYTES_CAP + 1],
        ]
        .concat();
        let slots = record(mounts(&host), STATIC_CONSUMER, input, 30_000_000).expect("extraction");
        assert_eq!(slots[0], word(1), "STATICCALL failed");
        assert_eq!(
            slots[1],
            keccak256(errors::GkVmInputOverflow {}.abi_encode())
        );
    }

    #[test]
    fn environment_failures_fail_the_execution_instead_of_reverting() {
        // Unknown program: no signable result.
        let host = host();
        let unknown = [B256::repeat_byte(0xee).as_slice(), B256::ZERO.as_slice()].concat();
        let err = record(mounts(&host), STATIC_CONSUMER, unknown, 3_000_000)
            .expect_err("unknown program must fail the pass");
        assert!(format!("{err:#}").contains("not installed"), "{err:#}");

        // Unmounted artifact.
        let mut input = wire(HELLO_C, &[]);
        input[32..64].copy_from_slice(B256::repeat_byte(0x77).as_slice());
        let err = record(mounts(&host), STATIC_CONSUMER, input, 3_000_000)
            .expect_err("unmounted artifact must fail the pass");
        assert!(format!("{err:#}").contains("not mounted"), "{err:#}");

        // No host at all: still intercepted, still fatal — never the
        // empty-account fallthrough that `GkVm.sol` reads as GkVmUnavailable.
        let err = record(
            ExecMounts::default(),
            STATIC_CONSUMER,
            wire(HELLO_C, &[]),
            3_000_000,
        )
        .expect_err("an unconfigured executor must fail the pass");
        assert!(
            format!("{err:#}").contains("no guest programs are configured"),
            "{err:#}"
        );
    }

    #[test]
    fn the_view_path_serves_guest_calls_too() {
        let host = host();
        let out = call_view_local_blocking(
            seeded_db(),
            mounts(&host),
            &test_env(),
            &test_tx(VIEW_FORWARDER, wire(HELLO_C, &[]), 3_000_000),
            SimProfile::Chain,
        )
        .expect("view call");
        assert_eq!(
            out.as_ref(),
            [&[GKVM_OK_TAG][..], b"GKVM-HELLO-V1\n"].concat()
        );
    }

    #[test]
    fn guest_gas_tracks_cycles_at_four_per_gas() {
        // bench retires exactly 8 instructions per iteration (M1): N = 1000 →
        // 8,157 cycles → 2,040 gas; N = 2000 → 16,157 → 4,040. Everything else
        // around the call is identical, so the consumer-observed gas differs
        // by exactly the guest delta.
        let host = host();
        let spent = |iterations: u64| {
            let slots = record(
                mounts(&host),
                STATIC_CONSUMER,
                wire(BENCH_C, &iterations.to_be_bytes()),
                3_000_000,
            )
            .expect("extraction");
            assert_eq!(slots[0], word(2), "STATICCALL succeeded");
            U256::from_be_bytes(slots[5].0)
        };
        let (small, large) = (spent(1_000), spent(2_000));
        assert_eq!(large - small, U256::from(2_000u64));
        assert!(small > U256::from(gkvm_intrinsic_gas(8) + 2_040));
    }
}

//! The UNBOUNDED_V3 gkvm precompile as an evm2 [`PrecompileProvider`] (campaign lane L2).
//!
//! A port of `gas_analyzer_evmsketch::gkvm_precompile` (revm 31 — the consensus
//! interpreter) with the SAME evaluation order, each step deterministic:
//!
//! 1. non-static frame → `GkVmStaticOnly`, nothing charged;
//! 2. intrinsic `gkvm_intrinsic_gas(payload_len)` → out of gas if unaffordable;
//! 3. wire shorter than the 64-byte header → empty revert;
//! 4. payload over the cap → `GkVmInputOverflow`;
//! 5. program / artifact lookup — an ENVIRONMENT failure: fatal, the pass fails and the
//!    operator abstains (never an EVM outcome);
//! 6. guest run with `remaining gas × 4` cycles; charge `ceil(cycles / 4)`; out of cycles
//!    burns everything that was forwarded.
//!
//! The address is intercepted even with no programs installed (fatal, not fall-through):
//! falling through would read the empty account and revert `GkVmUnavailable`, and a
//! misconfigured operator would sign a transition its peers do not produce.
//!
//! No result memo here: that is an optimization of the revm-31 host, not semantics.

use alloy_primitives::{Address, B256, Bytes};
use alloy_sol_types::SolError;
use evm2::{
    Evm, EvmTypesHost,
    evm::precompile::{PrecompileOutput, PrecompileProvider},
    interpreter::{GasTracker, Message, MessageKind},
    precompiles::{PrecompileError, PrecompileHalt, PrecompileId},
};
use gas_analyzer_core::gkvm::{
    GKVM_ADDRESS, GKVM_INPUT_BYTES_CAP, GKVM_OK_TAG, cycles_to_gas, errors, gas_to_cycle_limit,
    gkvm_intrinsic_gas,
};
use gas_analyzer_gkvm::{GkVmError, GkVmJob, GkVmOutcome, GuestProgramSet};
use std::collections::HashMap;

/// Length of the wire header: `programHash (32) || artifactRoot (32)`.
const WIRE_HEADER_LEN: usize = 64;

/// An operator-local failure to serve a gkvm call — the abstain class.
#[derive(Debug, thiserror::Error)]
pub enum GkvmEnvError {
    /// The called program is not installed on this executor.
    #[error("gkvm program {0} is not installed on this executor")]
    ProgramNotInstalled(B256),
    /// The called artifact is not mounted on this executor.
    #[error("gkvm artifact {0} is not mounted on this executor")]
    ArtifactNotMounted(B256),
    /// The guest runner itself failed (not a guest outcome).
    #[error("gkvm runner failure: {0}")]
    Runner(#[from] GkVmError),
}

/// `inner`'s precompiles plus the gkvm guest precompile at [`GKVM_ADDRESS`].
pub struct GkvmPrecompiles<P> {
    inner: P,
    programs: GuestProgramSet,
    schedules: HashMap<B256, Vec<(u32, u64)>>,
    guest_runs: u64,
}

impl<P> GkvmPrecompiles<P> {
    /// Wraps `inner`, serving guest calls from `programs`.
    pub fn new(inner: P, programs: GuestProgramSet) -> Self {
        Self {
            inner,
            programs,
            schedules: HashMap::new(),
            guest_runs: 0,
        }
    }

    /// Declares the page order `gk_artifact_read` is served in for `root`.
    pub fn with_artifact_schedule(mut self, root: B256, schedule: Vec<(u32, u64)>) -> Self {
        self.schedules.insert(root, schedule);
        self
    }

    /// Guest executions so far.
    pub const fn guest_runs(&self) -> u64 {
        self.guest_runs
    }

    fn run_guest(
        &mut self,
        program_hash: B256,
        artifact_root: B256,
        payload: &[u8],
        cycle_limit: u64,
    ) -> Result<(GkVmOutcome, u64), GkvmEnvError> {
        let program = self
            .programs
            .program(&program_hash)
            .ok_or(GkvmEnvError::ProgramNotInstalled(program_hash))?;
        let artifact = if artifact_root.is_zero() {
            None
        } else {
            Some(
                self.programs
                    .artifact(&artifact_root)
                    .ok_or(GkvmEnvError::ArtifactNotMounted(artifact_root))?,
            )
        };
        let schedule = self
            .schedules
            .get(&artifact_root)
            .map_or(&[][..], Vec::as_slice);
        self.guest_runs += 1;
        let report = gas_analyzer_gkvm::run(&GkVmJob {
            program: program.program.clone(),
            payload,
            artifact: artifact.as_deref(),
            schedule,
            cycle_limit,
        })?;
        Ok((report.outcome, report.cycles))
    }
}

fn revert(error: impl SolError) -> PrecompileError {
    PrecompileError::Revert(error.abi_encode().into())
}

impl<T, P> PrecompileProvider<T> for GkvmPrecompiles<P>
where
    T: EvmTypesHost,
    P: PrecompileProvider<T> + 'static,
{
    fn addresses(&self) -> Vec<Address> {
        let mut addresses = self.inner.addresses();
        addresses.push(GKVM_ADDRESS);
        addresses
    }

    fn precompile_ids(&self) -> Vec<(Address, PrecompileId)> {
        let mut ids = self.inner.precompile_ids();
        ids.push((GKVM_ADDRESS, PrecompileId::custom("gkvm")));
        ids
    }

    fn contains(&self, address: &Address) -> bool {
        *address == GKVM_ADDRESS || self.inner.contains(address)
    }

    fn execute(
        &mut self,
        evm: &mut Evm<'_, T>,
        message: &Message<T>,
        gas: &mut GasTracker,
    ) -> Option<Result<PrecompileOutput, PrecompileError>> {
        if message.code_address != GKVM_ADDRESS {
            return self.inner.execute(evm, message, gas);
        }
        Some(self.execute_gkvm(message, gas))
    }
}

impl<P> GkvmPrecompiles<P> {
    fn execute_gkvm<E>(
        &mut self,
        message: &evm2::interpreter::MessageExt<E>,
        gas: &mut GasTracker,
    ) -> Result<PrecompileOutput, PrecompileError> {
        // Keyed on the frame's static context, not the opcode: a CALL nested inside a
        // STATICCALL is static.
        let is_static = message.caller_is_static || message.kind == MessageKind::StaticCall;
        if !is_static {
            return Err(revert(errors::GkVmStaticOnly {}));
        }

        let input: &[u8] = &message.input;
        let payload_len = input.len().saturating_sub(WIRE_HEADER_LEN);
        gas.spend(gkvm_intrinsic_gas(payload_len))
            .map_err(|_| PrecompileHalt::OutOfGas)?;
        if input.len() < WIRE_HEADER_LEN {
            return Err(PrecompileError::Revert(Bytes::new()));
        }
        if payload_len > GKVM_INPUT_BYTES_CAP {
            return Err(revert(errors::GkVmInputOverflow {}));
        }

        let program_hash = B256::from_slice(&input[..32]);
        let artifact_root = B256::from_slice(&input[32..WIRE_HEADER_LEN]);
        let payload = &input[WIRE_HEADER_LEN..];

        let cycle_limit = gas_to_cycle_limit(gas.remaining());
        let (outcome, cycles) = self
            .run_guest(program_hash, artifact_root, payload, cycle_limit)
            .map_err(PrecompileError::fatal)?;

        // Within budget `ceil(cycles / 4) <= remaining` always holds; the clamp only keeps
        // the arithmetic total.
        let guest_gas = cycles_to_gas(cycles).min(gas.remaining());
        match outcome {
            GkVmOutcome::Ok { output } => {
                let _ = gas.spend(guest_gas);
                let mut tagged = Vec::with_capacity(1 + output.len());
                tagged.push(GKVM_OK_TAG);
                tagged.extend_from_slice(&output);
                Ok(PrecompileOutput::new(tagged.into()))
            }
            GkVmOutcome::Trap { code, data } => {
                let _ = gas.spend(guest_gas);
                Err(revert(errors::GkGuestTrap {
                    code,
                    data: data.into(),
                }))
            }
            GkVmOutcome::OutOfCycles { used, limit } => {
                gas.spend_all();
                Err(revert(errors::GkGuestOutOfCycles { used, limit }))
            }
            GkVmOutcome::InputOverflow => Err(revert(errors::GkVmInputOverflow {})),
            GkVmOutcome::OutputOverflow => {
                let _ = gas.spend(guest_gas);
                Err(revert(errors::GkVmOutputOverflow {}))
            }
        }
    }
}

//! UNBOUNDED_V3 guest VM: native execution of riscv64im guest programs
//! inside Gas Killer's pinned simulation environment.
//!
//! V1 pins gas limits, V2 pins `address → code` overlays, V3 pins a guest VM:
//! one precompile address, one instruction-set semantics (rv64im — SP1's
//! guest ISA), one cycle-metering rule. This crate is the runner half of that
//! axis: it loads committed guest ELFs and artifact bundles, executes them
//! under SP1's own executor lineage with deterministic instruction counting,
//! and reduces every run to either a deterministic guest outcome (signable)
//! or an operator-local environment error (abstain). The M3 precompile
//! provider in `evmsketch` wraps [`runner::run`]; the `gk-run` sidecar binary
//! wraps it for forge's ffi shim and operator preflights.
//!
//! Spec: `src/examples/onchain-llm/UNBOUNDED_V3_NATIVE.md` in solidity-sdk.

pub mod constants;
pub mod manifest;
pub mod mounts;
pub mod runner;

pub use constants::{
    GKVM_ADDRESS, GKVM_INPUT_BYTES_CAP, GKVM_OUTPUT_BYTES_CAP, UNBOUNDED_V3_CYCLES_PER_GAS,
    UNBOUNDED_V3_GKVM_SPEC_VERSION, cycles_to_gas, gas_to_cycle_limit,
};
pub use mounts::{ArtifactMountV3, GkVmMountError, GuestProgramSet, LoadedGuestProgram};
pub use runner::{EXEC_TIER, ExecTier, GkVmError, GkVmJob, GkVmOutcome, GkVmReport, run};

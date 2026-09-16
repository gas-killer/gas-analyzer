//! Deterministic guest execution under SP1's executor, with the outcome split
//! UNBOUNDED_V3_NATIVE.md's error semantics are built on.
//!
//! Two disjoint failure classes, and the split is load-bearing:
//!
//! * [`GkVmOutcome`] — **deterministic guest outcomes**. Every honest
//!   operator computes the same one from the same (program, artifact,
//!   payload, cycle limit); they surface as typed tracked-function reverts
//!   and settle through the existing revert-fallback path.
//! * [`GkVmError`] — **operator-local environment failures** (a panicking
//!   executor, host-pressure kills). Not an EVM outcome at all: the caller
//!   fails the analysis and the operator abstains, exactly like a missing
//!   mount.
//!
//! # Cycle budgets are enforced post-hoc
//!
//! SP1 v6's untraced executors run a program to completion in one call —
//! there is no per-cycle budget hook on either tier (the traced mode's chunk
//! pauses exist for the prover, at a large throughput cost). The budget rule
//! here is therefore *exact and post-hoc*: a guest that halts having executed
//! more than `cycle_limit` instructions is [`GkVmOutcome::OutOfCycles`], no
//! matter how it halted. That verdict is a pure function of pinned inputs, so
//! it is consensus-safe for every halting guest. A guest that never halts is
//! bounded by the caller's wall-clock supervisor (`gk-run --deadline-secs`),
//! which maps to the *environment* class — an abstain, never a signable
//! result — because wall clock is operator-local. Mid-run deterministic
//! cutoff via traced chunk pacing is the M3 provider's upgrade path if it
//! proves necessary.
//!
//! # Tier asymmetry, stated plainly
//!
//! The portable interpreter is the **consensus tier**: it enforces
//! [`GKVM_MEM_BYTES_CAP`] and reports guest faults (bad memory access,
//! unknown syscall) as deterministic errors. The x86_64 JIT tier is a
//! **performance preview**: SP1's own docs mark it "only suitable for known
//! programs" — it detects neither, and a fault inside its generated code can
//! take the process down. M1's differential matrix proves the tiers agree on
//! well-behaved guests (byte-identical output, identical instruction
//! counts); anything else belongs to the interpreter.

use crate::constants::{
    GKVM_INPUT_BYTES_CAP, GKVM_MEM_BYTES_CAP, GKVM_OUTPUT_BYTES_CAP, GKVM_TRAP_CODE_BARE_EXIT,
    GKVM_TRAP_CODE_MEM_CAP, GKVM_TRAP_EXIT_CODE, cycles_to_gas,
};
use crate::mounts::ArtifactMountV3;
use alloy_primitives::B256;
use sp1_core_executor::{ExecutionError, MinimalExecutorEnum, Program};
use std::{panic::AssertUnwindSafe, sync::Arc, time::Instant};

/// Magic trailer of a guest abort frame (see [`parse_abort_frame`]).
pub const GKVM_TRAP_FRAME_MAGIC: &[u8; 8] = b"GKTRAP01";

/// Which executor this binary was compiled with. One binary is one tier —
/// the choice is `sp1-core-executor`'s build-script cfg, forced to the
/// portable interpreter by this crate's `portable-exec` feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecTier {
    /// SP1's x86_64 JIT (`sp1-jit`) — the performance preview tier.
    NativeJit,
    /// SP1's portable pure-Rust interpreter — the consensus tier.
    PortableInterp,
}

/// The tier compiled into this binary, mirroring `sp1-core-executor`'s own
/// build-script conditions (x86_64 little-endian Linux without the
/// portable-forcing features selects the JIT).
pub const EXEC_TIER: ExecTier = if cfg!(feature = "portable-exec")
    || !cfg!(all(
        target_arch = "x86_64",
        target_os = "linux",
        target_endian = "little"
    )) {
    ExecTier::PortableInterp
} else {
    ExecTier::NativeJit
};

impl ExecTier {
    /// The `GK_GUEST_EXEC` value naming this tier.
    pub fn as_str(self) -> &'static str {
        match self {
            ExecTier::NativeJit => "jit",
            ExecTier::PortableInterp => "interp",
        }
    }
}

/// One guest execution request. All fields are consensus inputs except
/// nothing: the deadline lives at the caller, wall clock being
/// operator-local.
pub struct GkVmJob<'a> {
    /// The parsed guest program.
    pub program: Arc<Program>,
    /// The gkExec payload (`abi.encode(args…)` on the wire).
    pub payload: &'a [u8],
    /// The mounted artifact bundle, when `artifactRoot` is non-zero.
    pub artifact: Option<&'a ArtifactMountV3>,
    /// Artifact pages `(kind, page_idx)` served to the guest, in the order
    /// its declared access schedule will request them. The guest verifies
    /// each page against the manifest, so a wrong or reordered schedule is a
    /// deterministic guest trap, never silent corruption.
    pub schedule: &'a [(u32, u64)],
    /// Instruction budget: `gas × UNBOUNDED_V3_CYCLES_PER_GAS` at the
    /// provider, a raw count here.
    pub cycle_limit: u64,
}

/// A deterministic guest outcome — the signable class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GkVmOutcome {
    /// The guest halted cleanly within budget; `output` is the raw result
    /// payload the provider tags with `GKVM_OK_TAG`.
    Ok {
        /// Bytes the guest wrote through `gk_output_write`.
        output: Vec<u8>,
    },
    /// The guest aborted (`gk_abort`, a bare nonzero exit, a memory-cap or
    /// execution fault) — surfaced as `GkGuestTrap(code, data)`.
    Trap {
        /// The trap code (guest-chosen, or a `GKVM_TRAP_CODE_*` class).
        code: u32,
        /// The guest's abort message, or a diagnostic for fault classes.
        data: Vec<u8>,
    },
    /// The instruction budget was exceeded — surfaced as
    /// `GkGuestOutOfCycles(used, limit)` and consumes the call's full gas.
    OutOfCycles {
        /// Instructions actually executed (where the run was observed; for a
        /// halting guest this is its exact total).
        used: u64,
        /// The budget it exceeded.
        limit: u64,
    },
    /// The payload exceeded `GKVM_INPUT_BYTES_CAP` — checked before any
    /// execution, so the verdict never depends on guest behavior.
    InputOverflow,
    /// The guest wrote more than `GKVM_OUTPUT_BYTES_CAP`.
    OutputOverflow,
}

/// The result of a run: the deterministic outcome plus metering, and the
/// perf fields that must never feed a signed result.
#[derive(Debug)]
pub struct GkVmReport {
    /// The deterministic outcome.
    pub outcome: GkVmOutcome,
    /// Instructions executed (SP1 `global_clk`, 1 per retired instruction).
    pub cycles: u64,
    /// `ceil(cycles / UNBOUNDED_V3_CYCLES_PER_GAS)`.
    pub gas_used: u64,
    /// The tier this binary runs.
    pub tier: ExecTier,
    /// Wall-clock time of the execution loop. Operator-local; reporting only.
    pub wall_nanos: u128,
}

/// An operator-local environment failure — the abstain class.
#[derive(Debug, thiserror::Error)]
pub enum GkVmError {
    /// The executor panicked (portable tier; the JIT aborts the process
    /// instead). Panics are host code failing, not a guest verdict.
    #[error("SP1 executor panicked: {message}")]
    ExecutorPanic {
        /// The panic payload, when it was a string.
        message: String,
    },
    /// An executor error in the host-dependent class (child kills, RSS
    /// monitor); deterministic guest faults are mapped to
    /// [`GkVmOutcome::Trap`] instead and never appear here.
    #[error("SP1 executor error: {0}")]
    Execution(#[from] ExecutionError),
}

/// Trap code for deterministic execution faults (bad memory access, unknown
/// syscall, unimplemented opcode) reported by the consensus tier. The fault's
/// rendered description travels as the trap data.
pub const GKVM_TRAP_CODE_EXEC_FAULT: u32 = 0xF000_0002;

/// Run a guest to a deterministic verdict.
pub fn run(job: &GkVmJob<'_>) -> Result<GkVmReport, GkVmError> {
    let started = Instant::now();
    let report = |outcome: GkVmOutcome, cycles: u64| GkVmReport {
        outcome,
        cycles,
        gas_used: cycles_to_gas(cycles),
        tier: EXEC_TIER,
        wall_nanos: started.elapsed().as_nanos(),
    };

    if job.payload.len() > GKVM_INPUT_BYTES_CAP {
        return Ok(report(GkVmOutcome::InputOverflow, 0));
    }

    let mut executor = MinimalExecutorEnum::new_with_limit(
        job.program.clone(),
        false,
        None,
        Some(GKVM_MEM_BYTES_CAP),
    );

    // GKVM ABI v1 input framing: [artifactRoot][payload][manifest][pages…].
    let artifact_root = job.artifact.map_or(B256::ZERO, |mount| mount.root);
    executor.with_input(artifact_root.as_slice());
    executor.with_input(job.payload);
    if let Some(mount) = job.artifact {
        executor.with_input(&crate::manifest::manifest_blob(&mount.manifests()));
        for &(kind, page_idx) in job.schedule {
            let Some((page, branch)) = mount.page(kind, page_idx) else {
                // A schedule pointing outside the manifest is caller
                // misconfiguration, not a guest act; refuse to run rather
                // than let the guest starve into a host panic.
                return Err(GkVmError::ExecutorPanic {
                    message: format!(
                        "artifact schedule references kind {kind} page {page_idx} outside the mount"
                    ),
                });
            };
            let mut buffer = page;
            for sibling in branch {
                buffer.extend_from_slice(sibling.as_slice());
            }
            executor.with_input(&buffer);
        }
    }

    // The executor panics on guest protocol violations (an exhausted hint
    // stream, an unknown syscall id on some paths). On the portable tier
    // those unwind and land here as environment errors.
    let run_result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        while !executor.is_done() {
            executor.try_execute_chunk()?;
            if executor.global_clk() >= job.cycle_limit && !executor.is_done() {
                break;
            }
        }
        Ok::<(), ExecutionError>(())
    }));

    let cycles = executor.global_clk();
    let loop_result = match run_result {
        Ok(inner) => inner,
        Err(panic) => {
            let message = panic
                .downcast_ref::<&str>()
                .map(ToString::to_string)
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            return Err(GkVmError::ExecutorPanic { message });
        }
    };

    if let Err(error) = loop_result {
        return match error {
            // Deterministic for a given program+input: the portable tier's
            // own memory accounting crossed the pinned cap.
            ExecutionError::TooMuchMemory() => Ok(report(
                GkVmOutcome::Trap {
                    code: GKVM_TRAP_CODE_MEM_CAP,
                    data: Vec::new(),
                },
                cycles,
            )),
            // Deterministic guest faults: same program, same input, same
            // fault on every honest interpreter.
            fault @ (ExecutionError::InvalidMemoryAccess(..)
            | ExecutionError::InvalidMemoryAccessUntrustedProgram(..)
            | ExecutionError::UnsupportedSyscall(..)
            | ExecutionError::Breakpoint()
            | ExecutionError::InvalidSyscallUsage(..)
            | ExecutionError::Unimplemented()
            | ExecutionError::EndInUnconstrained()
            | ExecutionError::UnconstrainedCycleLimitExceeded(..)
            | ExecutionError::UnexpectedExitCode(..)
            | ExecutionError::InstructionNotFound()
            | ExecutionError::UnhandledTrap(..)) => Ok(report(
                GkVmOutcome::Trap {
                    code: GKVM_TRAP_CODE_EXEC_FAULT,
                    data: fault.to_string().into_bytes(),
                },
                cycles,
            )),
            // Host-dependent: abstain.
            other => Err(GkVmError::Execution(other)),
        };
    }

    if !executor.is_done() || cycles > job.cycle_limit {
        return Ok(report(
            GkVmOutcome::OutOfCycles {
                used: cycles,
                limit: job.cycle_limit,
            },
            cycles,
        ));
    }

    let public_values = executor.public_values_stream();
    let exit_code = executor.exit_code();
    let outcome = match exit_code {
        0 => {
            if public_values.len() > GKVM_OUTPUT_BYTES_CAP {
                GkVmOutcome::OutputOverflow
            } else {
                GkVmOutcome::Ok {
                    output: public_values.clone(),
                }
            }
        }
        code if code == u32::from(GKVM_TRAP_EXIT_CODE) => match parse_abort_frame(public_values) {
            Some((code, data)) => GkVmOutcome::Trap { code, data },
            // The trap exit code without a well-formed frame: still a trap,
            // classed as a bare exit so the malformation is visible.
            None => GkVmOutcome::Trap {
                code: GKVM_TRAP_CODE_BARE_EXIT | u32::from(GKVM_TRAP_EXIT_CODE),
                data: Vec::new(),
            },
        },
        code => GkVmOutcome::Trap {
            code: GKVM_TRAP_CODE_BARE_EXIT | (code & 0xff),
            data: Vec::new(),
        },
    };
    Ok(report(outcome, cycles))
}

/// Parse a guest abort frame from the tail of the public-values stream.
///
/// Layout, parsed from the end so it survives partial output written before
/// the abort: `… || msg || code (u32 BE) || msg_len (u32 BE) || "GKTRAP01"`.
pub fn parse_abort_frame(stream: &[u8]) -> Option<(u32, Vec<u8>)> {
    let magic_at = stream.len().checked_sub(GKVM_TRAP_FRAME_MAGIC.len())?;
    if &stream[magic_at..] != GKVM_TRAP_FRAME_MAGIC {
        return None;
    }
    let len_at = magic_at.checked_sub(4)?;
    let msg_len = u32::from_be_bytes(stream[len_at..magic_at].try_into().unwrap()) as usize;
    let code_at = len_at.checked_sub(4)?;
    let code = u32::from_be_bytes(stream[code_at..len_at].try_into().unwrap());
    let msg_at = code_at.checked_sub(msg_len)?;
    Some((code, stream[msg_at..code_at].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_frames_parse_from_the_tail() {
        let mut stream = b"partial output the guest wrote first".to_vec();
        stream.extend_from_slice(b"weights page 3 failed verification");
        stream.extend_from_slice(&7u32.to_be_bytes());
        stream.extend_from_slice(&(34u32).to_be_bytes());
        stream.extend_from_slice(GKVM_TRAP_FRAME_MAGIC);
        let (code, data) = parse_abort_frame(&stream).expect("well-formed frame");
        assert_eq!(code, 7);
        assert_eq!(data, b"weights page 3 failed verification");
    }

    #[test]
    fn malformed_frames_are_rejected_not_misread() {
        assert_eq!(parse_abort_frame(b""), None);
        assert_eq!(
            parse_abort_frame(b"GKTRAP01"),
            None,
            "magic alone has no code/len"
        );
        // A length pointing past the start of the stream must fail, not wrap.
        let mut stream = Vec::new();
        stream.extend_from_slice(&1u32.to_be_bytes());
        stream.extend_from_slice(&(1000u32).to_be_bytes());
        stream.extend_from_slice(GKVM_TRAP_FRAME_MAGIC);
        assert_eq!(parse_abort_frame(&stream), None);
    }

    #[test]
    fn the_compiled_tier_matches_the_feature_flags() {
        // This test compiles in both flavors; each asserts its own identity,
        // which is what lets the matrix script trust `--print-tier`.
        if cfg!(feature = "portable-exec") {
            assert_eq!(EXEC_TIER, ExecTier::PortableInterp);
        } else if cfg!(all(target_arch = "x86_64", target_os = "linux")) {
            assert_eq!(EXEC_TIER, ExecTier::NativeJit);
        }
    }
}

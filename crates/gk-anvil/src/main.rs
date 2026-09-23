//! anvil with the gkvm precompile: a local node where `GkVm.exec` works.
//!
//! Every anvil flag is accepted unchanged. On top:
//!   --guest <elf>                    install a guest program (hash = keccak of the file;
//!                                    repeatable). A dev node trusts the file — operators
//!                                    verify against a committed hash instead.
//!   --artifact <file>[,<file>…]      mount an artifact bundle (manifest-v3 root computed
//!                                    from the bytes; repeatable), served sequentially.
//! and the operator env (`GK_GUEST_PROGRAM[_N]` / `_HASH`, `GK_GUEST_ARTIFACT[_N]` / `_ROOT`)
//! is honored too.
//!
//! Semantics match the operator-side provider (crates/evmsketch/src/gkvm_precompile.rs):
//! static-only, intrinsic gas, header + payload cap, program lookup, guest budget = remaining
//! gas × 4, `0x01`-tagged output, typed reverts (`GkGuestTrap`, `GkGuestOutOfCycles`, …).
//! Differences, by design of a dev node: an environment failure (program not installed) is a
//! halt with a message rather than the operator's abstain, and there is no result memo.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{Address, B256, Bytes, keccak256};
use alloy_sol_types::SolError;
use anvil::{NodeConfig, PrecompileFactory, cmd::NodeArgs};
use clap::Parser;
use gas_analyzer_core::gkvm::{
    GKVM_ADDRESS, GKVM_INPUT_BYTES_CAP, GKVM_OK_TAG, cycles_to_gas, errors, gas_to_cycle_limit,
    gkvm_intrinsic_gas,
};
use gas_analyzer_gkvm::manifest::{ArtifactFileManifest, artifact_root};
use gas_analyzer_gkvm::{
    ArtifactMountV3, GkVmJob, GkVmOutcome, GuestProgramSet, LoadedGuestProgram,
};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};

/// `programHash (32) || artifactRoot (32)`.
const WIRE_HEADER_LEN: usize = 64;

#[derive(Parser, Debug)]
#[command(name = "gk-anvil", about = "anvil with the gkvm precompile (UNBOUNDED_V3 Phase B)")]
struct Args {
    /// Guest ELF to install (programHash = keccak256 of the file); repeatable
    #[arg(long = "guest", value_name = "ELF")]
    guests: Vec<PathBuf>,
    /// Artifact bundle to mount, files comma-separated (root computed from the bytes); repeatable
    #[arg(long = "artifact", value_name = "FILE[,FILE…]")]
    artifacts: Vec<String>,
    #[command(flatten)]
    node: NodeArgs,
}

/// The installed programs and artifacts, plus each artifact's sequential page schedule.
struct GkvmFactory {
    programs: GuestProgramSet,
    schedules: Vec<(B256, Vec<(u32, u64)>)>,
}

impl std::fmt::Debug for GkvmFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GkvmFactory")
            .field("programs", &self.programs.program_hashes())
            .field("artifacts", &self.programs.artifact_roots())
            .finish()
    }
}

/// What anvil registers: the shared factory behind the precompile closure.
#[derive(Debug)]
struct GkvmPrecompiles(Arc<GkvmFactory>);

impl GkvmFactory {
    fn new(programs: GuestProgramSet) -> Self {
        let schedules = programs
            .artifact_roots()
            .into_iter()
            .filter_map(|root| programs.artifact(&root).map(|a| (root, a.sequential_schedule())))
            .collect();
        Self { programs, schedules }
    }

    fn revert(gas_used: u64, error: impl SolError) -> PrecompileResult {
        Ok(PrecompileOutput::new_reverted(gas_used, error.abi_encode().into()))
    }

    fn call(&self, input: PrecompileInput<'_>) -> PrecompileResult {
        let gas_limit = input.gas;
        // (1) static-only, nothing charged
        if !input.is_static {
            return Self::revert(0, errors::GkVmStaticOnly {});
        }
        let data = input.data;
        let payload_len = data.len().saturating_sub(WIRE_HEADER_LEN);
        // (2) intrinsic
        let intrinsic = gkvm_intrinsic_gas(payload_len);
        if intrinsic > gas_limit {
            return Err(PrecompileError::OutOfGas);
        }
        // (3) short wire → empty revert
        if data.len() < WIRE_HEADER_LEN {
            return Ok(PrecompileOutput::new_reverted(intrinsic, Bytes::new()));
        }
        // (4) payload cap
        if payload_len > GKVM_INPUT_BYTES_CAP {
            return Self::revert(intrinsic, errors::GkVmInputOverflow {});
        }
        let program_hash = B256::from_slice(&data[..32]);
        let artifact_root = B256::from_slice(&data[32..WIRE_HEADER_LEN]);
        let payload = &data[WIRE_HEADER_LEN..];
        // (5) lookup: an environment failure — on a dev node, a halt that names the cause
        let Some(program) = self.programs.program(&program_hash) else {
            return Err(PrecompileError::Other(format!(
                "gkvm: program {program_hash} is not installed on this node (gk-anvil --guest <elf>)"
            ).into()));
        };
        let artifact = if artifact_root.is_zero() {
            None
        } else {
            match self.programs.artifact(&artifact_root) {
                Some(a) => Some(a),
                None => {
                    return Err(PrecompileError::Other(format!(
                        "gkvm: artifact {artifact_root} is not mounted on this node (gk-anvil --artifact <files>)"
                    ).into()));
                }
            }
        };
        let schedule = self
            .schedules
            .iter()
            .find(|(root, _)| *root == artifact_root)
            .map_or(&[][..], |(_, s)| s.as_slice());
        // (6) run with the remaining budget
        let remaining = gas_limit - intrinsic;
        let report = gas_analyzer_gkvm::run(&GkVmJob {
            program: program.program.clone(),
            payload,
            artifact: artifact.as_deref(),
            schedule,
            cycle_limit: gas_to_cycle_limit(remaining),
        })
        .map_err(|e| PrecompileError::Other(format!("gkvm: runner failure: {e}").into()))?;
        tracing::info!(
            program = %program_hash,
            cycles = report.cycles,
            gas_used = report.gas_used,
            tier = report.tier.as_str(),
            wall_ms = report.wall_nanos / 1_000_000,
            "gkvm guest executed"
        );
        let guest_gas = cycles_to_gas(report.cycles).min(remaining);
        match report.outcome {
            GkVmOutcome::Ok { output } => {
                let mut tagged = Vec::with_capacity(1 + output.len());
                tagged.push(GKVM_OK_TAG);
                tagged.extend_from_slice(&output);
                Ok(PrecompileOutput::new(intrinsic + guest_gas, tagged.into()))
            }
            GkVmOutcome::Trap { code, data } => Self::revert(
                intrinsic + guest_gas,
                errors::GkGuestTrap { code, data: data.into() },
            ),
            GkVmOutcome::OutOfCycles { used, limit } => {
                Self::revert(gas_limit, errors::GkGuestOutOfCycles { used, limit })
            }
            GkVmOutcome::InputOverflow => Self::revert(intrinsic, errors::GkVmInputOverflow {}),
            GkVmOutcome::OutputOverflow => {
                Self::revert(intrinsic + guest_gas, errors::GkVmOutputOverflow {})
            }
        }
    }
}

impl PrecompileFactory for GkvmPrecompiles {
    fn precompiles(&self) -> Vec<(Address, DynPrecompile)> {
        let factory = Arc::clone(&self.0);
        vec![(GKVM_ADDRESS, DynPrecompile::from(move |input: PrecompileInput<'_>| factory.call(input)))]
    }
}

fn load(args: &Args) -> eyre::Result<GuestProgramSet> {
    let mut programs = GuestProgramSet::from_env()
        .map_err(|e| eyre::eyre!("GK_GUEST_* environment: {e}"))?;
    for path in &args.guests {
        let elf = std::fs::read(path).map_err(|e| eyre::eyre!("{}: {e}", path.display()))?;
        let hash = keccak256(&elf);
        programs.insert_program(
            LoadedGuestProgram::from_bytes(elf, hash, &path.display().to_string())
                .map_err(|e| eyre::eyre!("{}: {e}", path.display()))?,
        );
        eprintln!("gkvm: installed program {hash}  ({})", path.display());
    }
    for bundle in &args.artifacts {
        let paths: Vec<&Path> = bundle.split(',').map(Path::new).collect();
        let mut manifests = Vec::with_capacity(paths.len());
        for p in &paths {
            let bytes = std::fs::read(p).map_err(|e| eyre::eyre!("{}: {e}", p.display()))?;
            manifests.push(ArtifactFileManifest::from_bytes(&bytes));
        }
        let root = artifact_root(&manifests);
        programs.insert_artifact(
            ArtifactMountV3::from_files(&paths, root).map_err(|e| eyre::eyre!("{bundle}: {e}"))?,
        );
        eprintln!("gkvm: mounted artifact {root}  ({bundle})");
    }
    if programs.program_count() == 0 {
        eprintln!("gkvm: no guest programs installed — every GkVm.exec halts. Pass --guest <elf>.");
    }
    Ok(programs)
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let args = Args::parse();
    let factory = Arc::new(GkvmFactory::new(load(&args)?));
    let config: NodeConfig = args.node.into_node_config()?.with_precompile_factory(GkvmPrecompiles(factory));
    eprintln!("gkvm: precompile at {GKVM_ADDRESS} (executor tier: {})", gas_analyzer_gkvm::EXEC_TIER.as_str());
    let (_api, handle) = anvil::try_spawn(config).await?;
    tokio::select! {
        result = handle => { result??; }
        _ = tokio::signal::ctrl_c() => {}
    }
    Ok(())
}

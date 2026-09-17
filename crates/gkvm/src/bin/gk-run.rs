//! `gk-run`: the gkvm sidecar. One guest execution per invocation, speaking
//! the sidecar discipline forge's ffi shim consumes: arguments on argv, the
//! result as **one hex line on stdout**, everything else on stderr.
//!
//! ```text
//! gk-run --program guest.elf --program-hash 0x… --input 0x…|@file
//!        [--artifact blob[,blob…] --artifact-root 0x…]
//!        [--schedule kind:page,kind:page,…]
//!        [--cycle-limit N] [--deadline-secs N] [--print-tier]
//! ```
//!
//! Exit codes map the runner's outcome split so a shim can react without
//! parsing: 0 success, 10 guest trap, 11 out of cycles, 12 input overflow,
//! 13 output overflow, 2 environment/executor error (the abstain class),
//! 3 usage error. The typed failures keep the one-hex-line discipline so a
//! shim can rebuild the exact revert data: a trap prints `code (u32 BE) ||
//! data`, out of cycles prints `used (u64 BE) || limit (u64 BE)`; every other
//! failure prints nothing. A one-line JSON report always goes to stderr — cycles, gas,
//! tier, wall time — which is what the M1 matrix and throughput runs consume.

use alloy_primitives::{B256, keccak256};
use anyhow::{Context, Result, anyhow, bail};
use gas_analyzer_gkvm::{
    ArtifactMountV3, EXEC_TIER, GkVmJob, GkVmOutcome, GkVmReport, LoadedGuestProgram, run,
};
use std::{path::Path, process::ExitCode, time::Duration};

const EXIT_TRAP: u8 = 10;
const EXIT_OUT_OF_CYCLES: u8 = 11;
const EXIT_INPUT_OVERFLOW: u8 = 12;
const EXIT_OUTPUT_OVERFLOW: u8 = 13;
const EXIT_ENV_ERROR: u8 = 2;
const EXIT_USAGE: u8 = 3;

struct Args {
    program: String,
    program_hash: Option<B256>,
    input: Vec<u8>,
    artifact: Vec<String>,
    artifact_root: Option<B256>,
    schedule: Vec<(u32, u64)>,
    cycle_limit: u64,
    deadline: Option<Duration>,
    print_tier: bool,
}

fn parse_hex32(value: &str, flag: &str) -> Result<B256> {
    let raw = value.strip_prefix("0x").unwrap_or(value);
    let bytes: [u8; 32] =
        hex::FromHex::from_hex(raw).with_context(|| format!("{flag} must be 32 hex bytes"))?;
    Ok(B256::from(bytes))
}

fn parse_args() -> Result<Args> {
    let mut args = Args {
        program: String::new(),
        program_hash: None,
        input: Vec::new(),
        artifact: Vec::new(),
        artifact_root: None,
        schedule: Vec::new(),
        // Effectively unlimited by default: the sidecar's callers pass the
        // provider-derived budget; a bare invocation is a dev loop.
        cycle_limit: u64::MAX,
        deadline: Some(Duration::from_secs(600)),
        print_tier: false,
    };
    let mut iter = std::env::args().skip(1);
    let mut input_seen = false;
    while let Some(arg) = iter.next() {
        let mut value = |name: &str| {
            iter.next()
                .ok_or_else(|| anyhow!("{name} requires a value"))
        };
        match arg.as_str() {
            "--program" => args.program = value("--program")?,
            "--program-hash" => {
                args.program_hash = Some(parse_hex32(&value("--program-hash")?, "--program-hash")?);
            }
            "--input" => {
                let raw = value("--input")?;
                args.input = if let Some(path) = raw.strip_prefix('@') {
                    std::fs::read(path).with_context(|| format!("reading input file {path}"))?
                } else {
                    let raw = raw.strip_prefix("0x").unwrap_or(&raw);
                    hex::decode(raw).context("--input must be hex or @file")?
                };
                input_seen = true;
            }
            "--artifact" => {
                args.artifact = value("--artifact")?
                    .split(',')
                    .map(str::to_string)
                    .collect();
            }
            "--artifact-root" => {
                args.artifact_root =
                    Some(parse_hex32(&value("--artifact-root")?, "--artifact-root")?);
            }
            "--schedule" => {
                for entry in value("--schedule")?.split(',').filter(|e| !e.is_empty()) {
                    let (kind, page) = entry
                        .split_once(':')
                        .ok_or_else(|| anyhow!("--schedule entries are kind:page"))?;
                    args.schedule.push((kind.parse()?, page.parse()?));
                }
            }
            "--cycle-limit" => args.cycle_limit = value("--cycle-limit")?.parse()?,
            "--deadline-secs" => {
                let secs: u64 = value("--deadline-secs")?.parse()?;
                args.deadline = (secs > 0).then(|| Duration::from_secs(secs));
            }
            "--print-tier" => args.print_tier = true,
            other => bail!("unknown argument {other}"),
        }
    }
    if args.print_tier {
        return Ok(args);
    }
    if args.program.is_empty() {
        bail!("--program is required");
    }
    if !input_seen {
        bail!("--input is required (use --input 0x for an empty payload)");
    }
    if args.artifact.is_empty() != args.artifact_root.is_none() {
        bail!("--artifact and --artifact-root are all-or-nothing");
    }
    Ok(args)
}

fn report_line(report: &GkVmReport) -> String {
    let outcome = match &report.outcome {
        GkVmOutcome::Ok { .. } => "ok".to_string(),
        GkVmOutcome::Trap { code, .. } => format!("trap:{code:#010x}"),
        GkVmOutcome::OutOfCycles { used, limit } => format!("out-of-cycles:{used}/{limit}"),
        GkVmOutcome::InputOverflow => "input-overflow".to_string(),
        GkVmOutcome::OutputOverflow => "output-overflow".to_string(),
    };
    serde_json::json!({
        "outcome": outcome,
        "cycles": report.cycles,
        "gas_used": report.gas_used,
        "tier": report.tier.as_str(),
        "wall_nanos": report.wall_nanos,
    })
    .to_string()
}

fn main() -> ExitCode {
    match main_inner() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("gk-run: {error:#}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn main_inner() -> Result<ExitCode> {
    let args = parse_args()?;
    if args.print_tier {
        println!("{}", EXEC_TIER.as_str());
        return Ok(ExitCode::SUCCESS);
    }

    let elf = std::fs::read(&args.program)
        .with_context(|| format!("reading guest ELF {}", args.program))?;
    // Without a committed hash (dev loop), trust the bytes we just read;
    // operators always pass the commitment.
    let expected = args.program_hash.unwrap_or_else(|| keccak256(&elf));
    let program =
        LoadedGuestProgram::from_bytes(elf, expected, &args.program).map_err(|e| anyhow!("{e}"))?;

    let artifact = if args.artifact.is_empty() {
        None
    } else {
        let paths: Vec<&Path> = args.artifact.iter().map(Path::new).collect();
        let root = args.artifact_root.expect("checked in parse_args");
        Some(ArtifactMountV3::from_files(&paths, root).map_err(|e| anyhow!("{e}"))?)
    };

    // The wall-clock supervisor: execution runs on a worker thread and the
    // process exits (environment class) if the deadline passes. Thread
    // teardown is irrelevant — the sidecar is single-shot by design.
    let (tx, rx) = std::sync::mpsc::channel();
    let payload = args.input.clone();
    let schedule = args.schedule.clone();
    let program_arc = program.program.clone();
    std::thread::spawn(move || {
        let job = GkVmJob {
            program: program_arc,
            payload: &payload,
            artifact: artifact.as_ref(),
            schedule: &schedule,
            cycle_limit: args.cycle_limit,
        };
        let _ = tx.send(run(&job));
    });
    let result = match args.deadline {
        Some(deadline) => match rx.recv_timeout(deadline) {
            Ok(result) => result,
            Err(_) => {
                eprintln!(
                    "gk-run: deadline of {}s exceeded (environment class — the guest kept running)",
                    deadline.as_secs()
                );
                return Ok(ExitCode::from(EXIT_ENV_ERROR));
            }
        },
        None => rx
            .recv()
            .expect("runner thread never drops the sender before sending"),
    };

    let report = match result {
        Ok(report) => report,
        Err(error) => {
            eprintln!("gk-run: {error}");
            return Ok(ExitCode::from(EXIT_ENV_ERROR));
        }
    };
    eprintln!("{}", report_line(&report));

    Ok(match report.outcome {
        GkVmOutcome::Ok { output } => {
            println!("0x{}", hex::encode(output));
            ExitCode::SUCCESS
        }
        GkVmOutcome::Trap { code, data } => {
            eprintln!(
                "gk-run: guest trap {code:#010x}: {}",
                String::from_utf8_lossy(&data)
            );
            println!("0x{}{}", hex::encode(code.to_be_bytes()), hex::encode(data));
            ExitCode::from(EXIT_TRAP)
        }
        GkVmOutcome::OutOfCycles { used, limit } => {
            eprintln!("gk-run: out of cycles ({used} used, {limit} allowed)");
            println!(
                "0x{}{}",
                hex::encode(used.to_be_bytes()),
                hex::encode(limit.to_be_bytes())
            );
            ExitCode::from(EXIT_OUT_OF_CYCLES)
        }
        GkVmOutcome::InputOverflow => {
            eprintln!("gk-run: input exceeds GKVM_INPUT_BYTES_CAP");
            ExitCode::from(EXIT_INPUT_OVERFLOW)
        }
        GkVmOutcome::OutputOverflow => {
            eprintln!("gk-run: output exceeds GKVM_OUTPUT_BYTES_CAP");
            ExitCode::from(EXIT_OUTPUT_OVERFLOW)
        }
    })
}

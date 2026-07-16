//! `gk-fast-view` sidecar binary.
//!
//! The revm-31 service/analyzer cannot link revm-41 + revmc in-process (see the
//! Phase-4 report / crate README for the co-link blocker), so the node shells
//! out to this binary when `GK_SHARD_FAST_EXECUTOR=1`. It reads a `.job` (the
//! [`gk_fast_view::job`] text format), executes the overlay-mounted view call on
//! the revmc-compiled path, and returns the raw returndata as hex.
//!
//! ## Two modes
//!
//! * **One-shot** (`gk-fast-view [JOB_FILE]`) — reads ONE job (from a path arg,
//!   or stdin if omitted), executes it through a fresh [`FastView`], prints the
//!   lowercase-hex returndata as one line on stdout, and exits. A revert/halt
//!   exits non-zero with the reason on stderr. Used by ad-hoc replays and any
//!   caller that wants a single self-contained invocation.
//!
//! * **Daemon** (`gk-fast-view --serve` / `gk-fast-view serve`) — the amortized
//!   path the service uses. Runs a loop over stdin serving MANY framed jobs
//!   against a PERSISTENT [`FastView`], so the ~20KB seg-engine bytecode is
//!   JIT-compiled **once** (memoized by codehash) and every subsequent segment
//!   runs on the compiled artifact. Spawning a fresh one-shot per segment would
//!   re-run LLVM codegen every time (~116s on the real engine) — the bug this
//!   mode fixes.
//!
//! ### Daemon wire protocol (length-prefixed, binary-safe)
//!
//! Returndata can reach ~1.5MB, so both directions are length-prefixed rather
//! than newline-delimited. All lengths are decimal ASCII.
//!
//! ```text
//! request  (service -> daemon):  "<job_byte_len>\n" then <job_byte_len> bytes of job text
//! response (daemon -> service):  "<STATUS> <payload_byte_len>\n" then <payload_byte_len> bytes
//!     STATUS = OK   payload = lowercase-hex returndata (no 0x)
//!     STATUS = ERR  payload = utf-8 error message (revert/halt/parse/compile)
//! ```
//!
//! An `ERR` frame is a per-job execution failure (revert/halt/mismatch); the
//! daemon stays healthy and ready for the next job — the caller surfaces it and
//! falls back to the interpreter. EOF on stdin (a zero-length read of the length
//! line) shuts the daemon down cleanly. Only frames go to stdout; every
//! diagnostic goes to stderr.

use std::io::{BufRead, Read, Write};

use anyhow::{Context, Result, bail};
use gk_fast_view::FastView;
use gk_fast_view::job::{Job, hex_encode};
use revm_primitives::{B256, Bytes, hardfork::SpecId, keccak256};

fn main() {
    if let Err(e) = run() {
        eprintln!("gk-fast-view: error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--serve") | Some("serve") => serve(),
        _ => run_one_shot(&args),
    }
}

/// One-shot mode: read a single job (file arg or whole stdin), execute through a
/// fresh [`FastView`], print the hex returndata. Unchanged behaviour — the path
/// the tests/examples and ad-hoc replays rely on.
fn run_one_shot(args: &[String]) -> Result<()> {
    let text = if let Some(path) = args.first() {
        std::fs::read_to_string(path).with_context(|| format!("read job file {path}"))?
    } else {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("read job from stdin")?;
        buf
    };

    let job = Job::parse(&text).context("parse job")?;
    let started = std::time::Instant::now();
    let returndata = job.execute().context("execute view call")?;
    let elapsed = started.elapsed();
    eprintln!(
        "gk-fast-view: ok, {} bytes returndata in {:?}",
        returndata.len(),
        elapsed
    );

    if let Some(expected) = &job.expected {
        if expected.as_ref() != returndata.as_ref() {
            bail!(
                "returndata mismatch vs expected:\n  expected 0x{}\n  got      0x{}",
                hex_encode(expected),
                hex_encode(&returndata)
            );
        }
        eprintln!("gk-fast-view: returndata matches expected golden");
    }

    println!("{}", hex_encode(&returndata));
    Ok(())
}

/// Daemon mode: serve framed jobs from stdin against persistent per-engine
/// [`FastView`]s, so each engine's bytecode compiles once and is reused across
/// every segment. Loops until stdin EOF.
fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let stdout = std::io::stdout();
    let mut writer = stdout.lock();

    // One FastView per (SpecId, engine-codehash). revmc's JIT module FINALIZES
    // after its first compile, so a single FastView can hold exactly one
    // compiled engine — a second distinct engine through the same FastView
    // fails with "cannot compile more functions after finalizing the module"
    // (observed live when one node served both the 0.6B and 35B models). Each
    // model therefore gets its own FastView (own JIT module + mount + warm
    // caches, which are per-model anyway); none is ever discarded.
    let mut views: Vec<((SpecId, B256), FastView)> = Vec::new();

    eprintln!(
        "gk-fast-view: --serve daemon ready (compile-once; length-prefixed frames on stdin)"
    );

    loop {
        let text = match read_request(&mut reader)? {
            Some(t) => t,
            None => {
                eprintln!("gk-fast-view: --serve daemon: stdin EOF, exiting");
                return Ok(());
            }
        };

        let started = std::time::Instant::now();
        match execute_job_text(&mut views, &text) {
            Ok(returndata) => {
                eprintln!(
                    "gk-fast-view: --serve served job, {} bytes returndata in {:?} (engines compiled: {} across {} views)",
                    returndata.len(),
                    started.elapsed(),
                    views.iter().map(|(_, fv)| fv.compiled_count()).sum::<usize>(),
                    views.len(),
                );
                let hex = hex_encode(&returndata);
                write_response(&mut writer, "OK", hex.as_bytes())?;
            }
            Err(e) => {
                let msg = format!("{e:#}");
                eprintln!("gk-fast-view: --serve job error: {msg}");
                write_response(&mut writer, "ERR", msg.as_bytes())?;
            }
        }
    }
}

/// Parse + execute one job against the persistent per-(spec, engine) [`FastView`]
/// cache, compiling each engine on first sight and reusing it forever after. Also
/// honours a fixture `expected` line (turns a mismatch into a loud error),
/// mirroring the one-shot path.
fn execute_job_text(views: &mut Vec<((SpecId, B256), FastView)>, text: &str) -> Result<Bytes> {
    let job = Job::parse(text).context("parse job")?;

    // The engine is the base account at the call target; its codehash picks the
    // FastView. A job whose target resolves through the overlay (no inline code)
    // keys on B256::ZERO — such calls never JIT-compile, so sharing is safe.
    let engine_key = job
        .accounts
        .iter()
        .find(|(addr, _)| *addr == job.to)
        .map(|(_, code)| keccak256(code))
        .unwrap_or(B256::ZERO);

    let key = (job.spec, engine_key);
    let idx = match views.iter().position(|(k, _)| *k == key) {
        Some(i) => i,
        None => {
            views.push((key, FastView::new(job.spec).context("new FastView")?));
            views.len() - 1
        }
    };
    let returndata = job
        .execute_with(&mut views[idx].1)
        .context("execute view call")?;

    if let Some(expected) = &job.expected {
        if expected.as_ref() != returndata.as_ref() {
            bail!(
                "returndata mismatch vs expected:\n  expected 0x{}\n  got      0x{}",
                hex_encode(expected),
                hex_encode(&returndata)
            );
        }
    }
    Ok(returndata)
}

/// Read one length-prefixed request frame: a decimal byte-length line, then that
/// many bytes of job text. Returns `Ok(None)` on a clean EOF (no more jobs).
fn read_request<R: BufRead>(reader: &mut R) -> Result<Option<String>> {
    let mut len_line = String::new();
    let n = reader
        .read_line(&mut len_line)
        .context("read request length line")?;
    if n == 0 {
        return Ok(None); // clean EOF between frames
    }
    let trimmed = len_line.trim();
    if trimmed.is_empty() {
        // Tolerate a stray blank line between frames.
        return read_request(reader);
    }
    let len: usize = trimmed
        .parse()
        .with_context(|| format!("parse request length {trimmed:?}"))?;
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .context("read request body")?;
    let text = String::from_utf8(buf).context("request body not utf-8")?;
    Ok(Some(text))
}

/// Write one length-prefixed response frame: `"<STATUS> <len>\n"` then `<len>`
/// bytes of payload, then flush.
fn write_response<W: Write>(writer: &mut W, status: &str, payload: &[u8]) -> Result<()> {
    write!(writer, "{status} {}\n", payload.len()).context("write response header")?;
    writer.write_all(payload).context("write response body")?;
    writer.flush().context("flush response")?;
    Ok(())
}

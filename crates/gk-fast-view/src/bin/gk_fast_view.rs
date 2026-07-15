//! `gk-fast-view` sidecar binary.
//!
//! The revm-31 service/analyzer cannot link revm-41 + revmc in-process (see the
//! Phase-4 report / crate README for the co-link blocker), so the node shells
//! out to this binary when `GK_SHARD_FAST_EXECUTOR=1`. It reads a `.job` (the
//! [`gk_fast_view::job`] text format) from a path argument or stdin, executes
//! the overlay-mounted view call on the revmc-compiled path, and prints the raw
//! returndata as hex to stdout (nothing else on stdout — diagnostics go to
//! stderr). A revert/halt exits non-zero with the reason on stderr, so the node
//! surfaces the failure loudly instead of hash-committing an empty result.
//!
//! Usage:
//!   gk-fast-view [JOB_FILE]      # reads stdin if JOB_FILE is omitted
//!
//! On success stdout is exactly one line: the lowercase hex returndata (no
//! `0x`). If the job carries an `expected` line (a golden fixture), the binary
//! also asserts byte-identity and fails loudly on mismatch.

use std::io::Read;

use anyhow::{Context, Result};
use gk_fast_view::job::{Job, hex_encode};

fn main() {
    if let Err(e) = run() {
        eprintln!("gk-fast-view: error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

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
            anyhow::bail!(
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

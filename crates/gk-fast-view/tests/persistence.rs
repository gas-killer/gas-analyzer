//! Persistent-daemon (`--serve`) amortization + byte-identity gate.
//!
//! Proves the compile-once fix: a SINGLE `gk-fast-view --serve` process
//! JIT-compiles the engine on its first job, then serves every subsequent job on
//! the cached artifact. Two things are asserted:
//!
//!   1. **Byte-identity** — the daemon path returns EXACTLY the same returndata
//!      as the one-shot library path (`Job::execute`) and the committed golden
//!      `expected`. The amortization must not perturb a single byte.
//!   2. **Amortization** — the SECOND job on a `--serve` process is dramatically
//!      faster than the first (the first pays LLVM codegen; the second reuses
//!      it). Measured live and printed; hard-asserted on the real engine.
//!
//! The default test uses the small committed `forward_range.fixture` so it runs
//! in plain `cargo test`. The real-engine money metric (~116s compile → ~1s
//! reuse) is `#[ignore]`d — run it with:
//!   `cargo test --release --test persistence -- --ignored --nocapture`

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use gk_fast_view::job::{Job, hex_encode};
use revm_primitives::{B256, keccak256};

/// A test-side client for the length-prefixed `--serve` wire protocol.
struct Serve {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Serve {
    fn spawn() -> Self {
        let bin = env!("CARGO_BIN_EXE_gk-fast-view");
        let mut child = Command::new(bin)
            .arg("--serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // daemon diagnostics (compile logs) to test output
            .spawn()
            .expect("spawn gk-fast-view --serve");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Serve { child, stdin, stdout }
    }

    /// Send one framed job, read one framed response, return `(status, payload,
    /// wall_time)`. `status` is "OK" (payload = hex returndata) or "ERR".
    fn exchange(&mut self, job_text: &str) -> (String, Vec<u8>, Duration) {
        let t0 = Instant::now();
        // request frame: "<len>\n<bytes>"
        write!(self.stdin, "{}\n", job_text.len()).expect("write len");
        self.stdin.write_all(job_text.as_bytes()).expect("write job");
        self.stdin.flush().expect("flush");
        // response frame: "<STATUS> <len>\n<bytes>"
        let mut header = String::new();
        let n = self.stdout.read_line(&mut header).expect("read header");
        assert!(n > 0, "daemon closed stdout without a response");
        let header = header.trim();
        let (status, len_str) = header
            .split_once(' ')
            .unwrap_or_else(|| panic!("bad response header {header:?}"));
        let len: usize = len_str
            .parse()
            .unwrap_or_else(|_| panic!("bad response length {len_str:?}"));
        let mut payload = vec![0u8; len];
        self.stdout.read_exact(&mut payload).expect("read payload");
        (status.to_string(), payload, t0.elapsed())
    }

    /// Send a job expecting success; return `(returndata, wall_time)`.
    fn call(&mut self, job_text: &str) -> (Vec<u8>, Duration) {
        let (status, payload, dt) = self.exchange(job_text);
        assert_eq!(
            status,
            "OK",
            "daemon returned error frame: {}",
            String::from_utf8_lossy(&payload)
        );
        let hex = std::str::from_utf8(&payload).expect("hex payload utf8");
        let rd = hex::decode(hex.trim()).expect("hex decode");
        (rd, dt)
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        // Closing stdin makes the daemon exit on EOF; then reap it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The daemon path must be byte-identical to the one-shot library path AND the
/// committed golden — and the second job must reuse the compiled engine (no
/// recompile), which we verify by identical returndata + report the timings.
#[test]
fn serve_matches_oneshot_and_reuses_engine() {
    let fixture = include_str!("golden/forward_range.fixture");
    let job = Job::parse(fixture).expect("parse fixture");
    let expected = job.expected.clone().expect("fixture expected");

    // One-shot library path (fresh FastView, compiles from scratch).
    let oneshot = job.execute().expect("one-shot execute");
    assert_eq!(
        hex_encode(&oneshot),
        hex_encode(&expected),
        "one-shot path diverged from golden"
    );

    // Daemon path: same job twice through ONE --serve process.
    let mut serve = Serve::spawn();
    let (rd1, t1) = serve.call(fixture);
    let (rd2, t2) = serve.call(fixture);

    assert_eq!(hex_encode(&rd1), hex_encode(&expected), "daemon job 1 != golden");
    assert_eq!(hex_encode(&rd2), hex_encode(&expected), "daemon job 2 != golden");
    assert_eq!(hex_encode(&rd1), hex_encode(&rd2), "daemon job 2 != job 1 (cache corrupted?)");
    assert_eq!(hex_encode(&rd1), hex_encode(&oneshot), "daemon != one-shot library path");

    let ratio = t2.as_secs_f64() / t1.as_secs_f64().max(1e-9);
    eprintln!(
        "[persistence OK] fixture daemon byte-identical to one-shot + golden ({} bytes)\n\
         [persistence MONEY METRIC] job1={t1:?} (startup+compile+exec)  job2={t2:?} (reuse)  \
         job2 = {:.4}% of job1  ({:.0}x faster)",
        rd1.len(),
        ratio * 100.0,
        1.0 / ratio.max(1e-9),
    );
    // The dramatic-amortization gate: job2 reuses the compiled engine and skips
    // all of the first-job cost (process startup + LLVM init + codegen), so it is
    // an order of magnitude+ faster. On this tiny synthetic engine execution is
    // ~microseconds while the first job is ~hundreds of ms, so the ratio is well
    // under 20% (typically < 1%).
    assert!(
        ratio < 0.20,
        "amortization failed: job2 ({t2:?}) was {:.2}% of job1 ({t1:?}); expected < 20% \
         (compiled engine not reused)",
        ratio * 100.0
    );
}

// ---------------------------------------------------------------------------
// Real-engine amortization (the money metric). #[ignore]d: needs the multi-GB
// weights overlay and ~2 min (one real LLVM codegen of the ~20KB seg engine).
// Run: cargo test --release --test persistence -- --ignored --nocapture
// ---------------------------------------------------------------------------

const PROMPT_IDS: &[u32] = &[
    151644, 872, 198, 3838, 374, 33946, 30, 151645, 198, 151644, 77091, 198, 151667, 271, 151668,
    271,
];

const PACKED_CONFIG_HEX: [&str; 3] = [
    "04000c001c100800800002518004000101000000000000000000000000000000",
    "0000000010c6f7a10000000016a09e6600000000239791f10000000000000000",
    "00182bc20002505d0002505b0000000000000000000000000000000000000000",
];

fn word(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

/// Hand-rolled `forwardRange(...)` ABI encoding — the same shape `real_repro`
/// and the live sidecar use (ported here so the test is self-contained).
fn encode_forward_range(
    manifest: B256,
    packed: [[u8; 32]; 3],
    max_pos: u64,
    pos_hi: u64,
    layer_hi: u64,
    token_ids: &[u32],
) -> Vec<u8> {
    let sig = "forwardRange(address,bytes32,bytes32[3],((uint256,uint256,uint256,uint256,uint256),uint32[],bytes,bytes,bytes32,bytes32))";
    let selector = &keccak256(sig.as_bytes())[..4];
    let empty_keccak = keccak256([]);

    let q_head_words = 10u64;
    let tokenids_off = q_head_words * 32;
    let tokenids_len_words = 1 + token_ids.len() as u64;
    let xin_off = tokenids_off + tokenids_len_words * 32;
    let kvin_off = xin_off + 32;

    let mut q = Vec::new();
    q.extend_from_slice(&word(max_pos));
    q.extend_from_slice(&word(0)); // posLo
    q.extend_from_slice(&word(pos_hi));
    q.extend_from_slice(&word(0)); // layerLo
    q.extend_from_slice(&word(layer_hi));
    q.extend_from_slice(&word(tokenids_off));
    q.extend_from_slice(&word(xin_off));
    q.extend_from_slice(&word(kvin_off));
    q.extend_from_slice(empty_keccak.as_slice());
    q.extend_from_slice(empty_keccak.as_slice());
    q.extend_from_slice(&word(token_ids.len() as u64));
    for &t in token_ids {
        q.extend_from_slice(&word(t as u64));
    }
    q.extend_from_slice(&word(0)); // xIn tail
    q.extend_from_slice(&word(0)); // kvIn tail

    let q_off = 6u64 * 32;
    let mut out = Vec::new();
    out.extend_from_slice(selector);
    out.extend_from_slice(&word(0)); // rootDirectory = address(0) (overlay mode)
    out.extend_from_slice(manifest.as_slice());
    out.extend_from_slice(&packed[0]);
    out.extend_from_slice(&packed[1]);
    out.extend_from_slice(&packed[2]);
    out.extend_from_slice(&word(q_off));
    out.extend_from_slice(&q);
    out
}

#[test]
#[ignore = "needs multi-GB weights overlay + ~2min (one real engine LLVM codegen)"]
fn serve_amortizes_real_engine_compile() {
    let repro = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../repro");
    let engine_hex = repro.join("bytecode/engine06.hex");
    let weights = repro.join("overlays/qwen06/weights.bin");
    let tokenizer = repro.join("overlays/qwen06/tokenizer.bin");
    for p in [&engine_hex, &weights, &tokenizer] {
        assert!(p.exists(), "missing repro input {}", p.display());
    }

    let engine_code_hex = std::fs::read_to_string(&engine_hex).unwrap();
    let engine_code_hex = engine_code_hex.trim().trim_start_matches("0x");
    let manifest = B256::from_slice(
        &hex::decode("23216cb9ed9ef2b4bc20c84d27b68fa62ab194fc0845dfa707836f48ec4a7ae9").unwrap(),
    );
    let to = "18C8b1677a731f7507ea51D99e23e513D9613Aa4";

    let mut packed = [[0u8; 32]; 3];
    for (i, s) in PACKED_CONFIG_HEX.iter().enumerate() {
        packed[i].copy_from_slice(&hex::decode(s).unwrap());
    }
    let calldata = encode_forward_range(manifest, packed, 16, 16, 7, PROMPT_IDS);

    // Build the gk_fast_view::job text (overlay mode, UnboundedV1Xl @ 2^40 gas —
    // matching the live sidecar shape).
    let gas: u64 = 1 << 40;
    let job_text = format!(
        "spec CANCUN\n\
         profile UnboundedV1Xl\n\
         from {}\n\
         to {}\n\
         input {}\n\
         gas {gas}\n\
         account {} {}\n\
         mount_files {} {} {}\n",
        "0".repeat(40),
        to.to_ascii_lowercase(),
        hex::encode(&calldata),
        to.to_ascii_lowercase(),
        engine_code_hex,
        weights.display(),
        tokenizer.display(),
        hex::encode(manifest.as_slice()),
    );
    // Sanity: it parses.
    Job::parse(&job_text).expect("real job parses");

    let mut serve = Serve::spawn();
    // Three jobs: job1 pays the one-time engine codegen; jobs 2 & 3 must reuse
    // the compiled artifact. steady-state exec = min(t2, t3), so the compile
    // amortized away on every segment after the first is (t1 - steady).
    eprintln!("[real amortize] job 1 (one-time LLVM codegen of the seg engine + exec)...");
    let (rd1, t1) = serve.call(&job_text);
    eprintln!("[real amortize] job 1 done: {} bytes in {t1:?}", rd1.len());
    eprintln!("[real amortize] job 2 (must reuse compiled engine)...");
    let (rd2, t2) = serve.call(&job_text);
    eprintln!("[real amortize] job 2 done: {} bytes in {t2:?}", rd2.len());
    eprintln!("[real amortize] job 3 (must reuse compiled engine)...");
    let (rd3, t3) = serve.call(&job_text);
    eprintln!("[real amortize] job 3 done: {} bytes in {t3:?}", rd3.len());

    // Byte-identity of the reused-cache path is non-negotiable.
    assert_eq!(hex_encode(&rd1), hex_encode(&rd2), "real engine: job 2 != job 1");
    assert_eq!(hex_encode(&rd1), hex_encode(&rd3), "real engine: job 3 != job 1");

    let steady = t2.min(t3);
    let compile_amortized = t1.saturating_sub(steady);
    eprintln!(
        "\n===== COMPILE-ONCE MONEY METRIC (real 0.6B seg engine, {} bytes returndata) =====\n\
         job1  cold  (compile + exec): {t1:?}\n\
         job2  warm  (reuse + exec):   {t2:?}\n\
         job3  warm  (reuse + exec):   {t3:?}\n\
         steady-state exec:            {steady:?}\n\
         LLVM codegen amortized away every segment after the first: {compile_amortized:?}\n\
         (the OLD spawn-per-segment paid this codegen on EVERY segment; the daemon pays it ONCE)\n\
         ================================================================================",
        rd1.len(),
    );
    // Robust, hardware-independent invariants: the warm jobs skip the codegen, so
    // they are strictly faster than the cold job, and the amortized codegen is a
    // real, measurable cost (hundreds of ms+ for a ~20KB engine that unrolls to
    // huge native code). A fixed per-segment saving x N segments is the win.
    assert!(
        t2 < t1 && t3 < t1,
        "warm jobs ({t2:?}, {t3:?}) not faster than cold job ({t1:?}) — engine was recompiled"
    );
    assert!(
        compile_amortized >= Duration::from_millis(200),
        "amortized codegen {compile_amortized:?} implausibly small — is the engine really reused?"
    );
}

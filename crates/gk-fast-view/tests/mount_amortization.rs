//! MOUNT + STATE amortization gate (the second half of the daemon's cost fix).
//!
//! `tests/persistence.rs` proves the *compile* is amortized across jobs. This
//! test proves the other two per-job costs are amortized too: the overlay mount
//! **verify** (`OverlayMount::from_files`'s streaming keccak over the whole
//! weights blob) and the overlay/base **state resolution**. A persistent
//! `FastView` must run the verify ONCE per distinct overlay and serve every
//! subsequent identical job from a warm read-through state cache.
//!
//! Self-contained: it synthesizes a multi-megabyte weights blob on disk so the
//! streaming-keccak verify is a real, measurable cost, mounts it via
//! `mount_files`, and runs an `EXTCODECOPY`-an-overlay-chunk engine twice through
//! ONE `FastView`. It asserts:
//!   1. both jobs' returndata are byte-identical to each other AND to the golden
//!      computed directly from the blob (amortization must not perturb a byte);
//!   2. the mount verify ran exactly once and the warm state cache was built
//!      exactly once (deterministic proof, via `FastView`'s build counters);
//!   3. the 2nd job's wall-time is dramatically lower than the 1st (the money
//!      metric — printed, and hard-gated).
//!
//! The engine compile is pre-warmed before timing so the measured 1st-vs-2nd
//! delta isolates the MOUNT + STATE amortization (compile amortization is
//! covered by `persistence.rs`).

use std::path::PathBuf;
use std::time::Instant;

use gk_fast_view::job::{Job, hex_encode};
use gk_fast_view::{FastView, overlay_chunk_address, overlay_manifest_hash};
use revm_primitives::hardfork::SpecId;

/// Synthetic weights blob: big enough that the streaming-keccak verify over it
/// is a clearly-measurable cost (so amortizing it is visible), small enough that
/// the test stays fast in both debug and release.
const WEIGHTS_LEN: usize = 24 * 1024 * 1024;

/// Deterministic filler byte for position `i` (prime modulus, so it isn't a
/// trivially-compressible/constant pattern).
fn filler(i: usize) -> u8 {
    (i % 251) as u8
}

/// Minimal engine runtime bytecode: `EXTCODECOPY(chunk0_addr, 0, 0, 64)` then
/// `RETURN(0, 64)` — copies the first 64 bytes of the overlay chunk-0 account's
/// code into memory and returns them. Forces the overlay chunk to be resolved
/// (the state read we want to see amortized).
fn extcodecopy_engine(chunk0: &[u8; 20]) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&[0x60, 0x40]); // PUSH1 0x40   size (μs[3])
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0x00   codeOffset (μs[2])
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0x00   destOffset (μs[1])
    code.push(0x73); // PUSH20 <addr>                       address (μs[0], top)
    code.extend_from_slice(chunk0);
    code.push(0x3c); // EXTCODECOPY
    code.extend_from_slice(&[0x60, 0x40]); // PUSH1 0x40   return size
    code.extend_from_slice(&[0x60, 0x00]); // PUSH1 0x00   return offset
    code.push(0xf3); // RETURN
    code
}

struct TempBlobs {
    dir: PathBuf,
    weights: PathBuf,
    tokenizer: PathBuf,
}

impl TempBlobs {
    fn create(weights: &[u8], tokenizer: &[u8]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "gkfv_amort_{}_{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let w = dir.join("weights.bin");
        let t = dir.join("tokenizer.bin");
        std::fs::write(&w, weights).expect("write weights");
        std::fs::write(&t, tokenizer).expect("write tokenizer");
        TempBlobs {
            dir,
            weights: w,
            tokenizer: t,
        }
    }
}

impl Drop for TempBlobs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn mount_and_state_amortize_across_jobs() {
    // 1. Synthesize the overlay blobs on disk.
    let weights: Vec<u8> = (0..WEIGHTS_LEN).map(filler).collect();
    let tokenizer: Vec<u8> = b"gk-fast-view-amortization-tokenizer-blob"
        .iter()
        .cycle()
        .take(4096)
        .copied()
        .collect();
    let blobs = TempBlobs::create(&weights, &tokenizer);

    // 2. Derive the manifest + chunk-0 address exactly as the mount will.
    let manifest = overlay_manifest_hash(&weights, &tokenizer);
    let chunk0_addr = overlay_chunk_address(manifest, 0);

    // 3. The golden returndata: chunk-0 code is `0x00 || weights[..PAYLOAD]`, so
    //    the first 64 bytes are `[0x00] ++ weights[0..63]`.
    let mut golden = Vec::with_capacity(64);
    golden.push(0x00);
    for i in 0..63 {
        golden.push(filler(i));
    }

    // 4. Build the engine + the job text (overlay mode, mmap-backed mount).
    let engine_addr: [u8; 20] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        0x01, 0x02, 0x03, 0x04, 0x05,
    ];
    let mut chunk0_bytes = [0u8; 20];
    chunk0_bytes.copy_from_slice(chunk0_addr.as_slice());
    let engine_code = extcodecopy_engine(&chunk0_bytes);

    let job_text = format!(
        "spec CANCUN\n\
         profile UnboundedV1\n\
         from {}\n\
         to {}\n\
         input\n\
         gas 5000000\n\
         account {} {}\n\
         mount_files {} {} {}\n",
        "0".repeat(40),
        hex_encode(&engine_addr),
        hex_encode(&engine_addr),
        hex_encode(&engine_code),
        blobs.weights.display(),
        blobs.tokenizer.display(),
        hex_encode(manifest.as_slice()),
    );
    let job = Job::parse(&job_text).expect("parse amortization job");

    // 5. ONE FastView across both jobs. Pre-warm the compile so the measured
    //    1st-vs-2nd delta is purely the MOUNT verify + STATE resolution.
    let mut fv = FastView::new(SpecId::CANCUN).expect("FastView");
    fv.warm_code(&engine_code).expect("pre-warm compile");
    assert_eq!(fv.compiled_count(), 1, "engine pre-compiled once");

    let t0 = Instant::now();
    let rd1 = job.execute_with(&mut fv).expect("job 1");
    let t1 = t0.elapsed();

    let t0 = Instant::now();
    let rd2 = job.execute_with(&mut fv).expect("job 2");
    let t2 = t0.elapsed();

    // --- Byte-identity (non-negotiable) ---
    assert_eq!(hex_encode(&rd1), hex_encode(&golden), "job 1 != golden");
    assert_eq!(hex_encode(&rd2), hex_encode(&golden), "job 2 != golden");
    assert_eq!(
        hex_encode(&rd1),
        hex_encode(&rd2),
        "job 2 != job 1 (warm cache corrupted the result?)"
    );

    // --- Deterministic amortization proof ---
    assert_eq!(
        fv.mount_builds(),
        1,
        "overlay verify must run exactly ONCE across the two jobs (mount not cached)"
    );
    assert_eq!(
        fv.warm_builds(),
        1,
        "warm state cache must be built exactly ONCE across the two jobs (state not cached)"
    );
    assert_eq!(fv.compiled_count(), 1, "no recompile happened");

    // --- Money metric (printed + hard-gated) ---
    let ratio = t2.as_secs_f64() / t1.as_secs_f64().max(1e-9);
    eprintln!(
        "\n===== MOUNT + STATE AMORTIZATION (synthetic {} MiB overlay, {} bytes returndata) =====\n\
         job1  cold  (verify {} MiB blob + resolve overlay chunk + exec): {t1:?}\n\
         job2  warm  (mount reused, chunk read served from warm cache + exec): {t2:?}\n\
         job2 = {:.3}% of job1  ({:.0}x faster)\n\
         mount verify runs: {}   warm-cache builds: {}\n\
         (the OLD per-job path re-verified the whole blob + re-resolved state EVERY job)\n\
         =============================================================================",
        WEIGHTS_LEN / (1024 * 1024),
        rd1.len(),
        WEIGHTS_LEN / (1024 * 1024),
        ratio * 100.0,
        1.0 / ratio.max(1e-9),
        fv.mount_builds(),
        fv.warm_builds(),
    );
    assert!(
        t2 < t1,
        "warm job2 ({t2:?}) not faster than cold job1 ({t1:?}) — mount/state not amortized"
    );
    // The verify over a multi-MiB blob dominates the cold job while the warm job
    // is a trivial re-execution, so job2 is well under half of job1.
    assert!(
        t2.saturating_mul(2) < t1,
        "amortization not dramatic: job2 ({t2:?}) was {:.2}% of job1 ({t1:?}); expected < 50%",
        ratio * 100.0
    );
}

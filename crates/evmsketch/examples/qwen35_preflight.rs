//! Local pre-flight for the 35 GB Qwen3.5-A3B mmap overlay (gas-analyzer#172).
//!
//! This is **not** a correctness proof of model output — that is the
//! Solidity engine's job (integer-reference parity tests, run against a
//! deployed chain). This binary proves the narrower, deploy-blocking claim:
//! the mmap-backed [`OverlayMount::from_files`] mounts the *real* 34.7 GB
//! `weights.bin` without loading it into heap, the manifest verifies against
//! the pinned on-chain commitment, and a sample of chunk addresses/code
//! matches the on-chain derivation bit-for-bit — all while steady-state
//! physical memory footprint stays far below the file size, which is the
//! entire point of the mmap path (see `crates/evmsketch/src/overlay_mount.rs`
//! module docs). See [`memory_bytes`] for why "physical footprint" and not
//! `getrusage().ru_maxrss` is the metric asserted against — the two diverge
//! by >100x on macOS for this workload.
//!
//! Not part of `cargo test` / `cargo build` — examples are opt-in
//! (`cargo run --example`), so this never runs in normal CI, and it will
//! fail fast if the artifact directory isn't present (a developer machine
//! with the real weights checked out, not a CI runner).
//!
//! Usage:
//!   cargo run -p gas-analyzer-evmsketch --release --example qwen35_preflight
//!
//! Optional env override (defaults to the path in the task brief):
//!   QWEN35_ARTIFACTS_DIR=/path/to/artifacts cargo run ... --example qwen35_preflight

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Instant;

use alloy::primitives::{B256, keccak256};
use gas_analyzer_core::{OVERLAY_CHUNK_PAYLOAD, overlay_chunk_address};
use gas_analyzer_evmsketch::OverlayMount;
use rand::Rng;
use rand::SeedableRng;
use rand::rngs::StdRng;

/// Pinned on-chain manifest for the Qwen3.5-35B-A3B artifact set (from the
/// deploy brief / `artifacts/manifest.json`).
const PINNED_MANIFEST: &str = "0x7bdf4876a6861287521dadab3d3870f74dfa557507ed200d49f75bcb09f01fa9";

/// Number of randomly sampled chunk addresses to touch — enough to exercise
/// the mmap path across the whole 34.7 GB file (not just the front, which
/// the page cache would make trivially fast) while keeping the run under a
/// few minutes on spinning disk-backed mmap.
const SAMPLE_COUNT: usize = 10_000;

/// Memory budget this pre-flight asserts against. The mmap source's own memo
/// is bounded (`DEFAULT_OVERLAY_MEMO_CHUNKS` = 1024 chunks ≈ 25 MB of raw
/// code), so touching 10k *distinct* addresses (memo evicts well before
/// that) must stay far under the 34.7 GB file size. 4 GB leaves generous
/// headroom for the mmap'd page-table bookkeeping, the address index
/// (~1.43M entries), and process/runtime overhead.
///
/// This budget is checked against the **post-mount, steady-state** figure
/// (see [`memory_bytes`] below), not the transient high-water mark during
/// the mount's own streaming-hash pass, which necessarily reads every byte
/// of the file once (that's the manifest verification, not a leak).
const MEMORY_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// True memory-pressure contribution of this process right now.
///
/// `getrusage().ru_maxrss` is a **high-water mark**, and on this workload it
/// is actively misleading: `OverlayMount::from_files`'s mandatory streaming
/// keccak verify sequentially reads the entire mmapped file once, and on
/// macOS the kernel counts every clean, evictable, file-backed page it has
/// ever faulted in against `ru_maxrss` — it does not proactively reclaim
/// them absent memory pressure. Measured directly: after mounting the real
/// 32.33 GiB `weights.bin`, `ru_maxrss` read ~32.5 GiB while `vmmap
/// -summary`'s "Physical footprint" (Apple's own true-memory-pressure
/// metric, matching Activity Monitor) read ~249 MiB for the same process at
/// the same instant — a >130x gap, entirely clean page-cache pages the OS
/// is free to drop.
///
/// So: on macOS this reads `phys_footprint` via `proc_pid_rusage`
/// (`RUSAGE_INFO_V2`) — the same counter `vmmap`/Activity Monitor/the OOM
/// killer use — which correctly excludes reclaimable file-backed pages. On
/// Linux, `ru_maxrss` is used directly (KiB, converted to bytes); Linux's
/// mmap page-cache accounting does not fold into a single process's
/// `ru_maxrss` the way Darwin's resident-page accounting does, so it is not
/// subject to the same one-shot-sequential-read inflation for this
/// workload.
fn memory_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    unsafe {
        // `rusage_info_t` is typedef'd `void *`, but despite that the real
        // calling convention (confirmed against a minimal C repro — the
        // naive "pointer to a `rusage_info_t`" reading of the typedef
        // silently writes through the wrong indirection and comes back
        // all-zero) is single indirection: pass the struct's own address
        // cast directly to `rusage_info_t`, matching what the kernel
        // actually dereferences.
        let mut info: libc::rusage_info_v2 = std::mem::zeroed();
        let ret = libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V2,
            (&mut info as *mut libc::rusage_info_v2).cast(),
        );
        assert_eq!(ret, 0, "proc_pid_rusage(RUSAGE_INFO_V2) failed");
        info.ri_phys_footprint
    }
    #[cfg(not(target_os = "macos"))]
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        let ret = libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        assert_eq!(ret, 0, "getrusage failed");
        let raw = usage.ru_maxrss as u64;
        if cfg!(target_os = "linux") {
            raw * 1024
        } else {
            raw
        }
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

fn artifacts_dir() -> PathBuf {
    std::env::var("QWEN35_ARTIFACTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Users/wk/conductor/workspaces/solidity-sdk/monterrey-v3/artifacts")
        })
}

fn main() {
    let dir = artifacts_dir();
    let weights_path = dir.join("weights.bin");
    let tokenizer_path = dir.join("tokenizer.bin");

    for (label, path) in [
        ("weights.bin", &weights_path),
        ("tokenizer.bin", &tokenizer_path),
    ] {
        assert!(
            path.is_file(),
            "expected {label} at {} — set QWEN35_ARTIFACTS_DIR to override",
            path.display()
        );
    }

    let weights_len = std::fs::metadata(&weights_path)
        .expect("stat weights.bin")
        .len();
    let tokenizer_len = std::fs::metadata(&tokenizer_path)
        .expect("stat tokenizer.bin")
        .len();
    println!("== Qwen3.5-35B-A3B mmap overlay pre-flight ==");
    println!("artifacts dir : {}", dir.display());
    println!(
        "weights.bin   : {} ({} bytes)",
        human_bytes(weights_len),
        weights_len
    );
    println!(
        "tokenizer.bin : {} ({} bytes)",
        human_bytes(tokenizer_len),
        tokenizer_len
    );

    // QWEN35_MANIFEST lets this binary be smoke-tested against small
    // synthetic blobs (its own manifest) before pointing at the real 35 GB
    // artifacts with the pinned on-chain default.
    let manifest_str =
        std::env::var("QWEN35_MANIFEST").unwrap_or_else(|_| PINNED_MANIFEST.to_string());
    let pinned_manifest =
        B256::from_str(&manifest_str).expect("manifest must be a valid 32-byte hex hash");
    println!("pinned manifest: {pinned_manifest}");

    let footprint_before = memory_bytes();
    println!(
        "physical footprint before mount: {}",
        human_bytes(footprint_before)
    );

    // ------------------------------------------------------------------
    // 1. Mount: streaming keccak verify + address index build. This is the
    //    operation that must NOT require 34.7 GB of heap. It DOES read
    //    every byte of the file once (that's the manifest verification),
    //    so a transient bump here is expected and not what's being
    //    asserted against the budget — see the steady-state figure after
    //    sampling, below.
    // ------------------------------------------------------------------
    let mount_start = Instant::now();
    let mount = OverlayMount::from_files(&weights_path, &tokenizer_path, pinned_manifest).expect(
        "OverlayMount::from_files failed — manifest verification or file I/O error; \
             see the printed cause above",
    );
    let mount_elapsed = mount_start.elapsed();

    let footprint_after_mount = memory_bytes();
    println!(
        "\nMount complete in {:.2}s ({:.1} MB/s over {})",
        mount_elapsed.as_secs_f64(),
        (weights_len + tokenizer_len) as f64 / mount_elapsed.as_secs_f64() / 1_000_000.0,
        human_bytes(weights_len + tokenizer_len),
    );
    println!("chunk_count()     : {}", mount.chunk_count());
    println!("mount.manifest()  : {}", mount.manifest());
    assert_eq!(
        mount.manifest(),
        pinned_manifest,
        "OverlayMount reports a manifest different from the pinned one — should be unreachable, \
         from_files already verifies internally"
    );
    println!(
        "physical footprint after mount : {}",
        human_bytes(footprint_after_mount)
    );

    // ------------------------------------------------------------------
    // 2. Chunk-count sanity vs the published manifest.json
    //    (1,412,601 weight chunks + 116 tokenizer chunks = 1,412,717).
    // ------------------------------------------------------------------
    let expected_weight_chunks = weights_len.div_ceil(OVERLAY_CHUNK_PAYLOAD as u64) as usize;
    let expected_tokenizer_chunks = tokenizer_len.div_ceil(OVERLAY_CHUNK_PAYLOAD as u64) as usize;
    let expected_total = expected_weight_chunks + expected_tokenizer_chunks;
    println!(
        "expected chunks   : {expected_weight_chunks} (weights) + {expected_tokenizer_chunks} \
         (tokenizer) = {expected_total}"
    );
    assert_eq!(
        mount.chunk_count(),
        expected_total,
        "chunk_count() must equal ceil(weights/{OVERLAY_CHUNK_PAYLOAD}) + \
         ceil(tokenizer/{OVERLAY_CHUNK_PAYLOAD})"
    );

    // ------------------------------------------------------------------
    // 3. Sample SAMPLE_COUNT random chunk indices spread across the WHOLE
    //    file (not just the head, which the OS page cache would make free)
    //    and verify: (a) the derived address matches the on-chain formula,
    //    (b) account_code() returns Some, (c) the returned code has the
    //    STOP-prefixed shape the on-chain engine expects, and (d) the
    //    keccak(code) hash matches what account_code() reports.
    // ------------------------------------------------------------------
    let mut rng = StdRng::seed_from_u64(0x000C_0FFE_E35B_u64);
    let mut sample_indices: Vec<u64> = (0..SAMPLE_COUNT)
        .map(|_| rng.random_range(0..expected_total as u64))
        .collect();
    sample_indices.sort_unstable();
    sample_indices.dedup();
    println!(
        "\nSampling {} distinct chunk indices out of {expected_total} ({:.3}% coverage)...",
        sample_indices.len(),
        100.0 * sample_indices.len() as f64 / expected_total as f64
    );

    let sample_start = Instant::now();
    let mut checked = 0usize;
    for &idx in &sample_indices {
        let expected_addr = overlay_chunk_address(pinned_manifest, idx);
        assert!(
            mount.contains(&expected_addr),
            "chunk {idx}: derived address {expected_addr} not found in the mounted index"
        );
        let (code_hash, bytecode) = mount.account_code(&expected_addr).unwrap_or_else(|| {
            panic!("chunk {idx}: account_code returned None for {expected_addr}")
        });
        let code = bytecode.original_bytes();

        assert!(!code.is_empty(), "chunk {idx}: code must not be empty");
        assert_eq!(
            code[0], 0x00,
            "chunk {idx}: code must be STOP-prefixed (0x00 || payload)"
        );
        let payload_len = code.len() - 1;
        let is_last_weight_chunk = idx == expected_weight_chunks as u64 - 1;
        let is_last_chunk = idx == expected_total as u64 - 1;
        if idx < expected_weight_chunks as u64 {
            let remaining = weights_len - (idx * OVERLAY_CHUNK_PAYLOAD as u64);
            let expected_payload_len = if is_last_weight_chunk {
                remaining as usize
            } else {
                OVERLAY_CHUNK_PAYLOAD
            };
            assert_eq!(
                payload_len, expected_payload_len,
                "weight chunk {idx}: payload length mismatch"
            );
        } else {
            let tok_idx = idx - expected_weight_chunks as u64;
            let remaining = tokenizer_len - (tok_idx * OVERLAY_CHUNK_PAYLOAD as u64);
            let expected_payload_len = if is_last_chunk {
                remaining as usize
            } else {
                OVERLAY_CHUNK_PAYLOAD
            };
            assert_eq!(
                payload_len, expected_payload_len,
                "tokenizer chunk {idx} (tok-local {tok_idx}): payload length mismatch"
            );
        }
        assert!(
            payload_len <= OVERLAY_CHUNK_PAYLOAD,
            "chunk {idx}: payload {payload_len} exceeds OVERLAY_CHUNK_PAYLOAD"
        );
        assert_eq!(
            code_hash,
            keccak256(code.as_ref()),
            "chunk {idx}: reported code_hash must equal keccak256(code)"
        );

        checked += 1;
        if checked.is_multiple_of(2_000) {
            let footprint_now = memory_bytes();
            println!(
                "  ...{checked}/{} chunks verified, physical footprint so far: {}",
                sample_indices.len(),
                human_bytes(footprint_now)
            );
        }
    }
    let sample_elapsed = sample_start.elapsed();

    // Steady-state figure: the mount's one-time full-file streaming-hash
    // read is long done by now, so this reflects what the mmap source
    // actually holds resident while serving chunk lookups — the number the
    // 4 GB budget is about.
    let footprint_final = memory_bytes();

    println!("\n== Results ==");
    println!("weight chunks total      : {expected_weight_chunks}");
    println!("tokenizer chunks total   : {expected_tokenizer_chunks}");
    println!("chunks indexed (mount)   : {}", mount.chunk_count());
    println!("chunks sampled+verified  : {checked}");
    println!(
        "mount (verify+index) time: {:.2}s",
        mount_elapsed.as_secs_f64()
    );
    println!(
        "sample touch+verify time : {:.2}s",
        sample_elapsed.as_secs_f64()
    );
    println!(
        "total wall time          : {:.2}s",
        mount_start.elapsed().as_secs_f64()
    );
    println!(
        "physical footprint before mount : {}",
        human_bytes(footprint_before)
    );
    println!(
        "physical footprint after mount  : {}",
        human_bytes(footprint_after_mount)
    );
    println!(
        "physical footprint steady-state : {}",
        human_bytes(footprint_final)
    );
    println!(
        "memory budget (steady-state)    : {} ({})",
        human_bytes(MEMORY_BUDGET_BYTES),
        if footprint_final < MEMORY_BUDGET_BYTES {
            "PASS"
        } else {
            "FAIL"
        }
    );
    println!(
        "manifest verification    : PASS (OverlayMount::from_files recomputed and matched {pinned_manifest})"
    );
    println!(
        "address/code derivation  : PASS ({checked}/{checked} sampled chunks matched on-chain formula)"
    );

    assert!(
        footprint_final < MEMORY_BUDGET_BYTES,
        "steady-state physical footprint {} exceeded the {} budget — the mmap path is not \
         staying bounded",
        human_bytes(footprint_final),
        human_bytes(MEMORY_BUDGET_BYTES)
    );

    println!("\nPRE-FLIGHT PASSED: mmap overlay path is sound at 35 GB scale.");
}

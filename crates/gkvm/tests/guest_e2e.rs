//! End-to-end runner tests against the committed guest fixtures.
//!
//! The fixtures are real guest ELFs, committed so `cargo test` needs no
//! cross-toolchain: `hello-rs.elf` (rustc + rust-lld via the succinct
//! toolchain) and `hello-c.elf` / `bench-c.elf` / `artifact-probe-c.elf`
//! (riscv64-unknown-elf-gcc + GNU ld, linking gk-guest-crt). Rebuild them
//! with `make -C guest rust docker-c fixtures`; the hello twins implement
//! identical behavior, which is what turns the doc's "toolchain ABI
//! mismatch" risk into a checked property. `hello-py.elf` is the third twin:
//! hello.py frozen into the MicroPython gkvm port (`make -C guest/micropython
//! fetch docker fixtures`).

use alloy_primitives::{B256, Keccak256, b256, keccak256};
use gas_analyzer_gkvm::{
    ArtifactMountV3, EXEC_TIER, GkVmJob, GkVmOutcome, LoadedGuestProgram, constants, manifest, run,
};

fn fixture(name: &str) -> LoadedGuestProgram {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let elf = std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    let hash = keccak256(&elf);
    LoadedGuestProgram::from_bytes(elf, hash, &path).expect("fixture ELF loads")
}

fn run_simple(program: &LoadedGuestProgram, payload: &[u8], cycle_limit: u64) -> GkVmOutcome {
    let job = GkVmJob {
        program: program.program.clone(),
        payload,
        artifact: None,
        schedule: &[],
        cycle_limit,
    };
    run(&job).expect("environment must not fail").outcome
}

const HELLO_TAG: &[u8] = b"GKVM-HELLO-V1\n";

#[test]
fn both_hello_toolchains_produce_the_identical_answer() {
    let payload = [0x11u8, 0x22, 0x33, 0x44];
    let expected: Vec<u8> = HELLO_TAG
        .iter()
        .copied()
        .chain(payload.iter().rev().copied())
        .collect();
    for name in ["hello-rs.elf", "hello-c.elf", "hello-py.elf"] {
        let outcome = run_simple(&fixture(name), &payload, u64::MAX);
        assert_eq!(
            outcome,
            GkVmOutcome::Ok {
                output: expected.clone()
            },
            "{name} must answer the tag plus the reversed payload"
        );
    }
}

#[test]
fn cycle_counts_are_stable_across_repeated_runs() {
    // The in-process half of the determinism matrix: identical counts and
    // output on every run. The cross-tier and cross-toolchain halves live in
    // scripts/m1-matrix.sh, which runs both compiled tiers.
    let program = fixture("hello-c.elf");
    let payload = b"determinism".to_vec();
    let job = GkVmJob {
        program: program.program.clone(),
        payload: &payload,
        artifact: None,
        schedule: &[],
        cycle_limit: u64::MAX,
    };
    let first = run(&job).expect("run");
    for _ in 0..9 {
        let next = run(&job).expect("run");
        assert_eq!(
            next.cycles, first.cycles,
            "instruction counts must not vary"
        );
        assert_eq!(next.outcome, first.outcome, "outputs must not vary");
    }
}

/// keccak256 of the committed `hello-py.elf` — interpreter and frozen script
/// in one image, so this moves with either.
const HELLO_PY_PROGRAM_HASH: B256 =
    b256!("0x4055c9d63f25f2c67dc5f3bc8df3932d433d49a8f4e42cdf3220f631852374e7");

/// Instructions retired by `hello-py.elf` on the 4-byte payload, 16 MiB heap.
const HELLO_PY_CYCLES: u64 = 593_442;

#[test]
fn hello_py_answers_with_stable_pinned_counts() {
    // A whole interpreter — GC, heap init, frozen-module import — sits between
    // the payload and the answer here, so the count is pinned, not only
    // compared run to run. The cross-tier half lives in scripts/m1-matrix.sh.
    let program = fixture("hello-py.elf");
    assert_eq!(
        program.hash, HELLO_PY_PROGRAM_HASH,
        "hello-py.elf moved: rebuild it reproducibly and re-pin hash + count together"
    );
    let payload = [0x11u8, 0x22, 0x33, 0x44];
    let expected: Vec<u8> = HELLO_TAG
        .iter()
        .copied()
        .chain(payload.iter().rev().copied())
        .collect();
    let job = GkVmJob {
        program: program.program.clone(),
        payload: &payload,
        artifact: None,
        schedule: &[],
        cycle_limit: u64::MAX,
    };
    for i in 0..10 {
        let report = run(&job).expect("run");
        // One line per run under `--nocapture`: the tier is a compile-time
        // choice, so this is how a log shows which one produced the count.
        eprintln!(
            "hello-py run {i}: tier={} cycles={} output=0x{}",
            EXEC_TIER.as_str(),
            report.cycles,
            alloy_primitives::hex::encode(match &report.outcome {
                GkVmOutcome::Ok { output } => output.as_slice(),
                _ => &[],
            })
        );
        assert_eq!(report.cycles, HELLO_PY_CYCLES, "run {i}: count moved");
        assert_eq!(
            report.outcome,
            GkVmOutcome::Ok {
                output: expected.clone()
            },
            "run {i}: answer moved"
        );
    }
}

#[test]
fn the_cycle_budget_verdict_is_exact() {
    // bench-c executes a known instruction count (measured once, asserted
    // here): the budget boundary must sit exactly on it.
    let program = fixture("bench-c.elf");
    let n: u64 = 1000;
    let payload = n.to_be_bytes();

    let GkVmOutcome::Ok { .. } = run_simple(&program, &payload, u64::MAX) else {
        panic!("bench must succeed unbounded");
    };
    let job = GkVmJob {
        program: program.program.clone(),
        payload: &payload,
        artifact: None,
        schedule: &[],
        cycle_limit: u64::MAX,
    };
    let exact = run(&job).expect("run").cycles;

    // At exactly the consumed count the guest fits its budget…
    assert!(matches!(
        run_simple(&program, &payload, exact),
        GkVmOutcome::Ok { .. }
    ));
    // …one cycle less is OutOfCycles, and the report says how far it got.
    match run_simple(&program, &payload, exact - 1) {
        GkVmOutcome::OutOfCycles { used, limit } => {
            assert_eq!(limit, exact - 1);
            assert!(
                used >= limit,
                "the run must have been observed at or past the limit"
            );
        }
        other => panic!("expected OutOfCycles, got {other:?}"),
    }
}

#[test]
fn a_guest_abort_carries_its_code_and_message() {
    // bench-c calls gk_abort on a malformed payload; the GKTRAP01 frame must
    // come back as the typed trap it encodes.
    let outcome = run_simple(&fixture("bench-c.elf"), b"short", u64::MAX);
    match outcome {
        GkVmOutcome::Trap { code, data } => {
            assert_eq!(code, 1, "bench aborts with its own code 1");
            assert_eq!(data, b"bench wants a u64 BE iteration count");
        }
        other => panic!("expected a trap, got {other:?}"),
    }
}

#[test]
fn an_oversized_payload_never_reaches_the_guest() {
    let program = fixture("hello-c.elf");
    let payload = vec![0u8; constants::GKVM_INPUT_BYTES_CAP + 1];
    let job = GkVmJob {
        program: program.program.clone(),
        payload: &payload,
        artifact: None,
        schedule: &[],
        cycle_limit: u64::MAX,
    };
    let report = run(&job).expect("run");
    assert_eq!(report.outcome, GkVmOutcome::InputOverflow);
    assert_eq!(report.cycles, 0, "the verdict must precede execution");
}

/// The host-side image of artifact-probe-c's fold:
/// `acc_i = keccak(acc_{i-1}? || file_len (u64 BE) || page_i)`.
fn expected_fold(mount: &ArtifactMountV3, requests: &[(u32, u64)]) -> Vec<u8> {
    let manifests = mount.manifests();
    let mut acc: Option<[u8; 32]> = None;
    for &(kind, page_idx) in requests {
        let (page, _) = mount.page(kind, page_idx).expect("request in range");
        let mut hasher = Keccak256::new();
        if let Some(prev) = acc {
            hasher.update(prev);
        }
        hasher.update(manifests[kind as usize].len.to_be_bytes());
        hasher.update(&page);
        acc = Some(hasher.finalize().0);
    }
    acc.expect("at least one request").to_vec()
}

fn probe_payload(requests: &[(u32, u64)]) -> Vec<u8> {
    let mut payload = Vec::new();
    for &(kind, page_idx) in requests {
        payload.extend_from_slice(&kind.to_be_bytes());
        payload.extend_from_slice(&page_idx.to_be_bytes());
    }
    payload
}

fn test_mount() -> ArtifactMountV3 {
    // Two files: 2.5 pages of a counting pattern and half a page of 0x77 —
    // odd tree widths, distinct lens, a zero-padded tail on each.
    let weights: Vec<u8> = (0..(constants::GKVM_ARTIFACT_PAGE_SIZE * 5 / 2))
        .map(|i| (i % 251) as u8)
        .collect();
    let tokenizer = vec![0x77u8; constants::GKVM_ARTIFACT_PAGE_SIZE / 2];
    let root = manifest::artifact_root(&[
        manifest::ArtifactFileManifest::from_bytes(&weights),
        manifest::ArtifactFileManifest::from_bytes(&tokenizer),
    ]);
    ArtifactMountV3::from_bytes(vec![weights, tokenizer], root).expect("root matches")
}

#[test]
fn the_guest_verifies_and_uses_artifact_pages() {
    let program = fixture("artifact-probe-c.elf");
    let mount = test_mount();
    // Out-of-order and repeated requests, both kinds, including the padded
    // tail pages — the schedule serves exactly this order.
    let requests = [(0u32, 2u64), (1, 0), (0, 0), (0, 2), (0, 1)];
    let job = GkVmJob {
        program: program.program.clone(),
        payload: &probe_payload(&requests),
        artifact: Some(&mount),
        schedule: &requests,
        cycle_limit: u64::MAX,
    };
    let report = run(&job).expect("run");
    assert_eq!(
        report.outcome,
        GkVmOutcome::Ok {
            output: expected_fold(&mount, &requests)
        },
        "the guest's verified-page fold must match the host-side recomputation"
    );
}

#[test]
fn a_wrong_page_served_is_a_deterministic_guest_trap() {
    // The guest asks for (0,0) but the schedule serves (0,1): the branch
    // cannot authenticate the wrong page under the requested index, and the
    // SDK aborts rather than returning unverified bytes.
    let program = fixture("artifact-probe-c.elf");
    let mount = test_mount();
    let job = GkVmJob {
        program: program.program.clone(),
        payload: &probe_payload(&[(0, 0)]),
        artifact: Some(&mount),
        schedule: &[(0, 1)],
        cycle_limit: u64::MAX,
    };
    match run(&job).expect("run").outcome {
        GkVmOutcome::Trap { code, .. } => {
            assert_eq!(
                code, 0xE000_0002,
                "GK_TRAP_ARTIFACT_VERIFY from the guest SDK"
            );
        }
        other => panic!("expected the verification trap, got {other:?}"),
    }
}

#[test]
fn requests_past_the_manifest_are_guest_range_traps() {
    let program = fixture("artifact-probe-c.elf");
    let mount = test_mount();
    // Kind 0 has 3 pages (0..=2); the guest must refuse page 3 from its own
    // verified manifest before ever consuming a hint. Serve page 0 so a
    // (wrongly) permissive guest would starve rather than hang: the trap
    // must come from the range check, not the serve.
    let job = GkVmJob {
        program: program.program.clone(),
        payload: &probe_payload(&[(0, 3)]),
        artifact: Some(&mount),
        schedule: &[(0, 0)],
        cycle_limit: u64::MAX,
    };
    match run(&job).expect("run").outcome {
        GkVmOutcome::Trap { code, .. } => {
            assert_eq!(
                code, 0xE000_0003,
                "GK_TRAP_ARTIFACT_RANGE from the guest SDK"
            );
        }
        other => panic!("expected the range trap, got {other:?}"),
    }
}

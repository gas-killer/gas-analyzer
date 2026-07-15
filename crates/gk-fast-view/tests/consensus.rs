//! THE CONSENSUS GATE.
//!
//! Operators compare `keccak(returndata)` across independently-executed
//! segments; gas may differ but returndata must NOT. This test proves the
//! revm-41 + revmc fast executor is **byte-identical on returndata** to the
//! revm-31 interpreter path (`evmsketch::call_view_local_blocking`) on real
//! overlay-mounted view calls.
//!
//! ## Cross-version methodology (documented, as required)
//!
//! The two executors are pinned to different revm majors (revm-31 via the
//! SP1/reth entanglement; revm-41 here for revmc) and CANNOT be linked into one
//! test binary. So the differential is run in two stages against a committed
//! golden:
//!
//! 1. **Ground truth (revm-31):** `evmsketch`'s
//!    `phase4_emit_consensus_golden_fixtures` unit test runs each scenario
//!    through the real revm-31 interpreter view path and emits a self-describing
//!    `.fixture` (the [`gk_fast_view::job`] format) with the interpreter's exact
//!    returndata in its `expected` line. That test also runs in the analyzer's
//!    default `cargo test` as a regression lock on the golden.
//! 2. **This test (revm-41 + revmc):** replays the exact same scenario — same
//!    engine bytecode, same overlay chunk bytes/addresses, same calldata, same
//!    profile/spec — through [`gk_fast_view`] and asserts the returndata equals
//!    the committed `expected`, byte-for-byte.
//!
//! Fixtures under `tests/golden/` are regenerated with
//! `GK_GEN_GOLDEN=1 cargo test -p gas-analyzer-evmsketch phase4_emit_consensus_golden_fixtures`.

use gk_fast_view::job::{Job, hex_encode};
use gk_fast_view::{FastView, Profile, ViewEnv, ViewTx, overlay_chunk_address, overlay_manifest_hash};
use revm_primitives::{U256, address, hardfork::SpecId};

/// Replay one committed golden through revm-41 + revmc and assert the returndata
/// is byte-identical to the revm-31 interpreter's `expected`.
fn assert_consensus(name: &str, fixture: &str) {
    let job = Job::parse(fixture).unwrap_or_else(|e| panic!("parse {name}: {e:#}"));
    let expected = job
        .expected
        .clone()
        .unwrap_or_else(|| panic!("{name}: fixture has no `expected` line"));
    let got = job
        .execute()
        .unwrap_or_else(|e| panic!("{name}: gk-fast-view execute failed: {e:#}"));
    assert_eq!(
        hex_encode(&got),
        hex_encode(&expected),
        "\n[CONSENSUS BREACH] {name}: revm-41+revmc returndata != revm-31 interpreter golden\n\
         expected 0x{}\n got      0x{}\n",
        hex_encode(&expected),
        hex_encode(&got),
    );
    eprintln!("[consensus OK] {name}: {} bytes returndata byte-identical", got.len());
}

#[test]
fn consensus_matmul_pure_compute() {
    // Cross-version determinism of the packed-int arith loop (no overlay).
    assert_consensus("matmul", include_str!("golden/matmul.fixture"));
}

#[test]
fn consensus_extcodecopy_overlay() {
    // EXTCODECOPY an OVERLAY-mounted phantom chunk + KECCAK: proves the revmc
    // path reads overlay chunk bytes through OverlayStateDb identically to the
    // interpreter (the returndata is keccak of the mounted weight bytes).
    assert_consensus(
        "extcodecopy_overlay",
        include_str!("golden/extcodecopy_overlay.fixture"),
    );
}

#[test]
fn consensus_forward_range_overlay_loop() {
    // The seg-engine "copy weight chunk + integer fold" inner loop over an
    // overlay-mounted chunk — the closest shape to the real `forwardRange`.
    assert_consensus("forward_range", include_str!("golden/forward_range.fixture"));
}

/// The overlay address derivation port must match `gas_analyzer_core::overlay`
/// bit-for-bit — otherwise gk-fast-view would mount chunks at the wrong phantom
/// addresses and diverge from the interpreter. Pinned against the same
/// cross-ecosystem solidity vectors core commits to (`Qwen3Engine
/// .overlayChunkAddress`, verified via `cast call`).
#[test]
fn derivation_matches_core_solidity_vectors() {
    let manifest = overlay_manifest_hash(b"weights", b"tok");
    assert_eq!(
        hex_encode(manifest.as_slice()),
        "78273dda294c05581c6c3ccdb68f94d8df3e54038539debf5e3219534b9ee19f"
    );
    assert_eq!(
        hex_encode(overlay_chunk_address(manifest, 0).as_slice()),
        "cf1fbae4bca1b750ab9d36d4d37913e012f8ef18"
    );
    assert_eq!(
        hex_encode(overlay_chunk_address(manifest, 1).as_slice()),
        "d0a3009805eecd2b4e342d87d451d8df79ce030b"
    );
    assert_eq!(
        hex_encode(overlay_chunk_address(manifest, 24_298).as_slice()),
        "9c8f5689b42824b7699cf0d0f179553e91402922"
    );
}

/// Reverts/halts must be LOUD errors, never a silent empty output — parity with
/// `call_view_local_blocking` (a committee member must surface a segment's
/// witness revert rather than hash-commit an empty result).
#[test]
fn revert_is_a_loud_error() {
    let engine = address!("0x0000000000000000000000000000000000004321");
    let caller = address!("0x1000000000000000000000000000000000000001");
    // REVERT(0, 0): PUSH1 0 PUSH1 0 REVERT
    let text = format!(
        "spec CANCUN\nprofile UnboundedV1\nfrom {}\nto {}\ninput\ngas 1000000\naccount {} 60006000fd\n",
        hex_encode(caller.as_slice()),
        hex_encode(engine.as_slice()),
        hex_encode(engine.as_slice()),
    );
    let job = Job::parse(&text).expect("parse revert job");
    let err = job.execute().expect_err("revert must be an Err, not empty output");
    assert!(err.to_string().contains("reverted"), "got: {err:#}");
}

/// Demonstrates compile-once-run-many: ONE `FastView` compiles the engine
/// bytecode exactly once and serves repeated calls, all byte-identical to the
/// golden. This is the production win — the fixed seg-engine bytecode is
/// compiled once, then every segment view call runs on the compiled artifact.
#[test]
fn compile_once_run_many_reuses_the_compiled_engine() {
    let job = Job::parse(include_str!("golden/forward_range.fixture")).expect("parse");
    let expected = job.expected.clone().expect("expected");

    let mut fv = FastView::new(SpecId::CANCUN).expect("FastView");
    let env = job.view_env();
    let tx = job.view_tx();

    for i in 0..3 {
        let out = fv
            .call_view(job.base_db(), job.mount_set().unwrap(), &env, &tx, Profile::UnboundedV1)
            .expect("call_view");
        assert_eq!(out.as_ref(), expected.as_ref(), "call {i} diverged");
        // After the first call the engine is compiled and stays compiled; every
        // subsequent call reuses it (never recompiles).
        assert_eq!(fv.compiled_count(), 1, "expected exactly one compiled engine");
    }
    eprintln!("[reuse OK] 3 calls served by 1 compiled engine, all byte-identical");
}

/// Sanity that a plain (no-overlay) direct call through the typed API works and
/// matches the golden too — exercises `FastView::call_view` without going
/// through the `Job` convenience wrapper.
#[test]
fn typed_api_matmul_matches_golden() {
    let job = Job::parse(include_str!("golden/matmul.fixture")).expect("parse");
    let expected = job.expected.clone().expect("expected");
    let (engine_addr, engine_code) = job.accounts[0].clone();

    let mut fv = FastView::new(SpecId::CANCUN).expect("FastView");
    let mut base = revm_database::CacheDB::new(revm_database::EmptyDB::new());
    base.insert_account_info(
        engine_addr,
        revm_state::AccountInfo {
            balance: U256::from(1u64) << 200,
            code_hash: revm_primitives::keccak256(&engine_code),
            code: Some(revm_bytecode::Bytecode::new_raw(engine_code)),
            ..Default::default()
        },
    );

    let env = ViewEnv {
        spec: SpecId::CANCUN,
        ..ViewEnv::default()
    };
    let tx = ViewTx::call(
        address!("0x1000000000000000000000000000000000000001"),
        engine_addr,
        job.input.clone(),
        5_000_000,
    );
    let out = fv
        .call_view(base, Default::default(), &env, &tx, Profile::UnboundedV1)
        .expect("call_view");
    assert_eq!(out.as_ref(), expected.as_ref());
}

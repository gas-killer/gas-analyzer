//! The flagship guest (`guest/qwen/qwen.c`) against the engine-v2 integer
//! spec.
//!
//! `fixtures/qwen3-synthetic/` is solidity-sdk's `test/fixtures/onchain-llm-v2`
//! verbatim: the tiny Qwen-architecture model `tools/qwen3_convert.py
//! --synthetic` writes, with `vectors.json` produced by the bit-exact Python
//! reference (`tools/qwen3_int.py`) — the same vectors `Qwen3.sol` is tested
//! against. The guest must reproduce them byte for byte: same logits, same
//! greedy ids, same decoded text, ABI-encoded as `Qwen3Engine.chat` returns
//! them.
//!
//! `qwen-c.elf` / `qwen-logits-c.elf` are committed like the other guest
//! fixtures; rebuild with `make -C guest docker-qwen fixtures-qwen`.

use alloy_primitives::{U256, keccak256};
use gas_analyzer_gkvm::{
    ArtifactMountV3, GkVmJob, GkVmOutcome, GkVmReport, LoadedGuestProgram, manifest, run,
};

const QW_TRAP_BAD_PAYLOAD: u32 = 1;
const QW_TRAP_BAD_CONFIG: u32 = 2;
const QW_TRAP_CONTEXT_OVERFLOW: u32 = 3;
const QW_TRAP_BAD_TOKEN: u32 = 4;
const QW_TRAP_ARTIFACT_LEN: u32 = 5;
const GK_TRAP_ARTIFACT_VERIFY: u32 = 0xE000_0002;

fn fixture_path(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn guest(name: &str) -> LoadedGuestProgram {
    let path = fixture_path(name);
    let elf = std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {name}: {e}"));
    let hash = keccak256(&elf);
    LoadedGuestProgram::from_bytes(elf, hash, &path).expect("fixture ELF loads")
}

struct Vectors {
    config: [[u8; 32]; 3],
    prompt: Vec<u32>,
    logits_pos0: Vec<u8>,
    json: serde_json::Value,
}

impl Vectors {
    fn load() -> Self {
        let raw = std::fs::read(fixture_path("qwen3-synthetic/vectors.json")).expect("vectors");
        let json: serde_json::Value = serde_json::from_slice(&raw).expect("vectors.json parses");
        let unhex = |v: &serde_json::Value| {
            hex::decode(v.as_str().expect("hex string").trim_start_matches("0x")).expect("hex")
        };
        let mut config = [[0u8; 32]; 3];
        for (word, value) in config
            .iter_mut()
            .zip(json["packedConfig"].as_array().unwrap())
        {
            word.copy_from_slice(&unhex(value));
        }
        let prompt = ids_of(&json["promptIds"]);
        let logits_pos0 = unhex(&json["logitsPos0"]);
        Self {
            config,
            prompt,
            logits_pos0,
            json,
        }
    }

    /// `(maxNew, ids, text)` of a recorded generation.
    fn generation(&self, name: &str) -> (u64, Vec<u32>, Vec<u8>) {
        let entry = &self.json[name];
        let text = entry["textHex"].as_str().unwrap().trim_start_matches("0x");
        (
            entry["maxNew"].as_u64().unwrap(),
            ids_of(&entry["ids"]),
            hex::decode(text).unwrap(),
        )
    }
}

fn ids_of(value: &serde_json::Value) -> Vec<u32> {
    value
        .as_array()
        .expect("id array")
        .iter()
        .map(|id| id.as_u64().unwrap() as u32)
        .collect()
}

fn word(value: u64) -> [u8; 32] {
    U256::from(value).to_be_bytes::<32>()
}

/// `abi.encode(bytes32[3] packedConfig, uint32[] promptIds, uint256 maxNewTokens)`.
fn chat_payload(config: &[[u8; 32]; 3], prompt: &[u32], max_new: U256) -> Vec<u8> {
    let mut out = Vec::new();
    for w in config {
        out.extend_from_slice(w);
    }
    out.extend_from_slice(&word(160));
    out.extend_from_slice(&max_new.to_be_bytes::<32>());
    out.extend_from_slice(&word(prompt.len() as u64));
    for &id in prompt {
        out.extend_from_slice(&word(id as u64));
    }
    out
}

/// `abi.encode(string answer, uint32[] answerIds)`.
fn chat_result(text: &[u8], ids: &[u32]) -> Vec<u8> {
    let padded = text.len().div_ceil(32) * 32;
    let mut out = Vec::new();
    out.extend_from_slice(&word(64));
    out.extend_from_slice(&word(96 + padded as u64));
    out.extend_from_slice(&word(text.len() as u64));
    out.extend_from_slice(text);
    out.resize(96 + padded, 0);
    out.extend_from_slice(&word(ids.len() as u64));
    for &id in ids {
        out.extend_from_slice(&word(id as u64));
    }
    out
}

fn synthetic_mount() -> ArtifactMountV3 {
    let weights = std::fs::read(fixture_path("qwen3-synthetic/weights.bin")).expect("weights");
    let tokenizer =
        std::fs::read(fixture_path("qwen3-synthetic/tokenizer.bin")).expect("tokenizer");
    let root = manifest::artifact_root(&[
        manifest::ArtifactFileManifest::from_bytes(&weights),
        manifest::ArtifactFileManifest::from_bytes(&tokenizer),
    ]);
    ArtifactMountV3::from_bytes(vec![weights, tokenizer], root).expect("root matches")
}

fn run_with(
    program: &LoadedGuestProgram,
    mount: &ArtifactMountV3,
    schedule: &[(u32, u64)],
    payload: &[u8],
) -> GkVmReport {
    let job = GkVmJob {
        program: program.program.clone(),
        payload,
        artifact: Some(mount),
        schedule,
        cycle_limit: u64::MAX,
    };
    run(&job).expect("environment must not fail")
}

fn chat(program: &LoadedGuestProgram, mount: &ArtifactMountV3, payload: &[u8]) -> GkVmReport {
    run_with(program, mount, &mount.sequential_schedule(), payload)
}

fn trap_code(report: &GkVmReport) -> u32 {
    match &report.outcome {
        GkVmOutcome::Trap { code, .. } => *code,
        other => panic!("expected a guest trap, got {other:?}"),
    }
}

#[test]
fn position_zero_logits_match_the_integer_reference_bit_for_bit() {
    let vectors = Vectors::load();
    let mount = synthetic_mount();
    // The logits twin stops after the weights; it never asks for the token table.
    let weights_only: Vec<(u32, u64)> = mount
        .sequential_schedule()
        .into_iter()
        .filter(|&(kind, _)| kind == 0)
        .collect();
    let payload = chat_payload(&vectors.config, &vectors.prompt, U256::from(1));
    let report = run_with(&guest("qwen-logits-c.elf"), &mount, &weights_only, &payload);
    assert_eq!(
        report.outcome,
        GkVmOutcome::Ok {
            output: vectors.logits_pos0.clone()
        },
        "abi.encode(int256[] logits) at position 0 must equal vectors.json's logitsPos0"
    );
}

#[test]
fn greedy_generations_match_the_integer_reference() {
    let vectors = Vectors::load();
    let mount = synthetic_mount();
    let program = guest("qwen-c.elf");
    // Instruction counts of the COMMITTED qwen-c.elf — a property of those
    // bytes, identical on the jit and the portable tier (run this test with
    // `--features portable-exec` for the other half). Rebuilding the guest
    // moves them; re-record from this test's output.
    for (name, cycles) in [("genShort", 8_473_894u64), ("genLong", 14_593_174)] {
        let (max_new, ids, text) = vectors.generation(name);
        let payload = chat_payload(&vectors.config, &vectors.prompt, U256::from(max_new));
        let report = chat(&program, &mount, &payload);
        assert_eq!(
            report.outcome,
            GkVmOutcome::Ok {
                output: chat_result(&text, &ids)
            },
            "{name}: abi.encode(answer, answerIds) must equal the reference generation"
        );
        let positions = vectors.prompt.len() as u64 + max_new - 1;
        println!(
            "{name}: cycles={} gas={} forward_passes={positions} tier={}",
            report.cycles,
            report.gas_used,
            report.tier.as_str()
        );
        assert_eq!(report.cycles, cycles, "{name}: instruction count");
    }
}

#[test]
fn cycle_counts_are_a_function_of_the_payload() {
    let vectors = Vectors::load();
    let mount = synthetic_mount();
    let program = guest("qwen-c.elf");
    let payload = chat_payload(&vectors.config, &vectors.prompt, U256::from(6));
    let first = chat(&program, &mount, &payload);
    for _ in 0..3 {
        let again = chat(&program, &mount, &payload);
        assert_eq!(again.outcome, first.outcome);
        assert_eq!(again.cycles, first.cycles);
    }
}

#[test]
fn max_new_tokens_is_clamped_to_the_context() {
    // Qwen3.generate: maxPos = min(pLen + maxNew, seqCap). seqCap is 64 here,
    // so a uint256-max request yields 64 - 4 ids — and, greedy decoding being
    // a prefix property, they start with the recorded 16.
    let vectors = Vectors::load();
    let mount = synthetic_mount();
    let (_, long_ids, _) = vectors.generation("genLong");
    let payload = chat_payload(&vectors.config, &vectors.prompt, U256::MAX);
    let report = chat(&guest("qwen-c.elf"), &mount, &payload);
    let GkVmOutcome::Ok { output } = report.outcome else {
        panic!("expected an answer, got {:?}", report.outcome);
    };
    let ids_at = U256::from_be_slice(&output[32..64]).to::<usize>();
    let count = U256::from_be_slice(&output[ids_at..ids_at + 32]).to::<usize>();
    assert_eq!(count, 64 - vectors.prompt.len());
    let ids: Vec<u32> = (0..count)
        .map(|i| U256::from_be_slice(&output[ids_at + 32 * (i + 1)..ids_at + 32 * (i + 2)]).to())
        .collect();
    assert_eq!(&ids[..long_ids.len()], &long_ids[..]);
}

#[test]
fn malformed_requests_are_typed_traps() {
    let vectors = Vectors::load();
    let mount = synthetic_mount();
    let program = guest("qwen-c.elf");
    let good = chat_payload(&vectors.config, &vectors.prompt, U256::from(6));

    // Truncated ABI head.
    assert_eq!(
        trap_code(&chat(&program, &mount, &good[..159])),
        QW_TRAP_BAD_PAYLOAD
    );
    // A promptIds length the payload cannot hold.
    let mut short = good.clone();
    short.truncate(good.len() - 32);
    assert_eq!(
        trap_code(&chat(&program, &mount, &short)),
        QW_TRAP_BAD_PAYLOAD
    );
    // A uint32 with dirty high bits.
    let mut dirty = good.clone();
    dirty[160 + 32] = 1;
    assert_eq!(
        trap_code(&chat(&program, &mount, &dirty)),
        QW_TRAP_BAD_PAYLOAD
    );

    // Qwen3.BadToken: vocab is 256.
    let bad_token = chat_payload(&vectors.config, &[7, 256], U256::from(6));
    assert_eq!(
        trap_code(&chat(&program, &mount, &bad_token)),
        QW_TRAP_BAD_TOKEN
    );

    // Qwen3.ContextOverflow: empty prompt, and a prompt filling seqCap = 64.
    let empty = chat_payload(&vectors.config, &[], U256::from(6));
    assert_eq!(
        trap_code(&chat(&program, &mount, &empty)),
        QW_TRAP_CONTEXT_OVERFLOW
    );
    let full = chat_payload(&vectors.config, &[7; 64], U256::from(6));
    assert_eq!(
        trap_code(&chat(&program, &mount, &full)),
        QW_TRAP_CONTEXT_OVERFLOW
    );

    // Qwen3.BadConfig: weightLen no longer matches the layout (w1 bytes 16..24).
    let mut config = vectors.config;
    config[1][23] ^= 1;
    let bad_layout = chat_payload(&config, &vectors.prompt, U256::from(6));
    assert_eq!(
        trap_code(&chat(&program, &mount, &bad_layout)),
        QW_TRAP_BAD_CONFIG
    );
    // …and a zero dimension.
    let mut config = vectors.config;
    config[0][0] = 0;
    config[0][1] = 0;
    let zero_dim = chat_payload(&config, &vectors.prompt, U256::from(6));
    assert_eq!(
        trap_code(&chat(&program, &mount, &zero_dim)),
        QW_TRAP_BAD_CONFIG
    );

    // A config describing a different token table than the mounted one
    // (tokLen, w2 bytes 0..4): the layout still validates, the mount does not.
    let mut config = vectors.config;
    config[2][3] ^= 1;
    let other_table = chat_payload(&config, &vectors.prompt, U256::from(6));
    assert_eq!(
        trap_code(&chat(&program, &mount, &other_table)),
        QW_TRAP_ARTIFACT_LEN
    );
}

/// A model that traps on its first forward pass (token 7's embedding row
/// shift is 25 > 24), i.e. right after the weights are in memory: run cycles
/// minus the parse-only baseline = the cost of loading `pages` weight pages.
/// `seq_cap` sizes the RoPE tables, which is how the page count (and with it
/// the Merkle depth) is varied without touching the architecture.
fn load_only_run(vectors: &Vectors, seq_cap: u16) -> (u64, u64, u64) {
    let mut weights = std::fs::read(fixture_path("qwen3-synthetic/weights.bin")).unwrap();
    let tokenizer = std::fs::read(fixture_path("qwen3-synthetic/tokenizer.bin")).unwrap();
    let base = weights.len() - 2 * 64 * 8 * 4; // everything before the seqCap-64 tables
    weights.resize(base + 2 * seq_cap as usize * 8 * 4, 0);
    weights[7 * 49] = 25;

    let mut config = vectors.config;
    config[0][13..15].copy_from_slice(&seq_cap.to_be_bytes());
    config[1][16..24].copy_from_slice(&(weights.len() as u64).to_be_bytes());

    let root = manifest::artifact_root(&[
        manifest::ArtifactFileManifest::from_bytes(&weights),
        manifest::ArtifactFileManifest::from_bytes(&tokenizer),
    ]);
    let mount = ArtifactMountV3::from_bytes(vec![weights, tokenizer], root).expect("root");
    let pages = mount.manifests()[0].page_count();
    let branch_nodes: u64 = (0..pages)
        .map(|p| mount.page(0, p).expect("in range").1.len() as u64)
        .sum();

    let program = guest("qwen-c.elf");
    let loaded = chat(
        &program,
        &mount,
        &chat_payload(&config, &vectors.prompt, U256::from(1)),
    );
    assert_eq!(
        trap_code(&loaded),
        7,
        "QW_TRAP_NUMERIC_RANGE, raised by forward()"
    );
    // Same payload shape, rejected before the mount is touched.
    let parsed = chat(
        &program,
        &mount,
        &chat_payload(&config, &[7, 42, 99, 256], U256::from(1)),
    );
    assert_eq!(trap_code(&parsed), QW_TRAP_BAD_TOKEN);
    (pages, branch_nodes, loaded.cycles - parsed.cycles)
}

/// Measurement, not an assertion: what `gk_artifact_read` costs per page with
/// the crt's in-guest keccak, split into the leaf hash and one branch node by
/// loading the same model at two tree depths.
/// `cargo test -p gas-analyzer-gkvm --release --test qwen_guest -- --ignored --nocapture`
#[test]
#[ignore = "measurement; prints the per-page load cost"]
fn weight_load_cost_per_page() {
    let vectors = Vectors::load();
    let (p_a, n_a, c_a) = load_only_run(&vectors, 64);
    let (p_b, n_b, c_b) = load_only_run(&vectors, 16_384);
    println!("load A: pages={p_a} branch_nodes={n_a} cycles={c_a}");
    println!("load B: pages={p_b} branch_nodes={n_b} cycles={c_b}");
    // c = pages * leaf + branch_nodes * node, two unknowns.
    let det = (p_a * n_b) as f64 - (p_b * n_a) as f64;
    let leaf = (c_a as f64 * n_b as f64 - c_b as f64 * n_a as f64) / det;
    let node = (p_a as f64 * c_b as f64 - p_b as f64 * c_a as f64) / det;
    println!("per page: leaf+copy = {leaf:.1} cycles, per branch node = {node:.1} cycles");
    println!(
        "extrapolation (NOT a measurement): depth-18 page = {:.0} cycles; x 145,784 pages = {:.4e} cycles",
        leaf + 18.0 * node,
        145_784.0 * (leaf + 18.0 * node)
    );
}

#[test]
fn the_sidecar_re_manifests_a_v2_bundle_and_serves_it_sequentially() {
    // What the real-weights run does, on the synthetic bundle: the V2 files
    // as they are → `--print-artifact-root` → `--schedule sequential`.
    let vectors = Vectors::load();
    let bundle = format!(
        "{},{}",
        fixture_path("qwen3-synthetic/weights.bin"),
        fixture_path("qwen3-synthetic/tokenizer.bin")
    );
    let gk_run = env!("CARGO_BIN_EXE_gk-run");

    let printed = std::process::Command::new(gk_run)
        .args(["--print-artifact-root", "--artifact", &bundle])
        .output()
        .expect("gk-run starts");
    assert!(printed.status.success());
    let root = String::from_utf8(printed.stdout)
        .unwrap()
        .trim()
        .to_string();
    assert_eq!(root, format!("0x{}", hex::encode(synthetic_mount().root)));

    let (max_new, ids, text) = vectors.generation("genShort");
    let payload = chat_payload(&vectors.config, &vectors.prompt, U256::from(max_new));
    let answered = std::process::Command::new(gk_run)
        .args(["--program", &fixture_path("qwen-c.elf")])
        .args(["--input", &format!("0x{}", hex::encode(payload))])
        .args(["--artifact", &bundle, "--artifact-root", &root])
        .args(["--schedule", "sequential"])
        .output()
        .expect("gk-run starts");
    assert!(
        answered.status.success(),
        "{}",
        String::from_utf8_lossy(&answered.stderr)
    );
    assert_eq!(
        String::from_utf8(answered.stdout).unwrap().trim(),
        format!("0x{}", hex::encode(chat_result(&text, &ids)))
    );
}

#[test]
fn a_schedule_that_is_not_the_guests_load_order_is_a_verify_trap() {
    // The guest reads weights first; a host serving the token table first
    // hands it a page that does not verify at (kind 0, page 0).
    let vectors = Vectors::load();
    let mount = synthetic_mount();
    let mut schedule = mount.sequential_schedule();
    schedule.reverse();
    let payload = chat_payload(&vectors.config, &vectors.prompt, U256::from(6));
    let report = run_with(&guest("qwen-c.elf"), &mount, &schedule, &payload);
    assert_eq!(trap_code(&report), GK_TRAP_ARTIFACT_VERIFY);
}

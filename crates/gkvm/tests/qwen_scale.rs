//! jit ≡ interp instruction counts at 10^10-cycle scale (M4's determinism leg).
//!
//! The flagship guest (`qwen-c.elf`) over a model with Qwen3-0.6B's EXACT
//! architecture — dim 1024, hidden 3072, 28 layers, 16/8 heads of 128, vocab
//! 151,936, seqCap 1024 → a 597,135,857-byte engine-v2 weight blob (145,786
//! pages; 146,083 with the token table) — filled with deterministic
//! pseudo-random weights. Random int8 rows are legal engine-v2 weights; row
//! shifts, norm gains and RoPE tables are chosen so activations stay
//! unit-scale and no numeric trap fires (observed for `load+1` on the jit
//! tier; the longer cases are unverified).
//! It is NOT the real model (the release blob is a HUMAN item): the answers
//! are noise, and the counts are a property of this stand-in. What the run
//! settles is what does not depend on the weights' meaning:
//!
//! * one binary per tier, same pinned count on both — build this test with
//!   and without `--features portable-exec`; both assert the constants below;
//! * the real-size memory footprint (597 MB of weights in guest memory, a
//!   ~680 MB hint stream) under `GKVM_MEM_BYTES_CAP` on the consensus tier.
//!
//! **UNPINNED:** the per-case `cycles` / `output` constants are still zero.
//! Only the jit tier has completed a case so far: `load+1` answered `Ok` with
//! cycles = 46,465,876,532 and output keccak
//! 0xc4150374a20548e4cba5f06bdb00908b7b4d2e1079c97ca8bf663c3c4dcd39f1 — so on
//! that tier no numeric trap fires and the footprint fits. The portable tier
//! has not finished a run, and one tier's number is not a pin. A zero constant
//! fails with the measured values in the message; pin them only once BOTH
//! tiers have printed the same line.
//!
//! `#[ignore]`d: ~4.6×10^10 cycles per case (about a minute on an idle jit
//! and ten and more on the interpreter — estimates from the bench guest's
//! throughput, not observed), ~600 MB on disk under `target/gkvm-qwen-scale/`,
//! GBs of RAM: on a 16 GB host that is already swapping, the portable-tier run
//! had to be stopped to keep the box alive.
//! `cargo test -p gas-analyzer-gkvm --release --test qwen_scale -- --ignored --nocapture`
//! One test per case, so the usual name filter runs a subset (`… -- --ignored load_plus_1`).

use alloy_primitives::{B256, U256, b256, keccak256};
use gas_analyzer_gkvm::{ArtifactMountV3, GkVmJob, GkVmOutcome, LoadedGuestProgram, run};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

const DIM: u64 = 1024;
const HIDDEN: u64 = 3072;
const LAYERS: u64 = 28;
const HEADS: u64 = 16;
const KV_HEADS: u64 = 8;
const HEAD_DIM: u64 = 128;
const VOCAB: u64 = 151_936;
const SEQ_CAP: u64 = 1024;
/// `round(1e-6 * 2^48)` and `round(2^32 / sqrt(128))`, as `qwen3_int.py` derives them.
const EPS_Q48: u64 = 281_474_977;
const INV_SQRT_HD: u64 = 379_625_062;
/// No id equals it, so generation always runs to `maxNewTokens`.
const NO_STOP: u32 = u32::MAX;

const WEIGHT_LEN: u64 = 597_135_857;

/// Root of the generated bundle — pins the generator: a stale or edited blob
/// under `target/` fails the mount instead of moving the counts silently.
const ARTIFACT_ROOT: B256 =
    b256!("e2af7bbce9581ea0c9b8c515672c12af69b4ef45149b6adb1f0c938793d00bfd");

struct Case {
    name: &'static str,
    prompt: &'static [u32],
    max_new: u64,
    /// Instruction count of the COMMITTED qwen-c.elf on this bundle — the
    /// same constant is asserted by the jit and the portable-exec build.
    cycles: u64,
    /// keccak256 of the guest's output.
    output: B256,
}

/// Three shapes, so load / forward pass / classifier separate by differencing
/// (attention grows with the position, so the split is approximate).
const CASES: &[Case] = &[
    Case {
        name: "load+1",
        prompt: &[9707],
        max_new: 1,
        cycles: 0,
        output: b256!("0000000000000000000000000000000000000000000000000000000000000000"),
    },
    Case {
        name: "prefill2",
        prompt: &[9707, 11],
        max_new: 1,
        cycles: 0,
        output: b256!("0000000000000000000000000000000000000000000000000000000000000000"),
    },
    Case {
        name: "chat4x4",
        prompt: &[9707, 11, 1879, 0],
        max_new: 4,
        cycles: 0,
        output: b256!("0000000000000000000000000000000000000000000000000000000000000000"),
    },
];

/// xorshift64* — the same fixed stream `artifact_mount_bench.rs` uses.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

struct Blob<W: Write> {
    out: W,
    rng: Rng,
    written: u64,
}

impl<W: Write> Blob<W> {
    fn put(&mut self, bytes: &[u8]) {
        self.out.write_all(bytes).expect("write blob");
        self.written += bytes.len() as u64;
    }

    /// `rows x [u8 shift || int8 x cols]`, uniform int8.
    fn rows(&mut self, rows: u64, cols: u64, shift: u8) {
        let mut row = vec![0u8; 1 + cols as usize];
        row[0] = shift;
        for _ in 0..rows {
            for chunk in row[1..].chunks_mut(8) {
                let word = self.rng.next().to_le_bytes();
                chunk.copy_from_slice(&word[..chunk.len()]);
            }
            self.put(&row);
        }
    }

    /// `[u8 shift = 12][int16 BE x n]`, gains in [0.5, 1.5).
    fn gain(&mut self, n: u64) {
        let mut tensor = vec![12u8];
        for _ in 0..n {
            let g = 2048 + (self.rng.next() >> 52) as u16;
            tensor.extend_from_slice(&g.to_be_bytes());
        }
        self.put(&tensor);
    }

    /// `int32 BE Q30 x n` in [-0.5, 0.5): |cos|^2 + |sin|^2 < 1, so a rotation
    /// never grows its pair.
    fn rope(&mut self, n: u64) {
        let mut table = Vec::with_capacity(4 * n as usize);
        for _ in 0..n {
            let v = (self.rng.next() >> 34) as i64 - (1 << 29);
            table.extend_from_slice(&(v as i32).to_be_bytes());
        }
        self.put(&table);
    }
}

/// The engine-v2 weight blob (layout: `tools/qwen3_convert.py`'s header).
/// Row shifts bring a uniform-int8 row over unit-scale inputs back to unit
/// scale: std ≈ 74·sqrt(cols) → 2^11 for 1024 columns, 2^12 for 2048 / 3072.
fn write_weights(path: &Path) {
    let file = std::fs::File::create(path).expect("create weights");
    let mut blob = Blob {
        out: std::io::BufWriter::with_capacity(1 << 20, file),
        rng: Rng(0x9E37_79B9_7F4A_7C15),
        written: 0,
    };
    let (qd, kvd) = (HEADS * HEAD_DIM, KV_HEADS * HEAD_DIM);
    blob.rows(VOCAB, DIM, 7); // emb (and the tied classifier)
    for _ in 0..LAYERS {
        blob.gain(DIM); // ln1
        blob.gain(HEAD_DIM); // qn
        blob.gain(HEAD_DIM); // kn
        blob.rows(qd, DIM, 11); // wq
        blob.rows(kvd, DIM, 11); // wk
        blob.rows(kvd, DIM, 11); // wv
        blob.rows(DIM, qd, 12); // wo
        blob.gain(DIM); // ln2
        blob.rows(HIDDEN, DIM, 11); // wg
        blob.rows(HIDDEN, DIM, 11); // wu
        blob.rows(DIM, HIDDEN, 12); // wd
    }
    blob.gain(DIM); // norm
    blob.rope(SEQ_CAP * HEAD_DIM / 2); // ropeCos
    blob.rope(SEQ_CAP * HEAD_DIM / 2); // ropeSin
    blob.out.flush().expect("flush weights");
    assert_eq!(blob.written, WEIGHT_LEN, "layout arithmetic");
}

/// `[u8 1][u32 vocab][u32 stringsLen][(vocab+1) x u32 offsets][strings]`;
/// token i decodes to 1 + i % 7 copies of one lowercase letter.
fn token_table() -> Vec<u8> {
    let mut offsets = Vec::with_capacity(4 * (VOCAB as usize + 1));
    let mut strings = Vec::new();
    for id in 0..VOCAB {
        offsets.extend_from_slice(&(strings.len() as u32).to_be_bytes());
        strings.resize(
            strings.len() + 1 + (id % 7) as usize,
            b'a' + (id % 26) as u8,
        );
    }
    offsets.extend_from_slice(&(strings.len() as u32).to_be_bytes());
    let mut table = vec![1u8];
    table.extend_from_slice(&(VOCAB as u32).to_be_bytes());
    table.extend_from_slice(&(strings.len() as u32).to_be_bytes());
    table.extend_from_slice(&offsets);
    table.extend_from_slice(&strings);
    table
}

fn bundle_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/gkvm-qwen-scale")
}

fn packed_config(tok_len: u64) -> [[u8; 32]; 3] {
    let mut w = [[0u8; 32]; 3];
    w[0][0..2].copy_from_slice(&(DIM as u16).to_be_bytes());
    w[0][2..4].copy_from_slice(&(HIDDEN as u16).to_be_bytes());
    w[0][4] = LAYERS as u8;
    w[0][5] = HEADS as u8;
    w[0][6] = KV_HEADS as u8;
    w[0][7..9].copy_from_slice(&(HEAD_DIM as u16).to_be_bytes());
    w[0][9..13].copy_from_slice(&(VOCAB as u32).to_be_bytes());
    w[0][13..15].copy_from_slice(&(SEQ_CAP as u16).to_be_bytes());
    w[0][15] = 1; // tokType
    w[0][16] = 1; // wBits: int8 rows
    w[1][0..8].copy_from_slice(&EPS_Q48.to_be_bytes());
    w[1][8..16].copy_from_slice(&INV_SQRT_HD.to_be_bytes());
    w[1][16..24].copy_from_slice(&WEIGHT_LEN.to_be_bytes());
    w[2][0..4].copy_from_slice(&(tok_len as u32).to_be_bytes());
    w[2][4..8].copy_from_slice(&NO_STOP.to_be_bytes());
    w[2][8..12].copy_from_slice(&NO_STOP.to_be_bytes());
    w
}

fn word(value: u64) -> [u8; 32] {
    U256::from(value).to_be_bytes::<32>()
}

/// `abi.encode(bytes32[3] packedConfig, uint32[] promptIds, uint256 maxNewTokens)`.
fn chat_payload(config: &[[u8; 32]; 3], prompt: &[u32], max_new: u64) -> Vec<u8> {
    let mut out = config.concat();
    out.extend_from_slice(&word(160));
    out.extend_from_slice(&word(max_new));
    out.extend_from_slice(&word(prompt.len() as u64));
    for &id in prompt {
        out.extend_from_slice(&word(id as u64));
    }
    out
}

/// One real-size guest at a time: each run holds GBs of RAM.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
#[ignore = "10^10-cycle scale: minutes per case, ~600 MB on disk, GBs of RAM"]
fn real_size_load_plus_1_count_is_the_pinned_constant_on_this_tier() {
    run_case("load+1");
}

#[test]
#[ignore = "10^10-cycle scale: minutes per case, ~600 MB on disk, GBs of RAM"]
fn real_size_prefill2_count_is_the_pinned_constant_on_this_tier() {
    run_case("prefill2");
}

#[test]
#[ignore = "10^10-cycle scale: minutes per case, ~600 MB on disk, GBs of RAM"]
fn real_size_chat4x4_count_is_the_pinned_constant_on_this_tier() {
    run_case("chat4x4");
}

fn run_case(name: &str) {
    let _serial = SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let case = CASES
        .iter()
        .find(|case| case.name == name)
        .expect("known case");
    let dir = bundle_dir();
    let (weights, tokenizer) = (dir.join("weights.bin"), dir.join("tokenizer.bin"));
    let sized = |p: &Path| std::fs::metadata(p).map(|m| m.len()).ok();
    let table = token_table();
    if sized(&weights) != Some(WEIGHT_LEN) || sized(&tokenizer) != Some(table.len() as u64) {
        std::fs::create_dir_all(&dir).expect("create bundle dir");
        write_weights(&weights);
        std::fs::write(&tokenizer, &table).expect("write token table");
    }
    let mount = match ArtifactMountV3::from_files(&[&weights, &tokenizer], ARTIFACT_ROOT) {
        Ok(mount) => mount,
        Err(error) => panic!("generated bundle does not verify against ARTIFACT_ROOT: {error}"),
    };
    let schedule = mount.sequential_schedule();
    println!(
        "bundle: weights={WEIGHT_LEN} tokenizer={} pages={} root={}",
        table.len(),
        schedule.len(),
        mount.root
    );

    let elf_path = format!("{}/tests/fixtures/qwen-c.elf", env!("CARGO_MANIFEST_DIR"));
    let elf = std::fs::read(&elf_path).expect("qwen-c.elf fixture");
    let hash = keccak256(&elf);
    let program = LoadedGuestProgram::from_bytes(elf, hash, &elf_path).expect("fixture ELF loads");
    let config = packed_config(table.len() as u64);

    let payload = chat_payload(&config, case.prompt, case.max_new);
    let report = run(&GkVmJob {
        program: program.program.clone(),
        payload: &payload,
        artifact: Some(&mount),
        schedule: &schedule,
        cycle_limit: u64::MAX,
    })
    .expect("environment must not fail");
    let GkVmOutcome::Ok { output } = &report.outcome else {
        panic!(
            "{}: expected an answer, got {:?}",
            case.name, report.outcome
        );
    };
    // abi.encode(string, uint32[]): the ids array sits at the second head word.
    let ids_at = U256::from_be_slice(&output[32..64]).to::<usize>();
    let count = U256::from_be_slice(&output[ids_at..ids_at + 32]).to::<u64>();
    assert_eq!(
        count, case.max_new,
        "{}: NO_STOP never ends early",
        case.name
    );
    let output_hash = keccak256(output);
    println!(
        "{{\"case\":\"{}\",\"tier\":\"{}\",\"cycles\":{},\"gas\":{},\"forward_passes\":{},\
         \"generated\":{count},\"output_keccak\":\"{output_hash}\",\"wall_nanos\":{}}}",
        case.name,
        report.tier.as_str(),
        report.cycles,
        report.gas_used,
        case.prompt.len() as u64 + case.max_new - 1,
        report.wall_nanos,
    );
    assert_ne!(
        case.cycles, 0,
        "{}: UNPINNED — this tier measured cycles={} output={output_hash}; pin them once \
         the other tier agrees",
        case.name, report.cycles
    );
    assert_eq!(
        (report.cycles, output_hash),
        (case.cycles, case.output),
        "{}: cycles / output differ from the pinned constants",
        case.name
    );
}

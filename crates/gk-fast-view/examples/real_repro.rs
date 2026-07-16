//! Live-engine repro/differential for the Phase-4 fast executor.
//!
//! Mounts the REAL seg-engine bytecode + the REAL weights overlay and runs the
//! reconstructed seg-0 `forwardRange` through BOTH the pure revm-41 interpreter
//! and the revmc-JIT path, printing `keccak(returndata)` and the engine `chk`
//! (returndata[64..96]) for each. This is the deterministic local repro used to
//! root-cause the 0.6B divergence.
//!
//! Env knobs (all optional):
//!   GK_ENGINE_HEX  path to engine runtime bytecode hex   (default repro 0.6B)
//!   GK_WEIGHTS     weights.bin                            (default repro qwen06)
//!   GK_TOKENIZER   tokenizer.bin                          (default repro qwen06)
//!   GK_MANIFEST    overlay manifest hash (0x..)           (default 0.6B manifest)
//!   GK_TO          engine address (0x..)                  (default 0x18C8..)
//!   GK_MAXPOS      span.maxPos                            (default 16)
//!   GK_POSHI       span.posHi                             (default 16)
//!   GK_LAYERHI     span.layerHi                           (default 7)

use std::path::PathBuf;

use gk_fast_view::{FastView, Profile, ViewEnv, ViewTx};
use revm_database::{CacheDB, EmptyDB};
use revm_primitives::{Address, B256, U256, hardfork::SpecId, keccak256};
use revm_state::{AccountInfo, Bytecode};

const PROMPT_IDS: &[u32] = &[
    151644, 872, 198, 3838, 374, 33946, 30, 151645, 198, 151644, 77091, 198, 151667, 271, 151668,
    271,
];

const PACKED_CONFIG_HEX: [&str; 3] = [
    "04000c001c100800800002518004000101000000000000000000000000000000",
    "0000000010c6f7a10000000016a09e6600000000239791f10000000000000000",
    "00182bc20002505d0002505b0000000000000000000000000000000000000000",
];

fn h(s: &str) -> Vec<u8> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(s).expect("hex")
}

fn word(v: u64) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

fn env_or(k: &str, d: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| d.to_string())
}

/// Hand-rolled ABI encoding of
/// `forwardRange(address,bytes32,bytes32[3],((uint256*5),uint32[],bytes,bytes,bytes32,bytes32))`.
fn encode_forward_range(
    manifest: B256,
    packed: [[u8; 32]; 3],
    max_pos: u64,
    pos_lo: u64,
    pos_hi: u64,
    layer_lo: u64,
    layer_hi: u64,
    token_ids: &[u32],
) -> Vec<u8> {
    let sig = "forwardRange(address,bytes32,bytes32[3],((uint256,uint256,uint256,uint256,uint256),uint32[],bytes,bytes,bytes32,bytes32))";
    let selector = &keccak256(sig.as_bytes())[..4];

    let empty_keccak = keccak256([]); // expectXIn / expectKvIn (x_in, kv_in empty)

    // q tuple (dynamic): head is 10 words.
    let q_head_words = 10u64;
    let tokenids_off = q_head_words * 32; // 0x140
    let tokenids_len_words = 1 + token_ids.len() as u64;
    let xin_off = tokenids_off + tokenids_len_words * 32;
    let kvin_off = xin_off + 32; // xIn is empty: just its length word

    let mut q = Vec::new();
    // span (static, 5 words)
    q.extend_from_slice(&word(max_pos));
    q.extend_from_slice(&word(pos_lo));
    q.extend_from_slice(&word(pos_hi));
    q.extend_from_slice(&word(layer_lo));
    q.extend_from_slice(&word(layer_hi));
    // dynamic offsets
    q.extend_from_slice(&word(tokenids_off));
    q.extend_from_slice(&word(xin_off));
    q.extend_from_slice(&word(kvin_off));
    // expectXIn / expectKvIn
    q.extend_from_slice(empty_keccak.as_slice());
    q.extend_from_slice(empty_keccak.as_slice());
    // tokenIds tail
    q.extend_from_slice(&word(token_ids.len() as u64));
    for &t in token_ids {
        q.extend_from_slice(&word(t as u64));
    }
    // xIn tail (len 0)
    q.extend_from_slice(&word(0));
    // kvIn tail (len 0)
    q.extend_from_slice(&word(0));

    // top-level head: 6 words (rootDirectory, manifest, packed[3], q-offset)
    let q_off = 6u64 * 32; // 0xc0
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

fn summarize(tag: &str, rd: &[u8]) {
    let kc = keccak256(rd);
    let chk = if rd.len() >= 96 {
        format!("0x{}", hex::encode(&rd[64..96]))
    } else {
        "<short>".to_string()
    };
    println!(
        "  {tag:12} len={:6} keccak(rd)=0x{}  chk=({chk})",
        rd.len(),
        hex::encode(kc)
    );
}

fn main() -> anyhow::Result<()> {
    let repro = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../repro");
    let engine_hex = env_or(
        "GK_ENGINE_HEX",
        repro.join("bytecode/engine06.hex").to_str().unwrap(),
    );
    let weights = env_or(
        "GK_WEIGHTS",
        repro.join("overlays/qwen06/weights.bin").to_str().unwrap(),
    );
    let tokenizer = env_or(
        "GK_TOKENIZER",
        repro.join("overlays/qwen06/tokenizer.bin").to_str().unwrap(),
    );
    let manifest_hex = env_or(
        "GK_MANIFEST",
        "0x23216cb9ed9ef2b4bc20c84d27b68fa62ab194fc0845dfa707836f48ec4a7ae9",
    );
    let to_hex = env_or("GK_TO", "0x18C8b1677a731f7507ea51D99e23e513D9613Aa4");
    let max_pos: u64 = env_or("GK_MAXPOS", "16").parse()?;
    let pos_hi: u64 = env_or("GK_POSHI", "16").parse()?;
    let layer_hi: u64 = env_or("GK_LAYERHI", "7").parse()?;

    let engine_code = h(std::fs::read_to_string(&engine_hex)?.trim());
    let manifest = B256::from_slice(&h(&manifest_hex));
    let to = Address::from_slice(&h(&to_hex));
    let from = Address::from_slice(&h("f39fd6e51aad88f6f4ce6ab8827279cfffb92266"));

    let mut packed = [[0u8; 32]; 3];
    for (i, s) in PACKED_CONFIG_HEX.iter().enumerate() {
        packed[i].copy_from_slice(&h(s));
    }

    let calldata = encode_forward_range(
        manifest, packed, max_pos, 0, pos_hi, 0, layer_hi, PROMPT_IDS,
    );
    println!(
        "engine={} code={} bytes | manifest={} | maxPos={max_pos} posHi={pos_hi} layerHi={layer_hi} | calldata={} bytes",
        to,
        engine_code.len(),
        manifest,
        calldata.len()
    );

    // Base state: the engine account (code-only, funded).
    let mut base = CacheDB::new(EmptyDB::new());
    base.insert_account_info(
        to,
        AccountInfo {
            balance: U256::from(1u64) << 200,
            nonce: 0,
            code_hash: keccak256(&engine_code),
            code: Some(Bytecode::new_raw(engine_code.clone().into())),
            ..Default::default()
        },
    );
    base.insert_account_info(
        from,
        AccountInfo {
            balance: U256::from(1u64) << 200,
            ..Default::default()
        },
    );

    // Overlay mount from the real weights/tokenizer files.
    let mount = std::sync::Arc::new(gk_fast_view::OverlayMount::from_files(
        &weights, &tokenizer, manifest,
    )?);
    let mounts = gk_fast_view::OverlayMountSet::from(mount);
    println!("overlay chunks: {}", mounts.manifests().len());

    let spec = match env_or("GK_SPEC", "CANCUN").to_ascii_uppercase().as_str() {
        "CANCUN" => SpecId::CANCUN,
        "PRAGUE" => SpecId::PRAGUE,
        "SHANGHAI" => SpecId::SHANGHAI,
        "OSAKA" => SpecId::OSAKA,
        s => panic!("unknown spec {s}"),
    };
    let profile = match env_or("GK_PROFILE", "UnboundedV1Xl").as_str() {
        "Chain" => Profile::Chain,
        "UnboundedV1" => Profile::UnboundedV1,
        "UnboundedV1Xl" => Profile::UnboundedV1Xl,
        s => panic!("unknown profile {s}"),
    };
    let gas: u64 = env_or("GK_GAS", &(1u64 << 40).to_string()).parse()?;
    // `from` matches the live sidecar (zero address) unless overridden.
    let from = if let Ok(f) = std::env::var("GK_FROM") {
        Address::from_slice(&h(&f))
    } else {
        from
    };
    println!("spec={spec:?} profile={profile:?} gas={gas} from={from}");

    let env = ViewEnv {
        spec,
        gas_limit: gas.max(30_000_000),
        ..ViewEnv::default()
    };
    let tx = ViewTx::call(from, to, calldata.clone(), gas);

    let mut fv = FastView::new(spec)?;

    use std::io::Write as _;
    println!("[revmc-jit] running...");
    std::io::stdout().flush().ok();
    let jit = fv.call_view(&base, mounts.clone(), &env, &tx, profile);
    match &jit {
        Ok(rd) => summarize("revmc-jit", rd),
        Err(e) => println!("  revmc-jit ERROR: {e:#}"),
    }
    std::io::stdout().flush().ok();

    if std::env::var("GK_SKIP_INTERP").is_ok() {
        return Ok(());
    }

    println!("[interpreter] running (slow on real weights)...");
    std::io::stdout().flush().ok();
    let interp = fv.call_view_interpreted(&base, mounts.clone(), &env, &tx, profile);
    match &interp {
        Ok(rd) => summarize("interpreter", rd),
        Err(e) => println!("  interpreter ERROR: {e:#}"),
    }

    if let (Ok(a), Ok(b)) = (&interp, &jit) {
        if a == b {
            println!("MATCH: interpreter == revmc-jit (byte-identical returndata)");
        } else {
            println!("DIVERGENCE: interpreter != revmc-jit");
        }
    }
    Ok(())
}

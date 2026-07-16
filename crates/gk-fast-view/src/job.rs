//! A tiny, dependency-free text format describing a self-contained view-call
//! job, plus its executor. Shared by three call sites so they cannot drift:
//!
//! * the sidecar binary (`src/bin/gk_fast_view.rs`) reads a `.job` from stdin,
//! * the consensus-gate test (`tests/consensus.rs`) reads committed `.fixture`
//!   files (same format + an `expected` line) and asserts byte-identity,
//! * the revm-31 golden generator (in the analyzer's evmsketch crate) *emits*
//!   this format, so the golden it produces is the exact scenario replayed here.
//!
//! ## Why the base state is passed inline
//!
//! An overlay-mode seg-engine view call (`Qwen35SegEngine.forwardRange` with
//! `rootDirectory == address(0)`) touches only (a) the engine contract's own
//! code and (b) overlay chunk accounts (phantom addresses served from the
//! mounted weights). It reads no other chain state. So the base state the fast
//! executor needs is just the engine code — which the node already fetches via
//! `eth_getCode` and can hand over inline — while the multi-gigabyte weights
//! come from local `mount_files`. This keeps the sidecar RPC-free and fully
//! testable (see the crate README / final report for the RPC-backed variant).
//!
//! ## Format (one directive per line, `#` comments and blanks ignored)
//!
//! ```text
//! spec CANCUN                       # SpecId (CANCUN, PRAGUE, ...)
//! profile UnboundedV1               # Chain | UnboundedV1 | UnboundedV1Xl
//! from   <40-hex address>
//! to     <40-hex address>
//! input  <hex calldata>             # may be empty ("input" alone)
//! gas    <u64>
//! account <40-hex> <hex code>       # a base-state contract (code-only, funded)
//! mount_pairs <64-hex manifest>     # opens an inline-pairs overlay mount
//! pair    <40-hex> <hex code>       # a chunk of the most recent mount_pairs
//! mount_files <weights> <tokenizer> <64-hex manifest>   # mmap-backed mount
//! expected <hex>                    # (fixtures only) expected returndata
//! ```

use std::path::PathBuf;

use anyhow::{Context as _, Result, anyhow, bail};
use revm_database::{CacheDB, EmptyDB};
use revm_primitives::{Address, B256, Bytes, U256, hardfork::SpecId, keccak256};
use revm_state::{AccountInfo, Bytecode};

use crate::overlay::{OverlayMount, OverlayMountSet};
use crate::{FastView, Profile, ViewEnv, ViewTx};

/// One inline-pairs or mmap-file overlay mount.
#[derive(Debug, Clone)]
pub enum MountSpec {
    /// Pre-derived `(address, code)` chunk bindings under a manifest.
    Pairs {
        manifest: B256,
        pairs: Vec<(Address, Bytes)>,
    },
    /// The weights+tokenizer blob files + pinned manifest (mmap-backed).
    Files {
        weights: PathBuf,
        tokenizer: PathBuf,
        manifest: B256,
    },
}

/// A fully-specified, self-contained view-call job.
#[derive(Debug, Clone)]
pub struct Job {
    pub spec: SpecId,
    pub profile: Profile,
    pub from: Address,
    pub to: Address,
    pub input: Bytes,
    pub gas: u64,
    /// Base-state contracts (code-only, funded, nonce 0).
    pub accounts: Vec<(Address, Bytes)>,
    pub mounts: Vec<MountSpec>,
    /// Expected returndata, when this is a golden fixture.
    pub expected: Option<Bytes>,
}

fn parse_addr(s: &str) -> Result<Address> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex_decode(s)?;
    if bytes.len() != 20 {
        bail!("address must be 20 bytes, got {}", bytes.len());
    }
    Ok(Address::from_slice(&bytes))
}

fn parse_b256(s: &str) -> Result<B256> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex_decode(s)?;
    if bytes.len() != 32 {
        bail!("b256 must be 32 bytes, got {}", bytes.len());
    }
    Ok(B256::from_slice(&bytes))
}

fn parse_bytes(s: Option<&str>) -> Result<Bytes> {
    match s {
        None => Ok(Bytes::new()),
        Some(s) => {
            let s = s.strip_prefix("0x").unwrap_or(s);
            Ok(Bytes::from(hex_decode(s)?))
        }
    }
}

/// Minimal hex decoder (no external crate needed in the library).
pub fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
        bail!("odd-length hex string");
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let b = s.as_bytes();
    let nib = |c: u8| -> Result<u8> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(anyhow!("invalid hex digit {:?}", c as char)),
        }
    };
    for pair in b.chunks(2) {
        out.push((nib(pair[0])? << 4) | nib(pair[1])?);
    }
    Ok(out)
}

/// Lowercase hex, no `0x` prefix.
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

impl Job {
    /// Parse a job from the text format.
    pub fn parse(text: &str) -> Result<Self> {
        let mut spec: Option<SpecId> = None;
        let mut profile = Profile::Chain;
        let mut from = Address::ZERO;
        let mut to = Address::ZERO;
        let mut input = Bytes::new();
        let mut gas: u64 = 0;
        let mut accounts = Vec::new();
        let mut mounts: Vec<MountSpec> = Vec::new();
        let mut expected: Option<Bytes> = None;

        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut it = line.split_whitespace();
            let key = it.next().unwrap();
            let ctx = || format!("line {}: `{}`", lineno + 1, raw.trim());
            match key {
                "spec" => {
                    let v = it.next().with_context(ctx)?;
                    spec = Some(parse_spec(v).with_context(ctx)?);
                }
                "profile" => {
                    profile = parse_profile(it.next().with_context(ctx)?).with_context(ctx)?;
                }
                "from" => from = parse_addr(it.next().with_context(ctx)?).with_context(ctx)?,
                "to" => to = parse_addr(it.next().with_context(ctx)?).with_context(ctx)?,
                "input" => input = parse_bytes(it.next()).with_context(ctx)?,
                "gas" => {
                    gas = it
                        .next()
                        .with_context(ctx)?
                        .parse()
                        .with_context(ctx)?;
                }
                "account" => {
                    let addr = parse_addr(it.next().with_context(ctx)?).with_context(ctx)?;
                    let code = parse_bytes(it.next()).with_context(ctx)?;
                    accounts.push((addr, code));
                }
                "mount_pairs" => {
                    let manifest = parse_b256(it.next().with_context(ctx)?).with_context(ctx)?;
                    mounts.push(MountSpec::Pairs {
                        manifest,
                        pairs: Vec::new(),
                    });
                }
                "pair" => {
                    let addr = parse_addr(it.next().with_context(ctx)?).with_context(ctx)?;
                    let code = parse_bytes(it.next()).with_context(ctx)?;
                    match mounts.last_mut() {
                        Some(MountSpec::Pairs { pairs, .. }) => pairs.push((addr, code)),
                        _ => bail!("{}: `pair` with no preceding `mount_pairs`", ctx()),
                    }
                }
                "mount_files" => {
                    let weights = it.next().with_context(ctx)?.to_string();
                    let tokenizer = it.next().with_context(ctx)?.to_string();
                    let manifest = parse_b256(it.next().with_context(ctx)?).with_context(ctx)?;
                    mounts.push(MountSpec::Files {
                        weights: weights.into(),
                        tokenizer: tokenizer.into(),
                        manifest,
                    });
                }
                "expected" => expected = Some(parse_bytes(it.next()).with_context(ctx)?),
                other => bail!("{}: unknown directive `{}`", ctx(), other),
            }
        }

        Ok(Job {
            spec: spec.ok_or_else(|| anyhow!("job missing `spec`"))?,
            profile,
            from,
            to,
            input,
            gas,
            accounts,
            mounts,
            expected,
        })
    }

    /// Build the [`OverlayMountSet`] this job specifies (verifying mmap mounts).
    pub fn mount_set(&self) -> Result<OverlayMountSet> {
        let mut mounts = Vec::with_capacity(self.mounts.len());
        for m in &self.mounts {
            let mount = match m {
                MountSpec::Pairs { manifest, pairs } => {
                    OverlayMount::from_pairs(*manifest, pairs.iter().cloned())
                }
                MountSpec::Files {
                    weights,
                    tokenizer,
                    manifest,
                } => OverlayMount::from_files(weights, tokenizer, *manifest)?,
            };
            mounts.push(std::sync::Arc::new(mount));
        }
        Ok(OverlayMountSet::new(mounts))
    }

    /// The in-memory base-state db (funded engine/contract accounts).
    pub fn base_db(&self) -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::new());
        for (addr, code) in &self.accounts {
            db.insert_account_info(
                *addr,
                AccountInfo {
                    balance: U256::from(1u64) << 200,
                    nonce: 0,
                    code_hash: keccak256(code),
                    code: Some(Bytecode::new_raw(code.clone())),
                    ..Default::default()
                },
            );
        }
        // Fund the caller too, so the call succeeds even without the disable_*
        // flags (belt-and-suspenders; the flags are also set).
        db.insert_account_info(
            self.from,
            AccountInfo {
                balance: U256::from(1u64) << 200,
                ..Default::default()
            },
        );
        db
    }

    /// [`ViewEnv`] for this job. Non-execution-affecting header fields
    /// (number/timestamp/coinbase/prevrandao) are fixed constants — a view call
    /// that read them would already be non-consensus-deterministic, and the
    /// golden generator pins the identical values.
    pub fn view_env(&self) -> ViewEnv {
        ViewEnv {
            chain_id: 1,
            spec: self.spec,
            number: 0,
            timestamp: 0,
            gas_limit: self.gas.max(30_000_000),
            ..ViewEnv::default()
        }
    }

    pub fn view_tx(&self) -> ViewTx {
        ViewTx::call(self.from, self.to, self.input.clone(), self.gas)
    }

    /// Execute the job through a fresh [`FastView`] and return the returndata.
    ///
    /// One-shot semantics: the engine is JIT-compiled from scratch for this call
    /// (nothing is amortized). The sidecar's persistent `--serve` mode uses
    /// [`execute_with`](Self::execute_with) instead so the compiled-fn cache
    /// survives across jobs.
    pub fn execute(&self) -> Result<Bytes> {
        let mut fv = FastView::new(self.spec)?;
        self.execute_with(&mut fv)
    }

    /// Execute the job against a caller-owned [`FastView`], returning the
    /// returndata. Byte-for-byte identical to [`execute`](Self::execute) — the
    /// compiled artifact is deterministic per `(codehash, spec)` — but the
    /// caller's `fv` keeps the compiled-engine cache across calls, so only the
    /// FIRST job of a given engine pays the LLVM codegen cost. This is the
    /// compile-once-run-many amortization the persistent daemon relies on.
    ///
    /// `fv` MUST be pinned to this job's `spec` (`FastView::call_view` enforces
    /// it and errors loudly on mismatch).
    pub fn execute_with(&self, fv: &mut FastView) -> Result<Bytes> {
        fv.call_view(
            self.base_db(),
            self.mount_set()?,
            &self.view_env(),
            &self.view_tx(),
            self.profile,
        )
    }
}

fn parse_spec(s: &str) -> Result<SpecId> {
    match s.to_ascii_uppercase().as_str() {
        "CANCUN" => Ok(SpecId::CANCUN),
        "PRAGUE" => Ok(SpecId::PRAGUE),
        "SHANGHAI" => Ok(SpecId::SHANGHAI),
        "OSAKA" => Ok(SpecId::OSAKA),
        other => bail!("unknown spec `{other}`"),
    }
}

fn parse_profile(s: &str) -> Result<Profile> {
    match s {
        "Chain" => Ok(Profile::Chain),
        "UnboundedV1" => Ok(Profile::UnboundedV1),
        "UnboundedV1Xl" => Ok(Profile::UnboundedV1Xl),
        other => bail!("unknown profile `{other}`"),
    }
}

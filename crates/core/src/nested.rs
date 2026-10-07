//! Nested settlement: one quorum signature over a Merkle root authorizes a tree of frames
//! across SDK-enabled contracts, each applied by the contract it belongs to.
//!
//! [`compute_frame_tree_canonical`] generalizes the canonical-checkpoint encoder from one
//! target to a tree of targets: a `CALL` into an eligible SDK callee becomes a `NESTED` op in
//! the caller's program, and the callee's part of the trace becomes the callee's own program.
//! [`encode_frame_tree`] then hashes every frame into a leaf, builds the tree the quorum signs,
//! and produces the witnesses the on-chain settlement passes down the call stack.
//!
//! Everything here must match solidity-sdk's `NestedFrames` library and `GasKillerSDK`
//! byte for byte: the leaves are what the quorum signs and what each frame recomputes.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use alloy_primitives::{Address, B256, Bytes, FixedBytes, U256, b256, keccak256};
use alloy_rpc_types::trace::geth::{DefaultFrame, StructLog};
use alloy_sol_types::{SolValue, sol};
use anyhow::{Result, bail};
use sha2::{Digest, Sha256};

pub use crate::encoding::encode_state_updates_to_abi as encoded_program;
use crate::sim_profile::STATE_TRACKER_SLOT;
use crate::trace::append_state_update_from_struct_log;
use crate::types::Opcode;
pub use crate::types::{IStateUpdateTypes, StateUpdate};

/// `keccak256("gaskiller.nested.leaf.v1")`
pub const NESTED_LEAF_TAG: B256 =
    b256!("0x292ca629b989c44a85e556e7c24acc26f9ab093b2b23c689f951e91718228c18");
/// `keccak256("gaskiller.nested.expiry.v1")`
pub const EXPIRY_LEAF_TAG: B256 =
    b256!("0xc5763e3656f5ee85e7416218ca476fe6364ba9f67d13b29c7331d1d1131dd88a");
/// The only frame mode the SDK accepts today: transition-counter pinning.
pub const FRAME_MODE: u8 = 0;

sol! {
    /// `NestedFrames.nestedLeaf`'s preimage, field for field.
    struct NestedLeafPreimage {
        bytes32 tag;
        uint256 chainId;
        address target;
        uint256 transitionIndex;
        address caller;
        uint256 value;
        bytes32 calldataHash;
        uint8 mode;
        bytes storageUpdates;
    }

    /// The unsigned data a parent passes to a child alongside the child's leaf hash.
    struct Witness {
        bytes32 calldataHash;
        bytes storageUpdates;
        bytes32[] proof;
        bytes[] children;
    }
}

// ============================================================================
// Leaves and tree
// ============================================================================

fn sha256(data: &[u8]) -> B256 {
    B256::from_slice(&Sha256::digest(data))
}

/// The root frame's leaf: exactly today's single-transition digest.
pub fn root_leaf(
    transition_index: U256,
    target: Address,
    selector: FixedBytes<4>,
    program: &Bytes,
) -> B256 {
    sha256(&(transition_index, target, selector, program.clone()).abi_encode_params())
}

/// A callee frame's leaf.
pub fn nested_leaf(
    chain_id: u64,
    target: Address,
    transition_index: U256,
    caller: Address,
    value: U256,
    calldata_hash: B256,
    program: &Bytes,
) -> B256 {
    sha256(
        &NestedLeafPreimage {
            tag: NESTED_LEAF_TAG,
            chainId: U256::from(chain_id),
            target,
            transitionIndex: transition_index,
            caller,
            value,
            calldataHash: calldata_hash,
            mode: FRAME_MODE,
            storageUpdates: program.clone(),
        }
        .abi_encode_params(),
    )
}

/// The leaf that bounds when a tree may settle.
pub fn expiry_leaf(chain_id: u64, root_contract: Address, expiry_block: u64) -> B256 {
    sha256(
        &(
            EXPIRY_LEAF_TAG,
            U256::from(chain_id),
            root_contract,
            U256::from(expiry_block),
        )
            .abi_encode_params(),
    )
}

/// OpenZeppelin `MerkleProof`'s commutative pair hash.
pub fn hash_pair(a: B256, b: B256) -> B256 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(lo.as_slice());
    buf[32..].copy_from_slice(hi.as_slice());
    keccak256(buf)
}

/// Pairs adjacent nodes level by level; an odd last node moves up unchanged. Leaves are
/// never sorted: their order is execution order.
fn up(level: &[B256]) -> Vec<B256> {
    level
        .chunks(2)
        .map(|pair| match pair {
            [a, b] => hash_pair(*a, *b),
            [a] => *a,
            _ => unreachable!("chunks(2) yields one or two nodes"),
        })
        .collect()
}

pub fn merkle_root(leaves: &[B256]) -> Result<B256> {
    if leaves.is_empty() {
        bail!("a tree needs at least one leaf");
    }
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        level = up(&level);
    }
    Ok(level[0])
}

pub fn merkle_proof(leaves: &[B256], mut index: usize) -> Result<Vec<B256>> {
    if index >= leaves.len() {
        bail!("leaf {index} is outside a tree of {}", leaves.len());
    }
    let mut proof = Vec::new();
    let mut level = leaves.to_vec();
    while level.len() > 1 {
        if let Some(sibling) = level.get(index ^ 1) {
            proof.push(*sibling);
        }
        level = up(&level);
        index /= 2;
    }
    Ok(proof)
}

// ============================================================================
// Frame tree
// ============================================================================

/// One frame's program and everything its leaf binds.
#[derive(Debug, Clone)]
pub struct FrameProgram {
    pub target: Address,
    /// The frame's signed parent; zero for the root frame, which anyone may submit.
    pub caller: Address,
    pub value: U256,
    /// Hash of the native call this frame stands in for; recorded for fraud proofs.
    pub calldata_hash: B256,
    /// The target's transition count before this frame, read from the trace. `None` only
    /// for a root frame that never incremented its counter.
    pub transition_index: Option<U256>,
    /// The program, with each `NESTED` op's `childLeaf` still zero.
    pub updates: Vec<StateUpdate>,
    /// Child frame ids, in the order of this program's `NESTED` ops.
    pub children: Vec<usize>,
}

/// A trace split into per-frame programs, in execution (depth-first pre-order) order with
/// the root frame first.
#[derive(Debug)]
pub struct FrameTreeExtract {
    pub frames: Vec<FrameProgram>,
    pub skipped_opcodes: HashSet<Opcode>,
    /// Gas of every `CALL` op that replays natively, across all frames.
    pub call_gas_total: u64,
    /// Every contract whose transition counter the trace moves: the contracts a settlement
    /// pins, whether they are nested frames or run natively inside a `CALL`.
    pub counter_moves: BTreeSet<Address>,
}

/// Prices whether replaying a callee's frame beats running it natively.
///
/// Native execution and replay both pay the frame's storage writes and logs, so the
/// comparison is the frame's remaining native gas (its computation) against what nesting
/// adds: a fixed per-frame cost (registry lookup, parent check, proof, decode) plus the
/// witness bytes, paid once as calldata and again per ancestor that copies them down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NestingCostModel {
    pub frame_overhead_gas: u64,
    pub witness_byte_gas: u64,
    pub witness_byte_gas_per_level: u64,
}

/// Version 1 of the nesting cost model. The fixed overhead is the per-frame cost measured in
/// solidity-sdk's `NestedChain` example (17k–23k); changing any value changes which frames
/// nest, and so the signed bytes, so a change needs a fleet-wide rollout.
pub const NESTING_COST_MODEL_V1: NestingCostModel = NestingCostModel {
    frame_overhead_gas: 20_000,
    witness_byte_gas: 16,
    witness_byte_gas_per_level: 1,
};

/// Proof words assumed when sizing a witness before the tree exists (a 256-leaf tree).
const ESTIMATED_PROOF_WORDS: usize = 8;
/// ABI head of an encoded witness: offset, calldata hash, three offsets, three lengths.
const WITNESS_HEAD_BYTES: usize = 8 * 32;

/// How the trace treated a `CALL` into a callee, for the eligibility rule.
#[derive(Debug, Clone, Copy)]
enum Bump {
    /// No storage write, external call or log in the callee's own scope yet.
    Undecided,
    /// The callee's first effect incremented its counter to `index + 1`.
    First(U256),
    /// Something else came first, or no increment happened.
    Disqualified,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    bump: Bump,
    /// `TSTORE` / `SELFDESTRUCT` in the callee's scope: unsupported in a program, so the
    /// frame stays a `CALL` where it already replays natively.
    unsupported_op: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Root,
    Call(usize),
}

/// Pre-pass deciding which `CALL`s could become nested frames, and the root's index.
///
/// A callee frame qualifies when the first storage write, external call or log in its own
/// scope (the frame plus its `DELEGATECALL`/`CALLCODE` descendants) is the increment of its
/// transition counter. On replay `applyNested` increments first, so anything observable
/// before the increment natively would happen in a different order. This needs the whole
/// callee frame, which the main walker has not seen when it reaches the `CALL`.
fn scan_candidates(
    logs: &[StructLog],
    root: Address,
) -> Result<(HashMap<usize, Candidate>, Option<U256>)> {
    struct ScanFrame {
        depth: u64,
        ctx: Option<Address>,
        scope: Option<Scope>,
    }
    struct Pending {
        parent_depth: u64,
        ctx: Option<Address>,
        scope: Option<Scope>,
    }

    let mut candidates: HashMap<usize, Candidate> = HashMap::new();
    let mut root_index = None;
    let mut frames = vec![ScanFrame {
        depth: 1,
        ctx: Some(root),
        scope: Some(Scope::Root),
    }];
    let mut pending: Option<Pending> = None;

    for (i, log) in logs.iter().enumerate() {
        let depth = log.depth;
        if let Some(p) = pending.take() {
            if depth == p.parent_depth + 1 {
                frames.push(ScanFrame {
                    depth,
                    ctx: p.ctx,
                    scope: p.scope,
                });
            } else if let Some(Scope::Call(call)) = p.scope {
                candidates.remove(&call);
            }
        }
        while frames.last().map(|f| f.depth).unwrap_or(1) > depth {
            frames.pop();
        }
        let cur = frames.last().expect("root frame must remain");
        let (ctx, scope) = (cur.ctx, cur.scope);
        if log.error.is_some() {
            continue;
        }
        let stack = log.stack.as_deref().unwrap_or_default();
        let top = |n: usize| stack.get(stack.len().wrapping_sub(1 + n)).copied();

        let op = log.op.as_ref();
        match op {
            "SSTORE" => {
                let (Some(slot), Some(value)) = (top(0), top(1)) else {
                    bail!("SSTORE log at {i} has a short stack");
                };
                let tracker = B256::from(slot) == STATE_TRACKER_SLOT;
                match scope {
                    Some(Scope::Root) if tracker && root_index.is_none() => {
                        root_index = Some(value.saturating_sub(U256::from(1)));
                    }
                    Some(Scope::Call(call)) => {
                        if let Some(c) = candidates.get_mut(&call)
                            && matches!(c.bump, Bump::Undecided)
                        {
                            c.bump = if tracker {
                                Bump::First(value.saturating_sub(U256::from(1)))
                            } else {
                                Bump::Disqualified
                            };
                        }
                    }
                    _ => {}
                }
            }
            "TSTORE" | "SELFDESTRUCT" => {
                if let Some(Scope::Call(call)) = scope
                    && let Some(c) = candidates.get_mut(&call)
                {
                    c.unsupported_op = true;
                }
            }
            _ => {}
        }

        let observable = matches!(
            op,
            "CALL"
                | "STATICCALL"
                | "CREATE"
                | "CREATE2"
                | "LOG0"
                | "LOG1"
                | "LOG2"
                | "LOG3"
                | "LOG4"
        );
        if observable
            && let Some(Scope::Call(call)) = scope
            && let Some(c) = candidates.get_mut(&call)
            && matches!(c.bump, Bump::Undecided)
        {
            c.bump = Bump::Disqualified;
        }

        let callee = || top(1).map(|v| Address::from_word(v.into()));
        pending = match op {
            "CALL" => {
                candidates.insert(
                    i,
                    Candidate {
                        bump: Bump::Undecided,
                        unsupported_op: false,
                    },
                );
                Some(Pending {
                    parent_depth: depth,
                    ctx: callee(),
                    scope: Some(Scope::Call(i)),
                })
            }
            "STATICCALL" => Some(Pending {
                parent_depth: depth,
                ctx: callee(),
                scope: None,
            }),
            "DELEGATECALL" | "CALLCODE" => Some(Pending {
                parent_depth: depth,
                ctx,
                scope,
            }),
            "CREATE" | "CREATE2" => Some(Pending {
                parent_depth: depth,
                ctx: None,
                scope: None,
            }),
            _ => None,
        };
    }
    Ok((candidates, root_index))
}

struct ProgramState {
    target: Address,
    caller: Address,
    value: U256,
    calldata_hash: B256,
    transition_index: Option<U256>,
    parent: Option<usize>,
    updates: Option<Vec<StateUpdate>>,
    children: Vec<usize>,
    /// The `CALL` op this frame replaces, restored if the frame is pruned.
    fallback: Option<IStateUpdateTypes::Call>,
    dropped: bool,
    native_gas: u64,
    /// Gas of the storage writes and logs the program reproduces.
    effect_gas: u64,
    /// Native gas of calls made from this frame, nested or not.
    child_call_gas: u64,
}

struct WalkFrame {
    depth: u64,
    storage_ctx: Option<Address>,
    /// The program this frame emits into; `None` for frames that replay natively.
    program: Option<usize>,
    /// Writes to every contract's storage made in this frame and committed by its
    /// children; merged into the parent on success, dropped on revert.
    journal: BTreeMap<Address, BTreeMap<B256, B256>>,
    out: Vec<StateUpdate>,
    emitted_view: BTreeMap<B256, B256>,
    emitted_call_gas: Option<u64>,
    /// Set on the first frame of a nested program: gas at the `CALL` that opened it.
    nested_start_gas: Option<u64>,
    /// Programs with this id or above were opened inside this frame, and are dropped with
    /// it if it reverts.
    first_program: usize,
}

struct PendingFrame {
    parent_depth: u64,
    storage_ctx: Option<Address>,
    program: Option<usize>,
    emitted_view: BTreeMap<B256, B256>,
    emitted_call_gas: Option<u64>,
    nested_start_gas: Option<u64>,
}

/// The canonical storage image of `target` visible right now: every open frame's journal
/// for it, merged bottom-up.
fn visible_image(frames: &[WalkFrame], target: Address) -> BTreeMap<B256, B256> {
    let mut image = BTreeMap::new();
    for f in frames {
        if let Some(slots) = f.journal.get(&target) {
            for (slot, value) in slots {
                image.insert(*slot, *value);
            }
        }
    }
    image
}

/// Emit a canonical state slice for the program the top frame emits into.
fn flush_slice(frames: &mut [WalkFrame], programs: &[ProgramState]) {
    let cur = frames.last().expect("root frame must remain");
    let target = programs[cur.program.expect("only emitting frames flush")].target;
    let visible = visible_image(frames, target);
    let cur = frames.last_mut().expect("root frame must remain");
    for (slot, value) in visible {
        if cur.emitted_view.get(&slot) != Some(&value) {
            cur.out
                .push(StateUpdate::Store(IStateUpdateTypes::Store { slot, value }));
            cur.emitted_view.insert(slot, value);
        }
    }
}

/// Split a trace into per-frame programs with canonical state checkpointing.
///
/// The root frame's program is exactly what [`crate::compute_state_updates_canonical`]
/// produces when no callee is nested. A `CALL` from an emitting frame into a callee becomes a
/// `NESTED` op when the callee passes the trace-side eligibility rule (see
/// [`scan_candidates`]), `is_nestable(callee)` holds, and nesting it is cheaper under `cost`.
/// Every condition depends only on the trace and on `is_nestable`, which callers must answer
/// from state at the traced block so every prover splits the trace the same way.
///
/// Each nested frame's program gets its own slices: before every boundary (`CALL`, `CREATE`,
/// `NESTED`) its target is brought to the canonical image, and a final slice re-asserts every
/// slot the target's image holds when the frame returns. A callee that reverts natively yields
/// no frame and no `NESTED` op.
pub fn compute_frame_tree_canonical(
    trace: DefaultFrame,
    root: Address,
    is_nestable: &dyn Fn(Address) -> bool,
    cost: &NestingCostModel,
) -> Result<FrameTreeExtract> {
    split_frame_tree(trace, root, Eligibility::Consumers(is_nestable), cost)
}

/// [`compute_frame_tree_canonical`] for code that never integrated the SDK: what a historical
/// transaction would have settled as had `root` and every contract in `owned` been consumers.
///
/// Any `CALL` from a frame into a contract in `owned` may become a frame, since these
/// contracts have no transition counter whose increment the rule could look for. Frames are
/// still kept only where nesting is cheaper under `cost`, and a callee using an op a program
/// cannot express stays a `CALL`. A contract in `owned` that is reached only through a contract
/// outside it runs inside that contract's native `CALL`, as it would in a real settlement.
/// Frames carry their counter's index where the trace shows one, and zero otherwise; the split
/// is for pricing, not for signing.
pub fn compute_frame_tree_hypothetical(
    trace: DefaultFrame,
    root: Address,
    owned: &BTreeSet<Address>,
    cost: &NestingCostModel,
) -> Result<FrameTreeExtract> {
    split_frame_tree(trace, root, Eligibility::Hypothetical(owned), cost)
}

/// A program without its `NESTED` ops, for pricers that cannot execute them.
pub fn without_nested_ops(updates: &[StateUpdate]) -> Vec<StateUpdate> {
    updates
        .iter()
        .filter(|u| !matches!(u, StateUpdate::Nested(_)))
        .cloned()
        .collect()
}

/// Which callees a split may turn into frames.
#[derive(Clone, Copy)]
enum Eligibility<'a> {
    /// Deployed consumers: `is_nestable`, and the callee's first effect is its counter increment.
    Consumers(&'a dyn Fn(Address) -> bool),
    /// Contracts priced as if they were consumers.
    Hypothetical(&'a BTreeSet<Address>),
}

impl Eligibility<'_> {
    /// The transition index the callee's frame takes, or `None` when the call stays a `CALL`.
    fn frame_index(self, candidate: Option<&Candidate>, callee: Option<Address>) -> Option<U256> {
        let candidate = candidate.filter(|c| !c.unsupported_op)?;
        let callee = callee?;
        match (self, candidate.bump) {
            (Self::Consumers(is_nestable), Bump::First(index)) if is_nestable(callee) => {
                Some(index)
            }
            (Self::Hypothetical(owned), bump) if owned.contains(&callee) => Some(match bump {
                Bump::First(index) => index,
                Bump::Undecided | Bump::Disqualified => U256::ZERO,
            }),
            _ => None,
        }
    }
}

#[tracing::instrument(name = "gas.trace_parse_frame_tree", skip_all, fields(frame_count = tracing::field::Empty))]
fn split_frame_tree(
    trace: DefaultFrame,
    root: Address,
    eligibility: Eligibility<'_>,
    cost: &NestingCostModel,
) -> Result<FrameTreeExtract> {
    let (candidates, root_index) = scan_candidates(&trace.struct_logs, root)?;
    let mut skipped_opcodes = HashSet::new();
    let mut total_call_gas = 0u64;

    let mut programs = vec![ProgramState {
        target: root,
        caller: Address::ZERO,
        value: U256::ZERO,
        calldata_hash: B256::ZERO,
        transition_index: root_index,
        parent: None,
        updates: None,
        children: Vec::new(),
        fallback: None,
        dropped: false,
        native_gas: 0,
        effect_gas: 0,
        child_call_gas: 0,
    }];
    let mut frames = vec![WalkFrame {
        depth: 1,
        storage_ctx: Some(root),
        program: Some(0),
        journal: BTreeMap::new(),
        out: Vec::new(),
        emitted_view: BTreeMap::new(),
        emitted_call_gas: None,
        nested_start_gas: None,
        first_program: 0,
    }];
    let mut pending: Option<PendingFrame> = None;

    for (log_index, struct_log) in trace.struct_logs.into_iter().enumerate() {
        let depth = struct_log.depth;
        let op = struct_log.op.as_ref().to_string();

        if let Some(p) = pending.take() {
            if depth == p.parent_depth + 1 {
                let first_program = match (p.nested_start_gas, p.program) {
                    (Some(_), Some(id)) => id,
                    _ => programs.len(),
                };
                frames.push(WalkFrame {
                    depth,
                    storage_ctx: p.storage_ctx,
                    program: p.program,
                    journal: BTreeMap::new(),
                    out: Vec::new(),
                    emitted_view: p.emitted_view,
                    emitted_call_gas: p.emitted_call_gas,
                    nested_start_gas: p.nested_start_gas,
                    first_program,
                });
            } else {
                if p.nested_start_gas.is_some() {
                    bail!(
                        "a nested candidate at depth {} opened no frame",
                        p.parent_depth
                    );
                }
                // No frame was entered (EOA / precompile / failed call): the call completed
                // inline. Account its gas now if it was emitted.
                if let Some(gas_after_opcode) = p.emitted_call_gas {
                    let gas = gas_after_opcode.saturating_sub(struct_log.gas);
                    total_call_gas += gas;
                    if let Some(parent) = frames.last().and_then(|f| f.program) {
                        programs[parent].child_call_gas += gas;
                    }
                }
            }
        }

        // Pop frames we have stepped out of. The resume log (this one) carries the child's
        // success flag on top of the parent's stack.
        while frames.last().map(|f| f.depth).unwrap_or(1) > depth {
            let closing_depth = frames.last().expect("frame stack underflow").depth;
            if closing_depth != depth + 1 {
                bail!(
                    "trace depth jumped from {closing_depth} to {depth} without resume logs — cannot attribute frame outcomes"
                );
            }
            let stack = struct_log
                .stack
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("resume log at depth {depth} has no stack"))?;
            let success = stack
                .last()
                .map(|v| !v.is_zero())
                .ok_or_else(|| anyhow::anyhow!("resume log at depth {depth} has empty stack"))?;

            let nested_root = frames.last().and_then(|f| f.nested_start_gas);
            if success && nested_root.is_some() {
                flush_slice(&mut frames, &programs);
            }
            let frame = frames.pop().expect("frame stack underflow");
            let parent_program = frames.last().and_then(|f| f.program);
            if !success {
                for program in &mut programs[frame.first_program..] {
                    program.dropped = true;
                }
            }

            if let Some(gas_after_opcode) = frame.emitted_call_gas {
                let gas = gas_after_opcode.saturating_sub(struct_log.gas);
                total_call_gas += gas;
                if let Some(parent) = parent_program {
                    programs[parent].child_call_gas += gas;
                }
            }

            if let Some(start_gas) = nested_root {
                let id = frame.program.expect("a nested frame emits");
                if success {
                    let native = start_gas.saturating_sub(struct_log.gas);
                    programs[id].native_gas = native;
                    programs[id].updates = Some(frame.out);
                    if let Some(parent) = parent_program {
                        programs[parent].child_call_gas += native;
                    }
                    let parent = frames.last_mut().expect("root frame must remain");
                    for (addr, slots) in frame.journal {
                        parent.journal.entry(addr).or_default().extend(slots);
                    }
                } else {
                    // The callee reverted and its caller carried on: there is nothing to apply,
                    // so the op goes too.
                    let parent = frames.last_mut().expect("root frame must remain");
                    match parent.out.pop() {
                        Some(StateUpdate::Nested(_)) => {}
                        other => {
                            bail!("reverted nested frame's op was not the parent's last: {other:?}")
                        }
                    }
                }
                continue;
            }

            if success {
                let child_emitting = frame.program.is_some();
                let parent = frames.last_mut().expect("root frame must remain");
                for (addr, slots) in frame.journal {
                    parent.journal.entry(addr).or_default().extend(slots);
                }
                parent.out.extend(frame.out);
                // An emitting child ran while the parent was suspended, so its emitted view
                // is a superset of the parent's — adopt it.
                if child_emitting {
                    parent.emitted_view = frame.emitted_view;
                }
            }
            // On failure everything (journal, out, view) is dropped — the EVM rolled the
            // whole sub-frame back.
        }

        let idx = frames.len() - 1;
        let cur_program = frames[idx].program;
        let cur_emitting = cur_program.is_some();
        let cur_ctx = frames[idx].storage_ctx;
        let op_errored = struct_log.error.is_some();

        match op.as_str() {
            "SSTORE" => {
                if let Some(ctx) = cur_ctx
                    && !op_errored
                {
                    let mut stack = struct_log
                        .stack
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("SSTORE log has no stack"))?;
                    stack.reverse();
                    let slot: B256 = stack[0].into();
                    let value: B256 = stack[1].into();
                    frames[idx]
                        .journal
                        .entry(ctx)
                        .or_default()
                        .insert(slot, value);
                    if let Some(p) = cur_program
                        && programs[p].target == ctx
                    {
                        programs[p].effect_gas += struct_log.gas_cost;
                    }
                }
            }
            "TSTORE" | "SELFDESTRUCT" => {
                if cur_emitting {
                    skipped_opcodes.insert(op.clone());
                }
            }
            "DELEGATECALL" | "CALLCODE" => {
                if !op_errored {
                    let emitted_view = if cur_emitting {
                        frames[idx].emitted_view.clone()
                    } else {
                        BTreeMap::new()
                    };
                    pending = Some(PendingFrame {
                        parent_depth: depth,
                        storage_ctx: cur_ctx,
                        program: cur_program,
                        emitted_view,
                        emitted_call_gas: None,
                        nested_start_gas: None,
                    });
                }
            }
            "STATICCALL" => {
                if !op_errored {
                    let stack = struct_log
                        .stack
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("STATICCALL log has no stack"))?;
                    let callee = stack
                        .get(stack.len().wrapping_sub(2))
                        .map(|v| Address::from_word((*v).into()));
                    pending = Some(PendingFrame {
                        parent_depth: depth,
                        storage_ctx: callee,
                        program: None,
                        emitted_view: BTreeMap::new(),
                        emitted_call_gas: None,
                        nested_start_gas: None,
                    });
                }
            }
            "CALL" => {
                let stack = struct_log
                    .stack
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("CALL log has no stack"))?;
                let callee = stack
                    .get(stack.len().wrapping_sub(2))
                    .map(|v| Address::from_word((*v).into()));
                let gas_after_opcode = struct_log.gas;
                let mut next = PendingFrame {
                    parent_depth: depth,
                    storage_ctx: callee,
                    program: None,
                    emitted_view: BTreeMap::new(),
                    emitted_call_gas: None,
                    nested_start_gas: None,
                };
                if cur_emitting && !op_errored {
                    // Boundary: external code is about to observe the current target's
                    // storage — bring it to the canonical image first.
                    flush_slice(&mut frames, &programs);
                    let nested_index = eligibility.frame_index(candidates.get(&log_index), callee);
                    let mut decoded = Vec::with_capacity(1);
                    if append_state_update_from_struct_log(&mut decoded, struct_log)?.is_some() {
                        unreachable!("CALL is never a skipped opcode");
                    }
                    match (nested_index, decoded.pop()) {
                        (Some(index), Some(StateUpdate::Call(call))) => {
                            let parent = cur_program.expect("emitting");
                            let id = programs.len();
                            programs.push(ProgramState {
                                target: call.target,
                                caller: cur_ctx.unwrap_or_default(),
                                value: call.value,
                                calldata_hash: keccak256(&call.callargs),
                                transition_index: Some(index),
                                parent: Some(parent),
                                updates: None,
                                children: Vec::new(),
                                fallback: Some(call.clone()),
                                dropped: false,
                                native_gas: 0,
                                effect_gas: 0,
                                child_call_gas: 0,
                            });
                            programs[parent].children.push(id);
                            frames[idx]
                                .out
                                .push(StateUpdate::Nested(IStateUpdateTypes::Nested {
                                    target: call.target,
                                    value: call.value,
                                    childLeaf: B256::ZERO,
                                }));
                            next.program = Some(id);
                            next.nested_start_gas = Some(gas_after_opcode);
                        }
                        (_, Some(update)) => {
                            frames[idx].out.push(update);
                            next.emitted_call_gas = Some(gas_after_opcode);
                        }
                        (_, None) => bail!("CALL produced no update"),
                    }
                }
                if !op_errored {
                    pending = Some(next);
                }
            }
            "CREATE" | "CREATE2" => {
                if cur_emitting && !op_errored {
                    // Initcode can call back into the target: boundary here too.
                    flush_slice(&mut frames, &programs);
                    if append_state_update_from_struct_log(&mut frames[idx].out, struct_log)?
                        .is_some()
                    {
                        unreachable!("CREATE/CREATE2 are never skipped opcodes");
                    }
                }
                if !op_errored {
                    pending = Some(PendingFrame {
                        parent_depth: depth,
                        // A fresh deployment can never alias a target (targets already have
                        // code), so its writes are never emitted.
                        storage_ctx: None,
                        program: None,
                        emitted_view: BTreeMap::new(),
                        emitted_call_gas: None,
                        nested_start_gas: None,
                    });
                }
            }
            "LOG0" | "LOG1" | "LOG2" | "LOG3" | "LOG4" if cur_emitting && !op_errored => {
                let gas_cost = struct_log.gas_cost;
                let skipped =
                    append_state_update_from_struct_log(&mut frames[idx].out, struct_log)?;
                debug_assert!(skipped.is_none(), "LOG* is never a skipped opcode");
                programs[cur_program.expect("emitting")].effect_gas += gas_cost;
            }
            _ => {}
        }
    }

    if frames.len() != 1 {
        bail!(
            "trace ended with {} unclosed frame(s) — malformed trace",
            frames.len() - 1
        );
    }

    // Final slice: the signed program must fully determine the root target's end state,
    // even if a re-entrant call's on-chain replay diverged.
    flush_slice(&mut frames, &programs);
    let root_frame = frames.pop().expect("root frame present");
    programs[0].updates = Some(root_frame.out);

    let counter_moves = root_frame
        .journal
        .iter()
        .filter(|(_, slots)| slots.contains_key(&STATE_TRACKER_SLOT))
        .map(|(addr, _)| *addr)
        .collect();

    drop_descendants_of_dropped(&mut programs);
    let dropped: Vec<bool> = programs.iter().map(|p| p.dropped).collect();
    for program in &mut programs {
        program.children.retain(|c| !dropped[*c]);
    }
    total_call_gas += prune_unprofitable(&mut programs, cost);
    let frames = compact(programs)?;
    tracing::Span::current().record("frame_count", frames.len());

    Ok(FrameTreeExtract {
        frames,
        skipped_opcodes,
        call_gas_total: total_call_gas,
        counter_moves,
    })
}

/// Pre-order puts every parent before its children, so one forward pass reaches a whole
/// subtree.
fn drop_descendants_of_dropped(programs: &mut [ProgramState]) {
    for id in 1..programs.len() {
        if let Some(parent) = programs[id].parent
            && programs[parent].dropped
        {
            programs[id].dropped = true;
        }
    }
}

fn depth_of(programs: &[ProgramState], mut id: usize) -> u64 {
    let mut depth = 0;
    while let Some(parent) = programs[id].parent {
        depth += 1;
        id = parent;
    }
    depth
}

/// Turns back into a `CALL` every nested frame that does not pay for its nesting overhead,
/// children first so a parent's witness is sized with only the children that stay nested.
///
/// A frame's benefit is its own computation plus the net savings of the children it keeps
/// nested, because turning a frame back into a `CALL` runs its whole subtree natively. A cheap
/// frame in front of an expensive one therefore stays nested when the pair pays for itself.
/// Returns the native gas of the frames turned back into calls.
fn prune_unprofitable(programs: &mut [ProgramState], cost: &NestingCostModel) -> u64 {
    let mut witness_bytes = vec![0usize; programs.len()];
    let mut net_savings = vec![0u64; programs.len()];
    let mut restored_call_gas = 0u64;
    for id in (1..programs.len()).rev() {
        if programs[id].dropped {
            continue;
        }
        let program_bytes =
            encoded_program(programs[id].updates.as_deref().unwrap_or_default()).len();
        let children_bytes: usize = programs[id]
            .children
            .iter()
            .map(|c| witness_bytes[*c])
            .sum();
        witness_bytes[id] =
            WITNESS_HEAD_BYTES + program_bytes + ESTIMATED_PROOF_WORDS * 32 + children_bytes;

        let depth = depth_of(programs, id);
        let per_byte = cost.witness_byte_gas + depth * cost.witness_byte_gas_per_level;
        let overhead = cost
            .frame_overhead_gas
            .saturating_add((witness_bytes[id] as u64).saturating_mul(per_byte));
        let computation = programs[id]
            .native_gas
            .saturating_sub(programs[id].effect_gas)
            .saturating_sub(programs[id].child_call_gas);
        let children_savings: u64 = programs[id].children.iter().map(|c| net_savings[*c]).sum();
        let benefit = computation.saturating_add(children_savings);
        if benefit > overhead {
            net_savings[id] = benefit - overhead;
            continue;
        }

        let parent = programs[id].parent.expect("non-root frames have a parent");
        let position = programs[parent]
            .children
            .iter()
            .position(|c| *c == id)
            .expect("a kept frame is among its parent's children");
        let fallback = programs[id]
            .fallback
            .clone()
            .expect("nested frames keep their CALL");
        let updates = programs[parent]
            .updates
            .as_mut()
            .expect("a kept parent has a program");
        let op = updates
            .iter_mut()
            .filter(|u| matches!(u, StateUpdate::Nested(_)))
            .nth(position)
            .expect("one NESTED op per child");
        *op = StateUpdate::Call(fallback);
        programs[parent].children.remove(position);
        programs[id].dropped = true;
        restored_call_gas += programs[id].native_gas;
        for d in (id + 1)..programs.len() {
            if let Some(p) = programs[d].parent
                && programs[p].dropped
            {
                programs[d].dropped = true;
            }
        }
    }
    restored_call_gas
}

fn compact(programs: Vec<ProgramState>) -> Result<Vec<FrameProgram>> {
    let mut remap = vec![usize::MAX; programs.len()];
    let mut next = 0;
    for (id, p) in programs.iter().enumerate() {
        if !p.dropped {
            remap[id] = next;
            next += 1;
        }
    }
    programs
        .into_iter()
        .filter(|p| !p.dropped)
        .map(|p| {
            Ok(FrameProgram {
                target: p.target,
                caller: p.caller,
                value: p.value,
                calldata_hash: p.calldata_hash,
                transition_index: p.transition_index,
                updates: p
                    .updates
                    .ok_or_else(|| anyhow::anyhow!("a kept frame never returned"))?,
                children: p.children.into_iter().map(|c| remap[c]).collect(),
            })
        })
        .collect()
}

// ============================================================================
// Encoding
// ============================================================================

/// A frame tree hashed, built and packaged for `verifyAndUpdateTree`.
#[derive(Debug, Clone)]
pub struct EncodedTree {
    pub root: B256,
    /// The expiry leaf followed by one leaf per frame.
    pub leaves: Vec<B256>,
    pub frame_leaves: Vec<B256>,
    pub programs: Vec<Bytes>,
    /// `abi.encode(Witness)` per frame; empty for the root, which takes its fields directly.
    pub witnesses: Vec<Bytes>,
    pub root_children: Vec<Bytes>,
    pub root_proof: Vec<B256>,
    pub expiry_proof: Vec<B256>,
}

/// Hash every frame bottom-up (a `NESTED` op names its child's leaf, and a leaf hashes its
/// program), build the tree, then build witnesses bottom-up (a witness carries its children's).
pub fn encode_frame_tree(
    frames: &[FrameProgram],
    root_selector: FixedBytes<4>,
    chain_id: u64,
    expiry_block: u64,
) -> Result<EncodedTree> {
    let n = frames.len();
    if n == 0 {
        bail!("a frame tree needs a root frame");
    }
    let mut frame_leaves = vec![B256::ZERO; n];
    let mut programs = vec![Bytes::new(); n];

    for i in (0..n).rev() {
        let frame = &frames[i];
        let mut updates = frame.updates.clone();
        let mut children = frame.children.iter();
        for update in updates.iter_mut() {
            if let StateUpdate::Nested(nested) = update {
                let child = *children.next().ok_or_else(|| {
                    anyhow::anyhow!("frame {i} has more NESTED ops than children")
                })?;
                if child <= i || child >= n {
                    bail!("frame {i} names child {child} outside pre-order");
                }
                nested.childLeaf = frame_leaves[child];
            }
        }
        if children.next().is_some() {
            bail!("frame {i} has more children than NESTED ops");
        }
        programs[i] = encoded_program(&updates);
        let index = frame
            .transition_index
            .ok_or_else(|| anyhow::anyhow!("frame {i} has no transition index"))?;
        frame_leaves[i] = if i == 0 {
            root_leaf(index, frame.target, root_selector, &programs[0])
        } else {
            nested_leaf(
                chain_id,
                frame.target,
                index,
                frame.caller,
                frame.value,
                frame.calldata_hash,
                &programs[i],
            )
        };
    }

    let mut leaves = Vec::with_capacity(n + 1);
    leaves.push(expiry_leaf(chain_id, frames[0].target, expiry_block));
    leaves.extend_from_slice(&frame_leaves);
    let root = merkle_root(&leaves)?;

    let mut witnesses = vec![Bytes::new(); n];
    for i in (1..n).rev() {
        let witness = Witness {
            calldataHash: frames[i].calldata_hash,
            storageUpdates: programs[i].clone(),
            proof: merkle_proof(&leaves, i + 1)?,
            children: frames[i]
                .children
                .iter()
                .map(|c| witnesses[*c].clone())
                .collect(),
        };
        witnesses[i] = witness.abi_encode().into();
    }
    let root_children = frames[0]
        .children
        .iter()
        .map(|c| witnesses[*c].clone())
        .collect();

    Ok(EncodedTree {
        root,
        root_proof: merkle_proof(&leaves, 1)?,
        expiry_proof: merkle_proof(&leaves, 0)?,
        leaves,
        frame_leaves,
        programs,
        witnesses,
        root_children,
    })
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{address, b256, bytes};
    use alloy_sol_types::SolType;

    use super::*;
    use crate::trace::compute_state_updates_canonical;

    // ------------------------------------------------------------------------
    // Parity with solidity-sdk `test/NestedFramesParity.t.sol`
    // ------------------------------------------------------------------------

    const A11CE: Address = address!("0x00000000000000000000000000000000000a11ce");
    const B0B: Address = address!("0x0000000000000000000000000000000000000b0b");

    #[test]
    fn root_leaf_matches_solidity() {
        assert_eq!(
            root_leaf(
                U256::from(7),
                A11CE,
                FixedBytes::from([0x12, 0x34, 0x56, 0x78]),
                &bytes!("c0ffee")
            ),
            b256!("0x18fece75dabcbe169342c5e311d4af162b89a28a80cb659d125f7fe70e191b2b")
        );
    }

    #[test]
    fn nested_leaf_matches_solidity() {
        assert_eq!(
            nested_leaf(
                1,
                A11CE,
                U256::from(3),
                B0B,
                U256::from(5),
                keccak256("cd"),
                &bytes!("c0ffee")
            ),
            b256!("0x6178b9fced3f51c19e98e4b19474afc92e8e93f80bcccedbb8e4beffaf750c4a")
        );
    }

    #[test]
    fn expiry_leaf_matches_solidity() {
        assert_eq!(
            expiry_leaf(1, A11CE, 1060),
            b256!("0x1ca6f6e91b62fff1e8c6ef2374f2b9288cc36775340d330769a2acf58854ebb2")
        );
    }

    #[test]
    fn tree_matches_solidity() {
        let leaves: Vec<B256> = (0..5u64)
            .map(|i| keccak256(("tree".to_string(), U256::from(i)).abi_encode_params()))
            .collect();
        assert_eq!(
            merkle_root(&leaves).unwrap(),
            b256!("0x03f33a58ce39118744da6fa898233d2b505f3de76e7f62224053edafe87ab482")
        );
        let proof: Vec<u8> = merkle_proof(&leaves, 3)
            .unwrap()
            .iter()
            .flat_map(|p| p.0)
            .collect();
        assert_eq!(
            keccak256(proof),
            b256!("0x569c3f0a5fe11b3463b7d547bd91b8d31c5dee3f1af947c028439fe893ba0dac")
        );
    }

    #[test]
    fn witness_encoding_matches_solidity() {
        let witness = Witness {
            calldataHash: keccak256("cd"),
            storageUpdates: bytes!("c0ffee"),
            proof: vec![keccak256("p0"), keccak256("p1")],
            children: vec![bytes!("beef")],
        };
        assert_eq!(
            keccak256(witness.abi_encode()),
            b256!("0x40ce15e5d0c20c9213984d208f78f17446a30c62c1184ac83a678320116d7f6e")
        );
    }

    #[test]
    fn every_leaf_verifies_and_odd_nodes_move_up() {
        for n in 1..=9usize {
            let leaves: Vec<B256> = (0..n as u64).map(|i| keccak256(i.to_be_bytes())).collect();
            let root = merkle_root(&leaves).unwrap();
            for (i, leaf) in leaves.iter().enumerate() {
                assert!(verify(&merkle_proof(&leaves, i).unwrap(), root, *leaf));
            }
        }
    }

    fn verify(proof: &[B256], root: B256, leaf: B256) -> bool {
        proof.iter().fold(leaf, |acc, p| hash_pair(acc, *p)) == root
    }

    // ------------------------------------------------------------------------
    // Frame tree extraction
    // ------------------------------------------------------------------------

    const ROOT: Address = Address::new([0x77; 20]);
    const B: Address = Address::new([0xbb; 20]);
    const C: Address = Address::new([0xcc; 20]);
    const LIB: Address = Address::new([0x11; 20]);

    fn word(a: Address) -> U256 {
        U256::from_be_slice(a.as_slice())
    }

    fn slot(n: u64) -> B256 {
        B256::from(U256::from(n))
    }

    fn log(op: &str, depth: u64, gas: u64, top_first: &[U256], gas_cost: u64) -> StructLog {
        let mut stack = top_first.to_vec();
        stack.reverse();
        StructLog {
            pc: 0,
            op: op.to_string().into(),
            gas,
            gas_cost,
            depth,
            error: None,
            stack: Some(stack),
            return_data: None,
            memory: Some(vec![]),
            memory_size: None,
            storage: None,
            refund_counter: None,
        }
    }

    fn sstore(depth: u64, s: B256, v: u64) -> StructLog {
        log("SSTORE", depth, 5_000_000, &[s.into(), U256::from(v)], 0)
    }

    fn bump(depth: u64, to: u64) -> StructLog {
        sstore(depth, STATE_TRACKER_SLOT, to)
    }

    fn call(depth: u64, gas: u64, callee: Address) -> StructLog {
        let args = [
            U256::from(gas),
            word(callee),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
        ];
        log("CALL", depth, gas, &args, 0)
    }

    fn delegatecall(depth: u64, gas: u64, callee: Address) -> StructLog {
        let args = [
            U256::from(gas),
            word(callee),
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
            U256::ZERO,
        ];
        log("DELEGATECALL", depth, gas, &args, 0)
    }

    fn resume(depth: u64, gas: u64, success: bool) -> StructLog {
        log("JUMPDEST", depth, gas, &[U256::from(success as u64)], 0)
    }

    fn trace(logs: Vec<StructLog>) -> DefaultFrame {
        DefaultFrame {
            failed: false,
            gas: 0,
            return_value: Default::default(),
            struct_logs: logs,
        }
    }

    fn split(logs: Vec<StructLog>, nestable: &[Address]) -> FrameTreeExtract {
        let nestable = nestable.to_vec();
        compute_frame_tree_canonical(
            trace(logs),
            ROOT,
            &move |a| nestable.contains(&a),
            &NESTING_COST_MODEL_V1,
        )
        .expect("frame tree")
    }

    fn kinds(updates: &[StateUpdate]) -> Vec<&'static str> {
        updates
            .iter()
            .map(|u| match u {
                StateUpdate::Store(_) => "STORE",
                StateUpdate::Call(_) => "CALL",
                StateUpdate::Nested(_) => "NESTED",
                StateUpdate::Create(_) | StateUpdate::Create2(_) => "CREATE",
                _ => "LOG",
            })
            .collect()
    }

    fn stores(updates: &[StateUpdate]) -> Vec<(B256, B256)> {
        updates
            .iter()
            .filter_map(|u| match u {
                StateUpdate::Store(s) => Some((s.slot, s.value)),
                _ => None,
            })
            .collect()
    }

    /// The root bumps, writes slot 1, then calls `callee`, which bumps to `callee_index + 1`
    /// and writes slot 5. Native gas of the call is `call_gas - resume_gas`.
    fn root_calls(
        callee: Address,
        callee_index: u64,
        call_gas: u64,
        resume_gas: u64,
    ) -> Vec<StructLog> {
        vec![
            bump(1, 1),
            sstore(1, slot(1), 10),
            call(1, call_gas, callee),
            bump(2, callee_index + 1),
            sstore(2, slot(5), 50),
            resume(1, resume_gas, true),
        ]
    }

    #[test]
    fn eligible_callee_becomes_its_own_frame() {
        let tree = split(root_calls(B, 0, 1_000_000, 100_000), &[B]);
        assert_eq!(tree.frames.len(), 2);

        let root = &tree.frames[0];
        assert_eq!(kinds(&root.updates), ["STORE", "STORE", "NESTED"]);
        assert_eq!(root.children, vec![1]);
        assert_eq!(root.transition_index, Some(U256::ZERO));

        let b = &tree.frames[1];
        assert_eq!(b.target, B);
        assert_eq!(b.caller, ROOT);
        assert_eq!(b.transition_index, Some(U256::ZERO));
        assert_eq!(b.calldata_hash, keccak256([]));
        assert_eq!(
            stores(&b.updates),
            vec![(slot(5), slot(50)), (STATE_TRACKER_SLOT, slot(1))]
        );
        assert_eq!(tree.call_gas_total, 0, "nothing replays natively");
        assert_eq!(tree.counter_moves, BTreeSet::from([ROOT, B]));
    }

    #[test]
    fn without_a_nestable_callee_the_root_program_is_the_canonical_program() {
        let logs = root_calls(B, 0, 1_000_000, 100_000);
        let canonical = compute_state_updates_canonical(trace(logs.clone()), ROOT)
            .unwrap()
            .0;
        let tree = split(logs, &[]);
        assert_eq!(tree.frames.len(), 1);
        assert_eq!(
            encoded_program(&tree.frames[0].updates),
            encoded_program(&canonical)
        );
        assert_eq!(kinds(&canonical), ["STORE", "STORE", "CALL"]);
        assert_eq!(tree.call_gas_total, 900_000);
    }

    #[test]
    fn callee_that_writes_before_bumping_stays_a_call() {
        let logs = vec![
            bump(1, 1),
            call(1, 1_000_000, B),
            sstore(2, slot(5), 50),
            bump(2, 1),
            resume(1, 100_000, true),
        ];
        let tree = split(logs, &[B]);
        assert_eq!(tree.frames.len(), 1);
        assert_eq!(kinds(&tree.frames[0].updates), ["STORE", "CALL"]);
        assert!(
            tree.counter_moves.contains(&B),
            "a native call still pins B"
        );
    }

    #[test]
    fn callee_without_the_interface_stays_a_call() {
        let tree = split(root_calls(B, 0, 1_000_000, 100_000), &[C]);
        assert_eq!(tree.frames.len(), 1);
        assert_eq!(kinds(&tree.frames[0].updates), ["STORE", "STORE", "CALL"]);
    }

    #[test]
    fn cheap_callee_is_cheaper_as_a_call() {
        let tree = split(root_calls(B, 0, 1_000_000, 995_000), &[B]);
        assert_eq!(tree.frames.len(), 1);
        assert_eq!(kinds(&tree.frames[0].updates), ["STORE", "STORE", "CALL"]);
        assert_eq!(
            tree.call_gas_total, 5_000,
            "the restored call replays natively"
        );
    }

    #[test]
    fn cheap_frame_in_front_of_an_expensive_one_stays_nested() {
        let logs = vec![
            bump(1, 1),
            call(1, 3_000_000, B),
            bump(2, 1),
            call(2, 2_990_000, C),
            bump(3, 1),
            resume(2, 100_000, true),
            resume(1, 95_000, true),
        ];
        let tree = split(logs, &[B, C]);
        let targets: Vec<Address> = tree.frames.iter().map(|f| f.target).collect();
        assert_eq!(
            targets,
            vec![ROOT, B, C],
            "B alone computes ~5k, C carries it"
        );
    }

    #[test]
    fn cheap_frames_all_the_way_down_are_calls() {
        let logs = vec![
            bump(1, 1),
            call(1, 3_000_000, B),
            bump(2, 1),
            call(2, 2_995_000, C),
            bump(3, 1),
            resume(2, 2_990_000, true),
            resume(1, 2_985_000, true),
        ];
        let tree = split(logs, &[B, C]);
        assert_eq!(tree.frames.len(), 1);
    }

    #[test]
    fn storage_and_log_gas_do_not_count_as_savings() {
        let mut logs = root_calls(B, 0, 1_000_000, 100_000);
        logs[4].gas_cost = 880_000;
        let tree = split(logs, &[B]);
        assert_eq!(
            tree.frames.len(),
            1,
            "replay pays the same writes, so nothing is saved"
        );
    }

    #[test]
    fn reverted_callee_leaves_no_frame_and_no_op() {
        let mut logs = root_calls(B, 0, 1_000_000, 100_000);
        logs[5] = resume(1, 100_000, false);
        let tree = split(logs, &[B]);
        assert_eq!(tree.frames.len(), 1);
        assert_eq!(kinds(&tree.frames[0].updates), ["STORE", "STORE"]);
        assert!(tree.root_children_free());
    }

    impl FrameTreeExtract {
        fn root_children_free(&self) -> bool {
            self.frames[0].children.is_empty()
        }
    }

    #[test]
    fn chain_is_split_in_pre_order() {
        let logs = vec![
            bump(1, 1),
            call(1, 3_000_000, B),
            bump(2, 1),
            call(2, 2_000_000, C),
            bump(3, 1),
            sstore(3, slot(9), 90),
            resume(2, 1_000_000, true),
            sstore(2, slot(5), 50),
            resume(1, 100_000, true),
        ];
        let tree = split(logs, &[B, C]);
        let targets: Vec<Address> = tree.frames.iter().map(|f| f.target).collect();
        assert_eq!(targets, vec![ROOT, B, C]);
        assert_eq!(tree.frames[0].children, vec![1]);
        assert_eq!(tree.frames[1].children, vec![2]);
        assert_eq!(tree.frames[2].caller, B);
        // Slice before the boundary, the NESTED op, then a final slice holding only what
        // changed since: the tracker was already emitted.
        assert_eq!(kinds(&tree.frames[1].updates), ["STORE", "NESTED", "STORE"]);
        assert_eq!(stores(&tree.frames[1].updates)[1], (slot(5), slot(50)));
    }

    #[test]
    fn cycle_gives_the_root_contract_two_frames() {
        let logs = vec![
            bump(1, 1),
            sstore(1, slot(1), 10),
            call(1, 3_000_000, B),
            bump(2, 1),
            call(2, 2_000_000, ROOT),
            bump(3, 2),
            sstore(3, slot(2), 20),
            resume(2, 1_000_000, true),
            resume(1, 100_000, true),
        ];
        let tree = split(logs, &[B, ROOT]);
        assert_eq!(tree.frames.len(), 3);
        let inner = &tree.frames[2];
        assert_eq!(inner.target, ROOT);
        assert_eq!(inner.caller, B);
        assert_eq!(inner.transition_index, Some(U256::from(1)));
        // The outer root frame's final slice re-asserts what the inner frame wrote.
        assert_eq!(
            stores(&tree.frames[0].updates).last(),
            Some(&(STATE_TRACKER_SLOT, slot(2)))
        );
        assert!(stores(&tree.frames[0].updates).contains(&(slot(2), slot(20))));
        // ...but never before the NESTED op that applies it.
        let before_nested: Vec<_> = tree.frames[0]
            .updates
            .iter()
            .take_while(|u| !matches!(u, StateUpdate::Nested(_)))
            .cloned()
            .collect();
        assert!(!stores(&before_nested).contains(&(slot(2), slot(20))));
    }

    #[test]
    fn callee_called_twice_gets_consecutive_indices() {
        let logs = vec![
            bump(1, 1),
            call(1, 3_000_000, B),
            bump(2, 1),
            resume(1, 2_000_000, true),
            call(1, 2_000_000, B),
            bump(2, 2),
            resume(1, 100_000, true),
        ];
        let tree = split(logs, &[B]);
        let indices: Vec<_> = tree.frames.iter().map(|f| f.transition_index).collect();
        assert_eq!(
            indices,
            vec![Some(U256::ZERO), Some(U256::ZERO), Some(U256::from(1))]
        );
        assert_eq!(tree.frames[0].children, vec![1, 2]);
    }

    #[test]
    fn proxy_callee_bumping_through_delegatecall_is_eligible() {
        let logs = vec![
            bump(1, 1),
            call(1, 3_000_000, B),
            delegatecall(2, 2_900_000, LIB),
            bump(3, 1),
            sstore(3, slot(5), 50),
            resume(2, 200_000, true),
            resume(1, 100_000, true),
        ];
        let tree = split(logs, &[B]);
        assert_eq!(tree.frames.len(), 2);
        assert_eq!(
            stores(&tree.frames[1].updates),
            vec![(slot(5), slot(50)), (STATE_TRACKER_SLOT, slot(1))]
        );
    }

    #[test]
    fn self_call_before_the_bump_keeps_the_outer_frame_a_call() {
        let logs = vec![
            bump(1, 1),
            call(1, 3_000_000, B),
            call(2, 2_900_000, B),
            bump(3, 1),
            resume(2, 200_000, true),
            resume(1, 100_000, true),
        ];
        let tree = split(logs, &[B]);
        assert_eq!(tree.frames.len(), 1);
        assert_eq!(kinds(&tree.frames[0].updates), ["STORE", "CALL"]);
    }

    #[test]
    fn transient_storage_in_the_callee_keeps_it_a_call() {
        let mut logs = root_calls(B, 0, 1_000_000, 100_000);
        logs.insert(
            4,
            log("TSTORE", 2, 600_000, &[U256::from(1), U256::from(1)], 0),
        );
        let tree = split(logs, &[B]);
        assert_eq!(tree.frames.len(), 1);
        assert!(
            tree.skipped_opcodes.is_empty(),
            "the native call carries it, not a program"
        );
    }

    fn what_if(logs: Vec<StructLog>, owned: &[Address]) -> FrameTreeExtract {
        compute_frame_tree_hypothetical(
            trace(logs),
            ROOT,
            &owned.iter().copied().collect(),
            &NESTING_COST_MODEL_V1,
        )
        .expect("frame tree")
    }

    /// The root writes slot 1, then calls B and C in turn; each writes slot 5. Nothing touches
    /// a transition counter, as in code that never integrated the SDK.
    fn historical_root_calls_b_then_c() -> Vec<StructLog> {
        vec![
            sstore(1, slot(1), 10),
            call(1, 1_000_000, B),
            sstore(2, slot(5), 50),
            resume(1, 100_000, true),
            call(1, 1_000_000, C),
            sstore(2, slot(5), 60),
            resume(1, 100_000, true),
        ]
    }

    #[test]
    fn only_owned_callees_become_frames_in_a_what_if() {
        let tree = what_if(historical_root_calls_b_then_c(), &[C]);
        assert_eq!(tree.frames.len(), 2);
        assert_eq!(kinds(&tree.frames[0].updates), ["STORE", "CALL", "NESTED"]);
        let c = &tree.frames[1];
        assert_eq!(c.target, C);
        assert_eq!(c.caller, ROOT);
        assert_eq!(c.transition_index, Some(U256::ZERO));
        assert_eq!(stores(&c.updates), vec![(slot(5), slot(60))]);
    }

    #[test]
    fn a_what_if_owning_nothing_but_the_root_is_the_canonical_program() {
        let logs = historical_root_calls_b_then_c();
        let canonical = compute_state_updates_canonical(trace(logs.clone()), ROOT)
            .unwrap()
            .0;
        let tree = what_if(logs, &[]);
        assert_eq!(tree.frames.len(), 1);
        assert_eq!(
            encoded_program(&tree.frames[0].updates),
            encoded_program(&canonical)
        );
    }

    /// C is owned but only reached through B, which is not: it runs inside B's native call.
    #[test]
    fn an_owned_contract_behind_a_foreign_one_stays_inside_its_call() {
        let logs = vec![
            sstore(1, slot(1), 10),
            call(1, 1_000_000, B),
            call(2, 900_000, C),
            sstore(3, slot(5), 50),
            resume(2, 100_000, true),
            resume(1, 50_000, true),
        ];
        let tree = what_if(logs, &[C]);
        assert_eq!(tree.frames.len(), 1);
        assert_eq!(kinds(&tree.frames[0].updates), ["STORE", "CALL"]);
    }

    #[test]
    fn a_cheap_owned_callee_stays_a_call_in_a_what_if() {
        let logs = vec![
            sstore(1, slot(1), 10),
            call(1, 1_000_000, C),
            sstore(2, slot(5), 60),
            resume(1, 995_000, true),
        ];
        assert_eq!(what_if(logs, &[C]).frames.len(), 1);
    }

    #[test]
    fn an_owned_callee_using_transient_storage_stays_a_call_in_a_what_if() {
        let mut logs = historical_root_calls_b_then_c();
        logs.insert(
            5,
            log("TSTORE", 2, 600_000, &[U256::from(1), U256::from(1)], 0),
        );
        assert_eq!(what_if(logs, &[C]).frames.len(), 1);
    }

    #[test]
    fn reverted_delegatecall_drops_the_frames_opened_inside_it() {
        let logs = vec![
            bump(1, 1),
            delegatecall(1, 3_000_000, LIB),
            call(2, 2_000_000, B),
            bump(3, 1),
            resume(2, 1_000_000, true),
            resume(1, 100_000, false),
        ];
        let tree = split(logs, &[B]);
        assert_eq!(tree.frames.len(), 1);
        assert!(tree.root_children_free());
        assert!(
            !tree.counter_moves.contains(&B),
            "the revert undid B's bump"
        );
    }

    #[test]
    fn same_trace_splits_the_same_way_every_time() {
        let a = split(root_calls(B, 0, 1_000_000, 100_000), &[B]);
        let b = split(root_calls(B, 0, 1_000_000, 100_000), &[B]);
        let enc = |t: &FrameTreeExtract| -> Vec<Bytes> {
            t.frames
                .iter()
                .map(|f| encoded_program(&f.updates))
                .collect()
        };
        assert_eq!(enc(&a), enc(&b));
    }

    // ------------------------------------------------------------------------
    // Tree encoding
    // ------------------------------------------------------------------------

    #[test]
    fn encoded_tree_links_parents_to_children() {
        let tree = split(
            vec![
                bump(1, 1),
                call(1, 3_000_000, B),
                bump(2, 1),
                call(2, 2_000_000, C),
                bump(3, 1),
                resume(2, 1_000_000, true),
                resume(1, 100_000, true),
            ],
            &[B, C],
        );
        let selector = FixedBytes::from([0xde, 0xad, 0xbe, 0xef]);
        let encoded = encode_frame_tree(&tree.frames, selector, 1, 1060).unwrap();

        assert_eq!(encoded.leaves.len(), 4);
        assert_eq!(encoded.leaves[0], expiry_leaf(1, ROOT, 1060));
        assert!(verify(
            &encoded.expiry_proof,
            encoded.root,
            encoded.leaves[0]
        ));
        assert!(verify(
            &encoded.root_proof,
            encoded.root,
            encoded.frame_leaves[0]
        ));
        assert_eq!(
            encoded.frame_leaves[0],
            root_leaf(U256::ZERO, ROOT, selector, &encoded.programs[0])
        );

        // The root's NESTED op names B's leaf; B's witness carries C's.
        let (_, args) = <(
            alloy_sol_types::sol_data::Array<crate::types::StateUpdateType>,
            alloy_sol_types::sol_data::Array<alloy_sol_types::sol_data::Bytes>,
        )>::abi_decode_params(&encoded.programs[0])
        .unwrap();
        let nested =
            <IStateUpdateTypes::Nested as SolType>::abi_decode_sequence(args.last().unwrap())
                .unwrap();
        assert_eq!(nested.childLeaf, encoded.frame_leaves[1]);

        let b = <Witness as SolType>::abi_decode(&encoded.root_children[0]).unwrap();
        assert_eq!(b.storageUpdates, encoded.programs[1]);
        assert!(verify(&b.proof, encoded.root, encoded.frame_leaves[1]));
        assert_eq!(b.children, vec![encoded.witnesses[2].clone()]);
        let c = <Witness as SolType>::abi_decode(&b.children[0]).unwrap();
        assert!(verify(&c.proof, encoded.root, encoded.frame_leaves[2]));
        assert_eq!(
            encoded.frame_leaves[2],
            nested_leaf(
                1,
                C,
                U256::ZERO,
                B,
                U256::ZERO,
                keccak256([]),
                &encoded.programs[2]
            )
        );
    }

    #[test]
    fn encoding_rejects_a_frame_without_an_index() {
        let mut tree = split(root_calls(B, 0, 1_000_000, 100_000), &[B]);
        tree.frames[1].transition_index = None;
        assert!(encode_frame_tree(&tree.frames, FixedBytes::ZERO, 1, 1060).is_err());
    }
}

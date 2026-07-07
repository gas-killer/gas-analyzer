//! Simulation environment profiles for Gas Killer execution.
//!
//! A [`SimProfile`] pins the EVM environment a tracked function is simulated
//! under. `Chain` mirrors the real chain (today's behaviour everywhere).
//! `UnboundedV1` is the Gas Killer *unbounded execution* profile: the tracked
//! function is simulated with gas limits far above any real block, so
//! arbitrarily heavy Solidity can be executed off-chain — provided its **net
//! effect** still fits the Gas Killer on-chain payload shape (at most one
//! storage write per consumer contract, plus external calls and logs; see
//! [`validate_unbounded_shape`]). The intended consumer shape is the
//! single-slot commitment pattern (solidity-sdk PR #51), fanned out across
//! contracts via multi-call forwarding (solidity-sdk PRs #47/#48).
//!
//! # Determinism is protocol-critical
//!
//! The profile's limits are **pinned protocol constants**, not tunables.
//! Every party that re-executes a tracked function must use bit-identical
//! environment overrides:
//!
//! 1. operators, when extracting the state-update payload they sign;
//! 2. this analyzer, when simulating on a user's behalf;
//! 3. the SP1 slashing guest (`docs/SP1_REVM_IMPLEMENTATION_SPEC.md`), when a
//!    slasher re-executes the original function inside the zkVM to prove the
//!    signed updates wrong.
//!
//! If the guest ran with the real header's `gas_limit` while operators
//! simulated under lifted limits, a heavy-but-honest execution would OOG in
//! the guest and produce a *different* update set — falsely slashing honest
//! operators. Conversely, per-operator ad-hoc limits would make quorum
//! signatures diverge on the boundary. Hence the versioned `V1` constants:
//! any future change to these values is a new profile version, signalled
//! out-of-band, never a silent edit.
//!
//! Calldata note: neither revm nor the EVM protocol caps calldata *size*;
//! calldata is bounded only through intrinsic gas (EIP-7623 floor pricing).
//! Lifting the gas limits therefore lifts the effective calldata bound too —
//! multi-megabyte witnesses (e.g. the expanded state behind a single-slot
//! commitment) simulate fine under `UnboundedV1` even though they could never
//! land in a real transaction. Only the *signed payload* must fit on-chain.

use crate::types::StateUpdate;
use alloy_primitives::{B256, b256};

/// The Gas Killer SDK's `StateTracker` transition-counter slot
/// (`keccak256("gasKiller.stateTracker") - 1`, see solidity-sdk
/// `StateTracker.sol`). Every `trackState` function bumps it, so every
/// extracted diff carries a Store to this slot; `verifyAndUpdate`'s own
/// modifier writes the same value on-chain, making the payload copy
/// idempotent. It is a fixed protocol slot — one per consumer regardless of
/// state size — and is therefore exempt from the single-slot shape count.
pub const STATE_TRACKER_SLOT: B256 =
    b256!("0xdebfdfd5a50ad117c10898d68b5ccf0893c6b40d4f443f902e2e7646601bdeaf");

/// Block gas limit for the `UnboundedV1` profile: 2^40 ≈ 1.1 Tgas,
/// ~24,000× a 45M-gas mainnet block.
///
/// Chosen instead of `u64::MAX` so intrinsic-gas / refund / floor-cost
/// arithmetic (all `u64` in revm and in geth's tracer path) cannot overflow,
/// while still admitting ~27 GB of EIP-7623-priced calldata — far beyond any
/// realistic witness.
pub const UNBOUNDED_V1_BLOCK_GAS_LIMIT: u64 = 1 << 40;

/// Transaction gas limit for the `UnboundedV1` profile.
///
/// Equal to the block limit: the profile deliberately ignores the EIP-7825
/// per-transaction cap (2^24) — that cap protects real block building and has
/// no purpose in an off-chain simulation whose result is applied on-chain as
/// a Gas Killer payload.
pub const UNBOUNDED_V1_TX_GAS_LIMIT: u64 = UNBOUNDED_V1_BLOCK_GAS_LIMIT;

/// The EVM environment profile a tracked function is simulated under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SimProfile {
    /// Mirror the anchored chain's real environment (header gas limit,
    /// EIP-7825 tx cap where active). This is the historical behaviour and
    /// the right profile whenever the analyzed call must also be *executable*
    /// on-chain as-is.
    #[default]
    Chain,
    /// Gas Killer unbounded execution (version 1): simulate with
    /// [`UNBOUNDED_V1_BLOCK_GAS_LIMIT`] / [`UNBOUNDED_V1_TX_GAS_LIMIT`] and
    /// enforce the single-slot payload shape on the extracted updates.
    UnboundedV1,
}

impl SimProfile {
    /// Transaction-level gas limit override, if this profile lifts it.
    pub fn tx_gas_limit_override(&self) -> Option<u64> {
        match self {
            SimProfile::Chain => None,
            SimProfile::UnboundedV1 => Some(UNBOUNDED_V1_TX_GAS_LIMIT),
        }
    }

    /// Block-level gas limit override, if this profile lifts it.
    pub fn block_gas_limit_override(&self) -> Option<u64> {
        match self {
            SimProfile::Chain => None,
            SimProfile::UnboundedV1 => Some(UNBOUNDED_V1_BLOCK_GAS_LIMIT),
        }
    }

    /// Whether extracted updates must satisfy [`validate_unbounded_shape`].
    pub fn requires_unbounded_shape(&self) -> bool {
        matches!(self, SimProfile::UnboundedV1)
    }
}

/// Summary of a payload that passed [`validate_unbounded_shape`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnboundedShape {
    /// Number of `Store` ops other than the [`STATE_TRACKER_SLOT`] (0 or 1).
    pub stores: usize,
    /// Number of `Store` ops to the fixed [`STATE_TRACKER_SLOT`] (0 or 1).
    pub tracker_stores: usize,
    /// Number of `Call` ops. These re-execute **on-chain at real gas prices**
    /// when the payload is applied — unbounded compute inside a `Call` is NOT
    /// killed. Forwarded multi-call sub-payloads (`applyForwardedUpdates`)
    /// ride in this category by design.
    pub calls: usize,
    /// Number of `Log0`–`Log4` ops.
    pub logs: usize,
}

/// Why a payload is not valid under the unbounded profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnboundedShapeViolation {
    /// More than one storage write. The unbounded profile exists precisely
    /// because compute is unbounded while *state* stays O(1): a consumer that
    /// writes N slots per transition pays N cold SSTOREs on-chain and scales
    /// like a plain contract. Restructure the consumer around a single-slot
    /// commitment (solidity-sdk PR #51) instead.
    TooManyStores {
        /// Total `Store` ops found.
        count: usize,
    },
    /// `CREATE`/`CREATE2` found. Initcode replays on-chain inside
    /// `verifyAndUpdate` at real gas prices, so a deployment produced by an
    /// unbounded simulation may simply not fit in a real block; the struct-log
    /// path also cannot reconstruct replayable initcode from a net diff.
    CreateNotAllowed {
        /// Index of the offending op in the update list.
        index: usize,
    },
}

impl core::fmt::Display for UnboundedShapeViolation {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            UnboundedShapeViolation::TooManyStores { count } => write!(
                f,
                "unbounded profile requires at most one storage write per consumer \
                 (single-slot commitment pattern), found {count} Store ops; \
                 restructure the consumer to commit its state into one slot"
            ),
            UnboundedShapeViolation::CreateNotAllowed { index } => write!(
                f,
                "unbounded profile does not allow CREATE/CREATE2 (update #{index}): \
                 initcode replays on-chain at real gas prices and may not fit a real block"
            ),
        }
    }
}

impl std::error::Error for UnboundedShapeViolation {}

/// Enforce the `UnboundedV1` payload-shape invariant on extracted updates:
/// at most one `Store` beyond the fixed [`STATE_TRACKER_SLOT`] (which every
/// `trackState` diff carries), no `CREATE`/`CREATE2`; `Call`/`Log*` ops pass.
///
/// This is what makes "play fast and loose with the gas limit" sound: the
/// simulation may burn a terabyte of gas, but the thing that lands on-chain
/// is provably one SSTORE plus bounded calls/logs. A violation means the
/// consumer contract is not commitment-shaped — failing loudly here beats
/// signing a payload whose on-chain application costs O(slots touched).
pub fn validate_unbounded_shape(
    updates: &[StateUpdate],
) -> Result<UnboundedShape, UnboundedShapeViolation> {
    let mut shape = UnboundedShape {
        stores: 0,
        tracker_stores: 0,
        calls: 0,
        logs: 0,
    };
    for (index, update) in updates.iter().enumerate() {
        match update {
            StateUpdate::Store(store) if store.slot == STATE_TRACKER_SLOT => {
                shape.tracker_stores += 1
            }
            StateUpdate::Store(_) => shape.stores += 1,
            StateUpdate::Call(_) => shape.calls += 1,
            StateUpdate::Log0(_)
            | StateUpdate::Log1(_)
            | StateUpdate::Log2(_)
            | StateUpdate::Log3(_)
            | StateUpdate::Log4(_) => shape.logs += 1,
            StateUpdate::Create(_) | StateUpdate::Create2(_) => {
                return Err(UnboundedShapeViolation::CreateNotAllowed { index });
            }
        }
    }
    if shape.stores > 1 {
        return Err(UnboundedShapeViolation::TooManyStores {
            count: shape.stores,
        });
    }
    Ok(shape)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::IStateUpdateTypes;
    use alloy_primitives::{Address, Bytes, U256};

    fn store() -> StateUpdate {
        StateUpdate::Store(IStateUpdateTypes::Store {
            slot: B256::ZERO,
            value: B256::ZERO,
        })
    }

    fn call() -> StateUpdate {
        StateUpdate::Call(IStateUpdateTypes::Call {
            target: Address::ZERO,
            value: U256::ZERO,
            callargs: Bytes::new(),
        })
    }

    fn log1() -> StateUpdate {
        StateUpdate::Log1(IStateUpdateTypes::Log1 {
            data: Bytes::new(),
            topic1: B256::ZERO,
        })
    }

    fn create() -> StateUpdate {
        StateUpdate::Create(IStateUpdateTypes::Create {
            value: U256::ZERO,
            initcode: Bytes::new(),
        })
    }

    #[test]
    fn chain_profile_overrides_nothing() {
        assert_eq!(SimProfile::Chain.tx_gas_limit_override(), None);
        assert_eq!(SimProfile::Chain.block_gas_limit_override(), None);
        assert!(!SimProfile::Chain.requires_unbounded_shape());
        assert_eq!(SimProfile::default(), SimProfile::Chain);
    }

    #[test]
    fn unbounded_profile_pins_v1_constants() {
        assert_eq!(
            SimProfile::UnboundedV1.tx_gas_limit_override(),
            Some(UNBOUNDED_V1_TX_GAS_LIMIT)
        );
        assert_eq!(
            SimProfile::UnboundedV1.block_gas_limit_override(),
            Some(UNBOUNDED_V1_BLOCK_GAS_LIMIT)
        );
        assert!(SimProfile::UnboundedV1.requires_unbounded_shape());
        // The constants are protocol-pinned: a change here is a consensus
        // break with the SP1 slashing guest and must ship as a new version.
        assert_eq!(UNBOUNDED_V1_BLOCK_GAS_LIMIT, 1 << 40);
        assert_eq!(UNBOUNDED_V1_TX_GAS_LIMIT, 1 << 40);
    }

    #[test]
    fn single_store_with_calls_and_logs_is_valid() {
        let updates = vec![call(), store(), log1(), call(), log1()];
        let shape = validate_unbounded_shape(&updates).unwrap();
        assert_eq!(
            shape,
            UnboundedShape {
                stores: 1,
                tracker_stores: 0,
                calls: 2,
                logs: 2
            }
        );
    }

    #[test]
    fn zero_stores_is_valid() {
        let shape = validate_unbounded_shape(&[call(), log1()]).unwrap();
        assert_eq!(shape.stores, 0);
    }

    #[test]
    fn empty_payload_is_valid() {
        let shape = validate_unbounded_shape(&[]).unwrap();
        assert_eq!(
            shape,
            UnboundedShape {
                stores: 0,
                tracker_stores: 0,
                calls: 0,
                logs: 0
            }
        );
    }

    #[test]
    fn tracker_slot_store_is_exempt() {
        // The realistic trackState diff: counter bump + one commitment write + log.
        let tracker = StateUpdate::Store(IStateUpdateTypes::Store {
            slot: STATE_TRACKER_SLOT,
            value: B256::with_last_byte(1),
        });
        let shape = validate_unbounded_shape(&[tracker, store(), log1()]).unwrap();
        assert_eq!(shape.stores, 1);
        assert_eq!(shape.tracker_stores, 1);
    }

    #[test]
    fn two_stores_is_rejected() {
        let err = validate_unbounded_shape(&[store(), log1(), store()]).unwrap_err();
        assert_eq!(err, UnboundedShapeViolation::TooManyStores { count: 2 });
        assert!(err.to_string().contains("single-slot commitment"));
    }

    #[test]
    fn create_is_rejected_at_index() {
        let err = validate_unbounded_shape(&[store(), create()]).unwrap_err();
        assert_eq!(err, UnboundedShapeViolation::CreateNotAllowed { index: 1 });
    }

    #[test]
    fn create2_is_rejected() {
        let update = StateUpdate::Create2(IStateUpdateTypes::Create2 {
            salt: B256::ZERO,
            value: U256::ZERO,
            initcode: Bytes::new(),
        });
        let err = validate_unbounded_shape(&[update]).unwrap_err();
        assert!(matches!(
            err,
            UnboundedShapeViolation::CreateNotAllowed { index: 0 }
        ));
    }
}

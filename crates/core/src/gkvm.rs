//! Native guest execution for unbounded simulation (`UNBOUNDED_V3`, "gkvm").
//!
//! V3 adds a third axis to the pinned simulation env: next to `{gas limits}`
//! (`sim_profile`) and `{address → code}` (`overlay`), the env now contains a
//! precompile at [`GKVM_ADDRESS`] that runs riscv64im guest ELFs under SP1's
//! executor lineage and meters them in guest cycles. Spec:
//! `src/examples/onchain-llm/UNBOUNDED_V3_NATIVE.md` in gas-killer/solidity-sdk.
//!
//! # Determinism is protocol-critical (same bargain as `sim_profile`)
//!
//! Every constant here is a **versioned protocol value**, not a tunable: the
//! bundle {guest ISA, cycle model, hostcall surface, caps, metering rates} is
//! what [`UNBOUNDED_V3_GKVM_SPEC_VERSION`] names, and operators, this analyzer
//! and the SP1 dispute guest must agree on all of it bit-for-bit. Changing any
//! value changes what honest operators produce and must ship as a coordinated
//! spec-version bump — never as a silent edit. This module is the single home
//! of the set (the #166 doctrine): the executor crate (`gas-analyzer-gkvm`)
//! and the SP1 guest import it and never restate it.
//!
//! # What the env commitment binds — and what it does not
//!
//! [`env_commitment_v3`] binds the VM spec version and the cycle↔gas rate, so
//! a fraud proof simulated under a different cycle model cannot verify
//! (mirroring how V2 makes a proof under different weights unverifiable). The
//! **installed-program list is deliberately not bound**: programs are
//! self-authenticating per call (`programHash` travels in the wire format),
//! and a missing program is a liveness event, not an env difference.

use alloy_primitives::{Address, B256, address, keccak256};

use crate::overlay::{OverlayEnv, extend_overlay_segment};

/// The simulation-environment address of the gkvm precompile:
/// `address(uint160(uint256(keccak256("gaskiller.gkvm.addr.v1"))))`.
///
/// One fixed address for all guest programs — program identity travels as a
/// hash argument in the wire format, so there is no per-program address, no
/// registry, and nothing new to enumerate in the env commitment. On the real
/// chain this address is empty; a STATICCALL to it succeeds with empty
/// returndata, which `GkVm.sol` maps to a deterministic `GkVmUnavailable`
/// revert. Pinned against [`gkvm_address`] by a test.
pub const GKVM_ADDRESS: Address = address!("0x35597421749DeEad8ba95049eDEe0B94E66F3c59");

/// Domain string behind [`GKVM_ADDRESS`].
pub const GKVM_ADDRESS_DOMAIN: &[u8] = b"gaskiller.gkvm.addr.v1";

/// Domain tag hashed into [`env_commitment_v3`].
pub const ENV_COMMITMENT_DOMAIN_V3: &[u8] = b"gaskiller.env.unbounded.v3";

/// Domain string of the paged-Merkle artifact manifest (manifest v3).
pub const ARTIFACT_MANIFEST_DOMAIN_V3: &[u8] = b"gaskiller.artifact.v3";

/// The version of the guest VM spec bundle: {riscv64im, SP1 v6 cycle model,
/// hostcalls v1, the caps below}. Bumped only as a coordinated fleet-wide
/// change together with the env commitment.
pub const UNBOUNDED_V3_GKVM_SPEC_VERSION: u32 = 1;

/// Guest cycles per unit of EVM gas.
///
/// Sized so that "gas ≈ nanoseconds" — the semantics the 2^40/2^43 round
/// budgets were built around — survives the move to guest cycles, and V3 gas
/// reports stay comparable with V1/V2 consumers. Versioned, so a change is
/// unambiguous when it comes.
pub const UNBOUNDED_V3_CYCLES_PER_GAS: u64 = 4;

/// Gas charged before guest execution begins (ELF dispatch, mount lookup).
pub const GKVM_GAS_BASE: u64 = 65_536;

/// Gas charged per byte of gkExec payload — a DoS floor on large payloads,
/// mirroring calldata intrinsic pricing.
pub const GKVM_GAS_PER_INPUT_BYTE: u64 = 16;

/// Cap on the gkExec payload delivered through `gk_input_read`.
pub const GKVM_INPUT_BYTES_CAP: usize = 131_072;

/// Cap on the guest result delivered through `gk_output_write`.
pub const GKVM_OUTPUT_BYTES_CAP: usize = 131_072;

/// One artifact page: the unit `gk_artifact_read` serves and the Merkle leaf
/// granularity of manifest v3.
pub const GKVM_ARTIFACT_PAGE_SIZE: usize = 4_096;

/// Cap on guest memory (guest memory is zero-initialized, SP1 semantics).
pub const GKVM_MEM_BYTES_CAP: u64 = 1 << 31;

/// LRU entries in the provider's `(programHash, artifactRoot, keccak(payload))
/// → result` memo, sized so classify + replay + view passes never run an
/// inference twice.
pub const GKVM_RESULT_MEMO_ENTRIES: usize = 16;

/// The mandatory success tag prefixed to precompile returndata. Real chains
/// return empty data from the empty account at [`GKVM_ADDRESS`], so a tagged
/// non-empty reply is the only shape an honest simulation env produces —
/// `GkVm.sol` treats anything else as `GkVmUnavailable`.
pub const GKVM_OK_TAG: u8 = 0x01;

/// [`GKVM_ADDRESS`], recomputed from [`GKVM_ADDRESS_DOMAIN`] — the house
/// derivation pattern (cf. `overlay::overlay_chunk_address`).
pub fn gkvm_address() -> Address {
    Address::from_slice(&keccak256(GKVM_ADDRESS_DOMAIN)[12..])
}

/// Gas the provider charges before execution for a `payload_len`-byte wire
/// input: [`GKVM_GAS_BASE`] + [`GKVM_GAS_PER_INPUT_BYTE`] per byte, saturating.
pub fn gkvm_intrinsic_gas(payload_len: usize) -> u64 {
    GKVM_GAS_BASE.saturating_add((payload_len as u64).saturating_mul(GKVM_GAS_PER_INPUT_BYTE))
}

/// Convert an executed cycle count to gas: `ceil(cycles / CYCLES_PER_GAS)`.
pub fn cycles_to_gas(cycles: u64) -> u64 {
    cycles.div_ceil(UNBOUNDED_V3_CYCLES_PER_GAS)
}

/// Convert a gas allowance to a cycle budget: `gas × CYCLES_PER_GAS`,
/// saturating — the budget derives from pinned constants and pinned per-call
/// gas, never from operator hardware.
pub fn gas_to_cycle_limit(gas: u64) -> u64 {
    gas.saturating_mul(UNBOUNDED_V3_CYCLES_PER_GAS)
}

/// The gkvm axis of the simulation env, as bound by [`env_commitment_v3`].
///
/// Production code uses [`GkvmEnv::PINNED`]; the fields exist so the
/// commitment's sensitivity to each value is testable and so the SP1 guest
/// can state the values it verified under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GkvmEnv {
    /// See [`UNBOUNDED_V3_GKVM_SPEC_VERSION`].
    pub spec_version: u32,
    /// See [`UNBOUNDED_V3_CYCLES_PER_GAS`].
    pub cycles_per_gas: u64,
}

impl GkvmEnv {
    /// The pinned protocol values.
    pub const PINNED: GkvmEnv = GkvmEnv {
        spec_version: UNBOUNDED_V3_GKVM_SPEC_VERSION,
        cycles_per_gas: UNBOUNDED_V3_CYCLES_PER_GAS,
    };
}

/// The V3 arm of the env commitment, extending the `None → V1 / Some → V2`
/// dispatch of [`crate::overlay::env_commitment`]: a consumer that requires
/// the guest VM commits to this value instead, with or without overlays.
///
/// Layout (all fixed-width, keccak over concatenation):
/// `"gaskiller.env.unbounded.v3" || block_gas_limit_be || tx_gas_limit_be
///  || overlay_segment || gkvm_spec_version (u32 BE) || cycles_per_gas (u64 BE)`
/// where `overlay_segment` is exactly the V2 segment (`manifest || n_be ||
/// (address || keccak(code)) * n`) and is empty without overlays. The gkvm
/// tail is fixed-width and last, so the preimage parses unambiguously from
/// both ends.
pub fn env_commitment_v3(
    block_gas_limit: u64,
    tx_gas_limit: u64,
    overlay: Option<&OverlayEnv>,
    gkvm: &GkvmEnv,
) -> B256 {
    let mut pre = Vec::new();
    pre.extend_from_slice(ENV_COMMITMENT_DOMAIN_V3);
    pre.extend_from_slice(&block_gas_limit.to_be_bytes());
    pre.extend_from_slice(&tx_gas_limit.to_be_bytes());
    if let Some(env) = overlay {
        extend_overlay_segment(&mut pre, env);
    }
    pre.extend_from_slice(&gkvm.spec_version.to_be_bytes());
    pre.extend_from_slice(&gkvm.cycles_per_gas.to_be_bytes());
    keccak256(pre)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::env_commitment;
    use crate::sim_profile::{
        UNBOUNDED_V1_BLOCK_GAS_LIMIT, UNBOUNDED_V1_TX_GAS_LIMIT, UNBOUNDED_V1_XL_BLOCK_GAS_LIMIT,
        UNBOUNDED_V1_XL_TX_GAS_LIMIT,
    };
    use alloy_primitives::b256;

    #[test]
    fn gkvm_address_matches_its_derivation() {
        assert_eq!(gkvm_address(), GKVM_ADDRESS);
    }

    #[test]
    fn the_pin_set_is_what_the_spec_says() {
        // A change here is a consensus break with every other party that
        // re-executes a guest; it must ship as a GKVM_SPEC_VERSION bump.
        assert_eq!(UNBOUNDED_V3_GKVM_SPEC_VERSION, 1);
        assert_eq!(UNBOUNDED_V3_CYCLES_PER_GAS, 4);
        assert_eq!(GKVM_GAS_BASE, 65_536);
        assert_eq!(GKVM_GAS_PER_INPUT_BYTE, 16);
        assert_eq!(GKVM_INPUT_BYTES_CAP, 131_072);
        assert_eq!(GKVM_OUTPUT_BYTES_CAP, 131_072);
        assert_eq!(GKVM_ARTIFACT_PAGE_SIZE, 4_096);
        assert_eq!(GKVM_MEM_BYTES_CAP, 1 << 31);
        assert_eq!(GKVM_RESULT_MEMO_ENTRIES, 16);
        assert_eq!(GKVM_OK_TAG, 0x01);
        assert_eq!(GKVM_ADDRESS_DOMAIN, b"gaskiller.gkvm.addr.v1");
        assert_eq!(ENV_COMMITMENT_DOMAIN_V3, b"gaskiller.env.unbounded.v3");
        assert_eq!(ARTIFACT_MANIFEST_DOMAIN_V3, b"gaskiller.artifact.v3");
        assert_eq!(
            GkvmEnv::PINNED,
            GkvmEnv {
                spec_version: 1,
                cycles_per_gas: 4
            }
        );
    }

    #[test]
    fn metering_helpers_round_against_the_guest() {
        assert_eq!(cycles_to_gas(0), 0);
        assert_eq!(cycles_to_gas(1), 1);
        assert_eq!(cycles_to_gas(4), 1);
        assert_eq!(cycles_to_gas(5), 2);
        assert_eq!(gas_to_cycle_limit(1), 4);
        assert_eq!(gas_to_cycle_limit(u64::MAX), u64::MAX);
        // The V1 tier's 2^40 tx gas and the XL tier's 2^43, in guest cycles.
        assert_eq!(
            gas_to_cycle_limit(UNBOUNDED_V1_TX_GAS_LIMIT),
            4_398_046_511_104
        );
        assert_eq!(
            gas_to_cycle_limit(UNBOUNDED_V1_XL_TX_GAS_LIMIT),
            35_184_372_088_832
        );
        assert_eq!(gkvm_intrinsic_gas(0), 65_536);
        assert_eq!(gkvm_intrinsic_gas(64), 65_536 + 64 * 16);
        assert_eq!(gkvm_intrinsic_gas(usize::MAX), u64::MAX);
    }

    /// The preimage, assembled by hand from the spec's layout — independent of
    /// `extend_overlay_segment`, so a drift in the shared helper shows up here.
    fn spec_preimage(block: u64, tx: u64, overlay: Option<&OverlayEnv>, gkvm: &GkvmEnv) -> Vec<u8> {
        let mut pre = b"gaskiller.env.unbounded.v3".to_vec();
        pre.extend_from_slice(&block.to_be_bytes());
        pre.extend_from_slice(&tx.to_be_bytes());
        if let Some(env) = overlay {
            pre.extend_from_slice(env.manifest.as_slice());
            pre.extend_from_slice(&(env.overlays.len() as u64).to_be_bytes());
            for o in &env.overlays {
                pre.extend_from_slice(o.address.as_slice());
                pre.extend_from_slice(keccak256(&o.code).as_slice());
            }
        }
        pre.extend_from_slice(&gkvm.spec_version.to_be_bytes());
        pre.extend_from_slice(&gkvm.cycles_per_gas.to_be_bytes());
        pre
    }

    #[test]
    fn v3_layout_is_the_spec_layout() {
        let (block, tx) = (UNBOUNDED_V1_BLOCK_GAS_LIMIT, UNBOUNDED_V1_TX_GAS_LIMIT);
        let env = OverlayEnv::from_blobs(b"weights", b"tok").unwrap();

        let bare = spec_preimage(block, tx, None, &GkvmEnv::PINNED);
        // domain(26) + 8 + 8 + empty overlay segment + 4 + 8
        assert_eq!(bare.len(), 26 + 8 + 8 + 4 + 8);
        assert_eq!(
            bare[bare.len() - 12..],
            [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 4]
        );
        assert_eq!(
            env_commitment_v3(block, tx, None, &GkvmEnv::PINNED),
            keccak256(&bare)
        );

        let with_overlay = spec_preimage(block, tx, Some(&env), &GkvmEnv::PINNED);
        // two one-chunk blobs: manifest(32) + n(8) + 2 × (address(20) + codehash(32))
        assert_eq!(with_overlay.len(), bare.len() + 32 + 8 + 2 * 52);
        assert_eq!(
            env_commitment_v3(block, tx, Some(&env), &GkvmEnv::PINNED),
            keccak256(&with_overlay)
        );
    }

    /// Golden vectors for the byte-for-byte cross-check the SP1 fork's
    /// `chain_config_hash_with_overrides` owes this module (campaign task S1).
    /// Overlay = `OverlayEnv::from_blobs(b"weights", b"tok")`, the same blobs
    /// as `overlay::tests::derivation_matches_solidity_vectors`.
    #[test]
    fn v3_golden_vectors() {
        let env = OverlayEnv::from_blobs(b"weights", b"tok").unwrap();
        assert_eq!(
            env_commitment_v3(
                UNBOUNDED_V1_BLOCK_GAS_LIMIT,
                UNBOUNDED_V1_TX_GAS_LIMIT,
                None,
                &GkvmEnv::PINNED
            ),
            b256!("0x6cb5d625cfd62fbcc6bc1060ffafa657b6deb6f578a9a129a2d4188c0a8c6e6c")
        );
        assert_eq!(
            env_commitment_v3(
                UNBOUNDED_V1_BLOCK_GAS_LIMIT,
                UNBOUNDED_V1_TX_GAS_LIMIT,
                Some(&env),
                &GkvmEnv::PINNED
            ),
            b256!("0x9b328b3008a0fa78abfba712df46768e6c3a9f5bb28199dce8971b05085cf56a")
        );
        assert_eq!(
            env_commitment_v3(
                UNBOUNDED_V1_XL_BLOCK_GAS_LIMIT,
                UNBOUNDED_V1_XL_TX_GAS_LIMIT,
                Some(&env),
                &GkvmEnv::PINNED
            ),
            b256!("0x46d48e87de4d0b071bc8a7fc90da06a35ec59da1c89a8f5de97dab7eb6be6b42")
        );
    }

    #[test]
    fn v3_is_version_separated_and_binds_every_field() {
        let (block, tx) = (UNBOUNDED_V1_BLOCK_GAS_LIMIT, UNBOUNDED_V1_TX_GAS_LIMIT);
        let env = OverlayEnv::from_blobs(b"weights", b"tok").unwrap();
        let pinned = GkvmEnv::PINNED;

        let v3 = env_commitment_v3(block, tx, None, &pinned);
        let v3_overlay = env_commitment_v3(block, tx, Some(&env), &pinned);
        assert_ne!(v3, env_commitment(block, tx, None), "V3 ≠ V1");
        assert_ne!(v3_overlay, env_commitment(block, tx, Some(&env)), "V3 ≠ V2");
        assert_ne!(
            v3, v3_overlay,
            "overlay presence must change the commitment"
        );

        let other = OverlayEnv::from_blobs(b"weights2", b"tok").unwrap();
        assert_ne!(
            v3_overlay,
            env_commitment_v3(block, tx, Some(&other), &pinned),
            "different bytes must change the commitment"
        );
        assert_ne!(
            v3,
            env_commitment_v3(
                UNBOUNDED_V1_XL_BLOCK_GAS_LIMIT,
                UNBOUNDED_V1_XL_TX_GAS_LIMIT,
                None,
                &pinned
            ),
            "the XL gas tier must change the commitment"
        );
        assert_ne!(
            v3,
            env_commitment_v3(
                block,
                tx,
                None,
                &GkvmEnv {
                    spec_version: 2,
                    ..pinned
                }
            ),
            "a spec-version bump must change the commitment"
        );
        assert_ne!(
            v3,
            env_commitment_v3(
                block,
                tx,
                None,
                &GkvmEnv {
                    cycles_per_gas: 8,
                    ..pinned
                }
            ),
            "a different cycle↔gas rate must change the commitment"
        );
    }
}

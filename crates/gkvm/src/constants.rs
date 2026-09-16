//! The UNBOUNDED_V3 pinned constant set.
//!
//! Every constant here is a **versioned protocol value**, not a tunable: the
//! bundle {guest ISA, cycle model, hostcall surface, caps, metering rates} is
//! what [`UNBOUNDED_V3_GKVM_SPEC_VERSION`] names, and operators, the analyzer
//! and the SP1 dispute guest must agree on all of it bit-for-bit (the same
//! doctrine as `sim_profile.rs`'s gas limits, which these constants extend to
//! a third axis). Changing any value here changes what honest operators
//! produce and must ship as a coordinated spec-version bump — never as a
//! silent edit.
//!
//! M3 note: when the precompile provider lands (stacked on the LocalExecutor
//! PR), this module migrates into `gas-analyzer-core` next to `sim_profile.rs`
//! so the SP1 guest can import it without pulling the executor tree; the
//! values themselves must not change in that move.

use alloy_primitives::{Address, address};

/// The simulation-environment address of the gkvm precompile:
/// `address(uint160(uint256(keccak256("gaskiller.gkvm.addr.v1"))))`.
///
/// One fixed address for all guest programs — program identity travels as a
/// hash argument in the wire format, so there is no per-program address, no
/// registry, and nothing new to enumerate in the env commitment. On the real
/// chain this address is empty; a STATICCALL to it succeeds with empty
/// returndata, which `GkVm.sol` maps to a deterministic `GkVmUnavailable`
/// revert. The derivation is pinned by `gkvm_address_matches_its_derivation`.
pub const GKVM_ADDRESS: Address = address!("0x35597421749DeEad8ba95049eDEe0B94E66F3c59");

/// Domain string behind [`GKVM_ADDRESS`].
pub const GKVM_ADDRESS_DOMAIN: &[u8] = b"gaskiller.gkvm.addr.v1";

/// The version of the guest VM spec bundle: {riscv64im, SP1 v6 cycle model,
/// hostcalls v1, the caps below}. Bumped only as a coordinated fleet-wide
/// change together with the env commitment.
pub const UNBOUNDED_V3_GKVM_SPEC_VERSION: u32 = 1;

/// Guest cycles per unit of EVM gas.
///
/// The local executor measures ~1B gas/s, so 1 gas ≈ 1ns of honest-operator
/// wall clock ≈ 3–4 cycles at 3–4GHz; 4 preserves "gas ≈ nanoseconds", the
/// semantics the 2^40/2^43 round budgets were sized around. Provisional until
/// M1's throughput measurement revalidates it — but versioned, so a change is
/// unambiguous when it comes.
pub const UNBOUNDED_V3_CYCLES_PER_GAS: u64 = 4;

/// Gas charged before guest execution begins (ELF dispatch, mount lookup).
pub const GKVM_GAS_BASE: u64 = 65_536;

/// Gas charged per byte of gkExec payload — a DoS floor on large payloads,
/// mirroring calldata intrinsic pricing.
pub const GKVM_GAS_PER_INPUT_BYTE: u64 = 16;

/// Cap on the gkExec payload delivered through `gk_input_read`.
///
/// 128KiB matches the `call_data + storage_updates` transport cap: guest
/// input rides inside a tracked call's calldata, which is itself bounded by
/// what a round can carry.
pub const GKVM_INPUT_BYTES_CAP: usize = 131_072;

/// Cap on the guest result delivered through `gk_output_write`. Guest output
/// feeds consumer post-processing (fold to a root, emit logs), which is
/// transport-capped downstream; an unbounded result would just move the
/// failure later.
pub const GKVM_OUTPUT_BYTES_CAP: usize = 131_072;

/// One artifact page: the unit `gk_artifact_read` serves and the Merkle leaf
/// granularity of manifest v3.
pub const GKVM_ARTIFACT_PAGE_SIZE: usize = 4_096;

/// Cap on guest memory, enforced by the portable (consensus) executor tier.
///
/// The JIT tier maps a flat region and cannot enforce this mid-run; a guest
/// that exceeds the cap is deterministically trapped on the consensus tier,
/// and the matrix guests stay far below it so the tiers never diverge.
pub const GKVM_MEM_BYTES_CAP: u64 = 1 << 31;

/// LRU entries in the provider's `(programHash, artifactRoot, keccak(payload))
/// → result` memo, sized so classify + replay + view passes never run an
/// inference twice (used by the M3 provider; defined here with its siblings).
pub const GKVM_RESULT_MEMO_ENTRIES: usize = 16;

/// The mandatory success tag prefixed to precompile returndata. Real chains
/// return empty data from the empty account at [`GKVM_ADDRESS`], so a tagless
/// non-empty reply is the only shape an honest simulation env produces —
/// `GkVm.sol` treats anything else as `GkVmUnavailable`.
pub const GKVM_OK_TAG: u8 = 0x01;

/// Domain string of the paged-Merkle artifact manifest (see `manifest.rs`).
pub const ARTIFACT_MANIFEST_DOMAIN_V3: &[u8] = b"gaskiller.artifact.v3";

/// Exit code a guest halts with after writing a `GKTRAP01` abort frame via
/// `gk_abort` (see `runner.rs` for the frame layout). SP1 masks guest exit
/// codes to a byte, so the u32 trap code travels in the frame and the exit
/// code only marks that a frame is present.
pub const GKVM_TRAP_EXIT_CODE: u8 = 0xFA;

/// Trap code reported when the guest exceeds [`GKVM_MEM_BYTES_CAP`] on the
/// consensus tier (SP1's `TooMuchMemory`, which its own docs call
/// "deterministic for a given program+input").
pub const GKVM_TRAP_CODE_MEM_CAP: u32 = 0xF000_0001;

/// Trap code reported when a guest halts with a bare nonzero exit code (a C
/// `return 1`, an assert) without writing an abort frame. The exit byte is
/// carried as the low bits of the code.
pub const GKVM_TRAP_CODE_BARE_EXIT: u32 = 0xF000_0100;

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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;

    #[test]
    fn gkvm_address_matches_its_derivation() {
        let digest = keccak256(GKVM_ADDRESS_DOMAIN);
        assert_eq!(Address::from_slice(&digest[12..]), GKVM_ADDRESS);
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
        assert_eq!(ARTIFACT_MANIFEST_DOMAIN_V3, b"gaskiller.artifact.v3");
        assert_eq!(GKVM_OK_TAG, 0x01);
    }

    #[test]
    fn gas_cycle_conversion_rounds_against_the_guest() {
        assert_eq!(cycles_to_gas(0), 0);
        assert_eq!(cycles_to_gas(1), 1);
        assert_eq!(cycles_to_gas(4), 1);
        assert_eq!(cycles_to_gas(5), 2);
        assert_eq!(gas_to_cycle_limit(1), 4);
        assert_eq!(gas_to_cycle_limit(u64::MAX), u64::MAX);
    }
}

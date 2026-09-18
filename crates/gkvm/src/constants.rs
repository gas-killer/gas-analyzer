//! The UNBOUNDED_V3 pinned constant set — re-exported from its single home,
//! [`gas_analyzer_core::gkvm`].
//!
//! Every constant here is a **versioned protocol value**, not a tunable: the
//! bundle {guest ISA, cycle model, hostcall surface, caps, metering rates} is
//! what [`UNBOUNDED_V3_GKVM_SPEC_VERSION`] names, and operators, the analyzer
//! and the SP1 dispute guest must agree on all of it bit-for-bit. The values
//! are defined once, in `gas-analyzer-core` next to `sim_profile.rs` (the #166
//! doctrine: import, never restate), so the SP1 guest can take them without
//! pulling the executor tree. This module only keeps the executor crate's
//! historical paths (`gas_analyzer_gkvm::constants::*`) working; the test
//! below still pins the values as seen from here.

pub use gas_analyzer_core::gkvm::{
    ARTIFACT_MANIFEST_DOMAIN_V3, GKVM_ADDRESS, GKVM_ADDRESS_DOMAIN, GKVM_ARTIFACT_PAGE_SIZE,
    GKVM_GAS_BASE, GKVM_GAS_PER_INPUT_BYTE, GKVM_INPUT_BYTES_CAP, GKVM_MEM_BYTES_CAP, GKVM_OK_TAG,
    GKVM_OUTPUT_BYTES_CAP, GKVM_RESULT_MEMO_ENTRIES, GKVM_TRAP_CODE_BARE_EXIT,
    GKVM_TRAP_CODE_MEM_CAP, GKVM_TRAP_EXIT_CODE, UNBOUNDED_V3_CYCLES_PER_GAS,
    UNBOUNDED_V3_GKVM_SPEC_VERSION, cycles_to_gas, gas_to_cycle_limit,
};

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, keccak256};

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
        assert_eq!(GKVM_TRAP_EXIT_CODE, 0xFA);
        assert_eq!(GKVM_TRAP_CODE_MEM_CAP, 0xF000_0001);
        assert_eq!(GKVM_TRAP_CODE_BARE_EXIT, 0xF000_0100);
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

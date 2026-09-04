//! State update encoding and decoding functions.
//!
//! This module provides functions for ABI-encoding state updates
//! for transport to the StateChangeHandler contract.

use alloy_primitives::Bytes;
use alloy_sol_types::SolValue;

use crate::types::{StateUpdate, StateUpdateType};

/// The Turetzky upper gas limit for BLS-verified attestations - the floor gas cost
/// for executing a GasKiller transaction, i.e. the minimum StateChangeHandler
/// execution overhead. The floor depends on the signature scheme used to verify the
/// aggregated operator attestation on-chain, and BLS verification is the more
/// expensive of the two schemes.
pub const TURETZKY_UPPER_GAS_LIMIT_BLS: u64 = 250000u64;

/// The Turetzky upper gas limit for Schnorr-verified attestations. See
/// [`TURETZKY_UPPER_GAS_LIMIT_BLS`]; Schnorr verification is cheaper on-chain and so
/// yields a lower floor.
///
/// Measured on Sepolia against a 3-operator 2-of-3 `SchnorrStakeRegistry`: 32,066 with
/// full participation, 45,417 with one non-signer. One non-signer is the most a 2-of-3
/// quorum admits, and it costs that operator's record SLOADs plus one modexp point
/// subtraction, so the limit covers that case rather than full participation.
pub const TURETZKY_UPPER_GAS_LIMIT_SCHNORR: u64 = 50000u64;

/// Signature scheme used to verify the aggregated operator attestation on-chain.
///
/// Each scheme has a different on-chain verification cost, which sets the GasKiller
/// gas floor (the Turetzky upper gas limit). Gas figures are reported per scheme so
/// callers can compare the trade-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SignatureType {
    Bls,
    Schnorr,
}

impl SignatureType {
    /// All signature schemes, in the order they should be reported.
    pub const ALL: [SignatureType; 2] = [SignatureType::Bls, SignatureType::Schnorr];

    /// The Turetzky upper gas limit (GasKiller gas floor) for this scheme.
    pub fn turetzky_upper_gas_limit(self) -> u64 {
        match self {
            SignatureType::Bls => TURETZKY_UPPER_GAS_LIMIT_BLS,
            SignatureType::Schnorr => TURETZKY_UPPER_GAS_LIMIT_SCHNORR,
        }
    }

    /// Add this scheme's gas floor to a base state-change estimate to obtain the
    /// total GasKiller gas estimate.
    pub fn total_gas_estimate(self, base_estimate: u64) -> u64 {
        base_estimate + self.turetzky_upper_gas_limit()
    }

    /// GasKiller gas figures for this scheme against a transaction's actual usage,
    /// returned as `(total_estimate, gas_savings, percent_savings)`. `total_estimate`
    /// is the base state-change estimate plus this scheme's floor; `gas_savings`
    /// saturates at zero when the estimate exceeds `gas_used`; `percent_savings` is
    /// zero when `gas_used` is zero. Both callers (CLI and report) share this so the
    /// savings math cannot drift between them.
    pub fn savings(self, base_estimate: u64, gas_used: u64) -> (u64, u64, f64) {
        let total_estimate = self.total_gas_estimate(base_estimate);
        let gas_savings = gas_used.saturating_sub(total_estimate);
        let percent_savings = if gas_used > 0 {
            (gas_savings as f64 / gas_used as f64) * 100.0
        } else {
            0.0
        };
        (total_estimate, gas_savings, percent_savings)
    }

    /// Human-readable label for CLI and report output.
    pub fn label(self) -> &'static str {
        match self {
            SignatureType::Bls => "BLS",
            SignatureType::Schnorr => "Schnorr",
        }
    }
}

/// Encode state updates to Solidity types (for contract calls).
pub fn encode_state_updates_to_sol(
    state_updates: &[StateUpdate],
) -> (Vec<StateUpdateType>, Vec<Bytes>) {
    let state_update_types: Vec<StateUpdateType> = state_updates
        .iter()
        .map(|state_update| match state_update {
            StateUpdate::Store(_) => StateUpdateType::STORE,
            StateUpdate::Call(_) => StateUpdateType::CALL,
            StateUpdate::Log0(_) => StateUpdateType::LOG0,
            StateUpdate::Log1(_) => StateUpdateType::LOG1,
            StateUpdate::Log2(_) => StateUpdateType::LOG2,
            StateUpdate::Log3(_) => StateUpdateType::LOG3,
            StateUpdate::Log4(_) => StateUpdateType::LOG4,
            StateUpdate::Create(_) => StateUpdateType::CREATE,
            StateUpdate::Create2(_) => StateUpdateType::CREATE2,
        })
        .collect::<Vec<_>>();

    // This is ugly but I can't bother doing it with traits
    let datas: Vec<Bytes> = state_updates
        .iter()
        .map(|state_update| {
            Bytes::copy_from_slice(&match state_update {
                StateUpdate::Store(x) => x.abi_encode_sequence(),
                StateUpdate::Call(x) => x.abi_encode_sequence(),
                StateUpdate::Log0(x) => x.abi_encode_sequence(),
                StateUpdate::Log1(x) => x.abi_encode_sequence(),
                StateUpdate::Log2(x) => x.abi_encode_sequence(),
                StateUpdate::Log3(x) => x.abi_encode_sequence(),
                StateUpdate::Log4(x) => x.abi_encode_sequence(),
                StateUpdate::Create(x) => x.abi_encode_sequence(),
                StateUpdate::Create2(x) => x.abi_encode_sequence(),
            })
        })
        .collect::<Vec<_>>();

    (state_update_types, datas)
}

/// Encode state updates to ABI format for transport.
pub fn encode_state_updates_to_abi(state_updates: &[StateUpdate]) -> Bytes {
    let (state_update_types, datas) = encode_state_updates_to_sol(state_updates);

    // Encode as tuple (StateUpdateType[], bytes[])
    fn write_u256_word(buf: &mut Vec<u8>, value: usize) {
        let mut word = [0u8; 32];
        let bytes = (value as u128).to_be_bytes();
        word[32 - bytes.len()..].copy_from_slice(&bytes);
        buf.extend_from_slice(&word);
    }

    fn pad32_len(len: usize) -> usize {
        len.div_ceil(32) * 32
    }

    fn encode_bytes(value: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + pad32_len(value.len()));
        write_u256_word(&mut out, value.len());
        out.extend_from_slice(value);
        let padding = pad32_len(value.len()) - value.len();
        if padding > 0 {
            out.extend(std::iter::repeat_n(0u8, padding));
        }
        out
    }

    fn encode_bytes_array(values: &[Bytes]) -> Vec<u8> {
        let n = values.len();
        let encoded_elements: Vec<Vec<u8>> =
            values.iter().map(|b| encode_bytes(b.as_ref())).collect();

        let head_size = 32 * n;
        let mut out = Vec::new();
        write_u256_word(&mut out, n);

        let mut running_offset = head_size;
        for enc in &encoded_elements {
            write_u256_word(&mut out, running_offset);
            running_offset += enc.len();
        }

        for enc in encoded_elements {
            out.extend_from_slice(&enc);
        }

        out
    }

    // Encode StateUpdateType[] (enum array - each enum is a full 32-byte word)
    let mut types_payload = Vec::new();
    write_u256_word(&mut types_payload, state_update_types.len()); // array length
    for enum_val in &state_update_types {
        write_u256_word(&mut types_payload, *enum_val as u8 as usize); // each enum as 32 bytes
    }

    // Encode bytes[]
    let datas_payload = encode_bytes_array(&datas);

    // Build tuple with two offsets
    let offset_types = 0x40usize;
    let offset_datas = offset_types + types_payload.len();

    let mut encoded: Vec<u8> = Vec::with_capacity(64 + types_payload.len() + datas_payload.len());
    write_u256_word(&mut encoded, offset_types);
    write_u256_word(&mut encoded, offset_datas);
    encoded.extend_from_slice(&types_payload);
    encoded.extend_from_slice(&datas_payload);

    Bytes::copy_from_slice(&encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{IStateUpdateTypes, StateUpdate};
    use alloy_primitives::B256;

    /// A program with no operations still encodes to a full tuple: two offsets and two
    /// zero-length arrays. Nothing about the byte string says "empty" except its content, which
    /// is why [`encode_state_updates_to_abi`]'s callers are given the operation count rather than
    /// left to infer it from the payload.
    #[test]
    fn no_updates_encode_to_two_empty_arrays() {
        let encoded = encode_state_updates_to_abi(&[]);
        assert_eq!(encoded.len(), 128);
        assert_eq!(&encoded[31..32], &[0x40]); // offset: types
        assert_eq!(&encoded[63..64], &[0x60]); // offset: datas
        assert!(encoded[64..].iter().all(|&b| b == 0)); // both lengths zero
    }

    #[test]
    fn one_store_encodes_past_the_empty_form() {
        let store = StateUpdate::Store(IStateUpdateTypes::Store {
            slot: B256::ZERO,
            value: B256::with_last_byte(1),
        });
        let encoded = encode_state_updates_to_abi(&[store]);
        assert!(encoded.len() > 128);
        assert_ne!(encoded, encode_state_updates_to_abi(&[]));
    }

    #[test]
    fn turetzky_upper_gas_limit_matches_scheme() {
        assert_eq!(
            SignatureType::Bls.turetzky_upper_gas_limit(),
            TURETZKY_UPPER_GAS_LIMIT_BLS
        );
        assert_eq!(
            SignatureType::Schnorr.turetzky_upper_gas_limit(),
            TURETZKY_UPPER_GAS_LIMIT_SCHNORR
        );
        // Schnorr verification is cheaper on-chain, so its floor must be the smaller one.
        const { assert!(TURETZKY_UPPER_GAS_LIMIT_SCHNORR < TURETZKY_UPPER_GAS_LIMIT_BLS) };
    }

    #[test]
    fn total_gas_estimate_adds_scheme_floor() {
        let base = 100_000;
        assert_eq!(
            SignatureType::Bls.total_gas_estimate(base),
            base + TURETZKY_UPPER_GAS_LIMIT_BLS
        );
        assert_eq!(
            SignatureType::Schnorr.total_gas_estimate(base),
            base + TURETZKY_UPPER_GAS_LIMIT_SCHNORR
        );
    }

    #[test]
    fn savings_reports_estimate_savings_and_percent() {
        // gas_used well above the floor: savings are positive.
        let (estimate, savings, percent) = SignatureType::Bls.savings(50_000, 1_000_000);
        assert_eq!(estimate, 50_000 + TURETZKY_UPPER_GAS_LIMIT_BLS);
        assert_eq!(savings, 1_000_000 - estimate);
        assert!((percent - (savings as f64 / 1_000_000.0) * 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn savings_saturates_and_guards_zero_gas() {
        // Estimate exceeds usage: savings saturate to zero rather than underflow.
        let (_, savings, percent) = SignatureType::Bls.savings(1_000_000, 10_000);
        assert_eq!(savings, 0);
        assert_eq!(percent, 0.0);
        // Zero gas used: percentage is defined as zero, not NaN.
        let (_, _, percent_zero) = SignatureType::Bls.savings(0, 0);
        assert_eq!(percent_zero, 0.0);
    }

    #[test]
    fn all_covers_every_scheme_with_labels() {
        assert_eq!(
            SignatureType::ALL,
            [SignatureType::Bls, SignatureType::Schnorr]
        );
        assert_eq!(SignatureType::Bls.label(), "BLS");
        assert_eq!(SignatureType::Schnorr.label(), "Schnorr");
    }
}

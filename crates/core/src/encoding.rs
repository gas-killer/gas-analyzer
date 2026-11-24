//! State update encoding and decoding functions.
//!
//! This module provides functions for ABI-encoding state updates
//! for transport to the StateChangeHandler contract.

use alloy_primitives::Bytes;
use alloy_sol_types::SolValue;

use crate::signed_format::SignedFormat;
use crate::types::{IStateUpdateTypesV0_1, StateUpdate, StateUpdateType};

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
///
/// The settlements those figures come from, on verified contracts:
/// target <https://sepolia.etherscan.io/address/0xff7d6cdeeb44a357a936c8eb939fe8282d6da2e7>
/// verifying against
/// <https://sepolia.etherscan.io/address/0x69534bc676c60c0846736a5652b0779127b341a9>.
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

/// Encode state updates to Solidity types (for contract calls), in `format`'s layouts.
///
/// A v0.1.0 program is the v0.2.0 one without what v0.1.0 can't carry: `CALL` loses `resultHash`
/// and `gasLimit`, `CREATE` loses `expected`, and the read checks are left out. That is exactly
/// what an unpinned v0.1.0 program settles.
pub fn encode_state_updates_to_sol(
    state_updates: &[StateUpdate],
    format: SignedFormat,
) -> (Vec<StateUpdateType>, Vec<Bytes>) {
    let v0_1 = format == SignedFormat::V0_1;
    state_updates
        .iter()
        .filter(|update| !(v0_1 && update.is_check()))
        .map(|state_update| {
            let (kind, data) = match state_update {
                StateUpdate::Store(x) => (StateUpdateType::STORE, x.abi_encode_sequence()),
                StateUpdate::Call(x) if v0_1 => (
                    StateUpdateType::CALL,
                    IStateUpdateTypesV0_1::Call {
                        target: x.target,
                        value: x.value,
                        callargs: x.callargs.clone(),
                    }
                    .abi_encode_sequence(),
                ),
                StateUpdate::Call(x) => (StateUpdateType::CALL, x.abi_encode_sequence()),
                StateUpdate::Log0(x) => (StateUpdateType::LOG0, x.abi_encode_sequence()),
                StateUpdate::Log1(x) => (StateUpdateType::LOG1, x.abi_encode_sequence()),
                StateUpdate::Log2(x) => (StateUpdateType::LOG2, x.abi_encode_sequence()),
                StateUpdate::Log3(x) => (StateUpdateType::LOG3, x.abi_encode_sequence()),
                StateUpdate::Log4(x) => (StateUpdateType::LOG4, x.abi_encode_sequence()),
                StateUpdate::Create(x) if v0_1 => (
                    StateUpdateType::CREATE,
                    IStateUpdateTypesV0_1::Create {
                        value: x.value,
                        initcode: x.initcode.clone(),
                    }
                    .abi_encode_sequence(),
                ),
                StateUpdate::Create(x) => (StateUpdateType::CREATE, x.abi_encode_sequence()),
                StateUpdate::Create2(x) => (StateUpdateType::CREATE2, x.abi_encode_sequence()),
                StateUpdate::Nested(x) => (StateUpdateType::NESTED, x.abi_encode_sequence()),
                StateUpdate::StaticCallCheck(x) => {
                    (StateUpdateType::STATICCALL_CHECK, x.abi_encode_sequence())
                }
                StateUpdate::BalanceCheck(x) => {
                    (StateUpdateType::BALANCE_CHECK, x.abi_encode_sequence())
                }
                StateUpdate::CodehashCheck(x) => {
                    (StateUpdateType::CODEHASH_CHECK, x.abi_encode_sequence())
                }
            };
            (kind, Bytes::from(data))
        })
        .unzip()
}

/// Encode state updates to ABI format for transport, in `format`'s layouts (see
/// [`encode_state_updates_to_sol`]).
pub fn encode_state_updates_to_abi(state_updates: &[StateUpdate], format: SignedFormat) -> Bytes {
    let (state_update_types, datas) = encode_state_updates_to_sol(state_updates, format);

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
    use alloy_primitives::{Address, B256, U256};
    use alloy_sol_types::SolType;

    /// A program with no operations still encodes to a full tuple: two offsets and two
    /// zero-length arrays. Nothing about the byte string says "empty" except its content, which
    /// is why [`encode_state_updates_to_abi`]'s callers are given the operation count rather than
    /// left to infer it from the payload.
    #[test]
    fn no_updates_encode_to_two_empty_arrays() {
        let encoded = encode_state_updates_to_abi(&[], SignedFormat::V0_2);
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
        let encoded = encode_state_updates_to_abi(&[store], SignedFormat::V0_2);
        assert!(encoded.len() > 128);
        assert_ne!(
            encoded,
            encode_state_updates_to_abi(&[], SignedFormat::V0_2)
        );
    }

    fn pinned_call() -> StateUpdate {
        StateUpdate::Call(IStateUpdateTypes::Call {
            target: Address::repeat_byte(0xaa),
            value: U256::from(7),
            callargs: Bytes::from_static(&[1, 2, 3]),
            resultHash: B256::repeat_byte(0x11),
            gasLimit: U256::from(2_300),
        })
    }

    fn pinned_create() -> StateUpdate {
        StateUpdate::Create(IStateUpdateTypes::Create {
            value: U256::ZERO,
            initcode: Bytes::from_static(&[0x60, 0x00]),
            expected: Address::repeat_byte(0xbb),
        })
    }

    fn checks() -> Vec<StateUpdate> {
        vec![
            StateUpdate::StaticCallCheck(IStateUpdateTypes::StaticCallCheck {
                target: Address::repeat_byte(0xcc),
                data: Bytes::from_static(&[4]),
                resultHash: B256::repeat_byte(0x22),
            }),
            StateUpdate::BalanceCheck(IStateUpdateTypes::BalanceCheck {
                account: Address::repeat_byte(0xdd),
                balance: U256::from(9),
            }),
            StateUpdate::CodehashCheck(IStateUpdateTypes::CodehashCheck {
                account: Address::repeat_byte(0xee),
                codehash: B256::repeat_byte(0x33),
            }),
        ]
    }

    /// The head word solidity-sdk v0.2.0's `_requireOffset` checks: the offset of the op's
    /// dynamic field.
    fn dynamic_offset(arg: &[u8], at: usize) -> U256 {
        U256::from_be_slice(&arg[at..at + 32])
    }

    #[test]
    fn v0_2_layouts_match_the_sdks_canonical_offsets() {
        let (types, args) =
            encode_state_updates_to_sol(&[pinned_call(), pinned_create()], SignedFormat::V0_2);
        assert_eq!(
            types.iter().map(|t| *t as u8).collect::<Vec<_>>(),
            [StateUpdateType::CALL as u8, StateUpdateType::CREATE as u8]
        );
        assert_eq!(dynamic_offset(&args[0], 0x40), U256::from(0xa0));
        assert_eq!(dynamic_offset(&args[1], 0x20), U256::from(0x60));
        let call = <IStateUpdateTypes::Call as SolType>::abi_decode_sequence(&args[0]).unwrap();
        assert_eq!(call.resultHash, B256::repeat_byte(0x11));
        assert_eq!(call.gasLimit, U256::from(2_300));
    }

    #[test]
    fn v0_1_drops_what_it_cannot_carry() {
        let mut program = vec![pinned_call(), pinned_create()];
        program.extend(checks());
        let (types, args) = encode_state_updates_to_sol(&program, SignedFormat::V0_1);
        assert_eq!(
            types.iter().map(|t| *t as u8).collect::<Vec<_>>(),
            [StateUpdateType::CALL as u8, StateUpdateType::CREATE as u8]
        );
        assert_eq!(
            args[0].as_ref(),
            (
                Address::repeat_byte(0xaa),
                U256::from(7),
                Bytes::from_static(&[1, 2, 3])
            )
                .abi_encode_sequence()
        );
        assert_eq!(
            args[1].as_ref(),
            (U256::ZERO, Bytes::from_static(&[0x60, 0x00])).abi_encode_sequence()
        );
    }

    #[test]
    fn check_ops_take_the_sdks_enum_values() {
        let (types, args) = encode_state_updates_to_sol(&checks(), SignedFormat::V0_2);
        assert_eq!(
            types.iter().map(|t| *t as u8).collect::<Vec<_>>(),
            [10, 11, 12]
        );
        assert_eq!(
            args[0].as_ref(),
            (
                Address::repeat_byte(0xcc),
                Bytes::from_static(&[4]),
                B256::repeat_byte(0x22)
            )
                .abi_encode_sequence()
        );
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

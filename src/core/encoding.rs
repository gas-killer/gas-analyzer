//! State update encoding and decoding functions.
//!
//! This module provides functions for ABI-encoding state updates
//! for transport to the StateChangeHandler contract.

use alloy::primitives::{Bytes, U256};
use alloy::sol_types::SolValue;
use anyhow::{Result, bail};

use super::types::{StateUpdate, StateUpdateType};

/// The Turetzky upper gas limit - the floor gas cost for executing a GasKiller transaction.
/// This represents the minimum overhead for the StateChangeHandler execution.
pub const TURETZKY_UPPER_GAS_LIMIT: u64 = 250000u64;

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

/// Decode (uint256[], bytes[]) ABI tuple used for state update transport
#[allow(dead_code)]
pub fn decode_state_updates_tuple(data: &[u8]) -> Result<(Vec<U256>, Vec<Bytes>)> {
    fn read_u256_as_usize(word: &[u8]) -> usize {
        let mut buf = [0u8; 16];
        let copy_len = word.len().min(16);
        buf[16 - copy_len..].copy_from_slice(&word[word.len() - copy_len..]);
        u128::from_be_bytes(buf) as usize
    }

    fn get(data: &[u8], start: usize, len: usize) -> Result<&[u8]> {
        if start + len > data.len() {
            bail!("slice {}..{} of {}", start, start + len, data.len());
        }
        Ok(&data[start..start + len])
    }

    let types_offset = read_u256_as_usize(get(data, 0, 32)?);
    let data_offset = read_u256_as_usize(get(data, 32, 32)?);

    // types: uint256[]
    let n_types = read_u256_as_usize(get(data, types_offset, 32)?);
    let mut types = Vec::with_capacity(n_types);
    for i in 0..n_types {
        let word = get(data, types_offset + 32 + i * 32, 32)?;
        types.push(U256::from_be_slice(word));
    }

    // data: bytes[]
    let n_data = read_u256_as_usize(get(data, data_offset, 32)?);
    let head = data_offset + 32;
    let tail = head + 32 * n_data;
    let mut out = Vec::with_capacity(n_data);
    for i in 0..n_data {
        let rel = read_u256_as_usize(get(data, head + i * 32, 32)?);
        let start = tail + rel;
        let len = read_u256_as_usize(get(data, start, 32)?);
        out.push(Bytes::copy_from_slice(get(data, start + 32, len)?));
    }

    Ok((types, out))
}

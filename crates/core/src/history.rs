//! Historical state reads for tracked functions.
//!
//! A tracked function never executes on-chain: operators run it off-chain and only the resulting
//! state diff lands, through `verifyAndUpdate`. That makes it possible to give the off-chain
//! execution something the EVM cannot offer — read access to state at **earlier blocks**.
//!
//! The access point is a virtual precompile at [`HISTORY_PRECOMPILE_ADDRESS`] implementing
//! [`IGasKillerHistory`]. It exists only in the Gas Killer execution environment. On a real chain the
//! address has no code, so a tracked function that depends on it fails loudly if called directly
//! (an empty `STATICCALL` return cannot be ABI-decoded) instead of silently reading zeros.
//!
//! This module is the pure, protocol-level half: the ABI, the address, the pinned gas schedule, query
//! decoding and validation, result encoding, and the read log with its commitment. It does no I/O and
//! compiles in a zkVM guest, so the operator executor and a future slashing guest share one definition
//! of what a history read means. The I/O half (fetching state at a past block) lives in
//! `gas-analyzer-evmsketch`.
//!
//! Semantics: a read "at block `M`" observes the state **after** block `M` executed — the same state
//! `eth_getStorageAt(…, M)` returns. `M` must not be later than the block the tracked function itself
//! executes at; a later block is rejected, never answered.

use alloy_primitives::{Address, B256, Bytes, U256, address, keccak256};
use alloy_sol_types::{SolCall, SolValue, sol};

/// The virtual precompile tracked functions call to read historical state.
///
/// Derived as the low 20 bytes of `keccak256("gaskiller.history")`, far from the reserved
/// precompile range so no hardfork can collide with it.
pub const HISTORY_PRECOMPILE_ADDRESS: Address =
    address!("45ca7275e900a8cd49a62f59456402a57c192fc0");

/// Gas charged for a single-value history read (`storageAt`, `balanceAt`, `codeHashAt`,
/// `blockHashAt`, `blockTimestampAt`). Priced like a cold account access.
///
/// Part of the protocol: operators, the analyzer and a slashing guest must charge identical gas, or an
/// execution close to its gas limit could succeed for one party and run out of gas for another.
pub const HISTORY_READ_GAS: u64 = 2_600;

/// Flat gas charged to the caller for a `callAt`. The historical call's own execution runs in a
/// separate EVM against the historical block and does not consume the caller's gas; it is bounded by
/// [`HISTORY_CALL_INNER_GAS_LIMIT`] instead. A flat, pinned fee keeps the charge independent of
/// anything a prover would have to re-derive.
pub const HISTORY_CALL_GAS: u64 = 10_000;

/// Gas limit for the inner execution of a `callAt`. An inner call that exceeds it reports
/// `success = false` rather than reverting the tracked function.
pub const HISTORY_CALL_INNER_GAS_LIMIT: u64 = 30_000_000;

sol! {
    /// Read-only access to historical chain state for Gas Killer tracked functions.
    ///
    /// All functions must be reached with `STATICCALL`. Every `blockNumber` must be at or before the
    /// block the tracked function executes at.
    #[derive(Debug, PartialEq, Eq)]
    interface IGasKillerHistory {
        /// Storage slot `slot` of `account` after block `blockNumber`.
        function storageAt(address account, bytes32 slot, uint256 blockNumber) external view returns (bytes32 value);
        /// Ether balance of `account` after block `blockNumber`.
        function balanceAt(address account, uint256 blockNumber) external view returns (uint256 balance);
        /// Code hash of `account` after block `blockNumber` (zero for an account that does not exist).
        function codeHashAt(address account, uint256 blockNumber) external view returns (bytes32 codeHash);
        /// Hash of block `blockNumber`.
        function blockHashAt(uint256 blockNumber) external view returns (bytes32 blockHash);
        /// Timestamp of block `blockNumber`.
        function blockTimestampAt(uint256 blockNumber) external view returns (uint256 timestamp);
        /// Execute a read-only call to `target` with `data` against the state after block `blockNumber`,
        /// under that block's environment. State changes made by the call are discarded.
        function callAt(address target, bytes data, uint256 blockNumber) external view returns (bool success, bytes returnData);
    }

    /// One answered history query, in the order the tracked function issued it.
    ///
    /// The ordered list of these is what a prover needs to check an execution that used history: each
    /// entry is a claim about state at `blockNumber` that can be verified against that block's state
    /// root. Fields that do not apply to a `kind` are zero/empty.
    #[derive(Debug, PartialEq, Eq)]
    struct HistoricalRead {
        uint8 kind;
        uint64 blockNumber;
        address account;
        bytes32 slot;
        bytes input;
        bool success;
        bytes output;
    }
}

/// The kind of a [`HistoricalRead`], as stored in its `kind` field.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryReadKind {
    Storage = 0,
    Balance = 1,
    CodeHash = 2,
    BlockHash = 3,
    BlockTimestamp = 4,
    Call = 5,
}

/// A decoded, validated history query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryQuery {
    Storage {
        account: Address,
        slot: B256,
        block: u64,
    },
    Balance {
        account: Address,
        block: u64,
    },
    CodeHash {
        account: Address,
        block: u64,
    },
    BlockHash {
        block: u64,
    },
    BlockTimestamp {
        block: u64,
    },
    Call {
        target: Address,
        data: Bytes,
        block: u64,
    },
}

/// Why a history query was refused. Refusals revert the calling frame with [`Self::revert_reason`];
/// they are deterministic, so every party refuses the same queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryQueryError {
    /// The call data does not match any `IGasKillerHistory` function.
    UnknownSelector,
    /// The call data matched a selector but its arguments did not decode.
    MalformedInput,
    /// The query was reached with a regular `CALL` (or `CALLCODE`/`DELEGATECALL`), not `STATICCALL`.
    NotStatic,
    /// The query carried ether.
    ValueTransfer,
    /// The requested block is after the block the tracked function executes at.
    FutureBlock { requested: U256, current: u64 },
}

impl HistoryQueryError {
    /// The revert reason returned to the caller.
    pub fn revert_reason(&self) -> String {
        match self {
            Self::UnknownSelector => "GasKillerHistory: unknown function".into(),
            Self::MalformedInput => "GasKillerHistory: malformed input".into(),
            Self::NotStatic => "GasKillerHistory: must be called with STATICCALL".into(),
            Self::ValueTransfer => "GasKillerHistory: value transfer not allowed".into(),
            Self::FutureBlock { requested, current } => {
                format!("GasKillerHistory: block {requested} is after execution block {current}")
            }
        }
    }
}

impl HistoryQuery {
    /// The block this query reads from.
    pub fn block(&self) -> u64 {
        match self {
            Self::Storage { block, .. }
            | Self::Balance { block, .. }
            | Self::CodeHash { block, .. }
            | Self::BlockHash { block }
            | Self::BlockTimestamp { block }
            | Self::Call { block, .. } => *block,
        }
    }

    /// Gas charged to the caller for this query.
    pub fn gas_cost(&self) -> u64 {
        match self {
            Self::Call { .. } => HISTORY_CALL_GAS,
            _ => HISTORY_READ_GAS,
        }
    }

    /// Decode `input` (selector + ABI arguments) into a query and validate it against the block the
    /// tracked function executes at.
    ///
    /// `is_static` and `transfers_value` describe how the precompile was reached; both are checked
    /// here so that every implementation refuses exactly the same calls.
    pub fn decode(
        input: &[u8],
        execution_block: u64,
        is_static: bool,
        transfers_value: bool,
    ) -> Result<Self, HistoryQueryError> {
        if !is_static {
            return Err(HistoryQueryError::NotStatic);
        }
        if transfers_value {
            return Err(HistoryQueryError::ValueTransfer);
        }
        if input.len() < 4 {
            return Err(HistoryQueryError::UnknownSelector);
        }
        let selector: [u8; 4] = input[..4].try_into().expect("length checked");
        let block_of = |requested: U256| -> Result<u64, HistoryQueryError> {
            match u64::try_from(requested) {
                Ok(b) if b <= execution_block => Ok(b),
                _ => Err(HistoryQueryError::FutureBlock {
                    requested,
                    current: execution_block,
                }),
            }
        };
        use IGasKillerHistory as H;
        let bad = |_| HistoryQueryError::MalformedInput;
        Ok(match selector {
            H::storageAtCall::SELECTOR => {
                let c = H::storageAtCall::abi_decode(input).map_err(bad)?;
                Self::Storage {
                    account: c.account,
                    slot: c.slot,
                    block: block_of(c.blockNumber)?,
                }
            }
            H::balanceAtCall::SELECTOR => {
                let c = H::balanceAtCall::abi_decode(input).map_err(bad)?;
                Self::Balance {
                    account: c.account,
                    block: block_of(c.blockNumber)?,
                }
            }
            H::codeHashAtCall::SELECTOR => {
                let c = H::codeHashAtCall::abi_decode(input).map_err(bad)?;
                Self::CodeHash {
                    account: c.account,
                    block: block_of(c.blockNumber)?,
                }
            }
            H::blockHashAtCall::SELECTOR => {
                let c = H::blockHashAtCall::abi_decode(input).map_err(bad)?;
                Self::BlockHash {
                    block: block_of(c.blockNumber)?,
                }
            }
            H::blockTimestampAtCall::SELECTOR => {
                let c = H::blockTimestampAtCall::abi_decode(input).map_err(bad)?;
                Self::BlockTimestamp {
                    block: block_of(c.blockNumber)?,
                }
            }
            H::callAtCall::SELECTOR => {
                let c = H::callAtCall::abi_decode(input).map_err(bad)?;
                Self::Call {
                    target: c.target,
                    data: c.data,
                    block: block_of(c.blockNumber)?,
                }
            }
            _ => return Err(HistoryQueryError::UnknownSelector),
        })
    }
}

/// The answer to a [`HistoryQuery`], produced by whatever backend fetched the historical state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryAnswer {
    /// A single 32-byte word: the storage value, balance, code hash, block hash or timestamp.
    Word(B256),
    /// The outcome of a `callAt`.
    Call { success: bool, output: Bytes },
}

/// ABI-encode `answer` as the return data of `query`'s function.
pub fn encode_history_answer(answer: &HistoryAnswer) -> Bytes {
    match answer {
        HistoryAnswer::Word(w) => Bytes::from(w.abi_encode()),
        HistoryAnswer::Call { success, output } => {
            Bytes::from((*success, output.clone()).abi_encode_params())
        }
    }
}

/// Build the read-log entry for an answered query.
pub fn history_read_record(query: &HistoryQuery, answer: &HistoryAnswer) -> HistoricalRead {
    let mut r = HistoricalRead {
        kind: 0,
        blockNumber: query.block(),
        account: Address::ZERO,
        slot: B256::ZERO,
        input: Bytes::new(),
        success: true,
        output: Bytes::new(),
    };
    match query {
        HistoryQuery::Storage { account, slot, .. } => {
            r.kind = HistoryReadKind::Storage as u8;
            r.account = *account;
            r.slot = *slot;
        }
        HistoryQuery::Balance { account, .. } => {
            r.kind = HistoryReadKind::Balance as u8;
            r.account = *account;
        }
        HistoryQuery::CodeHash { account, .. } => {
            r.kind = HistoryReadKind::CodeHash as u8;
            r.account = *account;
        }
        HistoryQuery::BlockHash { .. } => r.kind = HistoryReadKind::BlockHash as u8,
        HistoryQuery::BlockTimestamp { .. } => r.kind = HistoryReadKind::BlockTimestamp as u8,
        HistoryQuery::Call { target, data, .. } => {
            r.kind = HistoryReadKind::Call as u8;
            r.account = *target;
            r.input = data.clone();
        }
    }
    match answer {
        HistoryAnswer::Word(w) => r.output = Bytes::copy_from_slice(w.as_slice()),
        HistoryAnswer::Call { success, output } => {
            r.success = *success;
            r.output = output.clone();
        }
    }
    r
}

/// Commitment to an ordered read log: `keccak256(abi.encode(HistoricalRead[]))`.
///
/// Two executions that issued the same queries and received the same answers produce the same
/// commitment, so it can be compared across operators or bound into a task without shipping the log.
/// An empty log commits to `keccak256(abi.encode(new HistoricalRead[](0)))`, not to zero.
pub fn history_reads_commitment(reads: &[HistoricalRead]) -> B256 {
    keccak256(reads.to_vec().abi_encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{b256, bytes};

    fn derived_address() -> Address {
        Address::from_slice(&keccak256("gaskiller.history")[12..])
    }

    #[test]
    fn address_is_the_documented_derivation() {
        assert_eq!(HISTORY_PRECOMPILE_ADDRESS, derived_address());
    }

    /// The selectors are what deployed Solidity calls; changing the interface silently would break
    /// every consumer, so pin them.
    #[test]
    fn selectors_are_stable() {
        use IGasKillerHistory as H;
        assert_eq!(H::storageAtCall::SELECTOR, [0xcf, 0xf9, 0x96, 0x35]);
        assert_eq!(H::balanceAtCall::SELECTOR, [0x3b, 0x8e, 0x6f, 0x2e]);
        assert_eq!(H::codeHashAtCall::SELECTOR, [0x4c, 0x28, 0x23, 0x33]);
        assert_eq!(H::blockHashAtCall::SELECTOR, [0x72, 0xa8, 0xa4, 0xfd]);
        assert_eq!(H::blockTimestampAtCall::SELECTOR, [0x68, 0xc7, 0x73, 0xf6]);
        assert_eq!(H::callAtCall::SELECTOR, [0xc3, 0x5a, 0x2c, 0xf0]);
    }

    fn storage_call(block: u64) -> Vec<u8> {
        IGasKillerHistory::storageAtCall {
            account: address!("00000000000000000000000000000000000000aa"),
            slot: B256::with_last_byte(7),
            blockNumber: U256::from(block),
        }
        .abi_encode()
    }

    #[test]
    fn decodes_a_valid_storage_query() {
        let q = HistoryQuery::decode(&storage_call(90), 100, true, false).unwrap();
        assert_eq!(
            q,
            HistoryQuery::Storage {
                account: address!("00000000000000000000000000000000000000aa"),
                slot: B256::with_last_byte(7),
                block: 90,
            }
        );
        assert_eq!(q.gas_cost(), HISTORY_READ_GAS);
    }

    #[test]
    fn the_execution_block_itself_is_allowed() {
        assert!(HistoryQuery::decode(&storage_call(100), 100, true, false).is_ok());
    }

    #[test]
    fn refuses_a_future_block() {
        let err = HistoryQuery::decode(&storage_call(101), 100, true, false).unwrap_err();
        assert_eq!(
            err,
            HistoryQueryError::FutureBlock {
                requested: U256::from(101),
                current: 100
            }
        );
        assert!(err.revert_reason().contains("after execution block 100"));
    }

    #[test]
    fn refuses_a_block_number_beyond_u64() {
        let mut input = storage_call(0);
        // blockNumber is the last word; set it to 2^64.
        let n = input.len();
        input[n - 32..].copy_from_slice(&(U256::from(1u64) << 64usize).to_be_bytes::<32>());
        assert!(matches!(
            HistoryQuery::decode(&input, u64::MAX, true, false),
            Err(HistoryQueryError::FutureBlock { .. })
        ));
    }

    #[test]
    fn refuses_non_static_and_value_bearing_calls() {
        assert_eq!(
            HistoryQuery::decode(&storage_call(1), 100, false, false),
            Err(HistoryQueryError::NotStatic)
        );
        assert_eq!(
            HistoryQuery::decode(&storage_call(1), 100, true, true),
            Err(HistoryQueryError::ValueTransfer)
        );
    }

    #[test]
    fn refuses_unknown_and_malformed_input() {
        assert_eq!(
            HistoryQuery::decode(&[1, 2, 3, 4], 100, true, false),
            Err(HistoryQueryError::UnknownSelector)
        );
        assert_eq!(
            HistoryQuery::decode(&[1, 2], 100, true, false),
            Err(HistoryQueryError::UnknownSelector)
        );
        let truncated = &storage_call(1)[..40];
        assert_eq!(
            HistoryQuery::decode(truncated, 100, true, false),
            Err(HistoryQueryError::MalformedInput)
        );
    }

    #[test]
    fn decodes_call_at() {
        let input = IGasKillerHistory::callAtCall {
            target: address!("00000000000000000000000000000000000000bb"),
            data: bytes!("6d4ce63c"),
            blockNumber: U256::from(5),
        }
        .abi_encode();
        let q = HistoryQuery::decode(&input, 10, true, false).unwrap();
        assert_eq!(q.gas_cost(), HISTORY_CALL_GAS);
        assert_eq!(q.block(), 5);
    }

    #[test]
    fn answers_round_trip_through_the_abi() {
        let w = b256!("00000000000000000000000000000000000000000000000000000000000004d2");
        let enc = encode_history_answer(&HistoryAnswer::Word(w));
        let back = IGasKillerHistory::storageAtCall::abi_decode_returns(&enc).unwrap();
        assert_eq!(back, w);

        let enc = encode_history_answer(&HistoryAnswer::Call {
            success: true,
            output: bytes!("deadbeef"),
        });
        let back = IGasKillerHistory::callAtCall::abi_decode_returns(&enc).unwrap();
        assert!(back.success);
        assert_eq!(back.returnData, bytes!("deadbeef"));
    }

    #[test]
    fn commitment_depends_on_order_and_content() {
        let q1 = HistoryQuery::BlockTimestamp { block: 1 };
        let q2 = HistoryQuery::BlockTimestamp { block: 2 };
        let a = HistoryAnswer::Word(B256::with_last_byte(1));
        let r1 = history_read_record(&q1, &a);
        let r2 = history_read_record(&q2, &a);
        let c12 = history_reads_commitment(&[r1.clone(), r2.clone()]);
        let c21 = history_reads_commitment(&[r2.clone(), r1.clone()]);
        assert_ne!(c12, c21);
        assert_eq!(c12, history_reads_commitment(&[r1, r2]));
        assert_ne!(history_reads_commitment(&[]), B256::ZERO);
    }
}

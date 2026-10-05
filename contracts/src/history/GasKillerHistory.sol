// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {IGasKillerHistory} from "./IGasKillerHistory.sol";

/// @title GasKillerHistory
/// @notice Helpers for reading historical chain state from a Gas Killer tracked function.
/// @dev Each helper `STATICCALL`s the history precompile and decodes its answer. Two failure modes
///      are distinguished:
///      - the precompile refused the query (for example a block after the execution block): its
///        revert reason is bubbled up unchanged;
///      - the precompile is absent, because the function is running on a real chain instead of in
///        the Gas Killer execution environment: reverts with `HistoryUnavailable`. A plain
///        `STATICCALL` to an address with no code succeeds with empty return data, so without this
///        check a direct on-chain call would silently read zeros.
library GasKillerHistory {
    /// @notice Address of the history precompile.
    address internal constant HISTORY = 0x45CA7275E900a8Cd49a62F59456402a57C192fC0;

    /// @notice The history precompile is not available in this execution environment.
    error HistoryUnavailable();

    function storageAt(address account, bytes32 slot, uint256 blockNumber) internal view returns (bytes32) {
        bytes memory ret = _query(abi.encodeCall(IGasKillerHistory.storageAt, (account, slot, blockNumber)));
        return abi.decode(ret, (bytes32));
    }

    function balanceAt(address account, uint256 blockNumber) internal view returns (uint256) {
        bytes memory ret = _query(abi.encodeCall(IGasKillerHistory.balanceAt, (account, blockNumber)));
        return abi.decode(ret, (uint256));
    }

    function codeHashAt(address account, uint256 blockNumber) internal view returns (bytes32) {
        bytes memory ret = _query(abi.encodeCall(IGasKillerHistory.codeHashAt, (account, blockNumber)));
        return abi.decode(ret, (bytes32));
    }

    function blockHashAt(uint256 blockNumber) internal view returns (bytes32) {
        bytes memory ret = _query(abi.encodeCall(IGasKillerHistory.blockHashAt, (blockNumber)));
        return abi.decode(ret, (bytes32));
    }

    function blockTimestampAt(uint256 blockNumber) internal view returns (uint256) {
        bytes memory ret = _query(abi.encodeCall(IGasKillerHistory.blockTimestampAt, (blockNumber)));
        return abi.decode(ret, (uint256));
    }

    /// @notice Run a read-only call against a past block. A failing historical call is reported
    ///         through `success`, not by reverting.
    function callAt(address target, bytes memory data, uint256 blockNumber)
        internal
        view
        returns (bool success, bytes memory returnData)
    {
        bytes memory ret = _query(abi.encodeCall(IGasKillerHistory.callAt, (target, data, blockNumber)));
        return abi.decode(ret, (bool, bytes));
    }

    /// @dev `STATICCALL` the precompile; bubble a refusal, reject an empty answer.
    function _query(bytes memory input) private view returns (bytes memory ret) {
        bool ok;
        (ok, ret) = HISTORY.staticcall(input);
        if (!ok) {
            assembly ("memory-safe") {
                revert(add(ret, 32), mload(ret))
            }
        }
        if (ret.length < 32) revert HistoryUnavailable();
    }
}

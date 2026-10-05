// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title IGasKillerHistory
/// @notice Read-only access to historical chain state for Gas Killer tracked functions.
/// @dev Served by a virtual precompile at `GasKillerHistory.HISTORY`
///      (`0x45CA7275E900a8Cd49a62F59456402a57C192fC0`, the low 20 bytes of
///      `keccak256("gaskiller.history")`). It exists only in the Gas Killer off-chain execution
///      environment: operators run the tracked function there and only the resulting state diff
///      lands on-chain through `verifyAndUpdate`. On a real chain the address has no code.
///
///      Every function must be reached with `STATICCALL` (Solidity does this for `view` calls) and
///      every `blockNumber` must be at or before the block the tracked function executes at. A read
///      "at block N" sees the state after block N executed — what `eth_getStorageAt(..., N)` returns.
///      Prefer the `GasKillerHistory` library, which also fails loudly when the precompile is absent.
interface IGasKillerHistory {
    /// @notice Storage slot `slot` of `account` after block `blockNumber`.
    function storageAt(address account, bytes32 slot, uint256 blockNumber) external view returns (bytes32 value);

    /// @notice Ether balance of `account` after block `blockNumber`.
    function balanceAt(address account, uint256 blockNumber) external view returns (uint256 balance);

    /// @notice Code hash of `account` after block `blockNumber` (zero for an empty account).
    function codeHashAt(address account, uint256 blockNumber) external view returns (bytes32 codeHash);

    /// @notice Hash of block `blockNumber`.
    function blockHashAt(uint256 blockNumber) external view returns (bytes32 blockHash);

    /// @notice Timestamp of block `blockNumber`.
    function blockTimestampAt(uint256 blockNumber) external view returns (uint256 timestamp);

    /// @notice Execute a read-only call to `target` with `data` against the state after block
    ///         `blockNumber`, under that block's environment. State changes are discarded.
    /// @dev The call runs with a fixed 30,000,000 gas limit and cannot itself read history.
    /// @return success Whether the historical call succeeded.
    /// @return returnData Its return data, or revert data if it failed.
    function callAt(address target, bytes calldata data, uint256 blockNumber)
        external
        view
        returns (bool success, bytes memory returnData);
}

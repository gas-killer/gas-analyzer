// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import {GasKillerHistory} from "../GasKillerHistory.sol";

/// @notice The part of ERC-4626 this example reads.
interface IERC4626SharePrice {
    function convertToAssets(uint256 shares) external view returns (uint256 assets);
}

/// @title VaultLossOracle
/// @notice Example tracked contract that needs historical state: it records how far an ERC-4626
///         vault's share price fell between two past blocks — the check an on-chain insurance
///         protocol makes before paying a claim.
/// @dev Intended to run as a Gas Killer tracked function. Operators execute `assessLoss`
///      off-chain, where the history precompile can read the vault at `fromBlock` and `toBlock`;
///      only the stored result and the event land on-chain. Called directly on a real chain it
///      reverts with `GasKillerHistory.HistoryUnavailable`, because no EVM can read past state.
///
///      A production consumer inherits `GasKillerSDK` and marks `assessLoss` with `trackState`, as
///      the SDK's `ArraySummation` example does; that wiring is left out to keep the example focused
///      on the history reads.
contract VaultLossOracle {
    uint256 internal constant ONE_SHARE = 1e18;
    uint256 internal constant BPS = 10_000;

    /// @notice Loss in basis points, keyed by `keccak256(abi.encode(vault, fromBlock, toBlock))`.
    mapping(bytes32 => uint256) public lossBps;

    error InvalidRange(uint256 fromBlock, uint256 toBlock);
    error SharePriceUnavailable(address vault, uint256 blockNumber);

    event LossAssessed(
        address indexed vault,
        uint256 fromBlock,
        uint256 toBlock,
        uint256 priceBefore,
        uint256 priceAfter,
        uint256 lossBps
    );

    /// @notice Measure and store the share-price loss of `vault` between two past blocks.
    /// @return bps The loss in basis points; zero if the price did not fall.
    function assessLoss(address vault, uint256 fromBlock, uint256 toBlock) external returns (uint256 bps) {
        if (fromBlock >= toBlock || toBlock > block.number) revert InvalidRange(fromBlock, toBlock);

        uint256 priceBefore = _sharePrice(vault, fromBlock);
        uint256 priceAfter = _sharePrice(vault, toBlock);
        bps = priceAfter >= priceBefore ? 0 : ((priceBefore - priceAfter) * BPS) / priceBefore;

        lossBps[keccak256(abi.encode(vault, fromBlock, toBlock))] = bps;
        emit LossAssessed(vault, fromBlock, toBlock, priceBefore, priceAfter, bps);
    }

    function _sharePrice(address vault, uint256 blockNumber) internal view returns (uint256) {
        (bool ok, bytes memory ret) = GasKillerHistory.callAt(
            vault, abi.encodeCall(IERC4626SharePrice.convertToAssets, (ONE_SHARE)), blockNumber
        );
        if (!ok || ret.length < 32) revert SharePriceUnavailable(vault, blockNumber);
        return abi.decode(ret, (uint256));
    }
}

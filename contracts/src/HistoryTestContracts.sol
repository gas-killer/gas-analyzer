// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title MockSharePriceVault
/// @notice Minimal ERC-4626 share-price source for the historical-state tests: the share price is
///         whatever `setPrice` last stored, so tests can give each block a known price.
contract MockSharePriceVault {
    /// @notice Assets per 1e18 shares.
    uint256 public price;

    function setPrice(uint256 newPrice) external {
        price = newPrice;
    }

    function convertToAssets(uint256 shares) external view returns (uint256) {
        return (shares * price) / 1e18;
    }
}

// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.13;

import { StateChangeHandlerLib, StateUpdateType } from "../lib/gas-killer-avs-sol/src/StateChangeHandlerLib.sol";

/// @notice Gas estimator with transparent-proxy fallback.
///
/// When injected into the simulation at the original contract's address, this
/// contract handles `runStateUpdatesCall` for gas measurement and forwards every
/// other call (e.g. callbacks from oracles or AMMs) to the original implementation
/// via DELEGATECALL, preserving `address(this)`, `msg.sender`, and storage context.
///
/// The implementation address is stored in an EIP-1967-style isolated storage slot
/// to avoid colliding with the original contract's storage layout when the fallback
/// DELEGATECALLs into the backup bytecode.
///
/// Rust bypasses the constructor: it loads the deployed bytecode directly from the
/// Foundry artifact and writes `backup_addr` to IMPL_SLOT via `insert_account_storage`.
contract StateChangeHandlerGasEstimator {
    /// @dev Isolated storage slot for the implementation address.
    /// keccak256("gas.estimator.implementation") - 1
    bytes32 private constant IMPL_SLOT =
        0x96d8ea8ab34935626cad61e29511d513907fd7c186676bae1b82974066723cbf;

    /// @dev Isolated storage slot for tracking if fallback was called during `runStateUpdatesCall`
    /// keccak256("gas.estimator.reentrancy") - 1
    bytes32 private constant REENTRANCY_CHECK_SLOT =
        0x242ffd5c5678a014b50279a5c080b8776eeb4383dff0782c0b229c029f801303;

    constructor(address _implementation) {
        assembly {
            sstore(IMPL_SLOT, _implementation)
        }
    }

    function runStateUpdatesCall(StateUpdateType[] memory types, bytes[] memory args) external {
        StateChangeHandlerLib._runStateUpdates(types, args);

        // cold sload: 2000
        // hot sload: 100
        // if gas diff is less than 2000 than guaranteed it was a hot slot
        uint256 gasBefore = gas();
        assembly {
            sload(REENTRANCY_CHECK_SLOT)
        }
        uint256 gasAfter = gas();
        if (gasBefore - gasAfter < 2000) {
            assembly {
                sstore(REENTRANCY_CHECK_SLOT, 1)
            }
        }
    }

    function fallbackWasCalled() external view returns (bool result) {
        assembly {
            result := sload(REENTRANCY_CHECK_SLOT)
        }
    }

    /// @dev Forward any unknown selector to the original implementation via DELEGATECALL.
    fallback() external payable {
        assembly {
            sload(REENTRANCY_CHECK_SLOT)  // load slot so in the end of `runStateUpdatesCall` gas introspection detects if it has been read
            let impl := sload(IMPL_SLOT)
            calldatacopy(0, 0, calldatasize())
            let success := delegatecall(gas(), impl, 0, calldatasize(), 0, 0)
            returndatacopy(0, 0, returndatasize())
            switch success
            case 0 { revert(0, returndatasize()) }
            default { return(0, returndatasize()) }
        }
    }

    receive() external payable {}
}

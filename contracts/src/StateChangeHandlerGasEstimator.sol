// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.27;

import {StateChangeHandlerLib, StateUpdateType} from "../lib/solidity-sdk/src/StateChangeHandlerLib.sol";
import {StorageGuard} from "../lib/solidity-sdk/src/libraries/StorageGuard.sol";

/// @notice Applies a solidity-sdk v0.2.0 program the way settlement does, checking every STORE
/// against the consumer's protected ranges, to measure what settling it costs.
///
/// Rust injects it in place of the consumer's code and writes the original contract's address to
/// `IMPL_SLOT` without running a constructor. Every selector other than `runStateUpdatesCall` is
/// DELEGATECALLed to the original, so external protocols calling back into the consumer during a
/// CALL get valid responses.
///
/// What it leaves out of settlement: the consumer's call-policy hooks (`_allowSelfCall`,
/// `_authorizeCall`, `_maxOutgoingValue`) and its `_beforeProgram`/`_afterProgram`. They are
/// internal overrides with no view to read them through, so every CALL, CREATE and CREATE2 runs
/// unrestricted here. Settlement still enforces them.
contract StateChangeHandlerGasEstimator {
    /// @dev Isolated storage slot for the implementation address.
    /// keccak256("gas.estimator.implementation") - 1
    bytes32 private constant IMPL_SLOT = 0x96d8ea8ab34935626cad61e29511d513907fd7c186676bae1b82974066723cbf;

    constructor(address _implementation) {
        assembly {
            sstore(IMPL_SLOT, _implementation)
        }
    }

    /// @param ranges The consumer's `gasKillerProtectedRanges()`, which settlement checks every
    ///        STORE against. Empty when the consumer opted out of the guard, which runs the
    ///        program unchecked as settlement does.
    function runStateUpdatesCall(
        StateUpdateType[] memory types,
        bytes[] memory args,
        StorageGuard.ProtectedRange[] memory ranges
    ) external payable {
        if (ranges.length == 0) {
            StateChangeHandlerLib._runStateUpdatesUnguarded(types, args, StateChangeHandlerLib._unrestrictedCalls());
        } else {
            // Settlement uses the prevalidated path too: the consumer's constructor validated the list.
            StateChangeHandlerLib._runStateUpdatesPrevalidated(
                types, args, ranges, StateChangeHandlerLib._unrestrictedCalls()
            );
        }
    }

    /// @dev Forward any unknown selector to the original implementation via DELEGATECALL.
    fallback() external payable {
        assembly {
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

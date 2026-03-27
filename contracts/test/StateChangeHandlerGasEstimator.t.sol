// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.13;

import "forge-std/Test.sol";
import "../src/StateChangeHandlerGasEstimator.sol";
import { StateUpdateType } from "gk/StateChangeHandlerLib.sol";

/// @dev Calls back into msg.sender with an unknown selector, triggering the estimator's fallback.
contract ReentrantCaller {
    function reenter() external {
        (bool success, ) = msg.sender.call(abi.encodeWithSignature("someUnknownFunction()"));
        require(success, "Reentrant call failed");
    }
}

/// @dev Dummy implementation — accepts any delegatecall without reverting.
contract DummyImplementation {
    fallback() external payable {}
    receive() external payable {}
}

/// @dev Does not call back into the estimator.
contract NonReentrantCallee {
    function doNothing() external {}
}

/// @dev Implementation that stores a value when called via delegatecall, so we can verify delegation happened.
contract RecordingImplementation {
    // uses a fixed slot to avoid collision
    bytes32 private constant CALLED_SLOT = 0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa;

    fallback() external payable {
        assembly {
            sstore(CALLED_SLOT, 1)
        }
    }

    receive() external payable {}
}

contract StateChangeHandlerGasEstimatorReentrancyTest is Test {
    /// keccak256("gas.estimator.reentrancy") - 1
    bytes32 private constant REENTRANCY_CHECK_SLOT =
        0x242ffd5c5678a014b50279a5c080b8776eeb4383dff0782c0b229c029f801303;

    /// keccak256("gas.estimator.implementation") - 1
    bytes32 private constant IMPL_SLOT =
        0x96d8ea8ab34935626cad61e29511d513907fd7c186676bae1b82974066723cbf;

    bytes32 private constant RECORDING_CALLED_SLOT = 0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa;

    StateChangeHandlerGasEstimator estimator;
    DummyImplementation impl;
    ReentrantCaller reentrant;
    NonReentrantCallee nonReentrant;

    function setUp() public {
        impl = new DummyImplementation();
        estimator = new StateChangeHandlerGasEstimator(address(impl));
        reentrant = new ReentrantCaller();
        nonReentrant = new NonReentrantCallee();
    }

    // ---------------------------------------------------------------
    // fallbackWasCalled() read tests
    // ---------------------------------------------------------------

    function test_fallbackWasCalled_falseInitially() public view {
        assertFalse(estimator.fallbackWasCalled());
    }

    function test_fallbackWasCalled_trueWhenSlotSet() public {
        vm.store(address(estimator), REENTRANCY_CHECK_SLOT, bytes32(uint256(1)));
        assertTrue(estimator.fallbackWasCalled());
    }

    function test_fallbackWasCalled_falseWhenSlotZero() public {
        vm.store(address(estimator), REENTRANCY_CHECK_SLOT, bytes32(uint256(1)));
        assertTrue(estimator.fallbackWasCalled());

        vm.store(address(estimator), REENTRANCY_CHECK_SLOT, bytes32(uint256(0)));
        assertFalse(estimator.fallbackWasCalled());
    }

    // ---------------------------------------------------------------
    // fallback delegation tests
    // ---------------------------------------------------------------

    function test_fallback_delegatesToImplementation() public {
        RecordingImplementation recordingImpl = new RecordingImplementation();
        StateChangeHandlerGasEstimator est = new StateChangeHandlerGasEstimator(address(recordingImpl));

        (bool success, ) = address(est).call(abi.encodeWithSignature("unknownFunction()"));
        assertTrue(success, "Fallback delegatecall should succeed");

        bytes32 val = vm.load(address(est), RECORDING_CALLED_SLOT);
        assertEq(val, bytes32(uint256(1)), "Implementation should have written to estimator's storage via delegatecall");
    }

    // ---------------------------------------------------------------
    // runStateUpdatesCall: state updates execute correctly
    // ---------------------------------------------------------------

    function test_runStateUpdatesCall_executesStores() public {
        StateUpdateType[] memory types = new StateUpdateType[](1);
        bytes[] memory args = new bytes[](1);

        bytes32 slot = bytes32(uint256(42));
        bytes32 value = bytes32(uint256(0xBEEF));

        types[0] = StateUpdateType.STORE;
        args[0] = abi.encode(slot, value);

        estimator.runStateUpdatesCall(types, args);

        bytes32 stored = vm.load(address(estimator), slot);
        assertEq(stored, value);
    }

    function test_runStateUpdatesCall_executesCalls() public {
        StateUpdateType[] memory types = new StateUpdateType[](1);
        bytes[] memory args = new bytes[](1);

        types[0] = StateUpdateType.CALL;
        args[0] = abi.encode(
            address(nonReentrant),
            uint256(0),
            abi.encodeCall(NonReentrantCallee.doNothing, ())
        );

        estimator.runStateUpdatesCall(types, args);
    }

    function test_runStateUpdatesCall_emptyUpdates() public {
        estimator.runStateUpdatesCall(new StateUpdateType[](0), new bytes[](0));
    }

    // ---------------------------------------------------------------
    // Reentrancy detection: end-to-end
    // ---------------------------------------------------------------

    function test_noReentrancy_emptyUpdates() public {
        estimator.runStateUpdatesCall(new StateUpdateType[](0), new bytes[](0));
        assertFalse(estimator.fallbackWasCalled());
    }

    function test_noReentrancy_sstoreOnly() public {
        StateUpdateType[] memory types = new StateUpdateType[](1);
        bytes[] memory args = new bytes[](1);

        types[0] = StateUpdateType.STORE;
        args[0] = abi.encode(bytes32(uint256(42)), bytes32(uint256(1)));

        estimator.runStateUpdatesCall(types, args);
        assertFalse(estimator.fallbackWasCalled());
    }

    function test_noReentrancy_callWithoutCallback() public {
        StateUpdateType[] memory types = new StateUpdateType[](1);
        bytes[] memory args = new bytes[](1);

        types[0] = StateUpdateType.CALL;
        args[0] = abi.encode(
            address(nonReentrant),
            uint256(0),
            abi.encodeCall(NonReentrantCallee.doNothing, ())
        );

        estimator.runStateUpdatesCall(types, args);
        assertFalse(estimator.fallbackWasCalled());
    }

    function test_noReentrancy_multipleSstores() public {
        StateUpdateType[] memory types = new StateUpdateType[](3);
        bytes[] memory args = new bytes[](3);

        for (uint256 i = 0; i < 3; i++) {
            types[i] = StateUpdateType.STORE;
            args[i] = abi.encode(bytes32(i), bytes32(i + 1));
        }

        estimator.runStateUpdatesCall(types, args);
        assertFalse(estimator.fallbackWasCalled());
    }

    function test_noReentrancy_multipleNonReentrantCalls() public {
        StateUpdateType[] memory types = new StateUpdateType[](2);
        bytes[] memory args = new bytes[](2);

        for (uint256 i = 0; i < 2; i++) {
            types[i] = StateUpdateType.CALL;
            args[i] = abi.encode(
                address(nonReentrant),
                uint256(0),
                abi.encodeCall(NonReentrantCallee.doNothing, ())
            );
        }

        estimator.runStateUpdatesCall(types, args);
        assertFalse(estimator.fallbackWasCalled());
    }

    function test_noReentrancy_mixedStoresAndCalls() public {
        StateUpdateType[] memory types = new StateUpdateType[](2);
        bytes[] memory args = new bytes[](2);

        types[0] = StateUpdateType.STORE;
        args[0] = abi.encode(bytes32(uint256(42)), bytes32(uint256(1)));

        types[1] = StateUpdateType.CALL;
        args[1] = abi.encode(
            address(nonReentrant),
            uint256(0),
            abi.encodeCall(NonReentrantCallee.doNothing, ())
        );

        estimator.runStateUpdatesCall(types, args);
        assertFalse(estimator.fallbackWasCalled());
    }

    function test_reentrancy_detected() public {
        StateUpdateType[] memory types = new StateUpdateType[](1);
        bytes[] memory args = new bytes[](1);

        types[0] = StateUpdateType.CALL;
        args[0] = abi.encode(
            address(reentrant),
            uint256(0),
            abi.encodeCall(ReentrantCaller.reenter, ())
        );

        estimator.runStateUpdatesCall(types, args);
        assertTrue(estimator.fallbackWasCalled());
    }

    function test_reentrancy_detectedAmongMultipleUpdates() public {
        StateUpdateType[] memory types = new StateUpdateType[](3);
        bytes[] memory args = new bytes[](3);

        types[0] = StateUpdateType.STORE;
        args[0] = abi.encode(bytes32(uint256(1)), bytes32(uint256(0xAA)));

        types[1] = StateUpdateType.CALL;
        args[1] = abi.encode(
            address(reentrant),
            uint256(0),
            abi.encodeCall(ReentrantCaller.reenter, ())
        );

        types[2] = StateUpdateType.STORE;
        args[2] = abi.encode(bytes32(uint256(2)), bytes32(uint256(0xBB)));

        estimator.runStateUpdatesCall(types, args);
        assertTrue(estimator.fallbackWasCalled());

        // Verify SSTOREs were also applied
        assertEq(vm.load(address(estimator), bytes32(uint256(1))), bytes32(uint256(0xAA)));
        assertEq(vm.load(address(estimator), bytes32(uint256(2))), bytes32(uint256(0xBB)));
    }

    function test_reentrancy_fallbackDelegatesToImpl() public {
        // Verify the fallback still delegates correctly during reentrancy
        RecordingImplementation recordingImpl = new RecordingImplementation();
        StateChangeHandlerGasEstimator est = new StateChangeHandlerGasEstimator(address(recordingImpl));

        StateUpdateType[] memory types = new StateUpdateType[](1);
        bytes[] memory args = new bytes[](1);

        types[0] = StateUpdateType.CALL;
        args[0] = abi.encode(
            address(reentrant),
            uint256(0),
            abi.encodeCall(ReentrantCaller.reenter, ())
        );

        est.runStateUpdatesCall(types, args);

        bytes32 val = vm.load(address(est), RECORDING_CALLED_SLOT);
        assertEq(val, bytes32(uint256(1)), "Fallback should have delegatecalled to implementation during reentrant call");
        assertTrue(est.fallbackWasCalled());
    }

    function test_noReentrancy_fallbackNotTriggered() public {
        RecordingImplementation recordingImpl = new RecordingImplementation();
        StateChangeHandlerGasEstimator est = new StateChangeHandlerGasEstimator(address(recordingImpl));

        StateUpdateType[] memory types = new StateUpdateType[](2);
        bytes[] memory args = new bytes[](2);

        types[0] = StateUpdateType.STORE;
        args[0] = abi.encode(bytes32(uint256(42)), bytes32(uint256(1)));

        types[1] = StateUpdateType.CALL;
        args[1] = abi.encode(
            address(nonReentrant),
            uint256(0),
            abi.encodeCall(NonReentrantCallee.doNothing, ())
        );

        est.runStateUpdatesCall(types, args);

        bytes32 val = vm.load(address(est), RECORDING_CALLED_SLOT);
        assertEq(val, bytes32(uint256(0)), "Fallback should not fire for non-reentrant state updates");
        assertFalse(est.fallbackWasCalled());
    }
}

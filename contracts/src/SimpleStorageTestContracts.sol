// SPDX-License-Identifier: UNLICENSED
pragma solidity ^0.8.13;

// The contract behind the Sepolia `SimpleStorage` test transactions the analyzer was first
// checked against; the same source is embedded in `crates/core/src/types.rs`. Each function
// produces one shape of state update:
// ---
// set(x)                  1 SSTORE + 1 LOG1          -> 2 state updates
// deposit()               1 SSTORE + 1 LOG2          -> 2 state updates
// callExternalContract()  1 CALL (the callee writes) -> 1 state update
// ---
contract DummyExternal {
    uint256 private value;

    function externalFunction() external returns (uint256) {
        value += 1;
        return value;
    }
}

contract SimpleStorage {
    uint256 private storedData;
    mapping(address => uint256) private balances;
    DummyExternal private externalContract;

    event DataStored(uint256 newValue);
    event BalanceUpdated(address indexed user, uint256 newBalance);

    constructor() {
        externalContract = new DummyExternal();
    }

    function set(uint256 x) public {
        storedData = x;
        emit DataStored(x);
    }

    function get() public view returns (uint256) {
        return storedData;
    }

    function deposit() public payable {
        balances[msg.sender] += msg.value;
        emit BalanceUpdated(msg.sender, balances[msg.sender]);
    }

    function getBalance(address user) public view returns (uint256) {
        return balances[user];
    }

    function callExternalContract() public {
        externalContract.externalFunction();
    }
}

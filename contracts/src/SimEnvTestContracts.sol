// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

interface SimEnvOptsStructs {
    struct SimEnvOpts {
        // TX
        address txOrigin;
        uint256 txGasPrice;
        // BLOCK
        address blockCoinbase;
        uint256 blockNumber;
        uint256 blockTimestamp;
        uint256 blockGasLimit;
        uint256 blockPrevRandao;
    }

    error EnvironmentMismatch(SimEnvOpts expected, SimEnvOpts actual, string explanation);
}


/// @title Simulation Environment Test
/// @author Rubydusa
/// @notice Not covered: Blobs (BLOBHASH, BLOBBASEFEE), Block (BLOCKHASH). Possibly more
contract SimEnvOptsTestMain is SimEnvOptsStructs {
    SimEnvOptsCallee simEnvOptsCallee;
    
    constructor (
        address txOrigin,
        uint256 txGasPrice,
        address blockCoinbase,
        uint256 blockNumber,
        uint256 blockTimestamp,
        uint256 blockGasLimit,
        uint256 blockPrevRandao
    ) {
        simEnvOptsCallee = new SimEnvOptsCallee(
            txOrigin,
            txGasPrice,
            blockCoinbase,
            blockNumber,
            blockTimestamp,
            blockGasLimit,
            blockPrevRandao
        );
    }

    function call() external {
        simEnvOptsCallee.test();
    }
}

contract SimEnvOptsCallee is SimEnvOptsStructs {
    SimEnvOpts simEnv;
    // never read, exists so test will be state changing function;
    uint256 nonce;

    constructor (
        address txOrigin,
        uint256 txGasPrice,
        address blockCoinbase,
        uint256 blockNumber,
        uint256 blockTimestamp,
        uint256 blockGasLimit,
        uint256 blockPrevRandao
    ) {
        simEnv.txOrigin = txOrigin;
        simEnv.txGasPrice = txGasPrice;
        simEnv.blockCoinbase = blockCoinbase;
        simEnv.blockNumber = blockNumber;
        simEnv.blockTimestamp = blockTimestamp;
        simEnv.blockGasLimit = blockGasLimit;
        simEnv.blockPrevRandao = blockPrevRandao;
    }

    function test() external {
        SimEnvOpts memory expected = simEnv;
        SimEnvOpts memory actual;
        actual.txOrigin = tx.origin;
        actual.txGasPrice = tx.gasprice;
        actual.blockCoinbase = block.coinbase;
        actual.blockNumber = block.number;
        actual.blockTimestamp = block.timestamp;
        actual.blockGasLimit = block.gaslimit;
        actual.blockPrevRandao = block.prevrandao;

        bool correct = true;
        string memory reason = "Mismatched fields: ";
        if (actual.txOrigin != expected.txOrigin) {
            correct = false;
            reason = string.concat(reason, "txOrigin, ");
        }
        if (actual.txGasPrice != expected.txGasPrice) {
            correct = false;
            reason = string.concat(reason, "txGasPrice, ");
        }
        if (actual.blockCoinbase != expected.blockCoinbase) {
            correct = false;
            reason = string.concat(reason, "blockCoinbase, ");
        }
        if (actual.blockNumber != expected.blockNumber) {
            correct = false;
            reason = string.concat(reason, "blockNumber, ");
        }
        if (actual.blockTimestamp != expected.blockTimestamp) {
            correct = false;
            reason = string.concat(reason, "blockTimestamp, ");
        }
        if (actual.blockGasLimit != expected.blockGasLimit) {
            correct = false;
            reason = string.concat(reason, "blockGasLimit, ");
        }
        if (actual.blockPrevRandao != expected.blockPrevRandao) {
            correct = false;
            reason = string.concat(reason, "blockPrevRandao, ");
        }

        if (!correct) {
            revert EnvironmentMismatch(expected, actual, reason);
        }
        nonce++;
    }
}

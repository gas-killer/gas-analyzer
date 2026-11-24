//! Core types for state updates and ABI definitions.
//!
//! This module contains the shared type definitions used by both
//! Anvil and EvmSketch implementations.

use alloy_sol_types::sol;

sol! {
    enum StateUpdateType {
        STORE,
        CALL,
        LOG0,
        LOG1,
        LOG2,
        LOG3,
        LOG4,
        CREATE,
        CREATE2,
        NESTED,
        STATICCALL_CHECK,
        BALANCE_CHECK,
        CODEHASH_CHECK
    }

    #[derive(Debug)]
    interface IStateUpdateTypes {
        struct Store {
            bytes32 slot;
            bytes32 value;
        }

        /// solidity-sdk v0.2.0's `CALL`. A v0.1.0 program carries only the first three fields.
        struct Call {
            address target;
            uint256 value;
            bytes callargs;
            /// `keccak256(abi.encodePacked(success, returndata))` the trace observed, or zero
            /// for unchecked.
            bytes32 resultHash;
            /// The gas the callee gets; `type(uint256).max` forwards everything left.
            uint256 gasLimit;
        }

        struct Log0 {
            bytes data;
        }

        struct Log1 {
            bytes data;
            bytes32 topic1;
        }

        struct Log2 {
            bytes data;
            bytes32 topic1;
            bytes32 topic2;
        }

        struct Log3 {
            bytes data;
            bytes32 topic1;
            bytes32 topic2;
            bytes32 topic3;
        }

        struct Log4 {
            bytes data;
            bytes32 topic1;
            bytes32 topic2;
            bytes32 topic3;
            bytes32 topic4;
        }

        /// solidity-sdk v0.2.0's `CREATE`. A v0.1.0 program carries only the first two fields.
        struct Create {
            uint256 value;
            bytes initcode;
            /// The address the trace deployed to, or zero for unchecked.
            address expected;
        }

        struct Create2 {
            bytes32 salt;
            uint256 value;
            bytes initcode;
        }

        struct Nested {
            address target;
            uint256 value;
            bytes32 childLeaf;
        }

        /// Re-run a read and check its result hash (v0.2.0 only).
        struct StaticCallCheck {
            address target;
            bytes data;
            bytes32 resultHash;
        }

        /// Check an account's balance (v0.2.0 only).
        struct BalanceCheck {
            address account;
            uint256 balance;
        }

        /// Check an account's code hash; zero for an account without code (v0.2.0 only).
        struct CodehashCheck {
            address account;
            bytes32 codehash;
        }
    }

    /// The v0.1.0 layouts of the ops v0.2.0 extended, for encoding a v0.1.0 program.
    interface IStateUpdateTypesV0_1 {
        struct Call {
            address target;
            uint256 value;
            bytes callargs;
        }

        struct Create {
            uint256 value;
            bytes initcode;
        }
    }
}

/// `gasLimit` of a `CALL` that forwards all remaining gas, unchecked: the SDK's fallback, and what
/// a `CALL` the trace didn't size carries.
pub const CALL_GAS_UNCAPPED: alloy_primitives::U256 = alloy_primitives::U256::MAX;

impl IStateUpdateTypes::Call {
    /// A `CALL` with no pinned result and every remaining gas forwarded.
    pub fn unpinned(
        target: alloy_primitives::Address,
        value: alloy_primitives::U256,
        callargs: alloy_primitives::Bytes,
    ) -> Self {
        Self {
            target,
            value,
            callargs,
            resultHash: alloy_primitives::B256::ZERO,
            gasLimit: CALL_GAS_UNCAPPED,
        }
    }
}

impl IStateUpdateTypes::Create {
    /// A `CREATE` with no pinned address.
    pub fn unpinned(value: alloy_primitives::U256, initcode: alloy_primitives::Bytes) -> Self {
        Self {
            value,
            initcode,
            expected: alloy_primitives::Address::ZERO,
        }
    }
}

sol! {
    pragma solidity >=0.8.25;

    contract DummyExternal {
        uint256 private value;

        function externalFunction() external returns (uint256) {
            value += 1;
            return value;
        }
    }

    // ./artifacts/SimpleStorage.json
    #[sol(bytecode="6080604052348015600e575f5ffd5b506040516019906075565b604051809103905ff0801580156031573d5f5f3e3d5ffd5b5060025f6101000a81548173ffffffffffffffffffffffffffffffffffffffff021916908373ffffffffffffffffffffffffffffffffffffffff1602179055506082565b61013a8061056e83390190565b6104df8061008f5f395ff3fe608060405260043610610049575f3560e01c806360fe47b11461004d5780636d4ce63c14610075578063c7117edf1461009f578063d0e30db0146100b5578063f8b2cb4f146100bf575b5f5ffd5b348015610058575f5ffd5b50610073600480360381019061006e9190610332565b6100fb565b005b348015610080575f5ffd5b5061008961013b565b604051610096919061036c565b60405180910390f35b3480156100aa575f5ffd5b506100b3610143565b005b6100bd6101d5565b005b3480156100ca575f5ffd5b506100e560048036038101906100e091906103df565b6102b5565b6040516100f2919061036c565b60405180910390f35b805f819055507f9455957c3b77d1d4ed071e2b469dd77e37fc5dfd3b4d44dc8a997cc97c7b3d4981604051610130919061036c565b60405180910390a150565b5f5f54905090565b60025f9054906101000a900473ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff16633a32b5496040518163ffffffff1660e01b81526004016020604051808303815f875af11580156101ae573d5f5f3e3d5ffd5b505050506040513d601f19601f820116820180604052508101906101d2919061041e565b50565b3460015f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f205f8282546102219190610476565b925050819055503373ffffffffffffffffffffffffffffffffffffffff167f8ad64a0ac7700dd8425ab0499f107cb6e2cd1581d803c5b8c1c79dcb8190b1af60015f3373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f20546040516102ab919061036c565b60405180910390a2565b5f60015f8373ffffffffffffffffffffffffffffffffffffffff1673ffffffffffffffffffffffffffffffffffffffff1681526020019081526020015f20549050919050565b5f5ffd5b5f819050919050565b610311816102ff565b811461031b575f5ffd5b50565b5f8135905061032c81610308565b92915050565b5f60208284031215610347576103466102fb565b5b5f6103548482850161031e565b91505092915050565b610366816102ff565b82525050565b5f60208201905061037f5f83018461035d565b92915050565b5f73ffffffffffffffffffffffffffffffffffffffff82169050919050565b5f6103ae82610385565b9050919050565b6103be816103a4565b81146103c8575f5ffd5b50565b5f813590506103d9816103b5565b92915050565b5f602082840312156103f4576103f36102fb565b5b5f610401848285016103cb565b91505092915050565b5f8151905061041881610308565b92915050565b5f60208284031215610433576104326102fb565b5b5f6104408482850161040a565b91505092915050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f610480826102ff565b915061048b836102ff565b92508282019050808211156104a3576104a2610449565b5b9291505056fea26469706673582212207868e7ccfa90aa85c664d1882762e0593a03e6b149dc27d3e930577a35d522f464736f6c634300081b00336080604052348015600e575f5ffd5b5061011e8061001c5f395ff3fe6080604052348015600e575f5ffd5b50600436106026575f3560e01c80633a32b54914602a575b5f5ffd5b60306044565b604051603b91906078565b60405180910390f35b5f60015f5f8282546054919060bc565b925050819055505f54905090565b5f819050919050565b6072816062565b82525050565b5f60208201905060895f830184606b565b92915050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f60c4826062565b915060cd836062565b925082820190508082111560e25760e1608f565b5b9291505056fea264697066735822122014d66f5c68ea09cfe70be49436e899010630bdeda8b8b4308df33cee6520376a64736f6c634300081b0033")]
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

    // only used for encoding
    struct StateUpdates {
        uint8[] types;
        bytes[] data;
    }

    error RevertingContext(uint256 index, address target, bytes revertData, bytes callargs);
}

/// Opcode type alias
pub type Opcode = String;

/// Enum representing different types of state updates
#[allow(warnings)]
#[derive(Debug, Clone)]
pub enum StateUpdate {
    Store(IStateUpdateTypes::Store),
    Call(IStateUpdateTypes::Call),
    Log0(IStateUpdateTypes::Log0),
    Log1(IStateUpdateTypes::Log1),
    Log2(IStateUpdateTypes::Log2),
    Log3(IStateUpdateTypes::Log3),
    Log4(IStateUpdateTypes::Log4),
    Create(IStateUpdateTypes::Create),
    Create2(IStateUpdateTypes::Create2),
    /// Hand an SDK-enabled callee its own frame of a nested tree. Extraction leaves
    /// `childLeaf` zeroed; it is filled in when the frame tree is encoded, because a leaf
    /// hashes its frame's encoded program.
    Nested(IStateUpdateTypes::Nested),
    /// The read checks solidity-sdk v0.2.0 added. A v0.1.0 program has no way to carry them, so
    /// encoding one for v0.1.0 leaves them out.
    StaticCallCheck(IStateUpdateTypes::StaticCallCheck),
    BalanceCheck(IStateUpdateTypes::BalanceCheck),
    CodehashCheck(IStateUpdateTypes::CodehashCheck),
}

impl StateUpdate {
    /// Whether this op only checks an observation and changes no state.
    pub fn is_check(&self) -> bool {
        matches!(
            self,
            Self::StaticCallCheck(_) | Self::BalanceCheck(_) | Self::CodehashCheck(_)
        )
    }
}

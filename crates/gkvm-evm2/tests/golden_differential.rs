//! Differential check of the evm2 port against the golden vectors the revm-31 provider is
//! held to (`crates/evmsketch/tests/fixtures/gkvm/*_vectors.json`, recorded from direct
//! `gk-run`): same returndata, same typed reverts, same gas charged per guest cycle.

use alloy_primitives::{Address, B256, Bytes, hex, keccak256};
use alloy_sol_types::SolError;
use evm2::{
    BaseEvmTypes, Evm, Precompiles, SpecId,
    bytecode::Bytecode,
    env::{BlockEnvExt, TxEnvExt},
    evm::InMemoryDB,
    interpreter::{Host, InstrStop, MessageExt},
    registry::TxRegistry,
};
use gas_analyzer_core::gkvm::{GKVM_ADDRESS, GKVM_OK_TAG, errors};
use gas_analyzer_gkvm::{GuestProgramSet, LoadedGuestProgram};
use gkvm_evm2::GkvmPrecompiles;

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../evmsketch/tests/fixtures/gkvm"
);
const GUESTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../gkvm/tests/fixtures");
const GAS_LIMIT: u64 = 50_000_000;

/// calldata → STATICCALL(gas, gkvm, calldata) → bubble returndata (RETURN on success,
/// REVERT otherwise).
fn forwarder(static_call: bool) -> Bytecode {
    let mut code = vec![0x36, 0x5f, 0x5f, 0x37]; // CALLDATACOPY(0, 0, size)
    code.extend([0x5f, 0x5f, 0x36, 0x5f]); // out 0/0, in size/0
    if !static_call {
        code.push(0x5f); // value 0
    }
    code.push(0x73);
    code.extend(GKVM_ADDRESS.as_slice());
    code.extend([0x5a, if static_call { 0xfa } else { 0xf1 }]); // GAS, STATICCALL | CALL
    code.extend([0x3d, 0x5f, 0x5f, 0x3e]); // RETURNDATACOPY(0, 0, size)
    code.extend([0x3d, 0x5f, 0x82]); // size, 0, DUP3 (success)
    let dest = code.len() as u8 + 4;
    code.extend([0x60, dest, 0x57, 0xfd, 0x5b, 0xf3]); // JUMPI → REVERT | JUMPDEST RETURN
    Bytecode::new_legacy(code.into())
}

struct Vectors {
    program_hash: B256,
    rows: Vec<(Bytes, String, Bytes, u64)>, // input, outcome, stdout, gasUsed
}

fn vectors(name: &str) -> Vectors {
    let raw = std::fs::read_to_string(format!("{FIXTURES}/{name}_vectors.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let text = |value: &serde_json::Value| value.as_str().unwrap().to_string();
    let bytes = |value: &serde_json::Value| Bytes::from(hex::decode(text(value)).unwrap());
    let rows = doc["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            (
                bytes(&row["input"]),
                text(&row["outcome"]),
                bytes(&row["stdout"]),
                row["gasUsed"].as_u64().unwrap(),
            )
        })
        .collect();
    Vectors {
        program_hash: text(&doc["programHash"]).parse().unwrap(),
        rows,
    }
}

fn evm() -> Evm<'static, BaseEvmTypes> {
    let mut programs = GuestProgramSet::default();
    for guest in ["hello-c", "bench-c"] {
        let path = format!("{GUESTS}/{guest}.elf");
        let elf = std::fs::read(&path).unwrap();
        let hash = keccak256(&elf);
        programs.insert_program(LoadedGuestProgram::from_bytes(elf, hash, &path).unwrap());
    }
    Evm::new(
        SpecId::OSAKA,
        BlockEnvExt::default(),
        TxRegistry::new(),
        InMemoryDB::default(),
        GkvmPrecompiles::new(Precompiles::base(SpecId::OSAKA), programs),
    )
}

/// Runs the forwarder; returns (stop, returndata, gas spent by the whole frame).
fn call(
    evm: &mut Evm<'static, BaseEvmTypes>,
    static_call: bool,
    wire: Vec<u8>,
) -> (InstrStop, Bytes, u64) {
    let destination = Address::from([0xc0; 20]);
    let mut message = MessageExt {
        destination,
        code_address: destination,
        gas_limit: GAS_LIMIT,
        code: forwarder(static_call),
        input: wire.into(),
        ..Default::default()
    };
    let result = Host::execute_message(evm, &TxEnvExt::default(), &mut message);
    (result.stop, result.output, result.gas.spent())
}

fn wire(program_hash: B256, payload: &[u8]) -> Vec<u8> {
    [program_hash.as_slice(), B256::ZERO.as_slice(), payload].concat()
}

#[test]
fn ok_and_trap_vectors_return_the_golden_bytes() {
    let mut evm = evm();
    let mut checked = 0;
    for name in ["hello-c", "bench-c"] {
        let set = vectors(name);
        for (input, outcome, stdout, _) in &set.rows {
            let (stop, output, _) = call(&mut evm, true, wire(set.program_hash, input));
            match outcome.as_str() {
                "ok" => {
                    assert_eq!(stop, InstrStop::Return, "{name} {input}");
                    assert_eq!(output[0], GKVM_OK_TAG);
                    assert_eq!(&output[1..], &stdout[..], "{name} {input}");
                }
                "trap" => {
                    assert_eq!(stop, InstrStop::Revert, "{name} {input}");
                    let code = u32::from_be_bytes(stdout[..4].try_into().unwrap());
                    let expected = errors::GkGuestTrap {
                        code,
                        data: stdout[4..].to_vec().into(),
                    };
                    assert_eq!(output, Bytes::from(expected.abi_encode()), "{name} {input}");
                }
                other => panic!("unexpected outcome {other} in the unlimited vector sets"),
            }
            checked += 1;
        }
    }
    assert!(checked >= 4, "only {checked} vectors exercised");
}

/// Two `ok` bench vectors differ only in guest cycles (same payload length, same forwarder
/// path), so the frames' gas differs by exactly the golden `gasUsed` delta.
#[test]
fn guest_cycles_are_charged_at_the_golden_rate() {
    let set = vectors("bench-c");
    let ok: Vec<_> = set.rows.iter().filter(|row| row.1 == "ok").collect();
    let (small, large) = (ok.first().unwrap(), ok.last().unwrap());
    assert_ne!(
        small.3, large.3,
        "need two bench vectors with different cycle counts"
    );
    // A fresh EVM per call: `execute_message` runs below the transaction layer, so nothing
    // pre-warms the precompile address and a reused journal would make the second access
    // 2,500 gas cheaper.
    let (_, _, gas_small) = call(&mut evm(), true, wire(set.program_hash, &small.0));
    let (_, _, gas_large) = call(&mut evm(), true, wire(set.program_hash, &large.0));
    assert_eq!(gas_large - gas_small, large.3 - small.3);
}

#[test]
fn the_protocol_failures_match_the_revm_provider() {
    let set = vectors("hello-c");
    let mut evm = evm();

    // non-static frame: typed revert, guest never runs
    let (stop, output, _) = call(&mut evm, false, wire(set.program_hash, &[]));
    assert_eq!(stop, InstrStop::Revert);
    assert_eq!(output, Bytes::from(errors::GkVmStaticOnly {}.abi_encode()));

    // wire shorter than the header: empty revert
    let (stop, output, _) = call(&mut evm, true, vec![0u8; 63]);
    assert_eq!(stop, InstrStop::Revert);
    assert!(output.is_empty());

    let runs = evm
        .precompiles_as::<GkvmPrecompiles<Precompiles>>()
        .unwrap()
        .guest_runs();
    assert_eq!(runs, 0, "neither failure may reach the guest");

    // unknown program: an environment failure — fatal, never an EVM revert the operator signs
    let (stop, _, _) = call(&mut evm, true, wire(B256::repeat_byte(0xee), &[]));
    assert!(
        !stop.is_success() && stop != InstrStop::Revert,
        "fatal, got {stop:?}"
    );
}

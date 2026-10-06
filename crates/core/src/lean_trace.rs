//! Struct-log trace parsing that keeps only what state-update extraction reads.
//!
//! Memory snapshots are most of a struct-log trace, yet only a few opcodes read them (see
//! [`reads_memory`]). Parsing into [`LeanFrame`] skips the rest, along with `storage` and
//! `returnData`, so a trace takes a fraction of its size once parsed.

use std::fmt;

use alloy_primitives::Bytes;
use alloy_rpc_types::trace::geth::{DefaultFrame, StructLog};
use serde::Deserialize;
use serde::de::{self, IgnoredAny, MapAccess, Visitor};

use crate::trace::reads_memory;

/// A [`DefaultFrame`] whose steps hold memory only where [`reads_memory`] says it's needed.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeanFrame {
    failed: bool,
    gas: u64,
    return_value: Bytes,
    struct_logs: Vec<LeanStructLog>,
}

impl From<LeanFrame> for DefaultFrame {
    fn from(frame: LeanFrame) -> Self {
        DefaultFrame {
            failed: frame.failed,
            gas: frame.gas,
            return_value: frame.return_value,
            // Same layout, so this collects in place.
            struct_logs: frame.struct_logs.into_iter().map(|log| log.0).collect(),
        }
    }
}

/// Parse a `debug_traceTransaction` struct-log result, skipping memory no extractor reads.
pub fn parse_lean_frame(json: &[u8]) -> serde_json::Result<DefaultFrame> {
    serde_json::from_slice::<LeanFrame>(json).map(Into::into)
}

#[repr(transparent)]
struct LeanStructLog(StructLog);

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "camelCase")]
enum Field {
    Pc,
    Op,
    Gas,
    GasCost,
    Depth,
    Error,
    Stack,
    Memory,
    MemSize,
    Refund,
    #[serde(other)]
    Other,
}

impl<'de> Deserialize<'de> for LeanStructLog {
    fn deserialize<D: de::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(LeanStructLogVisitor)
    }
}

struct LeanStructLogVisitor;

impl<'de> Visitor<'de> for LeanStructLogVisitor {
    type Value = LeanStructLog;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a struct log")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut log = StructLog::default();
        let (mut pc, mut op, mut gas, mut gas_cost, mut depth) = (None, None, None, None, None);
        while let Some(field) = map.next_key()? {
            match field {
                Field::Pc => pc = Some(map.next_value()?),
                Field::Op => op = Some(map.next_value::<String>()?),
                Field::Gas => gas = Some(map.next_value()?),
                Field::GasCost => gas_cost = Some(map.next_value()?),
                Field::Depth => depth = Some(map.next_value()?),
                Field::Error => log.error = map.next_value()?,
                Field::Stack => log.stack = map.next_value()?,
                // Kept as an empty snapshot rather than None: extraction skips steps without
                // memory entirely. Geth sends `op` first; if a client doesn't, it's kept and
                // dropped below.
                Field::Memory => match &op {
                    Some(op) if !reads_memory(op) => {
                        log.memory = map.next_value::<Option<IgnoredAny>>()?.map(|_| Vec::new())
                    }
                    _ => log.memory = map.next_value()?,
                },
                Field::MemSize => log.memory_size = map.next_value()?,
                Field::Refund => log.refund_counter = map.next_value()?,
                Field::Other => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        let op = op.ok_or_else(|| de::Error::missing_field("op"))?;
        if !reads_memory(&op) && log.memory.is_some() {
            log.memory = Some(Vec::new());
        }
        log.op = op.into();
        log.pc = pc.ok_or_else(|| de::Error::missing_field("pc"))?;
        log.gas = gas.ok_or_else(|| de::Error::missing_field("gas"))?;
        log.gas_cost = gas_cost.ok_or_else(|| de::Error::missing_field("gasCost"))?;
        log.depth = depth.ok_or_else(|| de::Error::missing_field("depth"))?;
        Ok(LeanStructLog(log))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compute_state_updates, encode_state_updates_to_abi};

    const WORD: &str = "00000000000000000000000000000000000000000000000000000000deadbeef";

    fn step(op: &str, stack: &[&str], extra: serde_json::Value) -> serde_json::Value {
        let mut step = serde_json::json!({
            "pc": 7, "op": op, "gas": 90000, "gasCost": 3, "depth": 1, "stack": stack,
            "memory": [WORD, WORD], "storage": {}, "returnData": "0x01",
        });
        step.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        step
    }

    fn frame(steps: Vec<serde_json::Value>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "failed": false, "gas": 21000, "returnValue": "0x", "structLogs": steps,
        }))
        .unwrap()
    }

    #[test]
    fn keeps_memory_only_where_extraction_reads_it() {
        let json = frame(vec![
            step("MSTORE", &["0x0", "0x1"], serde_json::json!({})),
            step("LOG0", &["0x20", "0x0"], serde_json::json!({})),
            step(
                "SSTORE",
                &["0x2", "0x1"],
                serde_json::json!({ "refund": 5 }),
            ),
        ]);
        let lean = parse_lean_frame(&json).unwrap();
        let full: DefaultFrame = serde_json::from_slice(&json).unwrap();

        let memories: Vec<_> = lean.struct_logs.iter().map(|l| l.memory.clone()).collect();
        assert_eq!(memories[0], Some(vec![]));
        assert_eq!(memories[1], full.struct_logs[1].memory);
        assert_eq!(memories[2], Some(vec![]));

        for (lean, full) in lean.struct_logs.iter().zip(&full.struct_logs) {
            assert_eq!(lean.storage, None);
            assert_eq!(lean.return_data, None);
            let blank = |l: &StructLog| StructLog {
                memory: None,
                storage: None,
                return_data: None,
                ..l.clone()
            };
            assert_eq!(blank(lean), blank(full));
        }
    }

    #[test]
    fn memory_before_op_is_still_dropped_for_other_opcodes() {
        let json = br#"{"failed":false,"gas":1,"returnValue":"0x","structLogs":[
            {"memory":["00"],"pc":0,"op":"SSTORE","gas":1,"gasCost":1,"depth":1,"stack":["0x1","0x2"]},
            {"memory":["00"],"pc":1,"op":"CALL","gas":1,"gasCost":1,"depth":1,"stack":[]}
        ]}"#;
        let lean = parse_lean_frame(json).unwrap();
        assert_eq!(lean.struct_logs[0].memory, Some(vec![]));
        assert_eq!(lean.struct_logs[1].memory, Some(vec!["00".to_string()]));
    }

    #[test]
    fn missing_or_null_memory_stays_none() {
        let json = br#"{"failed":false,"gas":1,"returnValue":"0x","structLogs":[
            {"pc":0,"op":"SSTORE","gas":1,"gasCost":1,"depth":1,"stack":[],"memory":null},
            {"pc":1,"op":"SSTORE","gas":1,"gasCost":1,"depth":1,"stack":[]}
        ]}"#;
        let lean = parse_lean_frame(json).unwrap();
        assert!(lean.struct_logs.iter().all(|l| l.memory.is_none()));
    }

    #[test]
    fn missing_required_field_is_an_error() {
        let json = br#"{"failed":false,"gas":1,"returnValue":"0x","structLogs":[{"pc":0,"gas":1,"gasCost":1,"depth":1}]}"#;
        assert!(parse_lean_frame(json).is_err());
    }

    /// The state updates are identical however the trace is parsed, including LOG data and CALL
    /// calldata read from memory.
    #[test]
    fn extraction_matches_full_parse() {
        let target = "0x000000000000000000000000000000000000beef";
        let json = frame(vec![
            step("MSTORE", &["0x0", "0x1"], serde_json::json!({})),
            step("SSTORE", &["0x2", "0x1"], serde_json::json!({})),
            step("LOG1", &["0x7", "0x20", "0x4"], serde_json::json!({})),
            step(
                "CALL",
                &["0x0", "0x0", "0x4", "0x1c", "0x0", target, "0xffff"],
                serde_json::json!({}),
            ),
            step("TSTORE", &["0x1", "0x1"], serde_json::json!({})),
        ]);
        let lean = compute_state_updates(parse_lean_frame(&json).unwrap(), None).unwrap();
        let full = compute_state_updates(serde_json::from_slice(&json).unwrap(), None).unwrap();
        assert_eq!(lean.state_updates.len(), 3);
        assert_eq!(
            encode_state_updates_to_abi(&lean.state_updates),
            encode_state_updates_to_abi(&full.state_updates)
        );
        assert_eq!(lean.skipped_opcodes, full.skipped_opcodes);
    }

    /// Set GAS_ANALYZER_STRUCT_LOG_FIXTURE to a struct-log trace (a `result`, not an envelope) to
    /// check a real one, e.g. crates/evmsketch/benches/fixtures/sepolia_trace.json.
    #[test]
    #[ignore]
    fn fixture_extraction_matches_full_parse() {
        // CI runs every ignored test when RPC_URL is set; this one only runs when asked for.
        let Ok(path) = std::env::var("GAS_ANALYZER_STRUCT_LOG_FIXTURE") else {
            eprintln!("GAS_ANALYZER_STRUCT_LOG_FIXTURE not set, skipping");
            return;
        };
        let json = std::fs::read(path).unwrap();
        let lean = compute_state_updates(parse_lean_frame(&json).unwrap(), None).unwrap();
        let full = compute_state_updates(serde_json::from_slice(&json).unwrap(), None).unwrap();
        assert_eq!(
            encode_state_updates_to_abi(&lean.state_updates),
            encode_state_updates_to_abi(&full.state_updates)
        );
        assert_eq!(lean.skipped_opcodes, full.skipped_opcodes);
        assert_eq!(lean.call_gas_total, full.call_gas_total);
        assert_eq!(lean.sstore_gas_total, full.sstore_gas_total);
        assert_eq!(lean.refund_counter, full.refund_counter);
        assert_eq!(lean.reentered, full.reentered);
        println!("{} state updates", lean.state_updates.len());
    }
}

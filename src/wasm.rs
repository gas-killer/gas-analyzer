//! WebAssembly bindings for gas-analyzer-rs
//!
//! This module provides WASM-compatible functions that can be called from JavaScript.
//! Note: Functions that require Anvil (like GasKiller) are not available in WASM.

use alloy::{primitives::Address, rpc::types::trace::geth::DefaultFrame};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use wasm_bindgen::prelude::*;

use crate::sol_types::{IStateUpdateTypes, StateUpdate};
use crate::structs::Opcode;

/// Initialize panic hook for better error messages in console
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
    console_log::init_with_level(log::Level::Info).expect("Failed to initialize logger");
}

/// Compute state updates from a Geth trace (JSON string).
///
/// # Arguments
/// * `trace_json` - JSON string representation of a Geth DefaultFrame trace
///
/// # Returns
/// JSON string with `{ state_updates: [...], skipped_opcodes: [...] }`
#[wasm_bindgen]
pub fn compute_state_updates_from_trace_json(trace_json: &str) -> Result<String, JsValue> {
    let trace: DefaultFrame = serde_json::from_str(trace_json)
        .map_err(|e| JsValue::from_str(&format!("Failed to parse trace JSON: {}", e)))?;

    // Use WASM-compatible implementation that doesn't require opcode-tracer
    let (state_updates, skipped_opcodes) = compute_state_updates_wasm(trace)
        .map_err(|e| JsValue::from_str(&format!("Failed to compute state updates: {}", e)))?;

    let result = StateUpdateResult {
        state_updates: state_updates.iter().map(StateUpdateJson::from).collect(),
        skipped_opcodes: skipped_opcodes.into_iter().collect(),
    };

    serde_json::to_string(&result)
        .map_err(|e| JsValue::from_str(&format!("Failed to serialize result: {}", e)))
}

/// Result structure for state updates computation
#[derive(Serialize)]
struct StateUpdateResult {
    state_updates: Vec<StateUpdateJson>,
    skipped_opcodes: Vec<String>,
}

/// JSON-serializable representation of a state update
#[derive(Serialize)]
#[serde(tag = "type")]
enum StateUpdateJson {
    Store {
        slot: String,
        value: String,
    },
    Call {
        target: String,
        value: String,
        callargs: String,
    },
    Log0 {
        data: String,
    },
    Log1 {
        data: String,
        topic1: String,
    },
    Log2 {
        data: String,
        topic1: String,
        topic2: String,
    },
    Log3 {
        data: String,
        topic1: String,
        topic2: String,
        topic3: String,
    },
    Log4 {
        data: String,
        topic1: String,
        topic2: String,
        topic3: String,
        topic4: String,
    },
}

impl From<&StateUpdate> for StateUpdateJson {
    fn from(update: &StateUpdate) -> Self {
        match update {
            StateUpdate::Store(s) => StateUpdateJson::Store {
                slot: format!("0x{}", hex::encode(s.slot.as_slice())),
                value: format!("0x{}", hex::encode(s.value.as_slice())),
            },
            StateUpdate::Call(c) => StateUpdateJson::Call {
                target: format!("{:?}", c.target),
                value: c.value.to_string(),
                callargs: format!("0x{}", hex::encode(c.callargs.as_ref())),
            },
            StateUpdate::Log0(l) => StateUpdateJson::Log0 {
                data: format!("0x{}", hex::encode(l.data.as_ref())),
            },
            StateUpdate::Log1(l) => StateUpdateJson::Log1 {
                data: format!("0x{}", hex::encode(l.data.as_ref())),
                topic1: format!("0x{}", hex::encode(l.topic1.as_slice())),
            },
            StateUpdate::Log2(l) => StateUpdateJson::Log2 {
                data: format!("0x{}", hex::encode(l.data.as_ref())),
                topic1: format!("0x{}", hex::encode(l.topic1.as_slice())),
                topic2: format!("0x{}", hex::encode(l.topic2.as_slice())),
            },
            StateUpdate::Log3(l) => StateUpdateJson::Log3 {
                data: format!("0x{}", hex::encode(l.data.as_ref())),
                topic1: format!("0x{}", hex::encode(l.topic1.as_slice())),
                topic2: format!("0x{}", hex::encode(l.topic2.as_slice())),
                topic3: format!("0x{}", hex::encode(l.topic3.as_slice())),
            },
            StateUpdate::Log4(l) => StateUpdateJson::Log4 {
                data: format!("0x{}", hex::encode(l.data.as_ref())),
                topic1: format!("0x{}", hex::encode(l.topic1.as_slice())),
                topic2: format!("0x{}", hex::encode(l.topic2.as_slice())),
                topic3: format!("0x{}", hex::encode(l.topic3.as_slice())),
                topic4: format!("0x{}", hex::encode(l.topic4.as_slice())),
            },
        }
    }
}

/// Comprehensive analysis result from a trace
#[derive(Serialize, Deserialize)]
pub struct TraceAnalysis {
    /// Total number of state updates
    pub state_update_count: usize,
    /// Breakdown by type
    pub state_update_breakdown: StateUpdateBreakdown,
    /// List of skipped opcodes (CREATE, CREATE2, SELFDESTRUCT)
    pub skipped_opcodes: Vec<String>,
    /// Encoded state updates in ABI format (hex string)
    pub encoded_abi: String,
    /// Estimated gas for state updates (heuristic, not accurate)
    pub estimated_gas: u64,
}

/// Breakdown of state updates by type
#[derive(Serialize, Deserialize)]
pub struct StateUpdateBreakdown {
    pub stores: usize,
    pub calls: usize,
    pub log0: usize,
    pub log1: usize,
    pub log2: usize,
    pub log3: usize,
    pub log4: usize,
}

/// Analyze a Geth trace and return comprehensive information.
///
/// This is the main function for frontend use - it takes a trace JSON
/// and returns all relevant analysis data.
///
/// # Arguments
/// * `trace_json` - JSON string representation of a Geth DefaultFrame trace
///
/// # Returns
/// JSON string with `TraceAnalysis` containing state updates, breakdown, and encoded ABI
#[wasm_bindgen]
pub fn analyze_trace(trace_json: &str) -> Result<String, JsValue> {
    let trace: DefaultFrame = serde_json::from_str(trace_json)
        .map_err(|e| JsValue::from_str(&format!("Failed to parse trace JSON: {}", e)))?;

    // Use WASM-compatible implementation that doesn't require opcode-tracer
    let (state_updates, skipped_opcodes) = compute_state_updates_wasm(trace)
        .map_err(|e| JsValue::from_str(&format!("Failed to compute state updates: {}", e)))?;

    // Count state updates by type
    let mut breakdown = StateUpdateBreakdown {
        stores: 0,
        calls: 0,
        log0: 0,
        log1: 0,
        log2: 0,
        log3: 0,
        log4: 0,
    };

    for update in &state_updates {
        match update {
            StateUpdate::Store(_) => breakdown.stores += 1,
            StateUpdate::Call(_) => breakdown.calls += 1,
            StateUpdate::Log0(_) => breakdown.log0 += 1,
            StateUpdate::Log1(_) => breakdown.log1 += 1,
            StateUpdate::Log2(_) => breakdown.log2 += 1,
            StateUpdate::Log3(_) => breakdown.log3 += 1,
            StateUpdate::Log4(_) => breakdown.log4 += 1,
        }
    }

    // Encode to ABI
    let encoded = crate::encode_state_updates_to_abi(&state_updates);
    let encoded_abi = format!("0x{}", hex::encode(encoded.as_ref()));

    // Heuristic gas estimation (rough approximation)
    // These are base costs - actual costs depend on storage state, call complexity, etc.
    let estimated_gas = estimate_gas_heuristic(&state_updates);

    let analysis = TraceAnalysis {
        state_update_count: state_updates.len(),
        state_update_breakdown: breakdown,
        skipped_opcodes: skipped_opcodes.into_iter().collect(),
        encoded_abi,
        estimated_gas,
    };

    serde_json::to_string(&analysis)
        .map_err(|e| JsValue::from_str(&format!("Failed to serialize analysis: {}", e)))
}

/// Heuristic gas estimation based on state update types.
/// This is a rough approximation - actual gas costs depend on many factors.
fn estimate_gas_heuristic(state_updates: &[StateUpdate]) -> u64 {
    const TURETZKY_UPPER_GAS_LIMIT: u64 = 250000;
    let mut gas = TURETZKY_UPPER_GAS_LIMIT;

    for update in state_updates {
        match update {
            StateUpdate::Store(_) => {
                // SSTORE: 20,000 for new storage, 2,900 for existing, 5,000 for zeroing
                // We'll use an average of ~10,000
                gas += 10_000;
            }
            StateUpdate::Call(c) => {
                // CALL: base 700, plus calldata (16 per non-zero byte, 4 per zero byte)
                let calldata_gas = c.callargs.len() as u64 * 4; // Rough estimate
                gas += 700 + calldata_gas;
            }
            StateUpdate::Log0(_) => gas += 375,  // LOG0 base cost
            StateUpdate::Log1(_) => gas += 750,  // LOG1 base cost
            StateUpdate::Log2(_) => gas += 1125, // LOG2 base cost
            StateUpdate::Log3(_) => gas += 1500, // LOG3 base cost
            StateUpdate::Log4(_) => gas += 1875, // LOG4 base cost
        }
    }

    gas
}

/// Encode state updates to ABI format.
/// This function is deprecated - use `analyze_trace` which includes encoded_abi in the result.
///
/// # Arguments
/// * `trace_json` - JSON string representation of a Geth DefaultFrame trace
///
/// # Returns
/// Hex string of encoded bytes (0x-prefixed)
#[wasm_bindgen]
pub fn encode_state_updates_to_abi_from_trace(trace_json: &str) -> Result<String, JsValue> {
    let trace: DefaultFrame = serde_json::from_str(trace_json)
        .map_err(|e| JsValue::from_str(&format!("Failed to parse trace JSON: {}", e)))?;

    let (state_updates, _) = compute_state_updates_wasm(trace)
        .map_err(|e| JsValue::from_str(&format!("Failed to compute state updates: {}", e)))?;

    let encoded = crate::encode_state_updates_to_abi(&state_updates);
    Ok(format!("0x{}", hex::encode(encoded.as_ref())))
}

/// Parse trace memory from hex string array.
///
/// # Arguments
/// * `memory` - Array of hex strings (e.g., ["00", "01", "02"])
///
/// # Returns
/// Hex string of parsed memory (0x-prefixed)
#[wasm_bindgen]
pub fn parse_trace_memory(memory: Vec<String>) -> String {
    let parsed = crate::parse_trace_memory(memory);
    format!("0x{}", hex::encode(&parsed))
}

/// WASM-compatible implementation of compute_state_updates
/// This reimplements the logic without requiring opcode-tracer
pub(crate) fn compute_state_updates_wasm(
    trace: DefaultFrame,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>), anyhow::Error> {
    let mut state_updates: Vec<StateUpdate> = Vec::new();
    let mut target_depth: usize = 1;
    let mut skipped_opcodes = HashSet::new();

    for struct_log in trace.struct_logs {
        let depth = struct_log.depth as usize;

        // Whenever stepping up (leaving a CALL/CALLCODE/DELEGATECALL) reset the target depth
        if depth < target_depth {
            target_depth = depth;
        } else if depth == target_depth {
            // If we're going to step into a new execution context, increase the target depth
            if &*struct_log.op == "DELEGATECALL" || &*struct_log.op == "CALLCODE" {
                target_depth = depth + 1;
            } else if let Some(opcode) =
                append_to_state_updates_wasm(&mut state_updates, &struct_log)?
            {
                skipped_opcodes.insert(opcode);
            }
        }
    }

    Ok((state_updates, skipped_opcodes))
}

/// WASM-compatible helper to append state updates from a struct log
fn append_to_state_updates_wasm(
    state_updates: &mut Vec<StateUpdate>,
    struct_log: &alloy::rpc::types::trace::geth::StructLog,
) -> Result<Option<Opcode>, anyhow::Error> {
    use anyhow::bail;

    let mut stack = struct_log
        .stack
        .clone()
        .ok_or_else(|| anyhow::anyhow!("stack is empty"))?;
    stack.reverse();

    let memory = match &struct_log.memory {
        Some(memory) => crate::parse_trace_memory(memory.clone()),
        None => match struct_log.op.as_ref() {
            "CALL" | "LOG0" | "LOG1" | "LOG2" | "LOG3" | "LOG4" if struct_log.depth == 1 => {
                bail!("There is no memory for {:?} in depth 1", struct_log.op)
            }
            _ => return Ok(None),
        },
    };

    match struct_log.op.as_ref() {
        "CREATE" | "CREATE2" | "SELFDESTRUCT" => {
            return Ok(Some(struct_log.op.to_string()));
        }
        "DELEGATECALL" | "CALLCODE" => {
            bail!(
                "Calling opcode {:?}, this shouldn't even happen!",
                struct_log.op
            );
        }
        "SSTORE" => {
            if stack.len() >= 2 {
                state_updates.push(StateUpdate::Store(IStateUpdateTypes::Store {
                    slot: stack[0].into(),
                    value: stack[1].into(),
                }));
            }
        }
        "CALL" => {
            if stack.len() >= 5 && struct_log.depth == 1 {
                let args_offset: usize = stack[3].try_into().unwrap_or(0);
                let args_length: usize = stack[4].try_into().unwrap_or(0);
                let args = copy_memory_wasm(&memory, args_offset, args_length);
                state_updates.push(StateUpdate::Call(IStateUpdateTypes::Call {
                    target: Address::from_word(stack[1].into()),
                    value: stack[2],
                    callargs: args.into(),
                }));
            }
        }
        "LOG0" => {
            if stack.len() >= 2 && struct_log.depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory_wasm(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log0(IStateUpdateTypes::Log0 {
                    data: data.into(),
                }));
            }
        }
        "LOG1" => {
            if stack.len() >= 3 && struct_log.depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory_wasm(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log1(IStateUpdateTypes::Log1 {
                    data: data.into(),
                    topic1: stack[2].into(),
                }));
            }
        }
        "LOG2" => {
            if stack.len() >= 4 && struct_log.depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory_wasm(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log2(IStateUpdateTypes::Log2 {
                    data: data.into(),
                    topic1: stack[2].into(),
                    topic2: stack[3].into(),
                }));
            }
        }
        "LOG3" => {
            if stack.len() >= 5 && struct_log.depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory_wasm(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log3(IStateUpdateTypes::Log3 {
                    data: data.into(),
                    topic1: stack[2].into(),
                    topic2: stack[3].into(),
                    topic3: stack[4].into(),
                }));
            }
        }
        "LOG4" => {
            if stack.len() >= 6 && struct_log.depth == 1 {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = copy_memory_wasm(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log4(IStateUpdateTypes::Log4 {
                    data: data.into(),
                    topic1: stack[2].into(),
                    topic2: stack[3].into(),
                    topic3: stack[4].into(),
                    topic4: stack[5].into(),
                }));
            }
        }
        _ => {}
    }
    Ok(None)
}

/// Copy memory with bounds checking (WASM helper)
fn copy_memory_wasm(memory: &[u8], offset: usize, length: usize) -> Vec<u8> {
    if memory.len() >= offset + length {
        memory[offset..offset + length].to_vec()
    } else {
        let mut result = memory.to_vec();
        result.resize(offset + length, 0);
        result[offset..offset + length].to_vec()
    }
}

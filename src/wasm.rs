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
    /// Gas estimates for different scenarios
    pub gas_estimates: GasEstimates,
    /// Details about external CALLs (gas consumed by each)
    pub call_details: Vec<CallDetail>,
}

/// Gas estimates for best/average/worst case scenarios
#[derive(Serialize, Deserialize)]
pub struct GasEstimates {
    /// Best case: warm storage slots, no cold access (SSTORE = 2,900)
    pub best_case: GasBreakdown,
    /// Average case: mix of warm/cold storage (SSTORE = 5,000)
    pub average_case: GasBreakdown,
    /// Worst case: all cold storage slots (SSTORE = 20,000)
    pub worst_case: GasBreakdown,
    /// Gas for external CALLs (same across all scenarios - cannot be optimized)
    pub call_gas: u64,
}

/// Breakdown of gas costs
#[derive(Serialize, Deserialize)]
pub struct GasBreakdown {
    /// Base overhead (GasKiller transaction cost)
    pub base_overhead: u64,
    /// Gas for SSTOREs
    pub sstore_gas: u64,
    /// Gas for LOGs
    pub log_gas: u64,
    /// Total optimizable gas (sstore + log)
    pub optimizable_gas: u64,
    /// Total estimated gas (base + optimizable + calls)
    pub total_gas: u64,
}

/// Details about an external CALL operation
#[derive(Serialize, Deserialize)]
pub struct CallDetail {
    /// Target address
    pub target: String,
    /// ETH value sent
    pub value: String,
    /// Gas consumed by this call (from trace)
    pub gas_used: u64,
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

    // Use WASM-compatible implementation that tracks both state updates and call details
    let result = compute_state_updates_wasm_with_calls(trace)
        .map_err(|e| JsValue::from_str(&format!("Failed to compute state updates: {}", e)))?;

    let state_updates = result.state_updates;
    let skipped_opcodes = result.skipped_opcodes;
    let call_details = result.call_details;

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

    // Calculate gas estimates for different scenarios
    let gas_estimates = estimate_gas_scenarios(&state_updates, &call_details);

    let analysis = TraceAnalysis {
        state_update_count: state_updates.len(),
        state_update_breakdown: breakdown,
        skipped_opcodes: skipped_opcodes.into_iter().collect(),
        encoded_abi,
        gas_estimates,
        call_details,
    };

    serde_json::to_string(&analysis)
        .map_err(|e| JsValue::from_str(&format!("Failed to serialize analysis: {}", e)))
}

// SSTORE gas costs per EIP-2200 / EIP-3529
const SSTORE_BEST_CASE: u64 = 2_900; // Warm slot, changing non-zero to non-zero (SSTORE_RESET_GAS)
const SSTORE_AVERAGE_CASE: u64 = 5_000; // Mix of warm/cold, average across operations
const SSTORE_WORST_CASE: u64 = 20_000; // Cold slot, setting from zero (SSTORE_SET_GAS)

/// Estimate gas for different scenarios (best/average/worst case)
fn estimate_gas_scenarios(
    state_updates: &[StateUpdate],
    call_details: &[CallDetail],
) -> GasEstimates {
    let base_overhead = crate::constants::TURETZKY_UPPER_GAS_LIMIT;

    // Count stores and calculate LOG gas (LOG gas is the same across all scenarios)
    let mut store_count = 0u64;
    let mut log_gas = 0u64;
    let mut call_base_gas = 0u64;

    for update in state_updates {
        match update {
            StateUpdate::Store(_) => {
                store_count += 1;
            }
            StateUpdate::Call(c) => {
                // CALL: base 2600 (cold account access) + 700 base + calldata
                let calldata_gas = c.callargs.len() as u64 * 4;
                call_base_gas += 2600 + 700 + calldata_gas;
            }
            StateUpdate::Log0(l) => {
                // LOG0: 375 base + 8 per byte of data
                log_gas += 375 + (l.data.len() as u64 * 8);
            }
            StateUpdate::Log1(l) => {
                // LOG1: 375 base + 375 per topic + 8 per byte
                log_gas += 375 + 375 + (l.data.len() as u64 * 8);
            }
            StateUpdate::Log2(l) => {
                // LOG2: 375 base + 375*2 topics + 8 per byte
                log_gas += 375 + 750 + (l.data.len() as u64 * 8);
            }
            StateUpdate::Log3(l) => {
                // LOG3: 375 base + 375*3 topics + 8 per byte
                log_gas += 375 + 1125 + (l.data.len() as u64 * 8);
            }
            StateUpdate::Log4(l) => {
                // LOG4: 375 base + 375*4 topics + 8 per byte
                log_gas += 375 + 1500 + (l.data.len() as u64 * 8);
            }
        }
    }

    // Use actual call gas from trace if available, otherwise use heuristic
    let call_gas: u64 = if !call_details.is_empty() {
        call_details.iter().map(|c| c.gas_used).sum()
    } else {
        call_base_gas
    };

    // Calculate SSTORE gas for each scenario
    let sstore_best = store_count * SSTORE_BEST_CASE;
    let sstore_avg = store_count * SSTORE_AVERAGE_CASE;
    let sstore_worst = store_count * SSTORE_WORST_CASE;

    let best_case = GasBreakdown {
        base_overhead,
        sstore_gas: sstore_best,
        log_gas,
        optimizable_gas: sstore_best + log_gas,
        total_gas: base_overhead + sstore_best + log_gas + call_gas,
    };

    let average_case = GasBreakdown {
        base_overhead,
        sstore_gas: sstore_avg,
        log_gas,
        optimizable_gas: sstore_avg + log_gas,
        total_gas: base_overhead + sstore_avg + log_gas + call_gas,
    };

    let worst_case = GasBreakdown {
        base_overhead,
        sstore_gas: sstore_worst,
        log_gas,
        optimizable_gas: sstore_worst + log_gas,
        total_gas: base_overhead + sstore_worst + log_gas + call_gas,
    };

    GasEstimates {
        best_case,
        average_case,
        worst_case,
        call_gas,
    }
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

/// Result from compute_state_updates_wasm_with_calls
pub(crate) struct ComputeResult {
    pub state_updates: Vec<StateUpdate>,
    pub skipped_opcodes: HashSet<Opcode>,
    pub call_details: Vec<CallDetail>,
}

/// WASM-compatible implementation of compute_state_updates
/// This reimplements the logic without requiring opcode-tracer
pub(crate) fn compute_state_updates_wasm(
    trace: DefaultFrame,
) -> Result<(Vec<StateUpdate>, HashSet<Opcode>), anyhow::Error> {
    let result = compute_state_updates_wasm_with_calls(trace)?;
    Ok((result.state_updates, result.skipped_opcodes))
}

/// WASM-compatible implementation that also tracks CALL gas usage
pub(crate) fn compute_state_updates_wasm_with_calls(
    trace: DefaultFrame,
) -> Result<ComputeResult, anyhow::Error> {
    let mut state_updates: Vec<StateUpdate> = Vec::new();
    let mut skipped_opcodes = HashSet::new();
    let mut call_details: Vec<CallDetail> = Vec::new();

    // Debug: count opcodes of interest at various depths
    let mut call_count = 0u64;
    let mut log_count = 0u64;
    let mut sstore_count = 0u64;
    let mut min_depth = u64::MAX;
    let mut max_depth = 0u64;

    // First pass: find the minimum depth in the trace (some nodes use 0-indexed, some 1-indexed)
    for struct_log in &trace.struct_logs {
        min_depth = min_depth.min(struct_log.depth);
        max_depth = max_depth.max(struct_log.depth);
    }

    // Use the minimum depth found as the starting target_depth
    let mut target_depth: u64 = min_depth;

    log::info!(
        "Trace has {} steps, depth range: {} to {}",
        trace.struct_logs.len(),
        min_depth,
        max_depth
    );

    // Track pending CALLs to calculate gas used
    // Key: depth at which CALL was made, Value: (target, value, gas_before)
    let mut pending_calls: std::collections::HashMap<u64, (String, String, u64)> =
        std::collections::HashMap::new();

    let struct_logs: Vec<_> = trace.struct_logs.into_iter().collect();

    for struct_log in struct_logs.iter() {
        let depth = struct_log.depth;
        let gas = struct_log.gas;

        // Count opcodes for debugging
        match struct_log.op.as_ref() {
            "CALL" | "STATICCALL" => call_count += 1,
            "LOG0" | "LOG1" | "LOG2" | "LOG3" | "LOG4" => log_count += 1,
            "SSTORE" => sstore_count += 1,
            _ => {}
        }

        // Check if we're returning from a CALL (depth decreased)
        // Look for pending calls at higher depths that we've now returned from
        let depths_to_remove: Vec<u64> = pending_calls
            .keys()
            .filter(|&&call_depth| depth <= call_depth)
            .copied()
            .collect();

        for call_depth in depths_to_remove {
            if let Some((target, value, gas_before)) = pending_calls.remove(&call_depth) {
                // Gas used = gas before call - gas after return
                let gas_used = gas_before.saturating_sub(gas);
                call_details.push(CallDetail {
                    target,
                    value,
                    gas_used,
                });
            }
        }

        // Whenever stepping up (leaving a CALL/CALLCODE/DELEGATECALL) reset the target depth
        if depth < target_depth {
            target_depth = depth;
        } else if depth == target_depth {
            // If we're going to step into a new execution context, increase the target depth
            if &*struct_log.op == "DELEGATECALL" || &*struct_log.op == "CALLCODE" {
                target_depth = depth + 1;
            } else {
                // Track CALL/STATICCALL gas usage for calls at target depth
                if matches!(struct_log.op.as_ref(), "CALL" | "STATICCALL") {
                    if let Some(stack) = &struct_log.stack {
                        let mut stack = stack.clone();
                        stack.reverse();
                        if stack.len() >= 2 {
                            let target = format!("{:?}", Address::from_word(stack[1].into()));
                            let value = if struct_log.op.as_ref() == "CALL" && stack.len() >= 3 {
                                stack[2].to_string()
                            } else {
                                "0".to_string()
                            };
                            // Store the call info - we'll calculate gas when we return
                            pending_calls.insert(depth, (target, value, gas));
                        }
                    }
                }

                if let Some(opcode) =
                    append_to_state_updates_wasm(&mut state_updates, struct_log, target_depth)?
                {
                    skipped_opcodes.insert(opcode);
                }
            }
        }
    }

    // Handle any remaining pending calls (shouldn't happen in valid traces)
    for (_call_depth, (target, value, gas_before)) in pending_calls {
        call_details.push(CallDetail {
            target,
            value,
            gas_used: gas_before, // Use full gas as estimate
        });
    }

    log::info!(
        "Trace analysis: {} CALL ops, {} LOG ops, {} SSTORE ops in trace. Captured {} state updates, {} call details.",
        call_count,
        log_count,
        sstore_count,
        state_updates.len(),
        call_details.len()
    );

    Ok(ComputeResult {
        state_updates,
        skipped_opcodes,
        call_details,
    })
}

/// WASM-compatible helper to append state updates from a struct log
fn append_to_state_updates_wasm(
    state_updates: &mut Vec<StateUpdate>,
    struct_log: &alloy::rpc::types::trace::geth::StructLog,
    target_depth: u64,
) -> Result<Option<Opcode>, anyhow::Error> {
    use anyhow::bail;

    let mut stack = struct_log
        .stack
        .clone()
        .ok_or_else(|| anyhow::anyhow!("stack is empty"))?;
    stack.reverse();

    let depth = struct_log.depth;
    let is_target_depth = depth == target_depth;

    // Debug log for CALL/LOG opcodes
    if matches!(
        struct_log.op.as_ref(),
        "CALL" | "STATICCALL" | "LOG0" | "LOG1" | "LOG2" | "LOG3" | "LOG4"
    ) {
        log::debug!(
            "Found {} at depth {}, target_depth={}, is_target={}, stack_len={}",
            struct_log.op,
            depth,
            target_depth,
            is_target_depth,
            stack.len()
        );
    }

    let memory = match &struct_log.memory {
        Some(memory) => crate::parse_trace_memory(memory.clone()),
        None => match struct_log.op.as_ref() {
            "CALL" | "STATICCALL" | "LOG0" | "LOG1" | "LOG2" | "LOG3" | "LOG4"
                if is_target_depth =>
            {
                bail!(
                    "There is no memory for {:?} at target depth {}",
                    struct_log.op,
                    target_depth
                )
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
            if stack.len() >= 5 && is_target_depth {
                let args_offset: usize = stack[3].try_into().unwrap_or(0);
                let args_length: usize = stack[4].try_into().unwrap_or(0);
                let args = crate::copy_memory(&memory, args_offset, args_length);
                state_updates.push(StateUpdate::Call(IStateUpdateTypes::Call {
                    target: Address::from_word(stack[1].into()),
                    value: stack[2],
                    callargs: args.into(),
                }));
            }
        }
        // STATICCALL is like CALL but with value=0 and no state changes allowed
        // We still want to capture it to know what external calls are made
        "STATICCALL" => {
            if stack.len() >= 4 && is_target_depth {
                // STATICCALL: gas, addr, argsOffset, argsSize, retOffset, retSize
                let args_offset: usize = stack[2].try_into().unwrap_or(0);
                let args_length: usize = stack[3].try_into().unwrap_or(0);
                let args = crate::copy_memory(&memory, args_offset, args_length);
                state_updates.push(StateUpdate::Call(IStateUpdateTypes::Call {
                    target: Address::from_word(stack[1].into()),
                    value: alloy::primitives::U256::ZERO, // STATICCALL has no value
                    callargs: args.into(),
                }));
            }
        }
        "LOG0" => {
            if stack.len() >= 2 && is_target_depth {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = crate::copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log0(IStateUpdateTypes::Log0 {
                    data: data.into(),
                }));
            }
        }
        "LOG1" => {
            if stack.len() >= 3 && is_target_depth {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = crate::copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log1(IStateUpdateTypes::Log1 {
                    data: data.into(),
                    topic1: stack[2].into(),
                }));
            }
        }
        "LOG2" => {
            if stack.len() >= 4 && is_target_depth {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = crate::copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log2(IStateUpdateTypes::Log2 {
                    data: data.into(),
                    topic1: stack[2].into(),
                    topic2: stack[3].into(),
                }));
            }
        }
        "LOG3" => {
            if stack.len() >= 5 && is_target_depth {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = crate::copy_memory(&memory, data_offset, data_length);
                state_updates.push(StateUpdate::Log3(IStateUpdateTypes::Log3 {
                    data: data.into(),
                    topic1: stack[2].into(),
                    topic2: stack[3].into(),
                    topic3: stack[4].into(),
                }));
            }
        }
        "LOG4" => {
            if stack.len() >= 6 && is_target_depth {
                let data_offset: usize = stack[0].try_into().unwrap_or(0);
                let data_length: usize = stack[1].try_into().unwrap_or(0);
                let data = crate::copy_memory(&memory, data_offset, data_length);
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

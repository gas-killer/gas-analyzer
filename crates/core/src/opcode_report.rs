//! Opcode usage report for a transaction (#161).
//!
//! Buckets every step of a struct-log trace into a category (storage writes, logs, external
//! calls, ...) with its count and gas, and splits the same gas by what Gas Killer does with it:
//!
//! - **Kept**: state-changing opcodes the settlement still executes in the root contract's
//!   context: `SSTORE`, `LOG0`–`LOG4`, `CALL`, `CREATE`, `CREATE2`, `SELFDESTRUCT`.
//! - **Replayed**: everything executed inside a `CALL`/`CREATE` made from that context. The
//!   settlement makes the same call, so the callee's work runs again.
//! - **Optimizable**: everything else in the root contract's context (reads, hashing,
//!   arithmetic, memory, control flow, `DELEGATECALL`/`CALLCODE` into library code, and whole
//!   `STATICCALL` subtrees). The settlement replaces it with precomputed state updates, so this
//!   is the gas Gas Killer can remove.
//!
//! The split is a quick signal, not an estimate. The settlement also pays for its own payload
//! and signature check, and a write that was cold in the original transaction may be priced
//! differently when applied.
//!
//! Gas is each step's own cost. For a step that enters a call frame, geth's `gasCost` includes the
//! gas forwarded to the callee, so it is derived from the gas left before the call and after it
//! returns, minus what the callee's steps used. The categories therefore sum to the gas spent
//! executing the transaction (the receipt's `gasUsed` minus intrinsic gas, before refunds).

use std::collections::BTreeMap;

use alloy_rpc_types::trace::geth::StructLog;

/// What kind of work an opcode does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum OpcodeCategory {
    StorageWrites,
    StorageReads,
    TransientStorage,
    Logs,
    ExternalCalls,
    ContractCreation,
    AccountReads,
    Hashing,
    Other,
}

impl OpcodeCategory {
    pub fn of(op: &str) -> Self {
        match op {
            "SSTORE" => Self::StorageWrites,
            "SLOAD" => Self::StorageReads,
            "TLOAD" | "TSTORE" => Self::TransientStorage,
            "LOG0" | "LOG1" | "LOG2" | "LOG3" | "LOG4" => Self::Logs,
            "CALL" | "CALLCODE" | "DELEGATECALL" | "STATICCALL" => Self::ExternalCalls,
            "CREATE" | "CREATE2" | "SELFDESTRUCT" => Self::ContractCreation,
            "BALANCE" | "SELFBALANCE" | "EXTCODESIZE" | "EXTCODECOPY" | "EXTCODEHASH" => {
                Self::AccountReads
            }
            "KECCAK256" | "SHA3" => Self::Hashing,
            _ => Self::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::StorageWrites => "Storage writes",
            Self::StorageReads => "Storage reads",
            Self::TransientStorage => "Transient storage",
            Self::Logs => "Logs",
            Self::ExternalCalls => "External calls",
            Self::ContractCreation => "Create / selfdestruct",
            Self::AccountReads => "Account reads",
            Self::Hashing => "Hashing",
            Self::Other => "Other (compute, memory, control flow)",
        }
    }
}

/// What a Gas Killer settlement does with a step. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Treatment {
    Kept,
    Replayed,
    Optimizable,
}

/// Number of steps and the gas they used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub count: u64,
    pub gas: u64,
}

impl Usage {
    fn add(&mut self, gas: u64) {
        self.count += 1;
        self.gas += gas;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CategoryUsage {
    pub category: OpcodeCategory,
    pub total: Usage,
    /// Per opcode, by name.
    pub opcodes: BTreeMap<String, Usage>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpcodeReport {
    /// Categories that occur in the trace, in [`OpcodeCategory`] order.
    pub categories: Vec<CategoryUsage>,
    pub kept: Usage,
    pub replayed: Usage,
    pub optimizable: Usage,
}

impl OpcodeReport {
    pub fn total(&self) -> Usage {
        Usage {
            count: self.kept.count + self.replayed.count + self.optimizable.count,
            gas: self.kept.gas + self.replayed.gas + self.optimizable.gas,
        }
    }
}

/// Whose context a call frame runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    /// The root contract's own storage: depth 1, or reached from it only via
    /// `DELEGATECALL`/`CALLCODE`.
    Root,
    /// A `STATICCALL` subtree entered from the root context; the settlement drops it.
    DroppedRead,
    /// Inside a `CALL`/`CREATE` the settlement makes again.
    Replayed,
}

fn child_context(parent: Context, op: &str) -> Context {
    match (parent, op) {
        (Context::Root, "DELEGATECALL" | "CALLCODE") => Context::Root,
        (Context::Root, "STATICCALL") => Context::DroppedRead,
        (Context::Root, _) => Context::Replayed,
        (other, _) => other,
    }
}

fn treatment(context: Context, op: &str) -> Treatment {
    match context {
        Context::Replayed => Treatment::Replayed,
        Context::DroppedRead => Treatment::Optimizable,
        Context::Root => match op {
            "SSTORE" | "LOG0" | "LOG1" | "LOG2" | "LOG3" | "LOG4" | "CALL" | "CREATE"
            | "CREATE2" | "SELFDESTRUCT" => Treatment::Kept,
            _ => Treatment::Optimizable,
        },
    }
}

/// Each step's own gas, excluding gas a call step forwarded to its callee.
fn own_gas(steps: &[StructLog]) -> Vec<u64> {
    let mut own = vec![0u64; steps.len()];
    // Steps that entered a frame still running, with the gas their callee's steps used so far.
    let mut open_calls: Vec<(usize, u64)> = Vec::new();

    let close = |own: &mut Vec<u64>,
                 open_calls: &mut Vec<(usize, u64)>,
                 call: usize,
                 callee: u64,
                 gas_after: Option<u64>| {
        // Without a resume step (the trace ended first) fall back to geth's reported cost.
        let inclusive = gas_after.map_or(steps[call].gas_cost, |after| {
            steps[call].gas.saturating_sub(after)
        });
        own[call] = inclusive.saturating_sub(callee);
        if let Some((_, used)) = open_calls.last_mut() {
            *used += own[call] + callee;
        }
    };

    for (i, step) in steps.iter().enumerate() {
        // Close the calls whose frames have returned to this step's depth or above.
        while let Some(&(call, callee)) = open_calls.last() {
            if steps[call].depth < step.depth {
                break;
            }
            open_calls.pop();
            let resumed_here = steps[call].depth == step.depth;
            close(
                &mut own,
                &mut open_calls,
                call,
                callee,
                resumed_here.then_some(step.gas),
            );
        }

        match steps.get(i + 1) {
            Some(next) if next.depth > step.depth => open_calls.push((i, 0)),
            next => {
                own[i] = match next {
                    Some(next) if next.depth == step.depth => step.gas.saturating_sub(next.gas),
                    // The frame ends here (or the trace does).
                    _ => step.gas_cost,
                };
                if let Some((_, used)) = open_calls.last_mut() {
                    *used += own[i];
                }
            }
        }
    }
    while let Some((call, callee)) = open_calls.pop() {
        close(&mut own, &mut open_calls, call, callee, None);
    }
    own
}

/// Build the report from a struct-log trace's steps (`DefaultFrame::struct_logs`).
pub fn opcode_report(steps: &[StructLog]) -> OpcodeReport {
    let own = own_gas(steps);
    let mut report = OpcodeReport::default();
    let mut categories: BTreeMap<OpcodeCategory, CategoryUsage> = BTreeMap::new();
    // contexts[d - 1] is the context of the frame at depth d.
    let mut contexts: Vec<Context> = Vec::new();

    for (i, step) in steps.iter().enumerate() {
        let op: &str = step.op.as_ref();
        let depth = step.depth.max(1) as usize;
        contexts.truncate(depth);
        while contexts.len() < depth {
            // Depth 1 is the root; a deeper frame without a recorded entry is treated as replayed.
            contexts.push(if contexts.is_empty() {
                Context::Root
            } else {
                Context::Replayed
            });
        }
        let context = contexts[depth - 1];

        let gas = own[i];
        let category = OpcodeCategory::of(op);
        let entry = categories.entry(category).or_insert_with(|| CategoryUsage {
            category,
            total: Usage::default(),
            opcodes: BTreeMap::new(),
        });
        entry.total.add(gas);
        entry.opcodes.entry(op.to_owned()).or_default().add(gas);

        match treatment(context, op) {
            Treatment::Kept => report.kept.add(gas),
            Treatment::Replayed => report.replayed.add(gas),
            Treatment::Optimizable => report.optimizable.add(gas),
        }

        if steps.get(i + 1).is_some_and(|next| next.depth > step.depth) {
            contexts.push(child_context(context, op));
        }
    }

    report.categories = categories.into_values().collect();
    report
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(op: &str, depth: u64, gas: u64) -> StructLog {
        StructLog {
            pc: 0,
            op: op.to_string().into(),
            gas,
            gas_cost: 0,
            depth,
            error: None,
            stack: None,
            return_data: None,
            memory: None,
            memory_size: None,
            storage: None,
            refund_counter: None,
        }
    }

    /// Root reads, CALLs out (the callee writes), writes, DELEGATECALLs into a library that
    /// logs, and STATICCALLs a view that reads. 1000 gas in, 100 left.
    fn sample() -> Vec<StructLog> {
        vec![
            step("PUSH1", 1, 1000),       // 3
            step("SLOAD", 1, 997),        // 100
            step("CALL", 1, 897),         // 897 - 497 = 400 inclusive, 20 in the callee
            step("SSTORE", 2, 500),       // 20, replayed
            step("STOP", 2, 480),         // frame end: gas_cost 0
            step("POP", 1, 497),          // 2
            step("SSTORE", 1, 495),       // 100
            step("DELEGATECALL", 1, 395), // 395 - 300 = 95 inclusive, 50 in the library
            step("LOG1", 2, 200),         // 50, still the root's context
            step("RETURN", 2, 150),       // frame end
            step("JUMPDEST", 1, 300),     // 1
            step("STATICCALL", 1, 299),   // 299 - 100 = 199 inclusive, 100 in the view
            step("SLOAD", 2, 150),        // 100, dropped with the STATICCALL
            step("RETURN", 2, 50),        // frame end
            step("STOP", 1, 100),         // trace end
        ]
    }

    #[test]
    fn own_gas_excludes_what_the_callee_used() {
        assert_eq!(
            own_gas(&sample()),
            vec![3, 100, 380, 20, 0, 2, 100, 45, 50, 0, 1, 99, 100, 0, 0]
        );
    }

    #[test]
    fn gas_sums_to_what_execution_spent() {
        let report = opcode_report(&sample());
        assert_eq!(
            report.total(),
            Usage {
                count: 15,
                gas: 900
            }
        );
        let by_category: u64 = report.categories.iter().map(|c| c.total.gas).sum();
        assert_eq!(by_category, 900);
    }

    #[test]
    fn categories_group_opcodes() {
        let report = opcode_report(&sample());
        let get = |c| {
            report
                .categories
                .iter()
                .find(|u| u.category == c)
                .unwrap_or_else(|| panic!("{c:?} missing"))
        };
        assert_eq!(
            get(OpcodeCategory::StorageWrites).total,
            Usage { count: 2, gas: 120 }
        );
        assert_eq!(
            get(OpcodeCategory::StorageReads).total,
            Usage { count: 2, gas: 200 }
        );
        assert_eq!(get(OpcodeCategory::Logs).total, Usage { count: 1, gas: 50 });
        let calls = get(OpcodeCategory::ExternalCalls);
        assert_eq!(calls.total, Usage { count: 3, gas: 524 });
        assert_eq!(calls.opcodes["CALL"], Usage { count: 1, gas: 380 });
        assert_eq!(get(OpcodeCategory::Other).total, Usage { count: 7, gas: 6 });
        assert!(
            report
                .categories
                .iter()
                .all(|c| c.category != OpcodeCategory::Hashing),
            "categories absent from the trace are left out"
        );
    }

    #[test]
    fn splits_gas_by_what_the_settlement_keeps() {
        let report = opcode_report(&sample());
        // The CALL, the root's SSTORE and the library's LOG1.
        assert_eq!(report.kept, Usage { count: 3, gas: 530 });
        // The callee's SSTORE and STOP.
        assert_eq!(report.replayed, Usage { count: 2, gas: 20 });
        // Reads, compute, the DELEGATECALL/STATICCALL overhead and the whole view.
        assert_eq!(
            report.optimizable,
            Usage {
                count: 10,
                gas: 350
            }
        );
    }

    #[test]
    fn a_trace_that_ends_inside_a_call_uses_the_reported_cost() {
        let mut steps = vec![
            step("CALL", 1, 1000),
            step("SSTORE", 2, 500),
            step("STOP", 2, 480),
        ];
        steps[0].gas_cost = 600;
        // Inclusive cost falls back to the CALL's gas_cost; the callee used 20 of it.
        assert_eq!(own_gas(&steps), vec![580, 20, 0]);
    }

    #[test]
    fn empty_trace() {
        assert_eq!(opcode_report(&[]), OpcodeReport::default());
    }
}

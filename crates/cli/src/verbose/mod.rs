//! Verbose CLI mode: opt-in instrumentation that decodes state updates
//! against Etherscan-fetched ABIs and reruns failed estimations under a
//! tracing inspector.
//!
//! All side effects are hidden behind a single [`Verbose`] handle. When
//! verbose is disabled, every method short-circuits to a no-op so callers
//! can sprinkle hooks through `main` without `if verbose { ... }` walls.
//!
//! Layering:
//! - `etherscan` — HTTP + cache (no formatting)
//! - `decode` — pure ABI decoders over raw bytes (no I/O)
//! - `print` — verbose printers built on top of the two above

mod decode;
mod etherscan;
mod print;

use alloy_primitives::Address;
use colored::Colorize;
use gas_analyzer_core::{RevertingContext, StateUpdate};

#[cfg(feature = "evmsketch")]
use gas_analyzer_evmsketch::GasKillerEvmSketchDefault;

pub use etherscan::EtherscanClient;

/// Verbose-mode handle. Construct with [`Verbose::new`]; methods are
/// no-ops when verbose mode is disabled or when no Etherscan API key is
/// available.
pub struct Verbose {
    /// `Some` iff verbose was requested AND `ETHERSCAN_API_KEY` is set.
    client: Option<EtherscanClient>,
}

impl Verbose {
    /// Build a verbose handle. When `enabled` is false, returns a handle
    /// whose methods are all no-ops. When `enabled` is true but
    /// `ETHERSCAN_API_KEY` is missing, prints a one-line warning and still
    /// returns a no-op handle.
    pub fn new(enabled: bool) -> Self {
        if !enabled {
            return Self { client: None };
        }
        match std::env::var("ETHERSCAN_API_KEY") {
            Ok(key) => Self {
                client: Some(EtherscanClient::new(key, 1)),
            },
            Err(_) => {
                eprintln!(
                    "{} ETHERSCAN_API_KEY not set; verbose decoding disabled",
                    "warning:".yellow()
                );
                Self { client: None }
            }
        }
    }

    pub fn enabled(&self) -> bool {
        self.client.is_some()
    }

    /// Print every state update with its CALL targets ABI-decoded.
    pub async fn print_state_updates(&self, updates: &[StateUpdate]) {
        if let Some(c) = &self.client {
            print::state_updates(c, updates).await;
        }
    }

    /// Print a `RevertingContext` with the failing call and revert reason
    /// decoded against the target's ABI.
    pub async fn print_reverting_context(&self, ctx: &RevertingContext) {
        if let Some(c) = &self.client {
            print::reverting_context(c, ctx).await;
        }
    }

    /// Rerun gas estimation under a tracing inspector that emits a per-frame
    /// call trace to stderr. Result is discarded — the failure being
    /// diagnosed has already been reported. No-op when verbose is off.
    #[cfg(feature = "evmsketch")]
    pub fn rerun_with_tracing(
        &self,
        gk: &GasKillerEvmSketchDefault,
        contract_address: Address,
        caller_address: Address,
        state_updates: &[StateUpdate],
    ) {
        if !self.enabled() {
            return;
        }
        eprintln!(
            "\n{}",
            "── Rerunning under tracing inspector (stderr) ──"
                .yellow()
                .bold()
        );
        let _ =
            gk.estimate_state_changes_gas_traced(contract_address, caller_address, state_updates);
    }
}

#[cfg(feature = "evmsketch")]
use alloy::consensus::Transaction as _;
#[cfg(feature = "evmsketch")]
use alloy::sol_types::SolError;
use alloy::{hex, providers::ProviderBuilder};
use alloy_provider::Provider;
use anyhow::Result;
use colored::Colorize;
#[cfg(feature = "evmsketch")]
use gas_analyzer_core::RevertingContext;
use std::env;
use url::Url;

/// Try to decode a `RevertingContext` error from an anyhow error.
///
/// Looks for hex-encoded revert data in the error message (the format revm
/// produces: "Gas estimation reverted (gas: N): 0x...") and attempts to
/// ABI-decode it as a `RevertingContext`.
#[cfg(feature = "evmsketch")]
fn decode_reverting_context(e: &anyhow::Error) -> Option<RevertingContext> {
    let msg = format!("{e:?}");
    let hex_start = msg.rfind("0x")?;
    let hex_body = &msg[hex_start + 2..];
    let hex_end = hex_body
        .find(|c: char| !c.is_ascii_hexdigit())
        .unwrap_or(hex_body.len());
    let bytes = hex::decode(&hex_body[..hex_end]).ok()?;
    RevertingContext::abi_decode(&bytes).ok()
}

#[cfg(feature = "evmsketch")]
use alloy_eips::BlockNumberOrTag;

enum Commands {
    Transaction(String),
    Request(String),
}

struct CliArgs {
    command: Option<Commands>,
    use_anvil: bool,
    debug: bool,
    /// `--owned`: contracts priced as if they had integrated the SDK alongside the root.
    owned: Result<Vec<alloy::primitives::Address>, String>,
}

fn parse_args() -> CliArgs {
    let args: Vec<String> = env::args().collect();

    // Check for --anvil flag
    let use_anvil = args.iter().any(|a| a == "--anvil" || a == "--legacy");

    // Check for --debug flag
    let debug = args.iter().any(|a| a == "--debug");

    // `--owned A,B` or `--owned=A,B`; its value is not a positional argument.
    let mut owned_raw = Vec::new();
    let mut positional: Vec<&str> = Vec::new();
    let mut rest = args.iter().map(|s| s.as_str());
    while let Some(arg) = rest.next() {
        if arg == "--owned" {
            owned_raw.extend(rest.next());
        } else if let Some(value) = arg.strip_prefix("--owned=") {
            owned_raw.push(value);
        } else if !arg.starts_with("--") {
            positional.push(arg);
        }
    }
    let owned = owned_raw
        .iter()
        .flat_map(|list| list.split(','))
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(|a| {
            a.parse()
                .map_err(|e| format!("--owned: `{a}` is not an address: {e}"))
        })
        .collect();

    let command = if positional.len() < 3 {
        None
    } else {
        let input_type = positional[1];
        let value = positional[2].to_string();

        match input_type {
            "t" | "tx" => Some(Commands::Transaction(value)),
            "r" | "request" => Some(Commands::Request(value)),
            "d" | "debug" => Some(Commands::Transaction(value)),
            _ => None,
        }
    };

    // `debug <hash>` is an alias for `t <hash> --debug`
    let debug = debug
        || positional
            .get(1)
            .is_some_and(|s| *s == "debug" || *s == "d");

    CliArgs {
        command,
        use_anvil,
        debug,
        owned,
    }
}

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    let cli_args = parse_args();

    let log_level = if cli_args.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(log_level)),
        )
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
        .with_target(false)
        .init();

    let debug = cli_args.debug;
    let result = execute_command(cli_args).await;
    if let Err(e) = result {
        if debug {
            println!("{}", format!("{e:?}").red());
        } else {
            println!("{}", format!("{e}").red());
        }
    }
}

async fn execute_command(cli_args: CliArgs) -> Result<()> {
    let owned = cli_args.owned.clone().map_err(anyhow::Error::msg)?;
    let rpc_url: Url = std::env::var("RPC_URL")
        .expect("RPC_URL must be set")
        .parse()
        .expect("unable to parse rpc url");

    match cli_args.command {
        Some(Commands::Transaction(hash)) => {
            let provider = ProviderBuilder::new().connect_http(rpc_url.clone());
            let bytes: [u8; 32] = hex::const_decode_to_array(hash.as_bytes())
                .expect("failed to decode transaction hash");

            // Get the receipt to find the block and gas used
            let receipt = provider
                .get_transaction_receipt(bytes.into())
                .await?
                .expect("couldn't fetch tx receipt for tx");
            let block_number = receipt
                .block_number
                .expect("couldn't retrieve block number");
            #[cfg(feature = "evmsketch")]
            let tx_index = receipt
                .transaction_index
                .expect("couldn't retrieve transaction index");
            let gas_used = receipt.gas_used;
            let original_status = receipt.status();
            #[cfg(feature = "evmsketch")]
            let tx_sender = receipt.from;

            // Fetch the original tx so we can mirror its `msg.value` during
            // simulation. Pass-through contracts (deposit-then-forward, intent
            // settlers, swap routers) lose ETH otherwise and value-bearing
            // CALL state updates halt with OutOfFunds.
            #[cfg(feature = "evmsketch")]
            let tx = provider
                .get_transaction_by_hash(bytes.into())
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "tx 0x{} present in receipt but missing from eth_getTransactionByHash",
                        hex::encode(bytes)
                    )
                })?;
            #[cfg(feature = "evmsketch")]
            let tx_value = tx.value();

            if !owned.is_empty() {
                if cli_args.use_anvil {
                    anyhow::bail!("--owned runs on the EvmSketch backend; drop --anvil");
                }
                #[cfg(feature = "evmsketch")]
                return what_if::run(what_if::Inputs {
                    provider: &provider,
                    rpc_url: rpc_url.clone(),
                    tx_hash: bytes.into(),
                    status: original_status,
                    root: receipt
                        .to
                        .ok_or_else(|| anyhow::anyhow!("Transaction has no 'to' address"))?,
                    sender: tx_sender,
                    value: tx_value,
                    block_number,
                    tx_index,
                    gas_used,
                    owned: &owned,
                })
                .await;
                #[cfg(not(feature = "evmsketch"))]
                anyhow::bail!("--owned needs the evmsketch feature");
            }

            #[cfg(feature = "anvil")]
            if cli_args.use_anvil {
                println!("Using Anvil-based implementation...");

                use gas_analyzer_anvil::{GasKillerDefault, gas_estimate_tx};
                use gas_analyzer_core::compute_state_updates;
                use gas_analyzer_rpc::get_tx_trace;

                // Initialize GasKiller with Anvil
                let gk = GasKillerDefault::new(rpc_url.clone(), Some(block_number - 1))
                    .await
                    .expect("Failed to initialize GasKiller");

                // Get trace and compute state updates
                let trace = get_tx_trace(&provider, bytes.into(), original_status).await?;
                let gas_analyzer_core::TraceExtract {
                    state_updates,
                    skipped_opcodes,
                    ..
                } = compute_state_updates(trace, receipt.to)?;

                // Print state updates
                println!("\n{}", "=== State Updates ===".green().bold());
                println!("Total state updates: {}", state_updates.len());
                for (i, update) in state_updates.iter().enumerate() {
                    println!("  {}: {:?}", i + 1, update);
                }
                if !skipped_opcodes.is_empty() {
                    println!(
                        "\n{}: {}",
                        "Skipped opcodes".yellow(),
                        skipped_opcodes.into_iter().collect::<Vec<_>>().join(", ")
                    );
                }

                // Get full gas estimate
                let report = gas_estimate_tx(provider, bytes.into(), &gk).await?;

                // Print gas analysis
                use gas_analyzer_core::SignatureType;

                println!("\n{}", "=== Gas Analysis ===".blue().bold());
                println!("Transaction: 0x{}", hex::encode(bytes));
                println!(
                    "Block: {} ({})",
                    block_number,
                    receipt.block_hash.unwrap_or_default()
                );
                println!("Gas used: {}", gas_used);
                println!(
                    "GasKiller base estimate (before signature floor): {} {}",
                    report.gaskiller_base_gas_estimate,
                    "(measured via Anvil)".cyan()
                );
                // Report the total estimate and savings for each signature scheme, since
                // the Turetzky upper gas limit added to the base estimate differs per scheme.
                for (signature_type, gas_estimate, gas_savings, percent_savings) in [
                    (
                        SignatureType::Bls,
                        report.gaskiller_gas_estimate_bls,
                        report.gas_savings_bls,
                        report.percent_savings_bls,
                    ),
                    (
                        SignatureType::Schnorr,
                        report.gaskiller_gas_estimate_schnorr,
                        report.gas_savings_schnorr,
                        report.percent_savings_schnorr,
                    ),
                ] {
                    println!(
                        "\n{} (Turetzky upper gas limit: {})",
                        format!("[{}]", signature_type.label()).bold(),
                        signature_type.turetzky_upper_gas_limit()
                    );
                    println!("  GasKiller gas estimate: {}", gas_estimate);
                    println!("  Gas savings: {} ({:.2}%)", gas_savings, percent_savings);
                }
                if let Some(error) = &report.error_log {
                    if cli_args.debug {
                        println!("{}: {}", "Error".red(), error);
                    } else {
                        println!(
                            "{}: {}",
                            "Error".red(),
                            error.split('\n').next().unwrap_or("Unknown error")
                        );
                    }
                }

                return Ok(());
            }

            #[cfg(not(feature = "anvil"))]
            if cli_args.use_anvil {
                println!(
                    "{}",
                    "Error: Anvil feature not enabled. Rebuild with --features anvil".red()
                );
                return Ok(());
            }

            // Default: Use EvmSketch
            #[cfg(feature = "evmsketch")]
            {
                println!("Using EvmSketch implementation...");

                // Use shared trace function from rpc crate
                use gas_analyzer_rpc::compute_state_updates_from_tx;

                let state_updates_result = compute_state_updates_from_tx(
                    &provider,
                    bytes.into(),
                    original_status,
                    receipt.to,
                )
                .await;

                let (extract, use_fallback) = match state_updates_result {
                    Ok(extract) => (extract, false),
                    Err(e) => {
                        if original_status {
                            // Transaction succeeded originally but trace extraction failed
                            // Fall back to heuristic estimation
                            println!(
                                    "{}",
                                    "Warning: Trace extraction failed, using fallback heuristic estimation"
                                        .yellow()
                                );
                            if cli_args.debug {
                                println!("   Reason: {e:?}");
                            } else {
                                println!(
                                    "   Reason: {}",
                                    format!("{e}").split('\n').next().unwrap_or("Unknown error")
                                );
                            }

                            // Return an empty extraction and use fallback heuristic
                            (gas_analyzer_core::TraceExtract::default(), true)
                        } else {
                            // Transaction originally failed, so this is expected
                            let msg = format!(
                                "Cannot analyze failed transaction. Original transaction reverted.\n\
                                    Error: {}",
                                e
                            );
                            return Err(anyhow::Error::msg(msg));
                        }
                    }
                };

                // Get gas estimate using the state updates extracted from the actual trace
                use gas_analyzer_core::{
                    SignatureType, encode_state_updates_to_abi, estimate_gas_from_state_updates,
                };
                use gas_analyzer_evmsketch::GasKillerEvmSketchDefault;

                // Base estimate is the state-change execution cost only; the Turetzky
                // upper gas limit (which depends on the signature scheme) is added per
                // scheme when the estimate is reported.
                let (base_gas_estimate, is_heuristic) = if use_fallback
                    || extract.state_updates.is_empty()
                {
                    // Use heuristic estimation when trace extraction failed or no state updates
                    let gk = GasKillerEvmSketchDefault::builder(rpc_url.clone())
                        .at_block(BlockNumberOrTag::Number(block_number))
                        .build()
                        .await?;

                    // Try trace-based heuristic estimation
                    let fallback_estimate = match gk
                        .estimate_gas_from_trace(&provider, bytes.into(), original_status)
                        .await
                    {
                        Ok(estimate) => {
                            println!(
                                "   Using trace-based heuristic (extracted operations from original transaction)"
                            );
                            estimate
                        }
                        Err(e) => {
                            let msg = format!(
                                "Cannot analyze transaction: Failed to extract operations from trace.\n\
                                 Error: {}\n\
                                 \n\
                                 Please ensure your RPC provider supports debug_traceTransaction.",
                                e
                            );
                            return Err(anyhow::Error::msg(msg));
                        }
                    };
                    (fallback_estimate, true)
                } else {
                    // Normal path: try measured gas estimation using extracted state updates
                    // Get the contract address from the receipt
                    let contract_address = receipt
                        .to
                        .ok_or_else(|| anyhow::Error::msg("Transaction has no 'to' address"))?;

                    // Build EvmSketch for gas estimation (injecting StateChangeHandler contract)
                    let gk = GasKillerEvmSketchDefault::builder(rpc_url.clone())
                        .at_block(BlockNumberOrTag::Number(block_number))
                        .build()
                        .await?;

                    // Fetch preceding transactions for mid-block state accuracy.
                    // We fail hard here rather than falling back to block-N-1 state:
                    // a silent fallback can produce a confidently wrong gas number for
                    // any tx that depends on mid-block state.
                    let preceding_txs = gas_analyzer_rpc::get_preceding_transactions(
                        &provider,
                        block_number,
                        tx_index,
                    )
                    .await
                    .map_err(|e| {
                        anyhow::Error::msg(format!(
                            "Failed to fetch preceding transactions for block {} (tx index {}): {}",
                            block_number, tx_index, e
                        ))
                    })?;

                    if !preceding_txs.is_empty() {
                        println!(
                            "Replaying {} preceding transaction(s) for accurate mid-block state...",
                            preceding_txs.len()
                        );
                    }

                    // Try measured gas estimation with preceding tx replay
                    match gk.estimate_state_changes_gas_with_preceding(
                        contract_address,
                        tx_sender,
                        &extract.state_updates,
                        &preceding_txs,
                        tx_value,
                    ) {
                        Ok(gas) => (gas, false),
                        Err(e) => {
                            // Fall back to heuristic estimation
                            println!(
                                "{}",
                                "Warning: Measured gas estimation failed, using heuristic".yellow()
                            );
                            match decode_reverting_context(&e) {
                                Some(ctx) => {
                                    println!(
                                        "   Reason: {} CALL #{} to {} reverted",
                                        "RevertingContext".red(),
                                        ctx.index,
                                        ctx.target,
                                    );
                                    if cli_args.debug {
                                        if !ctx.revertData.is_empty() {
                                            println!(
                                                "   Revert data: 0x{}",
                                                hex::encode(&ctx.revertData)
                                            );
                                        }
                                        println!(
                                            "   Call args:   0x{}",
                                            hex::encode(&ctx.callargs)
                                        );
                                    }
                                }
                                None => {
                                    if cli_args.debug {
                                        println!("   Reason: {e:?}");
                                    } else {
                                        println!(
                                            "   Reason: {}",
                                            format!("{e}")
                                                .split('\n')
                                                .next()
                                                .unwrap_or("Unknown error")
                                        );
                                    }
                                }
                            }
                            let heuristic = estimate_gas_from_state_updates(&extract);
                            (heuristic, true)
                        }
                    }
                };

                // Encode the state updates
                let _encoded = encode_state_updates_to_abi(&extract.state_updates);

                // Print state updates (debug only)
                if cli_args.debug {
                    println!("\n{}", "=== State Updates ===".green().bold());
                    println!("Total state updates: {}", extract.state_updates.len());
                    for (i, update) in extract.state_updates.iter().enumerate() {
                        println!("  {}: {:?}", i + 1, update);
                    }
                    if !extract.skipped_opcodes.is_empty() {
                        println!(
                            "\n{}: {}",
                            "Skipped opcodes".yellow(),
                            extract
                                .skipped_opcodes
                                .into_iter()
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                }

                // Print gas analysis
                println!("\n{}", "=== Gas Analysis ===".blue().bold());
                println!("Transaction: 0x{}", hex::encode(bytes));
                println!(
                    "Block: {} ({}) | Tx Index: {}",
                    block_number,
                    receipt.block_hash.unwrap_or_default(),
                    tx_index
                );
                println!("Gas used: {}", gas_used);
                let estimate_type = if use_fallback {
                    "(fallback heuristic - replay failed)".yellow()
                } else if is_heuristic {
                    "(heuristic - measured estimation failed)".yellow()
                } else {
                    "(measured via StateChangeHandler)".cyan()
                };
                println!(
                    "GasKiller base estimate (before signature floor): {} {}",
                    base_gas_estimate, estimate_type
                );
                if extract.reentered {
                    println!(
                        "{}",
                        "Note: a callee re-entered the target contract during execution. \
                         External-call gas includes the contract's own callback logic, so \
                         heuristic estimates may overshoot."
                            .yellow()
                    );
                }
                // Report the total estimate and savings for each signature scheme, since
                // the Turetzky upper gas limit added to the base estimate differs per scheme.
                for signature_type in SignatureType::ALL {
                    let (gas_estimate, gas_savings, percent_savings) =
                        signature_type.savings(base_gas_estimate, gas_used);
                    println!(
                        "\n{} (Turetzky upper gas limit: {})",
                        format!("[{}]", signature_type.label()).bold(),
                        signature_type.turetzky_upper_gas_limit()
                    );
                    println!("  GasKiller gas estimate: {}", gas_estimate);
                    println!("  Gas savings: {} ({:.2}%)", gas_savings, percent_savings);
                }
            }

            #[cfg(not(feature = "evmsketch"))]
            {
                println!(
                    "{}",
                    "Error: No execution backend available. Rebuild with --features evmsketch or --features anvil".red()
                );
            }
        }

        Some(Commands::Request(_file)) => {
            // Note: The request command (for simulating unexecuted transactions) has been removed.
            // Use the transaction command to analyze existing transactions via their tx hash.
            println!(
                "{}",
                "Error: The request command is no longer supported.\n\
                Use the transaction command to analyze existing transactions: cli t <tx_hash>"
                    .red()
            );
        }

        None => {
            println!("Gas Killer Analyzer\n");
            println!("Usage:\n");
            println!("  {} for accepted transactions", "t/tx <HASH>".bold());
            println!("  {} alias for t <HASH> --debug", "debug <HASH>".bold());
            println!(
                "  {} for transaction requests",
                "r/request <JSON_FILE>".bold()
            );
            println!("\nFlags:\n");
            println!(
                "  {} Use Anvil-based implementation (requires --features anvil)",
                "--anvil".bold()
            );
            println!(
                "  {} Print full error details including RPC errors",
                "--debug".bold()
            );
            println!(
                "  {} Price the transaction as a nested settlement in which these\n           \
                 contracts, beside the root, had integrated the SDK",
                "--owned <ADDR,...>".bold()
            );
            println!("\nExamples:\n");
            println!("  # Default (EvmSketch - Anvil-free):");
            println!("  cargo run -- t <TX_HASH>");
            println!("\n  # With Anvil (legacy, more accurate gas estimates):");
            println!("  cargo run --features anvil -- --anvil t <TX_HASH>");
            println!("\n  # What the root and C would have saved together as one nested tree:");
            println!("  cargo run -- t <TX_HASH> --owned <C_ADDRESS>");
        }
    }
    Ok(())
}

/// `t <HASH> --owned ...`: what a historical transaction would have cost settled as a nested
/// tree, had its root and the owned contracts integrated the SDK.
#[cfg(feature = "evmsketch")]
mod what_if {
    use alloy::primitives::{Address, FixedBytes, U256};
    use alloy_eips::BlockNumberOrTag;
    use alloy_provider::Provider;
    use alloy_provider::ext::DebugApi;
    use anyhow::Result;
    use colored::Colorize;
    use gas_analyzer_core::SignatureType;
    use gas_analyzer_core::nested::{
        FrameProgram, NESTING_COST_MODEL_V1, compute_frame_tree_hypothetical,
    };
    use gas_analyzer_core::types::StateUpdate;
    use gas_analyzer_evmsketch::GasKillerEvmSketchDefault;
    use std::collections::BTreeSet;

    pub struct Inputs<'a, P> {
        pub provider: &'a P,
        pub rpc_url: url::Url,
        pub tx_hash: FixedBytes<32>,
        pub status: bool,
        pub root: Address,
        pub sender: Address,
        pub value: U256,
        pub block_number: u64,
        pub tx_index: u64,
        pub gas_used: u64,
        pub owned: &'a [Address],
    }

    pub async fn run<P: Provider + DebugApi>(inputs: Inputs<'_, P>) -> Result<()> {
        let Inputs {
            provider,
            rpc_url,
            tx_hash,
            status,
            root,
            sender,
            value,
            block_number,
            tx_index,
            gas_used,
            owned,
        } = inputs;
        let owned: BTreeSet<Address> = owned.iter().copied().filter(|a| *a != root).collect();
        let cost = NESTING_COST_MODEL_V1;

        let trace = gas_analyzer_rpc::get_tx_trace(provider, tx_hash, status).await?;
        // Root-only and nested are split from the same trace by the same walker, so the
        // difference between them is the nesting alone.
        let flat = compute_frame_tree_hypothetical(trace.clone(), root, &BTreeSet::new(), &cost)?;
        let nested = compute_frame_tree_hypothetical(trace, root, &owned, &cost)?;

        let gk = GasKillerEvmSketchDefault::builder(rpc_url)
            .at_block(BlockNumberOrTag::Number(block_number))
            .build()
            .await?;
        let preceding =
            gas_analyzer_rpc::get_preceding_transactions(provider, block_number, tx_index)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to fetch preceding transactions for block {block_number} \
                         (tx index {tx_index}): {e}"
                    )
                })?;
        let flat_gas = gk.estimate_frame_tree_gas_with_preceding(
            &flat.frames,
            sender,
            &preceding,
            value,
            &cost,
        )?;
        let nested_gas = gk.estimate_frame_tree_gas_with_preceding(
            &nested.frames,
            sender,
            &preceding,
            value,
            &cost,
        )?;

        println!("\n{}", "=== Nested Settlement What-If ===".blue().bold());
        println!("Transaction: {tx_hash}");
        println!("Block: {block_number} | Tx Index: {tx_index}");
        println!("Gas used: {gas_used}");
        println!("Root (tx.to): {root}");
        println!(
            "Owned: {}",
            if owned.is_empty() {
                "none beside the root".to_owned()
            } else {
                owned
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );

        println!("\n{}", "Frames".bold());
        for (i, (frame, gas)) in nested.frames.iter().zip(&nested_gas.frame_gas).enumerate() {
            let role = if i == 0 {
                "root".to_owned()
            } else {
                format!("called by {}", frame.caller)
            };
            println!(
                "  #{i} {} ({role}): {} ops, {} gas measured alone",
                frame.target,
                frame.updates.len(),
                gas
            );
        }
        for (contract, reason) in not_nested(&owned, &nested.frames) {
            println!("  {} {contract}: {reason}", "not nested".yellow());
        }

        println!(
            "\nRoot-only program: {} gas | Nested tree: {} gas (incl. {} per nested frame)",
            flat_gas.total, nested_gas.total, cost.frame_overhead_gas
        );
        for signature_type in SignatureType::ALL {
            let (flat_total, flat_savings, flat_pct) =
                signature_type.savings(flat_gas.total, gas_used);
            let (nested_total, nested_savings, nested_pct) =
                signature_type.savings(nested_gas.total, gas_used);
            println!(
                "\n{} (Turetzky upper gas limit: {})",
                format!("[{}]", signature_type.label()).bold(),
                signature_type.turetzky_upper_gas_limit()
            );
            println!("  Root only:   {flat_total} gas, saves {flat_savings} ({flat_pct:.2}%)");
            println!(
                "  Nested tree: {nested_total} gas, saves {nested_savings} ({nested_pct:.2}%)"
            );
        }
        println!(
            "\n{}",
            "Each frame is measured from the state before the transaction, and the cost model's \
             fixed overhead stands in for the per-frame proof and witness cost."
                .dimmed()
        );
        Ok(())
    }

    /// Why each owned contract did not get a frame.
    fn not_nested(
        owned: &BTreeSet<Address>,
        frames: &[FrameProgram],
    ) -> Vec<(Address, &'static str)> {
        let framed: BTreeSet<Address> = frames.iter().map(|f| f.target).collect();
        let called: BTreeSet<Address> = frames
            .iter()
            .flat_map(|f| &f.updates)
            .filter_map(|u| match u {
                StateUpdate::Call(call) => Some(call.target),
                _ => None,
            })
            .collect();
        owned
            .iter()
            .filter(|a| !framed.contains(*a))
            .map(|a| {
                let reason = if called.contains(a) {
                    "called directly, but cheaper as a plain CALL (or it reverted, or uses \
                     transient storage or selfdestruct)"
                } else {
                    "not called from a nested frame: its caller is outside the owned set or \
                     was itself kept as a CALL, so it ran inside that call"
                };
                (*a, reason)
            })
            .collect()
    }
}

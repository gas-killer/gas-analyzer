use alloy::{hex, providers::ProviderBuilder};
use alloy_provider::Provider;
use anyhow::Result;
use colored::Colorize;
use std::env;
use url::Url;

#[cfg(feature = "evmsketch")]
use alloy_eips::BlockNumberOrTag;

enum Commands {
    Transaction(String),
    Request(String),
}

struct CliArgs {
    command: Option<Commands>,
    use_anvil: bool,
}

fn parse_args() -> CliArgs {
    let args: Vec<String> = env::args().collect();

    // Check for --anvil flag
    let use_anvil = args.iter().any(|a| a == "--anvil" || a == "--legacy");

    // Filter out flags to get positional args
    let positional: Vec<&str> = args
        .iter()
        .map(|s| s.as_str())
        .filter(|a| !a.starts_with("--"))
        .collect();

    let command = if positional.len() < 3 {
        None
    } else {
        let input_type = positional[1];
        let value = positional[2].to_string();

        match input_type {
            "t" | "tx" => Some(Commands::Transaction(value)),
            "r" | "request" => Some(Commands::Request(value)),
            _ => None,
        }
    };

    CliArgs { command, use_anvil }
}

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    let cli_args = parse_args();

    let result = execute_command(cli_args).await;
    if let Err(e) = result {
        println!("{}", format!("{e:?}").red());
    }
}

async fn execute_command(cli_args: CliArgs) -> Result<()> {
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
            let gas_used = receipt.gas_used;
            let original_status = receipt.status();
            let tx_sender = receipt.from;

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
                let trace = get_tx_trace(&provider, bytes.into()).await?;
                let (state_updates, skipped_opcodes, _call_gas_total) =
                    compute_state_updates(trace)?;

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
                println!("\n{}", "=== Gas Analysis ===".blue().bold());
                println!("Transaction: 0x{}", hex::encode(bytes));
                println!(
                    "Block: {} ({})",
                    block_number,
                    receipt.block_hash.unwrap_or_default()
                );
                println!("Gas used: {}", gas_used);
                println!(
                    "GasKiller gas estimate: {} {}",
                    report.gaskiller_gas_estimate,
                    "(measured via Anvil)".cyan()
                );
                println!(
                    "Gas savings: {} ({:.2}%)",
                    report.gas_savings, report.percent_savings
                );
                if let Some(error) = &report.error_log {
                    println!("{}: {}", "Error".red(), error);
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

                let state_updates_result =
                    compute_state_updates_from_tx(&provider, bytes.into()).await;

                let (state_updates, skipped_opcodes, call_gas_total, use_fallback) =
                    match state_updates_result {
                        Ok(result) => (result.0, result.1, result.2, false),
                        Err(e) => {
                            if original_status {
                                // Transaction succeeded originally but trace extraction failed
                                // Fall back to heuristic estimation
                                println!(
                                    "{}",
                                    "Warning: Trace extraction failed, using fallback heuristic estimation"
                                        .yellow()
                                );
                                println!(
                                    "   Reason: {}",
                                    format!("{}", e)
                                        .split('\n')
                                        .next()
                                        .unwrap_or("Unknown error")
                                );

                                // Return empty state updates and use fallback heuristic
                                (Vec::new(), std::collections::HashSet::new(), 0, true)
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
                    TURETZKY_UPPER_GAS_LIMIT, encode_state_updates_to_abi,
                    estimate_gas_from_state_updates,
                };
                use gas_analyzer_evmsketch::GasKillerEvmSketchDefault;

                let (gas_estimate, is_heuristic) = if use_fallback || state_updates.is_empty() {
                    // Use heuristic estimation when trace extraction failed or no state updates
                    let gk = GasKillerEvmSketchDefault::builder(rpc_url.clone())
                        .at_block(BlockNumberOrTag::Number(block_number))
                        .build()
                        .await?;

                    // Try trace-based heuristic estimation
                    let fallback_estimate = match gk
                        .estimate_gas_from_trace(&provider, bytes.into())
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
                    (fallback_estimate + TURETZKY_UPPER_GAS_LIMIT, true)
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

                    // Try measured gas estimation first
                    match gk.estimate_state_changes_gas(contract_address, tx_sender, &state_updates) {
                        Ok(gas) => (gas + TURETZKY_UPPER_GAS_LIMIT, false),
                        Err(_) => {
                            // Fall back to heuristic estimation
                            println!(
                                "{}",
                                "Warning: Measured gas estimation failed, using heuristic".yellow()
                            );
                            let heuristic =
                                estimate_gas_from_state_updates(&state_updates, call_gas_total);
                            (heuristic + TURETZKY_UPPER_GAS_LIMIT, true)
                        }
                    }
                };

                // Encode the state updates
                let _encoded = encode_state_updates_to_abi(&state_updates);

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

                // Print gas analysis
                let gas_savings = gas_used.saturating_sub(gas_estimate);
                let percent_savings = if gas_used > 0 {
                    (gas_savings as f64 / gas_used as f64) * 100.0
                } else {
                    0.0
                };

                println!("\n{}", "=== Gas Analysis ===".blue().bold());
                println!("Transaction: 0x{}", hex::encode(bytes));
                println!(
                    "Block: {} ({})",
                    block_number,
                    receipt.block_hash.unwrap_or_default()
                );
                println!("Gas used: {}", gas_used);
                let estimate_type = if use_fallback {
                    "(fallback heuristic - replay failed)".yellow()
                } else if is_heuristic {
                    "(heuristic - measured estimation failed)".yellow()
                } else {
                    "(measured via StateChangeHandler)".cyan()
                };
                println!("GasKiller gas estimate: {} {}", gas_estimate, estimate_type);
                println!("Gas savings: {} ({:.2}%)", gas_savings, percent_savings);
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
            println!(
                "  {} for transaction requests",
                "r/request <JSON_FILE>".bold()
            );
            println!("\nFlags:\n");
            println!(
                "  {} Use Anvil-based implementation (requires --features anvil)",
                "--anvil".bold()
            );
            println!("\nExamples:\n");
            println!("  # Default (EvmSketch - Anvil-free):");
            println!("  cargo run -- t <TX_HASH>");
            println!("\n  # With Anvil (legacy, more accurate gas estimates):");
            println!("  cargo run --features anvil -- --anvil t <TX_HASH>");
        }
    }
    Ok(())
}

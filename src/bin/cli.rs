use alloy::{hex, providers::ProviderBuilder};
use alloy_eips::BlockNumberOrTag;
use alloy_provider::Provider;
use alloy_rpc_types::TransactionRequest;
use anyhow::Result;
use colored::Colorize;
use std::{env, fs::File, io::Read};
use url::Url;

#[cfg(feature = "evmsketch")]
use gas_analyzer_rs::{
    call_to_encoded_state_updates_with_evmsketch, compute_state_updates_with_evmsketch,
};

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
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();

    let command = if positional.len() < 3 {
        None
    } else {
        let input_type: &str = positional[1];
        let value = positional[2].clone();

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

            #[cfg(feature = "anvil")]
            if cli_args.use_anvil {
                println!("Using Anvil-based implementation...");

                use gas_analyzer_rs::{
                    compute_state_updates, gas_estimate_tx, get_tx_trace, gk::GasKillerDefault,
                };

                // Initialize GasKiller with Anvil
                let gk = GasKillerDefault::new(rpc_url.clone(), Some(block_number - 1))
                    .await
                    .expect("Failed to initialize GasKiller");

                // Get trace and compute state updates
                let trace = get_tx_trace(&provider, bytes.into()).await?;
                let (state_updates, skipped_opcodes) = compute_state_updates(trace).await?;

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
                println!("Using EvmSketch (Anvil-free) implementation...");

                // Get the transaction to reconstruct the request
                let tx = provider
                    .get_transaction_by_hash(bytes.into())
                    .await?
                    .expect("couldn't fetch transaction");

                use alloy::network::TransactionResponse;
                use alloy::primitives::Address;
                use alloy_rpc_types::TransactionTrait;

                let to_addr = tx.to().expect("transaction has no 'to' address");
                let tx_request = TransactionRequest::default()
                    .from(tx.from())
                    .to(Address::from(*to_addr))
                    .input(alloy::rpc::types::TransactionInput::new(tx.input().clone()))
                    .value(tx.value());

                // Execute at the block before the transaction was included
                let (state_updates, skipped_opcodes, _external_call_gas) =
                    compute_state_updates_with_evmsketch(
                        rpc_url.clone(),
                        tx_request.clone(),
                        BlockNumberOrTag::Number(block_number - 1),
                    )
                    .await?;

                // Get gas estimate (measured or heuristic fallback)
                let (_encoded, gas_estimate, is_heuristic, _) =
                    call_to_encoded_state_updates_with_evmsketch(
                        rpc_url,
                        tx_request,
                        BlockNumberOrTag::Number(block_number - 1),
                    )
                    .await?;

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
                let estimate_type = if is_heuristic {
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

        Some(Commands::Request(file)) => {
            #[cfg(feature = "evmsketch")]
            {
                println!("Using EvmSketch (Anvil-free) implementation...");

                let mut file = File::open(file).expect("couldn't find file");
                let mut contents = String::new();
                file.read_to_string(&mut contents)
                    .expect("unable to read file contents");
                let request = serde_json::from_str::<TransactionRequest>(contents.as_ref())
                    .expect("unable to read json data");

                match call_to_encoded_state_updates_with_evmsketch(
                    rpc_url,
                    request,
                    BlockNumberOrTag::Latest,
                )
                .await
                {
                    Ok((_, estimate, is_heuristic, _)) => {
                        let estimate_type = if is_heuristic {
                            "heuristic"
                        } else {
                            "measured"
                        };
                        println!("GasKiller estimate: {} ({})", estimate, estimate_type);
                    }
                    Err(e) => {
                        println!("{}", format!("Estimation failed: {:?}", e).red());
                    }
                }
            }

            #[cfg(not(feature = "evmsketch"))]
            {
                let _ = file; // suppress unused warning
                println!(
                    "{}",
                    "Error: EvmSketch feature required for request mode. Rebuild with --features evmsketch".red()
                );
            }
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

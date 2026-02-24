use alloy::{hex, providers::ProviderBuilder};
use alloy_provider::Provider;
use alloy_sol_types::SolError;
use anyhow::Result;
use colored::Colorize;
use regex::Regex;
use serde::Deserialize;
use std::{env, fmt::Write as _, sync::LazyLock};
use url::Url;

#[cfg(feature = "evmsketch")]
use alloy_eips::BlockNumberOrTag;

alloy::sol! {
    error RevertingContext(address contract, bytes revertData);
}

enum Commands {
    Transaction(String),
    Request(String),
    Debug(String),
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
            "d" | "debug" => Some(Commands::Debug(value)),
            _ => None,
        }
    };

    CliArgs { command, use_anvil }
}

#[derive(Debug, Deserialize)]
struct SourceCodeResponse {
    #[allow(dead_code)]
    status: String,
    #[allow(dead_code)]
    message: String,
    result: Vec<ContractMetadata>,
}

#[derive(Debug, Deserialize)]
struct ContractMetadata {
    #[serde(rename = "ContractName")]
    contract_name: String,
}

async fn get_etherscan_contract_name(address: &str, api_key: &str) -> Result<Option<String>> {
    let url = format!(
        "https://api.etherscan.io/v2/api?chainid=1&module=contract&action=getsourcecode&address={}&apikey={}",
        address, api_key
    );

    let resp: SourceCodeResponse = reqwest::get(url).await?.json().await?;

    // Etherscan always returns a 1-element array
    let Some(meta) = resp.result.into_iter().next() else {
        return Ok(None);
    };

    // If not verified, ContractName is empty
    if meta.contract_name.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(meta.contract_name))
}
/// Response from 4byte.directory signature lookup API.
#[derive(Debug, Deserialize)]
struct FourByteResponse {
    results: Vec<FourByteEntry>,
}

#[derive(Debug, Deserialize)]
struct FourByteEntry {
    text_signature: String,
}

/// Look up a 4-byte selector on 4byte.directory.
/// Returns the text signature if found (e.g. "swap(address,bool,int256,uint160,bytes)").
async fn lookup_4byte_selector(selector_hex: &str) -> Result<Option<String>> {
    let url = format!(
        "https://www.4byte.directory/api/v1/signatures/?hex_signature={}",
        selector_hex
    );
    let resp: FourByteResponse = reqwest::get(&url).await?.json().await?;
    Ok(resp.results.into_iter().next().map(|e| e.text_signature))
}

/// Look up a 4-byte error selector on 4byte.directory.
async fn lookup_4byte_error(selector_hex: &str) -> Result<Option<String>> {
    let url = format!(
        "https://www.4byte.directory/api/v1/event-signatures/?hex_signature={}",
        selector_hex
    );
    // 4byte.directory doesn't have a dedicated error endpoint; try function signatures
    // as errors often share selectors with functions. Fall back to the general lookup.
    let resp: FourByteResponse = reqwest::get(&url).await?.json().await?;
    if let Some(entry) = resp.results.into_iter().next() {
        return Ok(Some(entry.text_signature));
    }
    // Try function signatures as fallback
    lookup_4byte_selector(selector_hex).await
}

/// Decode revert data into a human-readable string.
/// Tries Error(string), Panic(uint256), then 4byte.directory lookup.
async fn decode_revert_data(revert_data: &[u8]) -> String {
    if revert_data.len() < 4 {
        if revert_data.is_empty() {
            return "empty revert data".to_string();
        }
        return format!("0x{}", hex::encode(revert_data));
    }

    let selector = &revert_data[..4];

    // Error(string) — selector 0x08c379a0
    if selector == [0x08, 0xc3, 0x79, 0xa0] && revert_data.len() >= 68 {
        // ABI-decode: skip selector (4) + offset (32) + length (32), then read string
        let len_offset = 36; // 4 + 32
        if revert_data.len() >= len_offset + 32 {
            let len_bytes = &revert_data[len_offset..len_offset + 32];
            let len = u64::from_be_bytes(len_bytes[24..32].try_into().unwrap_or([0; 8])) as usize;
            let str_start = len_offset + 32;
            if revert_data.len() >= str_start + len
                && let Ok(msg) = std::str::from_utf8(&revert_data[str_start..str_start + len])
            {
                return format!("Error(\"{}\")", msg);
            }
        }
    }

    // Panic(uint256) — selector 0x4e487b71
    if selector == [0x4e, 0x48, 0x7b, 0x71] && revert_data.len() >= 36 {
        let code_bytes = &revert_data[4..36];
        let code = u64::from_be_bytes(code_bytes[24..32].try_into().unwrap_or([0; 8]));
        let reason = match code {
            0x00 => "generic compiler panic",
            0x01 => "assert failed",
            0x11 => "arithmetic overflow/underflow",
            0x12 => "division by zero",
            0x21 => "invalid enum value",
            0x22 => "invalid storage byte array encoding",
            0x31 => "pop on empty array",
            0x32 => "array index out of bounds",
            0x41 => "too much memory allocated",
            0x51 => "zero-initialized function pointer",
            _ => "unknown panic code",
        };
        return format!("Panic(0x{:02x}: {})", code, reason);
    }

    // Try 4byte.directory for the error selector
    let selector_hex = format!("0x{}", hex::encode(selector));
    match lookup_4byte_error(&selector_hex).await {
        Ok(Some(sig)) => format!("{}(...)", sig.split('(').next().unwrap_or(&sig)),
        _ => format!("unknown error {}", selector_hex),
    }
}

/// Format a DefaultFrame execution trace into a condensed call tree string.
fn format_trace(trace: &alloy::rpc::types::trace::geth::DefaultFrame) -> String {
    let mut output = String::new();
    let mut prev_depth = 1u64;
    let mut interesting_count = 0usize;
    let max_interesting = 1000;

    for log in &trace.struct_logs {
        let depth = log.depth;
        let op = log.op.as_ref();
        let indent = "  ".repeat(depth as usize);

        match op {
            "CALL" | "STATICCALL" | "DELEGATECALL" | "CALLCODE" => {
                if interesting_count >= max_interesting {
                    continue;
                }
                interesting_count += 1;
                let stack = log.stack.as_ref();
                let target = stack.and_then(|s| {
                    // For CALL: stack[1] is target; for STATICCALL/DELEGATECALL: stack[1] is target
                    s.get(1)
                        .map(|v| format!("0x{}", &hex::encode(v.to_be_bytes::<32>())[24..]))
                });
                let gas = stack.and_then(|s| {
                    // For CALL: stack[0] is gas; for STATICCALL: stack[0] is gas
                    s.first().map(|v| v.to_string())
                });
                let target_str = target.as_deref().unwrap_or("?");
                let gas_str = gas.as_deref().unwrap_or("?");
                let line = format!(
                    "{}\u{2192} {} {} (gas: {})\n",
                    indent,
                    op.cyan(),
                    target_str,
                    gas_str
                );
                let _ = write!(output, "{}", line);
            }
            "SSTORE" => {
                if interesting_count >= max_interesting {
                    continue;
                }
                interesting_count += 1;
                let stack = log.stack.as_ref();
                let (slot, value) = stack
                    .map(|s| {
                        let slot = s
                            .first()
                            .map(|v| format!("0x{}", &hex::encode(v.to_be_bytes::<32>())[..8]))
                            .unwrap_or_default();
                        let val = s
                            .get(1)
                            .map(|v| format!("0x{}", &hex::encode(v.to_be_bytes::<32>())[..8]))
                            .unwrap_or_default();
                        (slot, val)
                    })
                    .unwrap_or_default();
                let line = format!(
                    "{}SSTORE [{}..] <- [{}..]\n",
                    indent,
                    slot.yellow(),
                    value.yellow()
                );
                let _ = write!(output, "{}", line);
            }
            "REVERT" => {
                if interesting_count >= max_interesting {
                    continue;
                }
                interesting_count += 1;
                let line = format!("{}{} REVERT\n", indent, "\u{2717}".red());
                let _ = write!(output, "{}", line);
            }
            "RETURN" => {
                // Only show RETURN when exiting a call (depth change)
                if depth < prev_depth {
                    if interesting_count >= max_interesting {
                        continue;
                    }
                    interesting_count += 1;
                    let line = format!("{}{} RETURN\n", indent, "\u{2190}".green());
                    let _ = write!(output, "{}", line);
                }
            }
            _ => {}
        }

        prev_depth = depth;
    }

    if interesting_count >= max_interesting {
        let _ = writeln!(
            output,
            "\n  ... truncated ({} operations shown of {})",
            max_interesting,
            trace.struct_logs.len()
        );
    }

    output
}

static RE_HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"0x[0-9a-fA-F]{8,}").unwrap());

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

    let etherscan_api_key = std::env::var("ETHERSCAN_API_KEY").ok();

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

            #[cfg(feature = "anvil")]
            if cli_args.use_anvil {
                println!("Using Anvil-based implementation...");

                use gas_analyzer_rs::{
                    GasKillerDefault, compute_state_updates, gas_estimate_tx, get_tx_trace,
                };

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

                // Use shared trace function from core
                use gas_analyzer_rs::compute_state_updates_from_tx;

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
                                    "⚠️  Warning: Trace extraction failed, using fallback heuristic estimation"
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
                use gas_analyzer_rs::core::{
                    TURETZKY_UPPER_GAS_LIMIT, encode_state_updates_to_abi,
                    estimate_gas_from_state_updates,
                };
                use gas_analyzer_rs::evmsketch::GasKillerEvmSketchDefault;

                let (gas_estimate, is_heuristic) = if use_fallback || state_updates.is_empty() {
                    // Use heuristic estimation when trace extraction failed or no state updates
                    let gk = GasKillerEvmSketchDefault::builder(rpc_url.clone())
                        .at_block(BlockNumberOrTag::Number(block_number - 1))
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
                        .at_block(BlockNumberOrTag::Number(block_number - 1))
                        .build()
                        .await?;

                    // Try measured gas estimation first
                    match gk.estimate_state_changes_gas(contract_address, &state_updates) {
                        Ok(gas) => (gas + TURETZKY_UPPER_GAS_LIMIT, false),
                        Err(e) => {
                            // Fall back to heuristic estimation
                            println!(
                                "{}",
                                format!(
                                    "⚠️  Warning: Measured gas estimation failed, using heuristic\n   Error: {:#}",
                                    e
                                )
                                .yellow()
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

        Some(Commands::Debug(hash)) => {
            #[cfg(feature = "evmsketch")]
            {
                let provider = ProviderBuilder::new().connect_http(rpc_url.clone());
                let bytes: [u8; 32] = hex::const_decode_to_array(hash.as_bytes())
                    .expect("failed to decode transaction hash");

                let receipt = provider
                    .get_transaction_receipt(bytes.into())
                    .await?
                    .expect("couldn't fetch tx receipt for tx");
                let block_number = receipt
                    .block_number
                    .expect("couldn't retrieve block number");
                let gas_used = receipt.gas_used;

                // Extract state updates
                use gas_analyzer_rs::compute_state_updates_from_tx;

                let state_updates_result =
                    compute_state_updates_from_tx(&provider, bytes.into()).await;
                let (state_updates, _skipped_opcodes, _call_gas_total) = match state_updates_result
                {
                    Ok(result) => result,
                    Err(e) => {
                        println!(
                            "{}: Could not extract state updates from trace: {:#}",
                            "Error".red(),
                            e
                        );
                        return Ok(());
                    }
                };

                // Try measured gas estimation
                use gas_analyzer_rs::core::TURETZKY_UPPER_GAS_LIMIT;
                use gas_analyzer_rs::evmsketch::GasKillerEvmSketchDefault;

                let contract_address = receipt
                    .to
                    .ok_or_else(|| anyhow::Error::msg("Transaction has no 'to' address"))?;

                let gk = GasKillerEvmSketchDefault::builder(rpc_url.clone())
                    .at_block(BlockNumberOrTag::Number(block_number - 1))
                    .build()
                    .await?;

                match gk.estimate_state_changes_gas(contract_address, &state_updates) {
                    Ok(gas) => {
                        let gas_estimate = gas + TURETZKY_UPPER_GAS_LIMIT;
                        let gas_savings = gas_used.saturating_sub(gas_estimate);
                        let percent_savings = if gas_used > 0 {
                            (gas_savings as f64 / gas_used as f64) * 100.0
                        } else {
                            0.0
                        };
                        println!(
                            "\n{}",
                            "=== Gas estimation succeeded (no debug info needed) ==="
                                .green()
                                .bold()
                        );
                        println!("Transaction: 0x{}", hex::encode(bytes));
                        println!("Gas used: {}", gas_used);
                        println!(
                            "GasKiller gas estimate: {} {}",
                            gas_estimate,
                            "(measured via StateChangeHandler)".cyan()
                        );
                        println!("Gas savings: {} ({:.2}%)", gas_savings, percent_savings);
                    }
                    Err(e) => {
                        let error_str = e.chain().last().unwrap().to_string();

                        // Check for RevertingContext error
                        let hex_match = RE_HEX.find_iter(&error_str).next();
                        let is_reverting_context = hex_match
                            .as_ref()
                            .is_some_and(|m| m.as_str().starts_with("0xaa86ecee"));

                        if !is_reverting_context {
                            println!(
                                "\n{}",
                                "=== Debug: Gas estimation failed (non-RevertingContext) ==="
                                    .red()
                                    .bold()
                            );
                            println!("{}: {:#}", "Error".red(), e);
                            return Ok(());
                        }

                        // Decode RevertingContext
                        let hex_match = hex_match.unwrap();
                        let error_bytes = hex::decode(hex_match.as_str()).unwrap();
                        let rc = RevertingContext::abi_decode(&error_bytes).unwrap();
                        let target = rc.contract;
                        let revert_data = rc.revertData.to_vec();

                        println!("\n{}", "=== Debug: RevertingContext ===".red().bold());
                        println!();

                        // Target address
                        println!("  {}: {}", "Target".bold(), target);

                        // Contract name via Etherscan
                        let contract_label = if let Some(ref api_key) = etherscan_api_key {
                            let addr_str = format!("{}", target);
                            match get_etherscan_contract_name(&addr_str, api_key).await {
                                Ok(Some(name)) => name,
                                Ok(None) => "unverified".to_string(),
                                Err(_) => "etherscan lookup failed".to_string(),
                            }
                        } else {
                            "etherscan key not set".to_string()
                        };
                        println!("  {}: {}", "Contract".bold(), contract_label);

                        // Decode revert data
                        let revert_msg = decode_revert_data(&revert_data).await;
                        println!("  {}: {}", "Revert".bold(), revert_msg.red());

                        // Find matching CALL in state_updates
                        use gas_analyzer_rs::StateUpdate;
                        let matching_call = state_updates.iter().find(|su| {
                            if let StateUpdate::Call(call) = su {
                                call.target == target
                            } else {
                                false
                            }
                        });

                        if let Some(StateUpdate::Call(call)) = matching_call {
                            // Decode function selector from callargs
                            if call.callargs.len() >= 4 {
                                let fn_selector = format!("0x{}", hex::encode(&call.callargs[..4]));
                                let fn_name = match lookup_4byte_selector(&fn_selector).await {
                                    Ok(Some(sig)) => sig,
                                    _ => format!("unknown selector {}", fn_selector),
                                };
                                println!("  {}: {}", "Function".bold(), fn_name);
                            }

                            // Trace the failing CALL via debug_traceCall
                            println!("\n{}", "=== Execution Trace ===".yellow().bold());

                            use alloy::rpc::types::eth::TransactionRequest;
                            use alloy_eips::BlockId;
                            use gas_analyzer_rs::rpc::get_trace_from_call;

                            let tx_request = TransactionRequest::default()
                                .from(contract_address)
                                .to(target)
                                .input(alloy::rpc::types::TransactionInput::new(
                                    call.callargs.clone(),
                                ));

                            match get_trace_from_call(
                                &provider,
                                tx_request,
                                BlockId::Number(alloy_eips::BlockNumberOrTag::Number(
                                    block_number - 1,
                                )),
                            )
                            .await
                            {
                                Ok(trace) => {
                                    let formatted = format_trace(&trace);
                                    if formatted.is_empty() {
                                        println!(
                                            "  {}",
                                            "(trace returned no interesting operations)".dimmed()
                                        );
                                    } else {
                                        print!("{}", formatted);
                                    }
                                }
                                Err(e) => {
                                    println!("  {}: {}", "Could not trace failing call".red(), e);
                                }
                            }
                        } else {
                            println!(
                                "\n  {}",
                                "No matching CALL found in state updates for the reverted target"
                                    .dimmed()
                            );
                        }
                    }
                }
            }

            #[cfg(not(feature = "evmsketch"))]
            {
                println!(
                    "{}",
                    "Error: Debug command requires evmsketch feature. Rebuild with --features evmsketch".red()
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
            println!("  {} debug failed gas estimation", "d/debug <HASH>".bold());
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
            println!("\n  # Debug failed estimation:");
            println!("  cargo run -- debug <TX_HASH>");
            println!("\n  # With Anvil (legacy, more accurate gas estimates):");
            println!("  cargo run --features anvil -- --anvil t <TX_HASH>");
        }
    }
    Ok(())
}

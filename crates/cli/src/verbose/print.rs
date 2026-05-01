//! Verbose-mode output rendering.
//!
//! These functions take an [`EtherscanClient`] reference and the data to
//! render. They do all the I/O and formatting; callers in `main.rs` only
//! decide *when* to invoke them.

use alloy_primitives::Bytes;
use colored::Colorize;
use gas_analyzer_core::{RevertingContext, StateUpdate};

use super::decode::{decode_call, decode_revert};
use super::etherscan::EtherscanClient;

/// Print every [`StateUpdate::Call`] with its target's ABI-decoded function
/// name and parameters. STORE / LOG updates are printed in their default
/// `Debug` representation.
pub async fn state_updates(client: &EtherscanClient, updates: &[StateUpdate]) {
    println!(
        "\n{}",
        "=== Decoded State Updates (via Etherscan) ===".green().bold()
    );
    for (i, update) in updates.iter().enumerate() {
        match update {
            StateUpdate::Call(call) => {
                println!(
                    "  [{}] CALL → {} (value={})",
                    i,
                    format!("0x{:x}", call.target).cyan(),
                    call.value
                );
                render_call_decode(client, call.target, &call.callargs, "        ").await;
            }
            other => {
                println!("  [{}] {:?}", i, other);
            }
        }
    }
    println!();
}

/// Print a [`RevertingContext`] with the failing call decoded against the
/// target's ABI and the revert data decoded as `Error(string)`, `Panic`, or
/// a custom error from the ABI when possible.
pub async fn reverting_context(client: &EtherscanClient, ctx: &RevertingContext) {
    let abi = client.fetch_abi(ctx.target).await.ok().flatten();

    println!("   {}", "── Failing call ──".red().bold());
    if let Some(abi) = abi.as_ref()
        && let Some(decoded) = decode_call(abi, &ctx.callargs)
    {
        println!("   sig: {}", decoded.signature.dimmed());
        println!("   fn:  {}", decoded.name.bold());
        for (param, val) in &decoded.args {
            println!("     {} = {}", param.dimmed(), val);
        }
    } else if ctx.callargs.len() >= 4 {
        let sel = hex::encode(&ctx.callargs[..4]);
        println!(
            "   selector 0x{} (not in ABI; try openchain.xyz)",
            sel
        );
        println!("   callargs: 0x{}", hex::encode(&ctx.callargs));
    }

    println!(
        "   revertData ({} bytes): 0x{}",
        ctx.revertData.len(),
        hex::encode(&ctx.revertData)
    );
    if !ctx.revertData.is_empty()
        && let Some(decoded) = decode_revert(abi.as_ref(), &ctx.revertData)
    {
        println!("   Revert: {}", decoded.red().bold());
    }
}

async fn render_call_decode(
    client: &EtherscanClient,
    target: alloy_primitives::Address,
    callargs: &Bytes,
    indent: &str,
) {
    match client.fetch_abi(target).await {
        Ok(Some(abi)) => match decode_call(&abi, callargs) {
            Some(decoded) => {
                println!("{indent}sig: {}", decoded.signature.dimmed());
                println!("{indent}fn:  {}", decoded.name.bold());
                for (param, val) in &decoded.args {
                    println!("{indent}  {} = {}", param.dimmed(), val);
                }
            }
            None => {
                let sel = if callargs.len() >= 4 {
                    hex::encode(&callargs[..4])
                } else {
                    "<short>".to_string()
                };
                println!(
                    "{indent}{} (selector 0x{sel} not in ABI)",
                    "<unknown function>".yellow()
                );
            }
        },
        Ok(None) => {
            println!(
                "{indent}{}",
                "<contract not verified on Etherscan>".yellow()
            );
        }
        Err(e) => {
            println!("{indent}{} {e}", "etherscan error:".red());
        }
    }
}

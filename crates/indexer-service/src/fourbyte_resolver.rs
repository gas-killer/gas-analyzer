//! Periodic 4byte.directory resolver. Picks the N most-frequent unresolved
//! function selectors out of `analysis`, queries 4byte for each, and writes
//! the result into `function_selectors`.
//!
//! A miss (4byte has no entry) is still recorded with `source='unresolved'`
//! so we don't re-query it every cycle.

use std::time::Duration;

use indexer_resolver::fourbyte::FourByteClient;
use indexer_store::Store;
use tokio::time::{interval, sleep};

use crate::config::{CommonConfig, RefresherConfig};

pub async fn run(common: CommonConfig, cfg: RefresherConfig, store: Store) {
    let client = match FourByteClient::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "4byte resolver disabled (client init failed)");
            return;
        }
    };

    let batch_size = cfg.fourbyte_batch_size.max(1);
    let tick_secs = cfg.fourbyte_tick_secs.max(60);
    let per_req_delay = Duration::from_millis(cfg.fourbyte_per_req_delay_ms.max(50));

    tracing::info!(batch_size, tick_secs, "4byte resolver enabled");

    let mut ticker = interval(Duration::from_secs(tick_secs));
    loop {
        ticker.tick().await;
        if let Err(e) = run_once(&common, &store, &client, batch_size, per_req_delay).await {
            tracing::warn!(error = %e, "4byte resolver tick failed");
        }
    }
}

async fn run_once(
    common: &CommonConfig,
    store: &Store,
    client: &FourByteClient,
    batch_size: i64,
    per_req_delay: Duration,
) -> Result<(), anyhow::Error> {
    let selectors = store
        .unresolved_selectors(common.chain_id, batch_size)
        .await
        .map_err(|e| anyhow::anyhow!("unresolved_selectors query failed: {e}"))?;

    if selectors.is_empty() {
        tracing::debug!("4byte resolver: no unresolved selectors");
        return Ok(());
    }

    tracing::info!(count = selectors.len(), "4byte resolver: batch fetch");
    let mut resolved = 0u64;
    let mut unresolved = 0u64;
    for selector in &selectors {
        match client.lookup(selector).await {
            Ok(Some(r)) => {
                let _ = store
                    .upsert_function_selector(
                        r.selector,
                        Some(&r.primary_name),
                        Some(&r.primary_sig),
                        &r.all_signatures,
                        "fourbyte",
                    )
                    .await;
                resolved += 1;
            }
            Ok(None) => {
                let _ = store
                    .upsert_function_selector(*selector, None, None, &[], "unresolved")
                    .await;
                unresolved += 1;
            }
            Err(e) => {
                tracing::warn!(
                    selector = %hex::encode(selector),
                    error = %e,
                    "4byte lookup failed"
                );
                // Don't mark as unresolved — transport errors retry next tick.
            }
        }
        sleep(per_req_delay).await;
    }
    tracing::info!(resolved, unresolved, "4byte resolver: batch complete");
    Ok(())
}

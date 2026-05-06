//! Periodic background tasks:
//!   - Resolver refresh (overlay + DefiLlama) — every 24h.
//!   - ETH/USD price snapshot — every 1h.
//!   - `project_daily` materialized view refresh — every 1h.
//!
//! Three independent loops driven by `tokio::time::interval`. Each tick is
//! best-effort: failures log and move on without bringing the binary down.

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bigdecimal::BigDecimal;
use chrono::Utc;
use indexer_resolver::Resolver;
use indexer_store::{Project, Store};
use serde::Deserialize;
use tokio::time::interval;

use crate::config::{CommonConfig, RefresherConfig};

pub async fn run(common: CommonConfig, cfg: RefresherConfig) -> Result<()> {
    let store = Store::connect(&common.database_url, 4).await?;
    store.migrate().await?;
    let resolver = Arc::new(Resolver::new());

    // Initial pass synchronously so we don't start with empty state.
    refresh_resolver_into_store(&resolver, &store, &common).await;
    refresh_eth_price(&store, &common).await;

    let resolver_secs = cfg.resolver_refresh_secs;
    let price_secs = cfg.price_refresh_secs;
    let rollup_secs = cfg.rollup_refresh_secs;

    let store_a = store.clone();
    let common_a = common.clone();
    let resolver_a = resolver.clone();
    let resolver_loop = tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(resolver_secs));
        // first tick fires immediately; we already did the initial pass.
        tick.tick().await;
        loop {
            tick.tick().await;
            refresh_resolver_into_store(&resolver_a, &store_a, &common_a).await;
        }
    });

    let store_b = store.clone();
    let common_b = common.clone();
    let price_loop = tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(price_secs));
        tick.tick().await;
        loop {
            tick.tick().await;
            refresh_eth_price(&store_b, &common_b).await;
        }
    });

    let store_c = store.clone();
    let rollup_loop = tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(rollup_secs));
        tick.tick().await;
        loop {
            tick.tick().await;
            if let Err(e) = store_c.refresh_rollups().await {
                tracing::warn!(error = %e, "rollup refresh failed");
            } else {
                tracing::info!("rollups refreshed");
            }
        }
    });

    let _ = tokio::try_join!(resolver_loop, price_loop, rollup_loop);
    Ok(())
}

async fn refresh_resolver_into_store(
    resolver: &Arc<Resolver>,
    store: &Store,
    common: &CommonConfig,
) {
    let defillama = if common.defillama_url.is_empty() {
        None
    } else {
        Some(common.defillama_url.as_str())
    };
    let overlay_path = if common.overlay_path.exists() {
        Some(common.overlay_path.as_path())
    } else {
        None
    };
    if let Err(e) = resolver.refresh(overlay_path, defillama).await {
        tracing::warn!(error = %e, "resolver refresh failed");
        return;
    }
    let snapshot = resolver.snapshot();
    let mut project_count = 0;
    for info in snapshot.projects() {
        let project = Project {
            slug: info.slug.clone(),
            name: info.name.clone(),
            category: info.category.clone(),
            contact_email: info.contact_email.clone(),
            contact_url: info.contact_url.clone(),
        };
        if let Err(e) = store.upsert_project(&project).await {
            tracing::warn!(error = %e, slug = info.slug, "upsert_project failed");
        } else {
            project_count += 1;
        }
    }

    // Persist resolver addresses (overlay + DefiLlama). The upsert is idempotent
    // and uses ON CONFLICT DO UPDATE — so a real slug always overwrites a prior
    // synthetic `unknown:*` entry on the same address.
    let mut address_count = 0u64;
    for (chain_id, addr, info) in snapshot.addresses() {
        if let Err(e) = store
            .upsert_address_project(chain_id, addr, &info.slug)
            .await
        {
            tracing::warn!(
                error = %e,
                slug = info.slug,
                addr = %hex::encode(addr),
                "upsert_address_project failed"
            );
        } else {
            address_count += 1;
        }
    }

    // Retro-relabel historical `analysis` rows that were written with the
    // synthetic `unknown:0xADDR` slug but now have a real mapping. Without
    // this, the leaderboard would only ever reflect *new* analyses post-refresh.
    let relabeled = match store.relabel_unknowns().await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "relabel_unknowns failed");
            0
        }
    };

    tracing::info!(
        projects = project_count,
        addresses = address_count,
        relabeled,
        "resolver refreshed"
    );
}

#[derive(Deserialize)]
struct CoingeckoResp {
    ethereum: CoingeckoEth,
}

#[derive(Deserialize)]
struct CoingeckoEth {
    usd: f64,
}

async fn refresh_eth_price(store: &Store, common: &CommonConfig) {
    if common.price_url.is_empty() {
        return;
    }
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "price client build failed");
            return;
        }
    };
    let resp: CoingeckoResp = match client
        .get(&common.price_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(r) => match r.json().await {
            Ok(j) => j,
            Err(e) => {
                tracing::warn!(error = %e, "price json decode failed");
                return;
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "price fetch failed");
            return;
        }
    };
    let price = BigDecimal::from_str(&format!("{:.8}", resp.ethereum.usd))
        .unwrap_or_else(|_| BigDecimal::from(0));
    let day = Utc::now().date_naive();
    if let Err(e) = store.upsert_eth_price(day, price.clone()).await {
        tracing::warn!(error = %e, "upsert_eth_price failed");
    } else {
        tracing::info!(day = %day, usd = ?price, "eth price stored");
    }
}

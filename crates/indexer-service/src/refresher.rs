//! Periodic background tasks:
//!   - Resolver refresh (overlay + DefiLlama) — every 24h.
//!   - ETH/USD price snapshot — every 1h.
//!   - `project_daily` materialized view refresh — every 1h.
//!
//! Three independent loops driven by `tokio::time::interval`. Each tick is
//! best-effort: failures log and move on without bringing the binary down.

use std::path::Path;
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

/// Counts returned by `refresh_resolver_now` so callers (loop, admin button)
/// can report what changed.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResolverRefreshOutcome {
    pub projects: u64,
    pub addresses: u64,
    pub relabeled: u64,
}

pub async fn run(common: CommonConfig, cfg: RefresherConfig) -> Result<()> {
    let store = Store::connect(&common.database_url, 4).await?;
    store.migrate().await?;
    let resolver = Arc::new(Resolver::new());

    // Initial pass synchronously so we don't start with empty state. The
    // rollup refresh in particular matters: dashboards read from the
    // materialized view, so without this the UI is stale until the first
    // tick fires (1h after startup).
    refresh_resolver_into_store(&resolver, &store, &common).await;
    refresh_eth_price(&store, &common).await;
    if let Err(e) = store.refresh_rollups().await {
        tracing::warn!(error = %e, "initial rollup refresh failed");
    } else {
        tracing::info!("initial rollups refreshed");
    }

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

    let store_d = store.clone();
    let common_d = common.clone();
    let cfg_d = cfg.clone();
    let labeler_loop = tokio::spawn(async move {
        crate::labeler::run(common_d, cfg_d, store_d).await;
    });

    let store_e = store.clone();
    let common_e = common.clone();
    let cfg_e = cfg.clone();
    let fourbyte_loop = tokio::spawn(async move {
        crate::fourbyte_resolver::run(common_e, cfg_e, store_e).await;
    });

    let _ = tokio::try_join!(resolver_loop, price_loop, rollup_loop, labeler_loop, fourbyte_loop);
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
    let _ = refresh_resolver_with(resolver, store, overlay_path, defillama).await;
}

/// Run one resolver-refresh cycle: reload the overlay + DefiLlama into the
/// passed-in `Resolver`, then persist projects + addresses to Postgres and
/// retro-relabel historical `analysis` rows. Public so admin endpoints can
/// trigger it on demand without going through the internal loop.
pub async fn refresh_resolver_with(
    resolver: &Arc<Resolver>,
    store: &Store,
    overlay_path: Option<&Path>,
    defillama_url: Option<&str>,
) -> ResolverRefreshOutcome {
    if let Err(e) = resolver.refresh(overlay_path, defillama_url).await {
        tracing::warn!(error = %e, "resolver refresh failed");
        return ResolverRefreshOutcome::default();
    }
    let snapshot = resolver.snapshot();
    let mut project_count = 0u64;
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
    ResolverRefreshOutcome {
        projects: project_count,
        addresses: address_count,
        relabeled,
    }
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
    if !common.price_url.is_empty() {
        let _ = refresh_eth_price_now(store, &common.price_url).await;
    }
}

/// Public entry point for an on-demand ETH/USD price refresh. Returns
/// `Ok(usd_per_eth)` on success so the caller can show "stored $X" in a
/// status banner.
pub async fn refresh_eth_price_now(store: &Store, price_url: &str) -> Result<BigDecimal> {
    if price_url.is_empty() {
        return Err(anyhow::anyhow!("price_url is empty"));
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| anyhow::anyhow!("price client build failed: {e}"))?;
    let resp: CoingeckoResp = client
        .get(price_url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| anyhow::anyhow!("price fetch failed: {e}"))?
        .json()
        .await
        .map_err(|e| anyhow::anyhow!("price json decode failed: {e}"))?;
    let price = BigDecimal::from_str(&format!("{:.8}", resp.ethereum.usd))
        .unwrap_or_else(|_| BigDecimal::from(0));
    let day = Utc::now().date_naive();
    store
        .upsert_eth_price(day, price.clone())
        .await
        .map_err(|e| anyhow::anyhow!("upsert_eth_price failed: {e}"))?;
    tracing::info!(day = %day, usd = ?price, "eth price stored");
    Ok(price)
}

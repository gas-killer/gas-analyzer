//! Auto-labeler: turns `unknown:0xADDR` rows into real project slugs by
//! looking up verified contract names on Etherscan, then heuristically
//! mapping them to a slug via the curated `known_names.yaml` dictionary.
//!
//! Two cooperating tasks:
//!   - Producer: every `producer_interval`, rebuilds a Redis sorted set
//!     keyed by `wei_saved_total` so highest-savings unknowns get processed
//!     first. Skips addresses we already attempted recently.
//!   - Consumer: continuous loop. ZPOPMAX one address, hits Etherscan, runs
//!     the name→slug heuristic, persists the outcome (and retro-relabels
//!     historical analyses on success).
//!
//! On consumer success, `Store::relabel_unknowns()` runs once — that one
//! statement fixes every historical `analysis` row for the newly-mapped
//! address in a single UPDATE.
//!
//! Failure modes are deliberately distinct in the attempt log so the
//! producer query knows what's worth re-trying:
//!   - `matched`     — name resolved to a real slug, address_project written
//!   - `unverified`  — Etherscan has no source for this contract
//!   - `no-match`    — verified, but the name didn't map to any slug
//!   - `error`       — transport failure; will retry on next producer cycle

use std::sync::Arc;
use std::time::Duration;

use indexer_resolver::blockscout::BlockscoutClient;
use indexer_resolver::etherscan::{ContractMeta, EtherscanClient, NameDict};
use indexer_store::Store;
use redis::AsyncCommands;
use tokio::time::{Instant, sleep, sleep_until};

use crate::config::{CommonConfig, RefresherConfig};

const QUEUE_KEY: &str = "labeler:queue";

pub async fn run(common: CommonConfig, cfg: RefresherConfig, store: Store) {
    if cfg.etherscan_api_key.is_empty() {
        tracing::info!("labeler disabled (ETHERSCAN_API_KEY not set)");
        return;
    }

    let client = match EtherscanClient::new(cfg.etherscan_api_key.clone(), common.chain_id) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::warn!(error = %e, "labeler disabled (etherscan client init failed)");
            return;
        }
    };

    // Blockscout is best-effort — its absence shouldn't disable the labeler.
    let blockscout: Option<Arc<BlockscoutClient>> = if cfg.blockscout_url.is_empty() {
        None
    } else {
        match BlockscoutClient::new(cfg.blockscout_url.clone()) {
            Ok(c) => Some(Arc::new(c)),
            Err(e) => {
                tracing::warn!(error = %e, "blockscout disabled (client init failed)");
                None
            }
        }
    };

    let names = match NameDict::load(cfg.known_names_path.as_path()).await {
        Ok(n) => Arc::new(n),
        Err(e) => {
            tracing::warn!(error = %e, "name dict load failed; using empty dict");
            Arc::new(NameDict::default())
        }
    };

    let redis_client = match redis::Client::open(common.redis_url.as_str()) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "labeler disabled (redis client init failed)");
            return;
        }
    };

    let conn_for_producer = match redis_client.get_multiplexed_async_connection().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "labeler disabled (redis connect failed)");
            return;
        }
    };
    let conn_for_consumer = conn_for_producer.clone();

    tracing::info!(
        producer_interval_secs = cfg.labeler_producer_interval_secs,
        batch_size = cfg.labeler_batch_size,
        "labeler enabled"
    );

    let store_a = store.clone();
    let cfg_a = cfg.clone();
    let common_a = common.clone();
    let producer = tokio::spawn(async move {
        producer_loop(common_a, cfg_a, store_a, conn_for_producer).await;
    });

    let store_b = store.clone();
    let cfg_b = cfg.clone();
    let common_b = common.clone();
    let blockscout_b = blockscout.clone();
    let consumer = tokio::spawn(async move {
        consumer_loop(
            common_b,
            cfg_b,
            store_b,
            conn_for_consumer,
            client,
            blockscout_b,
            names,
        )
        .await;
    });

    let _ = tokio::join!(producer, consumer);
}

async fn producer_loop(
    common: CommonConfig,
    cfg: RefresherConfig,
    store: Store,
    mut conn: redis::aio::MultiplexedConnection,
) {
    let interval = Duration::from_secs(cfg.labeler_producer_interval_secs.max(60));
    loop {
        let _ = producer_tick_once(
            &store,
            &mut conn,
            common.chain_id,
            cfg.labeler_batch_size,
            cfg.labeler_retry_days,
        )
        .await;
        sleep(interval).await;
    }
}

/// Run a single producer cycle: query top-N unknowns by `wei_saved` and
/// `ZADD` them into the priority queue. Public so admin endpoints can
/// trigger a fresh tick without waiting for the next interval. Returns
/// `(pushed, depth_after)` for status reporting.
///
/// Generic over the Redis connection so callers can pass either the
/// internal `MultiplexedConnection` or the web crate's `ConnectionManager`.
pub async fn producer_tick_once<C>(
    store: &Store,
    conn: &mut C,
    chain_id: u64,
    batch_size: i64,
    retry_days: i64,
) -> Result<(usize, i64), anyhow::Error>
where
    C: redis::aio::ConnectionLike + Send,
{
    let rows = store
        .top_unknown_addresses(chain_id, batch_size, retry_days)
        .await
        .map_err(|e| anyhow::anyhow!("labeler producer query failed: {e}"))?;
    if rows.is_empty() {
        tracing::info!("labeler producer: no unknown addresses to label");
        let depth: i64 = conn.zcard(QUEUE_KEY).await.unwrap_or(-1);
        return Ok((0, depth));
    }
    let mut pushed = 0usize;
    for row in rows {
        let score: f64 = bd_to_f64(&row.wei_saved_total);
        let member = hex::encode(&row.address);
        let res: redis::RedisResult<i64> = conn.zadd(QUEUE_KEY, member, score).await;
        match res {
            Ok(_) => pushed += 1,
            Err(e) => {
                tracing::warn!(error = %e, "labeler ZADD failed");
                break;
            }
        }
    }
    let depth: i64 = conn.zcard(QUEUE_KEY).await.unwrap_or(-1);
    tracing::info!(pushed, depth, "labeler producer tick");
    Ok((pushed, depth))
}

async fn consumer_loop(
    common: CommonConfig,
    cfg: RefresherConfig,
    store: Store,
    mut conn: redis::aio::MultiplexedConnection,
    client: Arc<EtherscanClient>,
    blockscout: Option<Arc<BlockscoutClient>>,
    names: Arc<NameDict>,
) {
    let min_delay = Duration::from_millis(cfg.labeler_min_delay_ms.max(50));
    loop {
        let next_after = Instant::now() + min_delay;

        // ZPOPMAX returns Vec<(member, score)>. We only ask for one at a time.
        let popped: Vec<(String, f64)> = match conn.zpopmax(QUEUE_KEY, 1).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "labeler ZPOPMAX failed");
                sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        let Some((member, score)) = popped.into_iter().next() else {
            // Empty queue; idle until the producer refills it.
            sleep(Duration::from_secs(30)).await;
            continue;
        };

        let Some(addr) = parse_addr_member(&member) else {
            tracing::warn!(member, "labeler: malformed queue entry, skipping");
            continue;
        };

        process_address(
            &common,
            &store,
            &client,
            blockscout.as_deref(),
            &names,
            addr,
            score,
        )
        .await;
        sleep_until(next_after).await;
    }
}

/// Single-address label attempt: fetch the contract name from Etherscan,
/// resolve it via the dictionary or DefiLlama-derived slugs in `projects`,
/// and persist the outcome. Always records an attempt row so the producer
/// won't immediately re-queue the same address.
async fn process_address(
    common: &CommonConfig,
    store: &Store,
    client: &EtherscanClient,
    blockscout: Option<&BlockscoutClient>,
    names: &NameDict,
    addr: [u8; 20],
    score: f64,
) {
    let meta = match client.get_contract_name(&addr).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(addr = %hex::encode(addr), error = %e, "labeler etherscan fetch failed");
            // Try Blockscout before declaring transport failure — independent
            // upstream may still answer.
            if let Some(bs) = blockscout {
                match bs.get_contract_name(&addr).await {
                    Ok(m) => m,
                    Err(e2) => {
                        tracing::warn!(addr = %hex::encode(addr), error = %e2, "labeler blockscout fallback failed");
                        let _ = store
                            .upsert_label_attempt(common.chain_id, addr, "error", None, None)
                            .await;
                        return;
                    }
                }
            } else {
                let _ = store
                    .upsert_label_attempt(common.chain_id, addr, "error", None, None)
                    .await;
                return;
            }
        }
    };

    let (name, _impl) = match meta {
        ContractMeta::Verified {
            name,
            implementation,
            ..
        } => (name, implementation),
        ContractMeta::Unverified => {
            // Fall through to Blockscout before giving up — Etherscan and
            // Blockscout often disagree on which contracts are "verified".
            let bs_meta = if let Some(bs) = blockscout {
                bs.get_contract_name(&addr).await.ok()
            } else {
                None
            };
            match bs_meta {
                Some(ContractMeta::Verified {
                    name,
                    implementation,
                    ..
                }) => (name, implementation),
                _ => {
                    tracing::debug!(addr = %hex::encode(addr), score, "labeler: unverified");
                    let _ = store
                        .upsert_label_attempt(common.chain_id, addr, "unverified", None, None)
                        .await;
                    return;
                }
            }
        }
    };

    let Some(slug) = names.lookup(&name) else {
        tracing::debug!(
            addr = %hex::encode(addr),
            contract_name = name.as_str(),
            "labeler: no slug match"
        );
        let _ = store
            .upsert_label_attempt(common.chain_id, addr, "no-match", Some(&name), None)
            .await;
        return;
    };

    // The static dictionary may map a contract name to a slug that DefiLlama
    // doesn't expose (e.g. `mev-blocker`). Ensure a `projects` row exists
    // first so the FK from `address_project.project_slug` resolves.
    if let Err(e) = store
        .upsert_project(&indexer_store::Project {
            slug: slug.to_string(),
            name: name.clone(),
            category: None,
            contact_email: None,
            contact_url: None,
        })
        .await
    {
        tracing::warn!(error = %e, slug, "labeler placeholder upsert_project failed");
        let _ = store
            .upsert_label_attempt(common.chain_id, addr, "error", Some(&name), Some(slug))
            .await;
        return;
    }

    // Persist the mapping. ON CONFLICT DO UPDATE replaces a synthetic
    // `unknown:*` entry with the real slug.
    if let Err(e) = store
        .upsert_address_project(common.chain_id, addr, slug)
        .await
    {
        tracing::warn!(error = %e, "labeler upsert_address_project failed");
        let _ = store
            .upsert_label_attempt(common.chain_id, addr, "error", Some(&name), Some(slug))
            .await;
        return;
    }

    // Retro-relabel every historical analysis row for this address. One
    // UPDATE per success — cheap given the indexed JOIN.
    let relabeled = store.relabel_unknowns().await.unwrap_or(0);

    let _ = store
        .upsert_label_attempt(common.chain_id, addr, "matched", Some(&name), Some(slug))
        .await;

    tracing::info!(
        addr = %hex::encode(addr),
        contract_name = name.as_str(),
        slug,
        relabeled,
        "labeled"
    );
}

fn parse_addr_member(member: &str) -> Option<[u8; 20]> {
    let bytes = hex::decode(member).ok()?;
    if bytes.len() != 20 {
        return None;
    }
    let mut out = [0u8; 20];
    out.copy_from_slice(&bytes);
    Some(out)
}

/// Lossy convert BigDecimal → f64 for ZSET scoring. Precision loss at the
/// least-significant bits is acceptable — we only need ordering, and even
/// 1 wei differences don't matter for "label this first" decisions.
fn bd_to_f64(bd: &bigdecimal::BigDecimal) -> f64 {
    use std::str::FromStr;
    f64::from_str(&bd.to_string()).unwrap_or(0.0)
}

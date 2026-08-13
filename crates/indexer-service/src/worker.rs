//! Job consumer: pulls `AnalyzeTxJob` from Redis, runs the analyzer, persists.

use std::sync::Arc;
use std::time::Duration;

use alloy::primitives::FixedBytes;
use anyhow::Result;
use indexer_api::{Analyzer, AnalyzerConfig, AnalyzerError, EvmSketchAnalyzer};
use indexer_resolver::Resolver;
use indexer_rpc::{RateLimiter, weights};
use indexer_store::Store;

use crate::config::{CommonConfig, WorkerConfig};
use crate::queue::{AnalyzeTxJob, Queue};

pub async fn run(common: CommonConfig, cfg: WorkerConfig) -> Result<()> {
    let limiter = RateLimiter::new(indexer_rpc::RateLimiterConfig {
        rps_budget: common.rpc_rps_budget,
        burst: common.rpc_burst,
        max_concurrency: common.rpc_max_concurrency,
    });

    let analyzer = Arc::new(EvmSketchAnalyzer::new(
        common.rpc_url.clone(),
        AnalyzerConfig {
            chain_id: common.chain_id,
            min_gas_used: common.min_gas_used,
            heuristic_only: common.heuristic_only,
        },
    ));

    let resolver = Arc::new(Resolver::new());
    // Initial overlay load — failures here aren't fatal, the worker resolves
    // everything as `unknown:*` until the refresher catches up.
    if let Err(e) = resolver
        .refresh(Some(common.overlay_path.as_path()), None)
        .await
    {
        tracing::warn!(error = %e, "initial resolver refresh failed");
    }

    let store = Store::connect(&common.database_url, 8).await?;
    store.migrate().await?;

    let queue = Queue::connect(&common.redis_url).await?;

    // Heuristic-only analyses skip the replay/fork RPC volume, so charge the
    // slimmer weight — at the full ANALYZE_TX cost the limiter would pace the
    // cheap mode as if it were expensive, eating most of the quota win (#162).
    let analyze_weight = if common.heuristic_only {
        weights::HEURISTIC_ANALYZE_TX
    } else {
        weights::ANALYZE_TX
    };

    tracing::info!("worker ready");

    let ttl_secs = cfg.queue_job_ttl_secs as i64;
    let mut expired_dropped: u64 = 0;

    loop {
        let job = match queue.claim(Duration::from_secs(5)).await? {
            Some(j) => j,
            None => continue,
        };

        // Enqueue outpaces drain whenever analysis capacity trails chain
        // volume, so stale jobs are dropped at claim time instead of being
        // analyzed arbitrarily late. Checked before `acquire` so expired
        // jobs never spend rate-limiter budget.
        if ttl_secs > 0 {
            let expired = job
                .age_secs(chrono::Utc::now().timestamp())
                .is_some_and(|age| age > ttl_secs);
            if expired {
                expired_dropped += 1;
                if expired_dropped.is_multiple_of(1000) {
                    tracing::info!(total = expired_dropped, "expired jobs dropped");
                }
                continue;
            }
        }

        let _permit = limiter.acquire(analyze_weight).await;
        if let Err(e) = handle(&job, &analyzer, &resolver, &store, &cfg, &queue).await {
            tracing::error!(?job, error = %e, "job handling failed");
        }
    }
}

async fn handle(
    job: &AnalyzeTxJob,
    analyzer: &Arc<EvmSketchAnalyzer>,
    resolver: &Arc<Resolver>,
    store: &Store,
    cfg: &WorkerConfig,
    queue: &Queue,
) -> Result<()> {
    let hash = FixedBytes::<32>::from(job.tx_hash);

    // Backstop: an individual analysis must complete in a bounded time.
    // Without this, a hung HTTP read pins the worker forever (alloy's default
    // transport has no read timeout). On timeout we requeue.
    let analyze = tokio::time::timeout(
        Duration::from_secs(cfg.analyze_timeout_secs),
        analyzer.analyze_tx(hash),
    )
    .await
    .unwrap_or_else(|_| {
        Err(AnalyzerError::Rpc(format!(
            "analyze_tx timed out after {}s",
            cfg.analyze_timeout_secs
        )))
    });

    match analyze {
        Ok(report) => {
            let project = resolver.resolve(report.chain_id, report.to);
            store.insert_analysis(&report, &project.slug).await?;
            // Best-effort upsert — first time we see a project, register it.
            store
                .upsert_project(&indexer_store::Project {
                    slug: project.slug.clone(),
                    name: project.name,
                    category: project.category,
                    contact_email: project.contact_email,
                    contact_url: project.contact_url,
                })
                .await?;
            store
                .upsert_address_project(report.chain_id, report.to, &project.slug)
                .await?;
            tracing::debug!(
                tx = %hex::encode(report.tx_hash),
                project = project.slug,
                gas_saved = report.gas_saved,
                "persisted"
            );
            Ok(())
        }
        Err(AnalyzerError::Skipped(reason)) => {
            tracing::debug!(tx = %hex::encode(job.tx_hash), %reason, "skipped");
            Ok(())
        }
        Err(e) => {
            // Transient-vs-permanent: we treat all non-skip errors as
            // potentially transient, retry up to `max_retries`, then
            // dead-letter.
            if job.attempt + 1 < cfg.max_retries {
                let mut retried = job.clone();
                retried.attempt += 1;
                queue.requeue(&retried).await?;
                tracing::warn!(
                    tx = %hex::encode(job.tx_hash),
                    attempt = retried.attempt,
                    error = %e,
                    "requeued"
                );
            } else {
                queue.dead_letter(job, &e.to_string()).await?;
                tracing::error!(
                    tx = %hex::encode(job.tx_hash),
                    error = %e,
                    "dead-lettered after exhausting retries"
                );
            }
            Ok(())
        }
    }
}

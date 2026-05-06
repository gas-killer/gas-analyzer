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

    tracing::info!("worker ready");

    loop {
        let job = match queue.claim(Duration::from_secs(5)).await? {
            Some(j) => j,
            None => continue,
        };

        if let Err(e) = handle(&job, &analyzer, &resolver, &store, &limiter, &cfg, &queue).await {
            tracing::error!(?job, error = %e, "job handling failed");
        }
    }
}

async fn handle(
    job: &AnalyzeTxJob,
    analyzer: &Arc<EvmSketchAnalyzer>,
    resolver: &Arc<Resolver>,
    store: &Store,
    limiter: &RateLimiter,
    cfg: &WorkerConfig,
    queue: &Queue,
) -> Result<()> {
    let _permit = limiter.acquire(weights::ANALYZE_TX).await;
    let hash = FixedBytes::<32>::from(job.tx_hash);

    match analyzer.analyze_tx(hash).await {
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

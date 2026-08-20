//! Polls the chain head, fans out one job per qualifying tx into Redis.
//!
//! Backpressure: if the queue depth exceeds `max_queue_depth`, we stop
//! enqueueing new blocks and wait for the workers to drain. By default we
//! never *drop* blocks — we just lag, then catch up (comprehensiveness >
//! speed). When analysis capacity permanently trails chain volume (e.g.
//! heuristic-only workers on a busy chain), `max_blocks_behind` bounds that
//! lag instead: the cursor skips forward and the intermediate blocks are
//! dropped, keeping analyses near head.

use std::time::Duration;

use alloy::primitives::FixedBytes;
use alloy::providers::{ProviderBuilder, RootProvider};
use alloy_provider::Provider;
use alloy_rpc_types::TransactionTrait;
use anyhow::{Context, Result};
use indexer_rpc::{RateLimiter, weights};
use tokio::time::sleep;

use crate::config::{CommonConfig, HeadTrackerConfig};
use crate::queue::{AnalyzeTxJob, Queue};
use crate::state;

pub async fn run(common: CommonConfig, cfg: HeadTrackerConfig) -> Result<()> {
    let provider = ProviderBuilder::new()
        .disable_recommended_fillers()
        .connect_http(common.rpc_url.clone());

    let limiter = RateLimiter::new(indexer_rpc::RateLimiterConfig {
        rps_budget: common.rpc_rps_budget,
        burst: common.rpc_burst,
        max_concurrency: common.rpc_max_concurrency,
    });

    let queue = Queue::connect(&common.redis_url)
        .await
        .context("connect redis")?;

    // Start at current head — live-only mode, never backfill.
    let mut next_block: u64 = {
        let _p = limiter.acquire(weights::HEAD_POLL).await;
        provider
            .get_block_number()
            .await
            .context("get_block_number")?
            + 1
    };
    tracing::info!(start_block = next_block, "head-tracker starting");

    let mut skipped_blocks: u64 = 0;

    loop {
        // Probe head every iteration so the admin health view sees a live
        // last-head value even when we're throttled by backpressure.
        let head = {
            let _p = limiter.acquire(weights::HEAD_POLL).await;
            provider
                .get_block_number()
                .await
                .context("get_block_number")?
        };
        if let Err(e) = queue.publish_last_head(head).await {
            tracing::warn!(error = %e, "publish_last_head failed");
        }

        // Backpressure check.
        let depth = queue.depth().await.context("queue depth")?;
        if depth > cfg.max_queue_depth {
            tracing::warn!(depth, "queue saturated, sleeping");
            sleep(Duration::from_millis(cfg.head_poll_ms.max(2000))).await;
            continue;
        }

        // Bounded lag: skip ahead rather than grind through a backlog of
        // ever-staler blocks (0 = disabled, never drop).
        if cfg.max_blocks_behind > 0 && head.saturating_sub(next_block) > cfg.max_blocks_behind {
            let new_start = head - cfg.max_blocks_behind;
            let skipped = new_start - next_block;
            skipped_blocks += skipped;
            tracing::warn!(
                skipped,
                total_skipped = skipped_blocks,
                from = next_block,
                to = new_start,
                "lag exceeds max_blocks_behind, skipping ahead"
            );
            // Publish the running total so the completeness gap this mode
            // creates is readable on /admin, not just inferable from logs.
            // Best-effort: losing the bookkeeping mustn't stall the cursor.
            if let Err(e) = queue.incr_dropped(state::SKIPPED_BLOCKS_KEY, skipped).await {
                tracing::warn!(error = %e, "publishing skipped-block count failed");
            }
            next_block = new_start;
        }

        if head < next_block {
            sleep(Duration::from_millis(cfg.head_poll_ms)).await;
            continue;
        }

        // Catch up one block at a time. The worker pool — not the head-tracker
        // — handles per-tx throughput.
        let block_n = next_block;
        if let Err(e) = enqueue_block(&provider, &limiter, &queue, common.chain_id, block_n).await {
            // Transient failure: log and retry the same block on next iteration.
            tracing::warn!(block_n, error = %e, "enqueue_block failed, will retry");
            sleep(Duration::from_millis(2000)).await;
            continue;
        }
        next_block = block_n + 1;
    }
}

async fn enqueue_block(
    provider: &RootProvider,
    limiter: &RateLimiter,
    queue: &Queue,
    chain_id: u64,
    block_number: u64,
) -> Result<()> {
    let block = {
        let _p = limiter.acquire(weights::BLOCK_FULL).await;
        provider
            .get_block_by_number(block_number.into())
            .full()
            .await
            .context("get_block_by_number")?
            .ok_or_else(|| anyhow::anyhow!("block {block_number} not found"))?
    };

    let mut enqueued = 0u32;
    for (idx, tx) in block.transactions.into_transactions().enumerate() {
        let hash: FixedBytes<32> = *tx.inner.tx_hash();
        // Cheap filter: skip create txs at enqueue time so we don't spend a
        // worker slot on them. The full skip-decision tree (gas threshold,
        // reverted, etc.) still runs in `EvmSketchAnalyzer::analyze_tx`.
        if tx.inner.to().is_none() {
            continue;
        }
        let job = AnalyzeTxJob {
            chain_id,
            tx_hash: hash.into(),
            block_number,
            tx_index: idx as u64,
            attempt: 0,
            enqueued_at: chrono::Utc::now().timestamp(),
        };
        queue.enqueue(&job).await.context("enqueue job")?;
        enqueued += 1;
    }
    tracing::info!(block_number, enqueued, "block fanned out");
    Ok(())
}

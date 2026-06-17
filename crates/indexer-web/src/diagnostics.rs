//! Builds a structured operational snapshot of the indexer for the AI
//! diagnostics button. Pulls counters from Postgres + Redis, plus the most
//! recent error events from the Redis stream that services publish into.
//!
//! Everything is best-effort — a partial bundle is more useful to the LLM
//! than no bundle, so individual fetch failures degrade gracefully.

use redis::AsyncCommands;
use serde::Serialize;
use sqlx::PgPool;

use crate::AppState;
use crate::queries;

/// One Postgres + Redis snapshot, serializable as JSON for the LLM prompt.
#[derive(Debug, Serialize)]
pub struct DiagnosticsBundle {
    pub now: String,
    pub service_health: ServiceHealth,
    pub throughput: Throughput,
    pub top_unknowns: Vec<UnknownEntry>,
    pub recent_labeler_outcomes: Vec<LabelerOutcome>,
    pub recent_events: Vec<RecentEvent>,
    pub config_summary: ConfigSummary,
}

#[derive(Debug, Serialize)]
pub struct ServiceHealth {
    pub head_block: Option<i64>,
    pub latest_analyzed_block: Option<i64>,
    pub blocks_behind: Option<i64>,
    pub pending_queue_depth: i64,
    pub dead_letter_depth: i64,
    pub labeler_queue_depth: i64,
    pub last_insert_age_secs: Option<i64>,
    pub total_rows: i64,
}

#[derive(Debug, Serialize)]
pub struct Throughput {
    pub rows_last_1h: i64,
    pub rows_last_24h: i64,
    pub rows_last_7d: i64,
}

#[derive(Debug, Serialize)]
pub struct UnknownEntry {
    pub address: String,
    pub tx_count: i64,
    pub wei_saved: String,
}

#[derive(Debug, Serialize)]
pub struct LabelerOutcome {
    pub address: String,
    pub last_result: String,
    pub contract_name: Option<String>,
    pub matched_slug: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RecentEvent {
    pub ts: String,
    pub level: String,
    pub target: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct ConfigSummary {
    pub chain_id: i64,
    pub etherscan_enabled: bool,
}

pub async fn collect(state: &AppState) -> DiagnosticsBundle {
    let pool = state.store.pool();
    let chain_id = state.chain_id;

    // Run the cheap reads concurrently — the LLM call is the slow part of
    // the request, so trimming a couple hundred ms here is worth it.
    let (analysis, throughput, top_unknowns, recent_outcomes) = tokio::join!(
        queries::analysis_health(pool, chain_id),
        collect_throughput(pool, chain_id),
        collect_top_unknowns(pool, chain_id, 10),
        collect_labeler_outcomes(pool, chain_id, 10),
    );
    let analysis = analysis.ok();
    let mut conn = state.redis.clone();

    let pending: i64 = conn
        .llen::<_, i64>(indexer_service::queue::QUEUE_KEY)
        .await
        .unwrap_or(0);
    let dead: i64 = conn
        .llen::<_, i64>(indexer_service::queue::DEAD_KEY)
        .await
        .unwrap_or(0);
    let labeler_depth: i64 = conn.zcard::<_, i64>("labeler:queue").await.unwrap_or(0);
    let head_block: Option<i64> = conn
        .get::<_, Option<i64>>(indexer_service::state::LAST_HEAD_KEY)
        .await
        .unwrap_or(None);

    let recent_events = collect_recent_events(&mut conn, 25).await;

    let blocks_behind = analysis
        .as_ref()
        .and_then(|a| a.latest_block.zip(head_block))
        .map(|(a, h)| h - a);
    let last_insert_age_secs = analysis
        .as_ref()
        .and_then(|a| a.last_insert_age_secs.map(|s| s as i64));
    let latest_analyzed_block = analysis.as_ref().and_then(|a| a.latest_block);
    let total_rows = analysis.as_ref().and_then(|a| a.total_rows).unwrap_or(0);

    DiagnosticsBundle {
        now: chrono::Utc::now().to_rfc3339(),
        service_health: ServiceHealth {
            head_block,
            latest_analyzed_block,
            blocks_behind,
            pending_queue_depth: pending,
            dead_letter_depth: dead,
            labeler_queue_depth: labeler_depth,
            last_insert_age_secs,
            total_rows,
        },
        throughput,
        top_unknowns,
        recent_labeler_outcomes: recent_outcomes,
        recent_events,
        config_summary: ConfigSummary {
            chain_id,
            etherscan_enabled: !state.etherscan_enabled_hint.is_empty(),
        },
    }
}

async fn collect_throughput(pool: &PgPool, chain_id: i64) -> Throughput {
    let one = count_since(pool, chain_id, "1 hour").await;
    let day = count_since(pool, chain_id, "24 hours").await;
    let week = count_since(pool, chain_id, "7 days").await;
    Throughput {
        rows_last_1h: one,
        rows_last_24h: day,
        rows_last_7d: week,
    }
}

async fn count_since(pool: &PgPool, chain_id: i64, interval: &str) -> i64 {
    let q = format!(
        r#"SELECT COUNT(*)::bigint FROM analysis
           WHERE chain_id = $1 AND inserted_at >= now() - interval '{interval}'"#
    );
    sqlx::query_scalar::<_, i64>(&q)
        .bind(chain_id)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

async fn collect_top_unknowns(pool: &PgPool, chain_id: i64, limit: i64) -> Vec<UnknownEntry> {
    let rows: Vec<(Vec<u8>, i64, sqlx::types::BigDecimal)> = sqlx::query_as(
        r#"SELECT to_address, COUNT(*)::bigint, COALESCE(SUM(wei_saved),0)::numeric
           FROM analysis
           WHERE chain_id = $1 AND project_slug LIKE 'unknown:%'
             AND block_timestamp >= now() - interval '30 days'
           GROUP BY to_address
           ORDER BY 3 DESC NULLS LAST
           LIMIT $2"#,
    )
    .bind(chain_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(|(addr, n, wei)| UnknownEntry {
            address: format!("0x{}", hex::encode(addr)),
            tx_count: n,
            wei_saved: wei.to_string(),
        })
        .collect()
}

async fn collect_labeler_outcomes(pool: &PgPool, chain_id: i64, limit: i64) -> Vec<LabelerOutcome> {
    let rows: Vec<(Vec<u8>, String, Option<String>, Option<String>)> = sqlx::query_as(
        r#"SELECT address, last_result, contract_name, matched_slug
           FROM address_label_attempt
           WHERE chain_id = $1
           ORDER BY last_attempted_at DESC
           LIMIT $2"#,
    )
    .bind(chain_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    rows.into_iter()
        .map(|(addr, result, name, slug)| LabelerOutcome {
            address: format!("0x{}", hex::encode(addr)),
            last_result: result,
            contract_name: name,
            matched_slug: slug,
        })
        .collect()
}

/// Reads the most recent N entries from the `indexer:events` Redis stream.
/// Services publish WARN/ERROR events into this stream from their tracing
/// layer (see `indexer-rpc::event_pub`). If the stream doesn't exist yet
/// (services not yet upgraded), returns an empty Vec — the bundle is still
/// useful, the LLM just lacks the most volatile signal.
async fn collect_recent_events(
    conn: &mut redis::aio::ConnectionManager,
    limit: usize,
) -> Vec<RecentEvent> {
    // XREVRANGE returns newest-first. Each entry is a (id, fields) tuple;
    // fields are key/value pairs we serialized at publish time.
    let raw: redis::RedisResult<Vec<(String, Vec<(String, String)>)>> = redis::cmd("XREVRANGE")
        .arg("indexer:events")
        .arg("+")
        .arg("-")
        .arg("COUNT")
        .arg(limit)
        .query_async(conn)
        .await;
    let entries = match raw {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    entries
        .into_iter()
        .map(|(id, fields)| {
            let map: std::collections::HashMap<_, _> = fields.into_iter().collect();
            RecentEvent {
                ts: id, // stream IDs are millis-time-based; good enough as a timestamp
                level: map.get("level").cloned().unwrap_or_default(),
                target: map.get("target").cloned().unwrap_or_default(),
                message: map.get("message").cloned().unwrap_or_default(),
            }
        })
        .collect()
}

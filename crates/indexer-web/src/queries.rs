//! Read-only aggregate queries that feed the BD pages and the admin health
//! view. All queries take a `chain_id` so a future per-chain deployment story
//! is not painted-into-corner.

use bigdecimal::BigDecimal;
use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;

use crate::error::WebError;

/// Time window helper. Mirrors `now() - interval ...` for sqlx.
#[derive(Debug, Clone, Copy)]
pub enum Window {
    Lifetime,
    Days(i32),
}

impl Window {
    fn interval_clause(self) -> Option<String> {
        match self {
            Window::Lifetime => None,
            Window::Days(d) => Some(format!("{d} days")),
        }
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OverviewTotals {
    pub usd_saved: Option<BigDecimal>,
    pub tx_count: Option<i64>,
    pub project_count: Option<i64>,
}

pub async fn overview_totals(
    pool: &PgPool,
    chain_id: i64,
    window: Window,
) -> Result<OverviewTotals, WebError> {
    let row = match window.interval_clause() {
        None => sqlx::query_as::<_, OverviewTotals>(
            r#"SELECT
                 COALESCE(SUM(usd_saved_total), 0)::numeric AS usd_saved,
                 COALESCE(SUM(tx_count), 0)::bigint        AS tx_count,
                 COUNT(DISTINCT project_slug)::bigint      AS project_count
               FROM project_daily
               WHERE chain_id = $1"#,
        )
        .bind(chain_id)
        .fetch_one(pool)
        .await?,
        Some(interval) => sqlx::query_as::<_, OverviewTotals>(&format!(
            r#"SELECT
                 COALESCE(SUM(usd_saved_total), 0)::numeric AS usd_saved,
                 COALESCE(SUM(tx_count), 0)::bigint        AS tx_count,
                 COUNT(DISTINCT project_slug)::bigint      AS project_count
               FROM project_daily
               WHERE chain_id = $1 AND day >= (now() - interval '{interval}')::date"#
        ))
        .bind(chain_id)
        .fetch_one(pool)
        .await?,
    };
    Ok(row)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HeuristicRate {
    pub rate: Option<f64>,
    pub sample_count: Option<i64>,
}

pub async fn heuristic_rate(
    pool: &PgPool,
    chain_id: i64,
    interval_str: &str,
) -> Result<HeuristicRate, WebError> {
    let q = format!(
        r#"SELECT
             CASE WHEN COUNT(*) > 0
               THEN SUM(CASE WHEN is_heuristic THEN 1 ELSE 0 END)::float8 / COUNT(*)::float8
               ELSE NULL
             END AS rate,
             COUNT(*)::bigint AS sample_count
           FROM analysis
           WHERE chain_id = $1 AND inserted_at >= now() - interval '{interval_str}'"#
    );
    let row = sqlx::query_as::<_, HeuristicRate>(&q)
        .bind(chain_id)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct LeaderboardRow {
    pub project_slug: String,
    pub project_name: Option<String>,
    pub category: Option<String>,
    pub usd_saved: Option<BigDecimal>,
    pub tx_count: Option<i64>,
    pub avg_savings_pct: Option<f64>,
    pub heuristic_rate: Option<f64>,
}

pub async fn leaderboard_30d(
    pool: &PgPool,
    chain_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<LeaderboardRow>, WebError> {
    let rows = sqlx::query_as::<_, LeaderboardRow>(
        r#"SELECT
             pd.project_slug,
             p.project_name,
             p.category,
             SUM(pd.usd_saved_total)::numeric AS usd_saved,
             SUM(pd.tx_count)::bigint         AS tx_count,
             AVG(pd.avg_savings_pct)::float8  AS avg_savings_pct,
             AVG(pd.heuristic_rate)::float8   AS heuristic_rate
           FROM project_daily pd
           LEFT JOIN projects p ON p.project_slug = pd.project_slug
           WHERE pd.chain_id = $1
             AND pd.day >= (now() - interval '30 days')::date
           GROUP BY pd.project_slug, p.project_name, p.category
           ORDER BY usd_saved DESC NULLS LAST
           LIMIT $2 OFFSET $3"#,
    )
    .bind(chain_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DailyPoint {
    pub day: NaiveDate,
    pub usd_saved: Option<BigDecimal>,
    pub tx_count: Option<i64>,
}

pub async fn daily_overview(
    pool: &PgPool,
    chain_id: i64,
    days: i32,
) -> Result<Vec<DailyPoint>, WebError> {
    let q = format!(
        r#"SELECT
             day,
             SUM(usd_saved_total)::numeric AS usd_saved,
             SUM(tx_count)::bigint         AS tx_count
           FROM project_daily
           WHERE chain_id = $1 AND day >= (now() - interval '{days} days')::date
           GROUP BY day
           ORDER BY day"#
    );
    let rows = sqlx::query_as::<_, DailyPoint>(&q)
        .bind(chain_id)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

pub async fn daily_for_project(
    pool: &PgPool,
    chain_id: i64,
    project_slug: &str,
    days: i32,
) -> Result<Vec<DailyPoint>, WebError> {
    let q = format!(
        r#"SELECT
             day,
             usd_saved_total::numeric AS usd_saved,
             tx_count::bigint         AS tx_count
           FROM project_daily
           WHERE chain_id = $1
             AND project_slug = $2
             AND day >= (now() - interval '{days} days')::date
           ORDER BY day"#
    );
    let rows = sqlx::query_as::<_, DailyPoint>(&q)
        .bind(chain_id)
        .bind(project_slug)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CategoryRow {
    pub category: Option<String>,
    pub usd_saved: Option<BigDecimal>,
}

pub async fn category_breakdown_30d(
    pool: &PgPool,
    chain_id: i64,
) -> Result<Vec<CategoryRow>, WebError> {
    let rows = sqlx::query_as::<_, CategoryRow>(
        r#"SELECT
             p.category,
             SUM(pd.usd_saved_total)::numeric AS usd_saved
           FROM project_daily pd
           LEFT JOIN projects p ON p.project_slug = pd.project_slug
           WHERE pd.chain_id = $1
             AND pd.day >= (now() - interval '30 days')::date
           GROUP BY p.category
           ORDER BY usd_saved DESC NULLS LAST"#,
    )
    .bind(chain_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProjectHeader {
    pub project_slug: String,
    pub project_name: Option<String>,
    pub category: Option<String>,
    pub contact_email: Option<String>,
    pub contact_url: Option<String>,
}

pub async fn project_header(
    pool: &PgPool,
    project_slug: &str,
) -> Result<Option<ProjectHeader>, WebError> {
    let row = sqlx::query_as::<_, ProjectHeader>(
        r#"SELECT project_slug, project_name, category, contact_email, contact_url
           FROM projects WHERE project_slug = $1"#,
    )
    .bind(project_slug)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ProjectTotals {
    pub usd_saved: Option<BigDecimal>,
    pub tx_count: Option<i64>,
    pub avg_savings_pct: Option<f64>,
    pub heuristic_rate: Option<f64>,
}

pub async fn project_totals(
    pool: &PgPool,
    chain_id: i64,
    project_slug: &str,
    window: Window,
) -> Result<ProjectTotals, WebError> {
    let q = match window.interval_clause() {
        None => format!(
            r#"SELECT
                 COALESCE(SUM(usd_saved_total), 0)::numeric AS usd_saved,
                 COALESCE(SUM(tx_count), 0)::bigint         AS tx_count,
                 AVG(avg_savings_pct)::float8               AS avg_savings_pct,
                 AVG(heuristic_rate)::float8                AS heuristic_rate
               FROM project_daily
               WHERE chain_id = $1 AND project_slug = $2"#
        ),
        Some(i) => format!(
            r#"SELECT
                 COALESCE(SUM(usd_saved_total), 0)::numeric AS usd_saved,
                 COALESCE(SUM(tx_count), 0)::bigint         AS tx_count,
                 AVG(avg_savings_pct)::float8               AS avg_savings_pct,
                 AVG(heuristic_rate)::float8                AS heuristic_rate
               FROM project_daily
               WHERE chain_id = $1 AND project_slug = $2
                 AND day >= (now() - interval '{i}')::date"#
        ),
    };
    let row = sqlx::query_as::<_, ProjectTotals>(&q)
        .bind(chain_id)
        .bind(project_slug)
        .fetch_one(pool)
        .await?;
    Ok(row)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TopAddressRow {
    pub address: Vec<u8>,
    pub tx_count: Option<i64>,
    pub gas_saved_total: Option<i64>,
    pub wei_saved_total: Option<BigDecimal>,
}

pub async fn top_contracts_for_project(
    pool: &PgPool,
    chain_id: i64,
    project_slug: &str,
) -> Result<Vec<TopAddressRow>, WebError> {
    let rows = sqlx::query_as::<_, TopAddressRow>(
        r#"SELECT
             to_address       AS address,
             COUNT(*)::bigint AS tx_count,
             SUM(gas_saved)::bigint AS gas_saved_total,
             SUM(wei_saved)::numeric AS wei_saved_total
           FROM analysis
           WHERE chain_id = $1
             AND project_slug = $2
             AND block_timestamp >= now() - interval '30 days'
           GROUP BY to_address
           ORDER BY wei_saved_total DESC NULLS LAST
           LIMIT 10"#,
    )
    .bind(chain_id)
    .bind(project_slug)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct TopSelectorRow {
    pub function_selector: Vec<u8>,
    pub tx_count: Option<i64>,
    pub gas_saved_total: Option<i64>,
    pub wei_saved_total: Option<BigDecimal>,
}

pub async fn top_selectors_for_project(
    pool: &PgPool,
    chain_id: i64,
    project_slug: &str,
) -> Result<Vec<TopSelectorRow>, WebError> {
    let rows = sqlx::query_as::<_, TopSelectorRow>(
        r#"SELECT
             function_selector,
             COUNT(*)::bigint AS tx_count,
             SUM(gas_saved)::bigint AS gas_saved_total,
             SUM(wei_saved)::numeric AS wei_saved_total
           FROM analysis
           WHERE chain_id = $1
             AND project_slug = $2
             AND block_timestamp >= now() - interval '30 days'
           GROUP BY function_selector
           ORDER BY wei_saved_total DESC NULLS LAST
           LIMIT 10"#,
    )
    .bind(chain_id)
    .bind(project_slug)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RecentTxRow {
    pub block_number: i64,
    pub tx_hash: Vec<u8>,
    pub gas_used: i64,
    pub gas_saved: i64,
    pub wei_saved: BigDecimal,
    pub is_heuristic: bool,
    pub block_timestamp: DateTime<Utc>,
}

pub async fn recent_txs_for_project(
    pool: &PgPool,
    chain_id: i64,
    project_slug: &str,
    limit: i64,
) -> Result<Vec<RecentTxRow>, WebError> {
    let rows = sqlx::query_as::<_, RecentTxRow>(
        r#"SELECT
             block_number,
             tx_hash,
             gas_used,
             gas_saved,
             wei_saved,
             is_heuristic,
             block_timestamp
           FROM analysis
           WHERE chain_id = $1 AND project_slug = $2
           ORDER BY block_timestamp DESC
           LIMIT $3"#,
    )
    .bind(chain_id)
    .bind(project_slug)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UnknownRow {
    pub address: Vec<u8>,
    pub tx_count: Option<i64>,
    pub wei_saved_total: Option<BigDecimal>,
    pub last_seen: Option<DateTime<Utc>>,
}

pub async fn top_unknowns(
    pool: &PgPool,
    chain_id: i64,
) -> Result<Vec<UnknownRow>, WebError> {
    let rows = sqlx::query_as::<_, UnknownRow>(
        r#"SELECT
             to_address       AS address,
             COUNT(*)::bigint AS tx_count,
             SUM(wei_saved)::numeric AS wei_saved_total,
             MAX(block_timestamp)    AS last_seen
           FROM analysis
           WHERE chain_id = $1
             AND project_slug LIKE 'unknown:%'
             AND block_timestamp >= now() - interval '30 days'
           GROUP BY to_address
           ORDER BY wei_saved_total DESC NULLS LAST
           LIMIT 20"#,
    )
    .bind(chain_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct AnalysisHealthRow {
    pub total_rows: Option<i64>,
    pub latest_block: Option<i64>,
    pub last_insert_age_secs: Option<f64>,
}

pub async fn analysis_health(pool: &PgPool, chain_id: i64) -> Result<AnalysisHealthRow, WebError> {
    let row = sqlx::query_as::<_, AnalysisHealthRow>(
        r#"SELECT
             COUNT(*)::bigint                                    AS total_rows,
             MAX(block_number)::bigint                            AS latest_block,
             EXTRACT(EPOCH FROM (now() - MAX(inserted_at)))::float8 AS last_insert_age_secs
           FROM analysis WHERE chain_id = $1"#,
    )
    .bind(chain_id)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

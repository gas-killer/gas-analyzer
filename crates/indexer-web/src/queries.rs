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
    pub wei_saved: Option<BigDecimal>,
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
                 COALESCE(SUM(usd_saved_total), 0)::numeric  AS usd_saved,
                 COALESCE(SUM(wei_saved_total), 0)::numeric  AS wei_saved,
                 COALESCE(SUM(tx_count), 0)::bigint          AS tx_count,
                 COUNT(DISTINCT project_slug)::bigint        AS project_count
               FROM project_daily
               WHERE chain_id = $1"#,
        )
        .bind(chain_id)
        .fetch_one(pool)
        .await?,
        Some(interval) => sqlx::query_as::<_, OverviewTotals>(&format!(
            r#"SELECT
                 COALESCE(SUM(usd_saved_total), 0)::numeric  AS usd_saved,
                 COALESCE(SUM(wei_saved_total), 0)::numeric  AS wei_saved,
                 COALESCE(SUM(tx_count), 0)::bigint          AS tx_count,
                 COUNT(DISTINCT project_slug)::bigint        AS project_count
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
pub struct LeaderboardRow {
    pub project_slug: String,
    pub project_name: Option<String>,
    pub category: Option<String>,
    pub usd_saved: Option<BigDecimal>,
    pub tx_count: Option<i64>,
    pub covered_tx_count: Option<i64>,
    pub wei_saved_total: Option<BigDecimal>,
    pub wei_spent_total: Option<BigDecimal>,
    pub avg_savings_pct: Option<f64>,
    /// Median (gas_saved / gas_used) across covered txs in the window.
    /// Computed on-demand from `analysis` because medians do not compose
    /// across daily rows in the rollup.
    pub median_savings_pct_covered: Option<f64>,
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
             SUM(pd.covered_tx_count)::bigint AS covered_tx_count,
             SUM(pd.wei_saved_total)::numeric AS wei_saved_total,
             SUM(pd.wei_spent_total)::numeric AS wei_spent_total,
             AVG(pd.avg_savings_pct)::float8  AS avg_savings_pct,
             m.median_savings_pct_covered
           FROM project_daily pd
           LEFT JOIN projects p ON p.project_slug = pd.project_slug
           LEFT JOIN (
             SELECT
               project_slug,
               percentile_cont(0.5) WITHIN GROUP (
                 ORDER BY gas_saved::float8 / NULLIF(gas_used, 0)::float8
               ) FILTER (WHERE gas_saved > 0) AS median_savings_pct_covered
             FROM analysis
             WHERE chain_id = $1
               AND block_timestamp >= now() - interval '30 days'
               AND cardinality(skipped_opcodes) = 0
             GROUP BY project_slug
           ) m ON m.project_slug = pd.project_slug
           WHERE pd.chain_id = $1
             AND pd.day >= (now() - interval '30 days')::date
           GROUP BY pd.project_slug, p.project_name, p.category, m.median_savings_pct_covered
           ORDER BY tx_count DESC NULLS LAST
           LIMIT $2 OFFSET $3"#,
    )
    .bind(chain_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Function-level leaderboard row. One row per `(contract, selector)`
/// tuple, joined to its parent project for display. Median savings is
/// computed on the fly from `analysis` because medians don't compose
/// across daily rollups.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FunctionLeaderRow {
    pub to_address: Vec<u8>,
    pub function_selector: Vec<u8>,
    pub project_slug: String,
    pub project_name: Option<String>,
    /// 4byte-resolved name (e.g. "transfer"). NULL when not yet resolved.
    pub function_name: Option<String>,
    pub function_sig: Option<String>,
    pub tx_count: Option<i64>,
    pub wei_saved_total: Option<BigDecimal>,
    pub wei_spent_total: Option<BigDecimal>,
    pub usd_saved: Option<BigDecimal>,
    pub avg_savings_pct: Option<f64>,
    pub median_savings_pct: Option<f64>,
}

pub async fn leaderboard_functions_30d(
    pool: &PgPool,
    chain_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<FunctionLeaderRow>, WebError> {
    let rows = sqlx::query_as::<_, FunctionLeaderRow>(
        r#"SELECT
             fd.to_address,
             fd.function_selector,
             fd.project_slug,
             p.project_name,
             fs.primary_name AS function_name,
             fs.primary_sig  AS function_sig,
             SUM(fd.tx_count)::bigint         AS tx_count,
             SUM(fd.wei_saved_total)::numeric AS wei_saved_total,
             SUM(fd.wei_spent_total)::numeric AS wei_spent_total,
             SUM(fd.usd_saved_total)::numeric AS usd_saved,
             AVG(fd.avg_savings_pct)::float8  AS avg_savings_pct,
             m.median_savings_pct
           FROM function_daily fd
           LEFT JOIN projects p ON p.project_slug = fd.project_slug
           LEFT JOIN function_selectors fs ON fs.selector = fd.function_selector
           LEFT JOIN (
             SELECT
               to_address,
               function_selector,
               percentile_cont(0.5) WITHIN GROUP (
                 ORDER BY gas_saved::float8 / NULLIF(gas_used, 0)::float8
               ) AS median_savings_pct
             FROM analysis
             WHERE chain_id = $1
               AND block_timestamp >= now() - interval '30 days'
               AND cardinality(skipped_opcodes) = 0
               AND gas_saved > 0
             GROUP BY to_address, function_selector
           ) m ON m.to_address = fd.to_address AND m.function_selector = fd.function_selector
           WHERE fd.chain_id = $1
             AND fd.day >= (now() - interval '30 days')::date
           GROUP BY fd.to_address, fd.function_selector, fd.project_slug,
                    p.project_name, fs.primary_name, fs.primary_sig, m.median_savings_pct
           ORDER BY wei_saved_total DESC NULLS LAST
           LIMIT $2 OFFSET $3"#,
    )
    .bind(chain_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Contract-level leaderboard row. Same data as the function view but
/// rolled up one level — useful when a BD wants to compare contracts
/// without function granularity.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContractLeaderRow {
    pub to_address: Vec<u8>,
    pub project_slug: String,
    pub project_name: Option<String>,
    pub tx_count: Option<i64>,
    pub function_count: Option<i64>,
    pub wei_saved_total: Option<BigDecimal>,
    pub wei_spent_total: Option<BigDecimal>,
    pub usd_saved: Option<BigDecimal>,
    pub avg_savings_pct: Option<f64>,
    pub median_savings_pct: Option<f64>,
}

pub async fn leaderboard_contracts_30d(
    pool: &PgPool,
    chain_id: i64,
    limit: i64,
    offset: i64,
) -> Result<Vec<ContractLeaderRow>, WebError> {
    let rows = sqlx::query_as::<_, ContractLeaderRow>(
        r#"SELECT
             fd.to_address,
             fd.project_slug,
             p.project_name,
             SUM(fd.tx_count)::bigint                      AS tx_count,
             COUNT(DISTINCT fd.function_selector)::bigint  AS function_count,
             SUM(fd.wei_saved_total)::numeric              AS wei_saved_total,
             SUM(fd.wei_spent_total)::numeric              AS wei_spent_total,
             SUM(fd.usd_saved_total)::numeric              AS usd_saved,
             AVG(fd.avg_savings_pct)::float8               AS avg_savings_pct,
             m.median_savings_pct
           FROM function_daily fd
           LEFT JOIN projects p ON p.project_slug = fd.project_slug
           LEFT JOIN (
             SELECT
               to_address,
               percentile_cont(0.5) WITHIN GROUP (
                 ORDER BY gas_saved::float8 / NULLIF(gas_used, 0)::float8
               ) AS median_savings_pct
             FROM analysis
             WHERE chain_id = $1
               AND block_timestamp >= now() - interval '30 days'
               AND cardinality(skipped_opcodes) = 0
               AND gas_saved > 0
             GROUP BY to_address
           ) m ON m.to_address = fd.to_address
           WHERE fd.chain_id = $1
             AND fd.day >= (now() - interval '30 days')::date
           GROUP BY fd.to_address, fd.project_slug, p.project_name, m.median_savings_pct
           ORDER BY wei_saved_total DESC NULLS LAST
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
    pub wei_saved: Option<BigDecimal>,
    pub tx_count: Option<i64>,
    pub avg_savings_pct: Option<f64>,
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
                 COALESCE(SUM(wei_saved_total), 0)::numeric AS wei_saved,
                 COALESCE(SUM(tx_count), 0)::bigint         AS tx_count,
                 AVG(avg_savings_pct)::float8               AS avg_savings_pct
               FROM project_daily
               WHERE chain_id = $1 AND project_slug = $2"#
        ),
        Some(i) => format!(
            r#"SELECT
                 COALESCE(SUM(usd_saved_total), 0)::numeric AS usd_saved,
                 COALESCE(SUM(wei_saved_total), 0)::numeric AS wei_saved,
                 COALESCE(SUM(tx_count), 0)::bigint         AS tx_count,
                 AVG(avg_savings_pct)::float8               AS avg_savings_pct
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
             AND gas_saved > 0
             AND cardinality(skipped_opcodes) = 0
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
             AND gas_saved > 0
             AND cardinality(skipped_opcodes) = 0
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
             block_timestamp
           FROM analysis
           WHERE chain_id = $1 AND project_slug = $2
             AND gas_saved > 0
             AND cardinality(skipped_opcodes) = 0
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
             AND gas_saved > 0
             AND cardinality(skipped_opcodes) = 0
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

/// Resolve `(chain_id, address)` → `(project_slug, project_name)` via
/// `address_project` JOIN `projects`. Returns `None` when the address
/// has no mapping (caller falls back to a synthetic `unknown:0xADDR`).
pub async fn resolved_label(
    pool: &PgPool,
    chain_id: i64,
    address: [u8; 20],
) -> Result<Option<(String, Option<String>)>, WebError> {
    let row: Option<(String, Option<String>)> = sqlx::query_as(
        r#"SELECT ap.project_slug, p.project_name
           FROM address_project ap
           LEFT JOIN projects p ON p.project_slug = ap.project_slug
           WHERE ap.chain_id = $1 AND ap.address = $2"#,
    )
    .bind(chain_id)
    .bind(&address[..])
    .fetch_optional(pool)
    .await?;
    Ok(row)
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

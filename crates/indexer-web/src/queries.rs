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

/// Date and savings filters applied to tx-list queries. All fields are
/// optional; an empty `TxFilters::default()` means no narrowing.
#[derive(Debug, Clone, Default)]
pub struct TxFilters {
    /// Inclusive lower bound on `block_timestamp` (YYYY-MM-DD).
    pub from: Option<chrono::NaiveDate>,
    /// Exclusive upper bound on `block_timestamp` (YYYY-MM-DD).
    pub to: Option<chrono::NaiveDate>,
    /// Minimum `gas_saved` to include — lets BD strip tiny-savings noise.
    pub min_gas_saved: Option<i64>,
}

impl TxFilters {
    /// Build a SQL fragment + bind values. Returns the WHERE clause
    /// addendum (always starts with " AND ") and the offsets where to
    /// bind from/to/min — the caller binds in the same order.
    fn where_clause(&self, next_idx: &mut usize) -> String {
        let mut s = String::new();
        if self.from.is_some() {
            s.push_str(&format!(" AND block_timestamp >= ${}::timestamptz", next_idx));
            *next_idx += 1;
        }
        if self.to.is_some() {
            s.push_str(&format!(" AND block_timestamp <  ${}::timestamptz", next_idx));
            *next_idx += 1;
        }
        if self.min_gas_saved.is_some() {
            s.push_str(&format!(" AND gas_saved >= ${}::bigint", next_idx));
            *next_idx += 1;
        }
        s
    }
}

pub async fn recent_txs_for_project(
    pool: &PgPool,
    chain_id: i64,
    project_slug: &str,
    limit: i64,
    offset: i64,
    filters: &TxFilters,
) -> Result<Vec<RecentTxRow>, WebError> {
    let mut idx = 3usize;
    let extra = filters.where_clause(&mut idx);
    let limit_param = idx;
    let offset_param = idx + 1;
    let q = format!(
        r#"SELECT
             block_number, tx_hash, gas_used, gas_saved, wei_saved, block_timestamp
           FROM analysis
           WHERE chain_id = $1 AND project_slug = $2
             AND gas_saved > 0
             AND cardinality(skipped_opcodes) = 0{extra}
           ORDER BY block_timestamp DESC
           LIMIT ${limit_param} OFFSET ${offset_param}"#,
    );
    let mut query = sqlx::query_as::<_, RecentTxRow>(&q)
        .bind(chain_id)
        .bind(project_slug);
    if let Some(from) = filters.from {
        query = query.bind(from.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc());
    }
    if let Some(to) = filters.to {
        query = query.bind(to.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc());
    }
    if let Some(min) = filters.min_gas_saved {
        query = query.bind(min);
    }
    let rows = query.bind(limit).bind(offset).fetch_all(pool).await?;
    Ok(rows)
}

// ---------- Per-contract drilldown ----------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContractHeader {
    pub project_slug: String,
    pub project_name: Option<String>,
}

pub async fn contract_header(
    pool: &PgPool,
    chain_id: i64,
    address: [u8; 20],
) -> Result<ContractHeader, WebError> {
    let row: Option<ContractHeader> = sqlx::query_as(
        r#"SELECT ap.project_slug, p.project_name
           FROM address_project ap
           LEFT JOIN projects p ON p.project_slug = ap.project_slug
           WHERE ap.chain_id = $1 AND ap.address = $2"#,
    )
    .bind(chain_id)
    .bind(&address[..])
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or(ContractHeader {
        project_slug: format!("unknown:0x{}", hex::encode(address)),
        project_name: None,
    }))
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContractTotals {
    pub usd_saved: Option<BigDecimal>,
    pub wei_saved: Option<BigDecimal>,
    pub tx_count: Option<i64>,
    pub function_count: Option<i64>,
    pub avg_savings_pct: Option<f64>,
}

pub async fn contract_totals(
    pool: &PgPool,
    chain_id: i64,
    address: [u8; 20],
    window: Window,
) -> Result<ContractTotals, WebError> {
    let q = match window.interval_clause() {
        None => format!(
            r#"SELECT
                 COALESCE(SUM(usd_saved_total), 0)::numeric AS usd_saved,
                 COALESCE(SUM(wei_saved_total), 0)::numeric AS wei_saved,
                 COALESCE(SUM(tx_count), 0)::bigint         AS tx_count,
                 COUNT(DISTINCT function_selector)::bigint  AS function_count,
                 AVG(avg_savings_pct)::float8               AS avg_savings_pct
               FROM function_daily
               WHERE chain_id = $1 AND to_address = $2"#
        ),
        Some(i) => format!(
            r#"SELECT
                 COALESCE(SUM(usd_saved_total), 0)::numeric AS usd_saved,
                 COALESCE(SUM(wei_saved_total), 0)::numeric AS wei_saved,
                 COALESCE(SUM(tx_count), 0)::bigint         AS tx_count,
                 COUNT(DISTINCT function_selector)::bigint  AS function_count,
                 AVG(avg_savings_pct)::float8               AS avg_savings_pct
               FROM function_daily
               WHERE chain_id = $1 AND to_address = $2
                 AND day >= (now() - interval '{i}')::date"#
        ),
    };
    let row = sqlx::query_as::<_, ContractTotals>(&q)
        .bind(chain_id)
        .bind(&address[..])
        .fetch_one(pool)
        .await?;
    Ok(row)
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ContractFunctionRow {
    pub function_selector: Vec<u8>,
    pub function_name: Option<String>,
    pub tx_count: Option<i64>,
    pub usd_saved: Option<BigDecimal>,
    pub avg_savings_pct: Option<f64>,
}

pub async fn functions_for_contract(
    pool: &PgPool,
    chain_id: i64,
    address: [u8; 20],
) -> Result<Vec<ContractFunctionRow>, WebError> {
    let rows = sqlx::query_as::<_, ContractFunctionRow>(
        r#"SELECT
             fd.function_selector,
             fs.primary_name AS function_name,
             SUM(fd.tx_count)::bigint         AS tx_count,
             SUM(fd.usd_saved_total)::numeric AS usd_saved,
             AVG(fd.avg_savings_pct)::float8  AS avg_savings_pct
           FROM function_daily fd
           LEFT JOIN function_selectors fs ON fs.selector = fd.function_selector
           WHERE fd.chain_id = $1
             AND fd.to_address = $2
             AND fd.day >= (now() - interval '30 days')::date
           GROUP BY fd.function_selector, fs.primary_name
           ORDER BY usd_saved DESC NULLS LAST
           LIMIT 50"#,
    )
    .bind(chain_id)
    .bind(&address[..])
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn daily_for_contract(
    pool: &PgPool,
    chain_id: i64,
    address: [u8; 20],
    days: i32,
) -> Result<Vec<DailyPoint>, WebError> {
    let rows = sqlx::query_as::<_, DailyPoint>(
        r#"SELECT
             day,
             SUM(usd_saved_total)::numeric AS usd_saved,
             SUM(tx_count)::bigint         AS tx_count
           FROM function_daily
           WHERE chain_id = $1
             AND to_address = $2
             AND day >= (now() - make_interval(days => $3::int))::date
           GROUP BY day
           ORDER BY day"#,
    )
    .bind(chain_id)
    .bind(&address[..])
    .bind(days)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn recent_txs_for_contract(
    pool: &PgPool,
    chain_id: i64,
    address: [u8; 20],
    limit: i64,
    offset: i64,
    filters: &TxFilters,
) -> Result<Vec<RecentTxRow>, WebError> {
    let mut idx = 3usize;
    let extra = filters.where_clause(&mut idx);
    let limit_param = idx;
    let offset_param = idx + 1;
    let q = format!(
        r#"SELECT
             block_number, tx_hash, gas_used, gas_saved, wei_saved, block_timestamp
           FROM analysis
           WHERE chain_id = $1 AND to_address = $2
             AND gas_saved > 0
             AND cardinality(skipped_opcodes) = 0{extra}
           ORDER BY block_timestamp DESC
           LIMIT ${limit_param} OFFSET ${offset_param}"#,
    );
    let mut query = sqlx::query_as::<_, RecentTxRow>(&q)
        .bind(chain_id)
        .bind(&address[..]);
    if let Some(from) = filters.from {
        query = query.bind(from.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc());
    }
    if let Some(to) = filters.to {
        query = query.bind(to.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc());
    }
    if let Some(min) = filters.min_gas_saved {
        query = query.bind(min);
    }
    let rows = query.bind(limit).bind(offset).fetch_all(pool).await?;
    Ok(rows)
}

// ---------- Per-function drilldown ----------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FunctionHeader {
    pub project_slug: String,
    pub project_name: Option<String>,
    pub function_name: Option<String>,
    pub function_sig: Option<String>,
}

pub async fn function_header(
    pool: &PgPool,
    chain_id: i64,
    address: [u8; 20],
    selector: [u8; 4],
) -> Result<FunctionHeader, WebError> {
    let row: Option<FunctionHeader> = sqlx::query_as(
        r#"SELECT
             COALESCE(ap.project_slug, 'unknown:0x' || encode($2, 'hex')) AS project_slug,
             p.project_name,
             fs.primary_name AS function_name,
             fs.primary_sig  AS function_sig
           FROM (SELECT 1) _
           LEFT JOIN address_project ap
             ON ap.chain_id = $1 AND ap.address = $2
           LEFT JOIN projects p ON p.project_slug = ap.project_slug
           LEFT JOIN function_selectors fs ON fs.selector = $3"#,
    )
    .bind(chain_id)
    .bind(&address[..])
    .bind(&selector[..])
    .fetch_optional(pool)
    .await?;
    Ok(row.unwrap_or(FunctionHeader {
        project_slug: format!("unknown:0x{}", hex::encode(address)),
        project_name: None,
        function_name: None,
        function_sig: None,
    }))
}

pub async fn function_totals(
    pool: &PgPool,
    chain_id: i64,
    address: [u8; 20],
    selector: [u8; 4],
    window: Window,
) -> Result<ContractTotals, WebError> {
    let q = match window.interval_clause() {
        None => format!(
            r#"SELECT
                 COALESCE(SUM(usd_saved_total), 0)::numeric AS usd_saved,
                 COALESCE(SUM(wei_saved_total), 0)::numeric AS wei_saved,
                 COALESCE(SUM(tx_count), 0)::bigint         AS tx_count,
                 1::bigint                                  AS function_count,
                 AVG(avg_savings_pct)::float8               AS avg_savings_pct
               FROM function_daily
               WHERE chain_id = $1 AND to_address = $2 AND function_selector = $3"#
        ),
        Some(i) => format!(
            r#"SELECT
                 COALESCE(SUM(usd_saved_total), 0)::numeric AS usd_saved,
                 COALESCE(SUM(wei_saved_total), 0)::numeric AS wei_saved,
                 COALESCE(SUM(tx_count), 0)::bigint         AS tx_count,
                 1::bigint                                  AS function_count,
                 AVG(avg_savings_pct)::float8               AS avg_savings_pct
               FROM function_daily
               WHERE chain_id = $1 AND to_address = $2 AND function_selector = $3
                 AND day >= (now() - interval '{i}')::date"#
        ),
    };
    let row = sqlx::query_as::<_, ContractTotals>(&q)
        .bind(chain_id)
        .bind(&address[..])
        .bind(&selector[..])
        .fetch_one(pool)
        .await?;
    Ok(row)
}

pub async fn daily_for_function(
    pool: &PgPool,
    chain_id: i64,
    address: [u8; 20],
    selector: [u8; 4],
    days: i32,
) -> Result<Vec<DailyPoint>, WebError> {
    let rows = sqlx::query_as::<_, DailyPoint>(
        r#"SELECT
             day,
             usd_saved_total::numeric AS usd_saved,
             tx_count::bigint         AS tx_count
           FROM function_daily
           WHERE chain_id = $1
             AND to_address = $2
             AND function_selector = $3
             AND day >= (now() - make_interval(days => $4::int))::date
           ORDER BY day"#,
    )
    .bind(chain_id)
    .bind(&address[..])
    .bind(&selector[..])
    .bind(days)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn recent_txs_for_function(
    pool: &PgPool,
    chain_id: i64,
    address: [u8; 20],
    selector: [u8; 4],
    limit: i64,
    offset: i64,
    filters: &TxFilters,
) -> Result<Vec<RecentTxRow>, WebError> {
    let mut idx = 4usize;
    let extra = filters.where_clause(&mut idx);
    let limit_param = idx;
    let offset_param = idx + 1;
    let q = format!(
        r#"SELECT
             block_number, tx_hash, gas_used, gas_saved, wei_saved, block_timestamp
           FROM analysis
           WHERE chain_id = $1
             AND to_address = $2
             AND function_selector = $3
             AND gas_saved > 0
             AND cardinality(skipped_opcodes) = 0{extra}
           ORDER BY block_timestamp DESC
           LIMIT ${limit_param} OFFSET ${offset_param}"#,
    );
    let mut query = sqlx::query_as::<_, RecentTxRow>(&q)
        .bind(chain_id)
        .bind(&address[..])
        .bind(&selector[..]);
    if let Some(from) = filters.from {
        query = query.bind(from.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc());
    }
    if let Some(to) = filters.to {
        query = query.bind(to.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc());
    }
    if let Some(min) = filters.min_gas_saved {
        query = query.bind(min);
    }
    let rows = query.bind(limit).bind(offset).fetch_all(pool).await?;
    Ok(rows)
}

/// Top unresolved function selectors by 30d wei_saved. "Unresolved"
/// means 4byte didn't find a signature for the selector — distinct
/// from "unknown contract" (no project mapping). Surfaces are
/// orthogonal: a contract can be labeled but use a selector 4byte
/// has never seen.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UnresolvedSelectorRow {
    pub selector: Vec<u8>,
    pub example_address: Vec<u8>,
    pub tx_count: Option<i64>,
    pub wei_saved_total: Option<BigDecimal>,
    pub usd_saved_total: Option<BigDecimal>,
}

pub async fn top_unresolved_selectors(
    pool: &PgPool,
    chain_id: i64,
    limit: i64,
) -> Result<Vec<UnresolvedSelectorRow>, WebError> {
    let rows = sqlx::query_as::<_, UnresolvedSelectorRow>(
        r#"SELECT
             fd.function_selector AS selector,
             (ARRAY_AGG(fd.to_address ORDER BY fd.wei_saved_total DESC))[1] AS example_address,
             SUM(fd.tx_count)::bigint        AS tx_count,
             SUM(fd.wei_saved_total)::numeric AS wei_saved_total,
             SUM(fd.usd_saved_total)::numeric AS usd_saved_total
           FROM function_daily fd
           LEFT JOIN function_selectors fs ON fs.selector = fd.function_selector
           WHERE fd.chain_id = $1
             AND fd.day >= (now() - interval '30 days')::date
             AND (fs.primary_name IS NULL OR fs.source = 'unresolved')
           GROUP BY fd.function_selector
           ORDER BY wei_saved_total DESC NULLS LAST
           LIMIT $2"#,
    )
    .bind(chain_id)
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

/// Heuristic-fallback share over the last 24h. Returns `None` when there
/// are no analyses in the window (don't render anything).
pub async fn heuristic_rate_24h(
    pool: &PgPool,
    chain_id: i64,
) -> Result<Option<f64>, WebError> {
    let row: (Option<i64>, Option<i64>) = sqlx::query_as(
        r#"SELECT
             count(*) FILTER (WHERE is_heuristic)::bigint AS heuristic_count,
             count(*)::bigint                              AS total_count
           FROM analysis
           WHERE chain_id = $1
             AND inserted_at >= now() - interval '24 hours'"#,
    )
    .bind(chain_id)
    .fetch_one(pool)
    .await?;
    let total = row.1.unwrap_or(0);
    if total == 0 {
        Ok(None)
    } else {
        Ok(Some(row.0.unwrap_or(0) as f64 / total as f64))
    }
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ErrorCategoryQueryRow {
    pub category: String,
    pub count: Option<i64>,
}

/// Categorize `failure_reason` strings into typed buckets via SQL CASE
/// matching. Patterns lifted from operator-observed errors; extend as
/// new shapes appear.
pub async fn error_categories_24h(
    pool: &PgPool,
    chain_id: i64,
) -> Result<Vec<ErrorCategoryQueryRow>, WebError> {
    let rows = sqlx::query_as::<_, ErrorCategoryQueryRow>(
        r#"SELECT
             CASE
                 WHEN failure_reason ILIKE '%rate limit%'   THEN 'rate_limit'
                 WHEN failure_reason ILIKE '%timeout%'      THEN 'timeout'
                 WHEN failure_reason ILIKE '%response size%' THEN 'response_too_large'
                 WHEN failure_reason ILIKE '%429%'          THEN 'rate_limit'
                 ELSE 'other'
             END                                            AS category,
             count(*)::bigint                               AS count
           FROM analysis
           WHERE chain_id = $1
             AND failure_reason IS NOT NULL
             AND inserted_at >= now() - interval '24 hours'
           GROUP BY 1
           ORDER BY count DESC"#,
    )
    .bind(chain_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
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

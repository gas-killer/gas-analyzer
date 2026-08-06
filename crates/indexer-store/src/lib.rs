//! Postgres-backed persistence for the indexer service.
//!
//! Owns the schema (see `migrations/`) and exposes a small `Store` API for the
//! head-tracker, workers, and refresher to call. All queries run against a
//! shared `sqlx::PgPool`.

use bigdecimal::BigDecimal;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use indexer_api::AnalysisReport;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use std::str::FromStr;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlx error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("migrate error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("decimal conversion error: {0}")]
    Decimal(String),
}

#[derive(Debug, Clone)]
pub struct Project {
    pub slug: String,
    pub name: String,
    pub category: Option<String>,
    pub contact_email: Option<String>,
    pub contact_url: Option<String>,
}

/// Row returned by `Store::top_unknown_addresses`. Used by the auto-labeler
/// to decide which contracts to look up first (highest wei_saved → highest
/// BD value to label).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UnknownAddressRow {
    pub address: Vec<u8>,
    pub wei_saved_total: BigDecimal,
    pub tx_count: i64,
}

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

impl Store {
    pub async fn connect(database_url: &str, max_connections: u32) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(database_url)
            .await?;
        Ok(Self { pool })
    }

    pub fn from_pool(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn migrate(&self) -> Result<(), StoreError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Insert one analysis row. `project_slug` is resolved by the caller —
    /// the store never reaches out to the resolver.
    pub async fn insert_analysis(
        &self,
        report: &AnalysisReport,
        project_slug: &str,
    ) -> Result<(), StoreError> {
        let block_timestamp: DateTime<Utc> = Utc
            .timestamp_opt(report.block_timestamp as i64, 0)
            .single()
            .ok_or_else(|| {
                StoreError::Decimal(format!(
                    "invalid block_timestamp: {}",
                    report.block_timestamp
                ))
            })?;
        let effective_gas_price = bigdecimal_from_u128(report.effective_gas_price_wei)?;
        let wei_saved = bigdecimal_from_u128(report.wei_saved)?;

        sqlx::query(
            r#"
            INSERT INTO analysis (
                chain_id, block_number, block_timestamp, tx_hash, tx_index,
                from_address, to_address, function_selector, project_slug,
                gas_used, effective_gas_price_wei, gaskiller_gas_estimate,
                gas_saved, wei_saved, is_heuristic, failure_reason,
                state_update_count, skipped_opcodes
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18
            )
            ON CONFLICT (chain_id, block_number, tx_index) DO NOTHING
            "#,
        )
        .bind(report.chain_id as i64)
        .bind(report.block_number as i64)
        .bind(block_timestamp)
        .bind(&report.tx_hash[..])
        .bind(report.tx_index as i32)
        .bind(&report.from[..])
        .bind(&report.to[..])
        .bind(&report.function_selector[..])
        .bind(project_slug)
        .bind(report.gas_used as i64)
        .bind(&effective_gas_price)
        .bind(report.gaskiller_gas_estimate as i64)
        .bind(report.gas_saved as i64)
        .bind(&wei_saved)
        .bind(report.is_heuristic)
        .bind(report.failure_reason.as_deref())
        .bind(report.state_update_count as i32)
        .bind(&report.skipped_opcodes)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn upsert_project(&self, project: &Project) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO projects (project_slug, project_name, category, contact_email, contact_url, last_seen_at)
            VALUES ($1, $2, $3, $4, $5, now())
            ON CONFLICT (project_slug) DO UPDATE SET
                project_name = EXCLUDED.project_name,
                category = EXCLUDED.category,
                contact_email = EXCLUDED.contact_email,
                contact_url = EXCLUDED.contact_url,
                last_seen_at = now()
            "#,
        )
        .bind(&project.slug)
        .bind(&project.name)
        .bind(project.category.as_deref())
        .bind(project.contact_email.as_deref())
        .bind(project.contact_url.as_deref())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Upsert an automatic mapping. Will NOT overwrite rows whose
    /// `manual_override` flag is set — that's how human edits made via the
    /// admin UI stay sticky across resolver / labeler refreshes.
    pub async fn upsert_address_project(
        &self,
        chain_id: u64,
        address: [u8; 20],
        slug: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO address_project (chain_id, address, project_slug, manual_override)
            VALUES ($1, $2, $3, FALSE)
            ON CONFLICT (chain_id, address) DO UPDATE SET
                project_slug = EXCLUDED.project_slug
            WHERE address_project.manual_override = FALSE
            "#,
        )
        .bind(chain_id as i64)
        .bind(&address[..])
        .bind(slug)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Upsert a human override. Always wins, sets `manual_override=true` so
    /// no automatic source can clobber it later. Caller is responsible for
    /// ensuring a `projects` row exists for `slug` (FK).
    pub async fn upsert_manual_address_project(
        &self,
        chain_id: u64,
        address: [u8; 20],
        slug: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO address_project (chain_id, address, project_slug, manual_override)
            VALUES ($1, $2, $3, TRUE)
            ON CONFLICT (chain_id, address) DO UPDATE SET
                project_slug    = EXCLUDED.project_slug,
                manual_override = TRUE
            "#,
        )
        .bind(chain_id as i64)
        .bind(&address[..])
        .bind(slug)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Rename a project (display-name only — the slug is the stable key).
    /// Returns whether a row was updated, so callers can distinguish
    /// "renamed" from "no such slug" without an extra SELECT.
    pub async fn rename_project(&self, slug: &str, new_name: &str) -> Result<bool, StoreError> {
        let res = sqlx::query(
            r#"UPDATE projects
               SET project_name = $2,
                   last_seen_at = now()
               WHERE project_slug = $1"#,
        )
        .bind(slug)
        .bind(new_name)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Returns whether the given address has a manual override set. Used by
    /// the UI to decide whether to show "(manual)" alongside the label.
    pub async fn is_manual_override(
        &self,
        chain_id: u64,
        address: [u8; 20],
    ) -> Result<bool, StoreError> {
        let row: Option<(bool,)> = sqlx::query_as(
            r#"SELECT manual_override FROM address_project
               WHERE chain_id = $1 AND address = $2"#,
        )
        .bind(chain_id as i64)
        .bind(&address[..])
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| r.0).unwrap_or(false))
    }

    /// Days already present in `eth_prices`. Used by the backfill flow to
    /// skip days we've already priced.
    pub async fn list_eth_price_days(&self) -> Result<Vec<NaiveDate>, StoreError> {
        let rows: Vec<(NaiveDate,)> = sqlx::query_as("SELECT day FROM eth_prices ORDER BY day")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|(d,)| d).collect())
    }

    /// `(min, max)` day spanned by the `analysis` table, or `None` if empty.
    /// Used by the backfill flow to size the coingecko range request.
    pub async fn analysis_day_range(&self) -> Result<Option<(NaiveDate, NaiveDate)>, StoreError> {
        let row: Option<(Option<NaiveDate>, Option<NaiveDate>)> = sqlx::query_as(
            "SELECT min(block_timestamp)::date, max(block_timestamp)::date FROM analysis",
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(lo, hi)| match (lo, hi) {
            (Some(l), Some(h)) => Some((l, h)),
            _ => None,
        }))
    }

    pub async fn upsert_eth_price(
        &self,
        day: NaiveDate,
        usd_per_eth: BigDecimal,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO eth_prices (day, usd_per_eth)
            VALUES ($1, $2)
            ON CONFLICT (day) DO UPDATE SET usd_per_eth = EXCLUDED.usd_per_eth
            "#,
        )
        .bind(day)
        .bind(&usd_per_eth)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Retro-relabel `analysis` rows whose `project_slug` is still
    /// `unknown:0xADDR` but whose `to_address` now resolves to a real project
    /// via `address_project`. Returns the number of rows updated.
    ///
    /// Called after a resolver refresh that may have introduced new mappings —
    /// historical analyses get corrected without re-running the worker.
    pub async fn relabel_unknowns(&self) -> Result<u64, StoreError> {
        let res = sqlx::query(
            r#"
            UPDATE analysis a
            SET project_slug = ap.project_slug
            FROM address_project ap
            WHERE a.chain_id = ap.chain_id
              AND a.to_address = ap.address
              AND a.project_slug LIKE 'unknown:%'
              AND ap.project_slug NOT LIKE 'unknown:%'
            "#,
        )
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }

    /// Returns top-N unmapped contract addresses for a chain, ranked by total
    /// `wei_saved` over the lifetime. Skips addresses whose last labeling
    /// attempt was within `retry_after_days` and resulted in a non-success
    /// (so we don't waste API budget on contracts that are still unverified).
    pub async fn top_unknown_addresses(
        &self,
        chain_id: u64,
        limit: i64,
        retry_after_days: i64,
    ) -> Result<Vec<UnknownAddressRow>, StoreError> {
        let rows = sqlx::query_as::<_, UnknownAddressRow>(
            r#"
            SELECT
                a.to_address                  AS address,
                COALESCE(SUM(a.wei_saved), 0)::numeric AS wei_saved_total,
                COUNT(*)::bigint              AS tx_count
            FROM analysis a
            LEFT JOIN address_label_attempt l
              ON l.chain_id = a.chain_id
             AND l.address  = a.to_address
            WHERE a.chain_id = $1
              AND a.project_slug LIKE 'unknown:%'
              AND a.gas_saved > 0
              AND cardinality(a.skipped_opcodes) = 0
              AND (
                l.last_attempted_at IS NULL
                -- Transient failures (rate limit, transport) are retried on
                -- every producer cycle, not throttled to retry_after_days.
                -- 'matched' rows can also reappear here if relabel_unknowns
                -- hasn't run yet for that address.
                OR l.last_result IN ('matched','error')
                OR l.last_attempted_at < now() - make_interval(days => $3::int)
              )
            GROUP BY a.to_address
            ORDER BY wei_saved_total DESC NULLS LAST
            LIMIT $2
            "#,
        )
        .bind(chain_id as i64)
        .bind(limit)
        .bind(retry_after_days)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Record an auto-labeler attempt. Idempotent — overwrites prior
    /// attempts for the same address.
    pub async fn upsert_label_attempt(
        &self,
        chain_id: u64,
        address: [u8; 20],
        result: &str,
        contract_name: Option<&str>,
        matched_slug: Option<&str>,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO address_label_attempt
                (chain_id, address, last_attempted_at, last_result, contract_name, matched_slug)
            VALUES ($1, $2, now(), $3, $4, $5)
            ON CONFLICT (chain_id, address) DO UPDATE SET
                last_attempted_at = EXCLUDED.last_attempted_at,
                last_result       = EXCLUDED.last_result,
                contract_name     = EXCLUDED.contract_name,
                matched_slug      = COALESCE(EXCLUDED.matched_slug, address_label_attempt.matched_slug)
            "#,
        )
        .bind(chain_id as i64)
        .bind(&address[..])
        .bind(result)
        .bind(contract_name)
        .bind(matched_slug)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Selectors that appear in `analysis` but aren't yet in
    /// `function_selectors`. Capped to `limit` highest-volume entries
    /// (4byte resolves the popular ones first).
    pub async fn unresolved_selectors(
        &self,
        chain_id: u64,
        limit: i64,
    ) -> Result<Vec<[u8; 4]>, StoreError> {
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(
            r#"SELECT a.function_selector
               FROM analysis a
               LEFT JOIN function_selectors fs
                 ON fs.selector = a.function_selector
               WHERE a.chain_id = $1
                 AND fs.selector IS NULL
                 AND a.gas_saved > 0
                 AND cardinality(a.skipped_opcodes) = 0
               GROUP BY a.function_selector
               ORDER BY count(*) DESC
               LIMIT $2"#,
        )
        .bind(chain_id as i64)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        let mut out = Vec::with_capacity(rows.len());
        for (b,) in rows {
            if b.len() == 4 {
                let mut sel = [0u8; 4];
                sel.copy_from_slice(&b);
                out.push(sel);
            }
        }
        Ok(out)
    }

    pub async fn upsert_function_selector(
        &self,
        selector: [u8; 4],
        primary_name: Option<&str>,
        primary_sig: Option<&str>,
        all_signatures: &[String],
        source: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO function_selectors
                (selector, primary_name, primary_sig, all_signatures, source, fetched_at)
            VALUES ($1, $2, $3, $4, $5, now())
            ON CONFLICT (selector) DO UPDATE SET
                primary_name   = EXCLUDED.primary_name,
                primary_sig    = EXCLUDED.primary_sig,
                all_signatures = EXCLUDED.all_signatures,
                source         = EXCLUDED.source,
                fetched_at     = now()
            "#,
        )
        .bind(&selector[..])
        .bind(primary_name)
        .bind(primary_sig)
        .bind(all_signatures)
        .bind(source)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn org_upsert(&self, org_slug: &str, org_name: &str) -> Result<(), StoreError> {
        sqlx::query(
            r#"INSERT INTO organizations (org_slug, org_name)
               VALUES ($1, $2)
               ON CONFLICT (org_slug) DO UPDATE SET org_name = EXCLUDED.org_name"#,
        )
        .bind(org_slug)
        .bind(org_name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn project_assign_org(
        &self,
        project_slug: &str,
        org_slug: Option<&str>,
    ) -> Result<bool, StoreError> {
        let res = sqlx::query(r#"UPDATE projects SET org_slug = $2 WHERE project_slug = $1"#)
            .bind(project_slug)
            .bind(org_slug)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn blacklist_add(
        &self,
        chain_id: u64,
        address: [u8; 20],
        selector: Option<[u8; 4]>,
        reason: &str,
        created_by: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"INSERT INTO analysis_exclusion
                 (chain_id, address, selector, reason, created_by)
               VALUES ($1, $2, $3, $4, $5)
               ON CONFLICT (chain_id, address, COALESCE(selector, ''::bytea))
                 DO UPDATE SET
                   reason     = EXCLUDED.reason,
                   created_by = EXCLUDED.created_by,
                   created_at = now()"#,
        )
        .bind(chain_id as i64)
        .bind(&address[..])
        .bind(selector.as_ref().map(|s| &s[..]))
        .bind(reason)
        .bind(created_by)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn blacklist_remove(
        &self,
        chain_id: u64,
        address: [u8; 20],
        selector: Option<[u8; 4]>,
    ) -> Result<bool, StoreError> {
        let res = sqlx::query(
            r#"DELETE FROM analysis_exclusion
               WHERE chain_id = $1
                 AND address  = $2
                 AND COALESCE(selector, ''::bytea) = COALESCE($3, ''::bytea)"#,
        )
        .bind(chain_id as i64)
        .bind(&address[..])
        .bind(selector.as_ref().map(|s| &s[..]))
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn refresh_rollups(&self) -> Result<(), StoreError> {
        // CONCURRENTLY needs the unique index, which both rollups declare.
        sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY project_daily")
            .execute(&self.pool)
            .await?;
        sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY function_daily")
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

fn bigdecimal_from_u128(v: u128) -> Result<BigDecimal, StoreError> {
    BigDecimal::from_str(&v.to_string()).map_err(|e| StoreError::Decimal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bigdecimal_round_trip() {
        let v = bigdecimal_from_u128(12345678901234567890u128).unwrap();
        assert_eq!(v.to_string(), "12345678901234567890");
    }

    #[test]
    fn bigdecimal_from_zero() {
        let v = bigdecimal_from_u128(0).unwrap();
        assert_eq!(v.to_string(), "0");
    }
}

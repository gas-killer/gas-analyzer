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
                state_update_count
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17
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

    pub async fn upsert_address_project(
        &self,
        chain_id: u64,
        address: [u8; 20],
        slug: &str,
    ) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            INSERT INTO address_project (chain_id, address, project_slug)
            VALUES ($1, $2, $3)
            ON CONFLICT (chain_id, address) DO UPDATE SET
                project_slug = EXCLUDED.project_slug
            "#,
        )
        .bind(chain_id as i64)
        .bind(&address[..])
        .bind(slug)
        .execute(&self.pool)
        .await?;
        Ok(())
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

    pub async fn refresh_rollups(&self) -> Result<(), StoreError> {
        // CONCURRENTLY needs the unique index, which we declared.
        sqlx::query("REFRESH MATERIALIZED VIEW CONCURRENTLY project_daily")
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

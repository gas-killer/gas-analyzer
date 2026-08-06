-- Rebuild project_daily to exclude rows where the analyzer encountered
-- unsupported opcodes (their gas_saved is unreliable). Also enforces the
-- "no 0% savings" rule at the source so downstream queries don't have to.
--
-- Schema is otherwise identical to 20260512000001_project_daily_with_spend.

DROP MATERIALIZED VIEW IF EXISTS project_daily CASCADE;

CREATE MATERIALIZED VIEW project_daily AS
SELECT
    a.chain_id,
    a.project_slug,
    date_trunc('day', a.block_timestamp)::date                  AS day,
    count(*)                                                    AS tx_count,
    count(*) FILTER (WHERE a.gas_saved > 0)                     AS covered_tx_count,
    sum(a.gas_used)                                             AS gas_used_total,
    sum(a.gas_saved)                                            AS gas_saved_total,
    sum(a.wei_saved)                                            AS wei_saved_total,
    sum(a.gas_used::numeric * a.effective_gas_price_wei)        AS wei_spent_total,
    coalesce(
        sum(a.wei_saved) / 1e18 * p.usd_per_eth,
        0
    )                                                           AS usd_saved_total,
    avg(a.gas_saved::numeric / NULLIF(a.gas_used, 0))           AS avg_savings_pct,
    sum(case when a.is_heuristic then 1 else 0 end)::float8 / count(*)
                                                                AS heuristic_rate
FROM analysis a
LEFT JOIN eth_prices p ON p.day = date_trunc('day', a.block_timestamp)::date
WHERE cardinality(a.skipped_opcodes) = 0
  AND a.gas_saved > 0
GROUP BY a.chain_id, a.project_slug, date_trunc('day', a.block_timestamp)::date, p.usd_per_eth;

CREATE UNIQUE INDEX project_daily_unique_idx
    ON project_daily (chain_id, project_slug, day);

REFRESH MATERIALIZED VIEW project_daily;

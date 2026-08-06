-- Extend project_daily with the columns BD needs to answer
-- "which project would benefit most from gas-killer":
--
--   * wei_spent_total   = total gas bill the project actually paid that day
--   * covered_tx_count  = txs where gas-killer would have produced savings
--
-- These let the leaderboard compute savings-to-gas-bill ratio and a
-- coverage % directly off the rollup. Median savings stays on-demand
-- against analysis (medians do not compose across daily rows).

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
GROUP BY a.chain_id, a.project_slug, date_trunc('day', a.block_timestamp)::date, p.usd_per_eth;

CREATE UNIQUE INDEX project_daily_unique_idx
    ON project_daily (chain_id, project_slug, day);

REFRESH MATERIALIZED VIEW project_daily;

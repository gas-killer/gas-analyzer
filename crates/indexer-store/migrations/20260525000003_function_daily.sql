-- function_daily: per-(contract, selector, day) rollup.
--
-- Why: project-level rollups hide the actual unit of optimization. A token
-- contract like Tethr can rank #1 on project_daily because its transfer()
-- saves a few percent across millions of txs — but a BD pitch needs the
-- function with the largest absolute USD-saved figure, regardless of which
-- contract houses it.
--
-- Filters at source mirror project_daily: no opcode-skipped rows, no 0%
-- savings rows.

CREATE MATERIALIZED VIEW IF NOT EXISTS function_daily AS
SELECT
    a.chain_id,
    a.to_address,
    a.function_selector,
    a.project_slug,
    date_trunc('day', a.block_timestamp)::date                  AS day,
    count(*)                                                    AS tx_count,
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
GROUP BY a.chain_id, a.to_address, a.function_selector, a.project_slug,
         date_trunc('day', a.block_timestamp)::date, p.usd_per_eth;

-- CONCURRENTLY refresh needs a unique index. Must include project_slug —
-- when an address is relabeled mid-day (e.g. an `unknown:0x…` row gets
-- mapped to a known project) the MV legitimately holds one row per slug
-- for that (address, selector, day).
CREATE UNIQUE INDEX IF NOT EXISTS function_daily_unique_idx
    ON function_daily (chain_id, to_address, function_selector, project_slug, day);

-- Common access patterns: leaderboard by chain (recent window).
CREATE INDEX IF NOT EXISTS function_daily_chain_day_idx
    ON function_daily (chain_id, day DESC);

REFRESH MATERIALIZED VIEW function_daily;

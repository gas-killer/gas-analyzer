-- One row per analyzed transaction. Not partitioned in v1 — Postgres handles
-- 100M+ rows fine with the indexes below. Add daily partitioning later if
-- needed (the partition column would be `block_timestamp`).
CREATE TABLE IF NOT EXISTS analysis (
    chain_id                BIGINT       NOT NULL,
    block_number            BIGINT       NOT NULL,
    block_timestamp         TIMESTAMPTZ  NOT NULL,
    tx_hash                 BYTEA        NOT NULL,
    tx_index                INTEGER      NOT NULL,
    from_address            BYTEA        NOT NULL,
    to_address              BYTEA        NOT NULL,
    function_selector       BYTEA        NOT NULL,
    project_slug            TEXT         NOT NULL,
    gas_used                BIGINT       NOT NULL,
    effective_gas_price_wei NUMERIC(40,0) NOT NULL,
    gaskiller_gas_estimate  BIGINT       NOT NULL,
    gas_saved               BIGINT       NOT NULL,
    wei_saved               NUMERIC(40,0) NOT NULL,
    is_heuristic            BOOLEAN      NOT NULL,
    failure_reason          TEXT,
    state_update_count      INTEGER      NOT NULL,
    inserted_at             TIMESTAMPTZ  NOT NULL DEFAULT now(),
    PRIMARY KEY (chain_id, block_number, tx_index)
);

CREATE INDEX IF NOT EXISTS analysis_project_time_idx
    ON analysis (project_slug, block_timestamp DESC);
CREATE INDEX IF NOT EXISTS analysis_chain_time_idx
    ON analysis (chain_id, block_timestamp DESC);
CREATE INDEX IF NOT EXISTS analysis_to_address_idx
    ON analysis (chain_id, to_address);

CREATE TABLE IF NOT EXISTS projects (
    project_slug   TEXT PRIMARY KEY,
    project_name   TEXT NOT NULL,
    category       TEXT,
    contact_email  TEXT,
    contact_url    TEXT,
    last_seen_at   TIMESTAMPTZ
);

CREATE TABLE IF NOT EXISTS eth_prices (
    day            DATE PRIMARY KEY,
    usd_per_eth    NUMERIC(20,8) NOT NULL
);

CREATE TABLE IF NOT EXISTS address_project (
    chain_id     BIGINT NOT NULL,
    address      BYTEA  NOT NULL,
    project_slug TEXT   NOT NULL REFERENCES projects(project_slug),
    PRIMARY KEY (chain_id, address)
);

-- Hourly-refreshed rollup feeding the BD dashboard.
CREATE MATERIALIZED VIEW IF NOT EXISTS project_daily AS
SELECT
    a.chain_id,
    a.project_slug,
    date_trunc('day', a.block_timestamp)::date AS day,
    count(*)                                    AS tx_count,
    sum(a.gas_used)                             AS gas_used_total,
    sum(a.gas_saved)                            AS gas_saved_total,
    sum(a.wei_saved)                            AS wei_saved_total,
    coalesce(
        sum(a.wei_saved) / 1e18 * p.usd_per_eth,
        0
    )                                           AS usd_saved_total,
    avg(a.gas_saved::numeric / NULLIF(a.gas_used, 0)) AS avg_savings_pct,
    sum(case when a.is_heuristic then 1 else 0 end)::float8 / count(*) AS heuristic_rate
FROM analysis a
LEFT JOIN eth_prices p ON p.day = date_trunc('day', a.block_timestamp)::date
GROUP BY a.chain_id, a.project_slug, date_trunc('day', a.block_timestamp)::date, p.usd_per_eth;

CREATE UNIQUE INDEX IF NOT EXISTS project_daily_unique_idx
    ON project_daily (chain_id, project_slug, day);

-- Blacklist of `(contract, [selector])` pairs that should be excluded
-- from every BD-visible aggregate. Contract-only rows (selector IS NULL)
-- hide the whole contract; selector-scoped rows hide just one function.
--
-- Filter applied at query time via LEFT JOIN — small table, kept hot in
-- memory, no MV rebuild required to add/remove entries.

CREATE TABLE IF NOT EXISTS analysis_exclusion (
    chain_id    BIGINT      NOT NULL,
    address     BYTEA       NOT NULL,
    selector    BYTEA,                       -- NULL = whole contract
    reason      TEXT        NOT NULL,
    created_by  TEXT        NOT NULL,        -- session username
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Coalesce NULL to a zero-byte tag so the UNIQUE works across contract
-- + per-selector entries.
CREATE UNIQUE INDEX IF NOT EXISTS analysis_exclusion_unique
    ON analysis_exclusion (chain_id, address, COALESCE(selector, ''::bytea));

CREATE INDEX IF NOT EXISTS analysis_exclusion_chain_idx
    ON analysis_exclusion (chain_id);

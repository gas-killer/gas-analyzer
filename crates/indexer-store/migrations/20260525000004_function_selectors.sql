-- 4byte.directory cache. The resolver scans `analysis` for selectors not
-- yet in this table, batch-fetches signatures, and writes the most-used
-- canonical signature as `primary_name`. `all_signatures` keeps the full
-- ambiguity set so the UI can disclose collisions.

CREATE TABLE IF NOT EXISTS function_selectors (
    selector       BYTEA       PRIMARY KEY,    -- 4 bytes
    primary_name   TEXT,                       -- "transfer"
    primary_sig    TEXT,                       -- "transfer(address,uint256)"
    all_signatures TEXT[]      NOT NULL DEFAULT '{}',
    source         TEXT        NOT NULL,       -- 'fourbyte' | 'llm' | 'manual' | 'unresolved'
    fetched_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS function_selectors_source_idx
    ON function_selectors (source);

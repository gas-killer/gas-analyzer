-- Tracks attempts by the auto-labeler to resolve `unknown:0xADDR` rows into a
-- real project_slug via external sources (Etherscan). One row per
-- (chain_id, address); upserted on every attempt. The producer query joins
-- this table to skip recently-failed addresses so we don't burn API budget.
CREATE TABLE IF NOT EXISTS address_label_attempt (
    chain_id          BIGINT       NOT NULL,
    address           BYTEA        NOT NULL,
    last_attempted_at TIMESTAMPTZ  NOT NULL,
    last_result       TEXT         NOT NULL,  -- 'matched' | 'unverified' | 'no-match' | 'error'
    contract_name     TEXT,
    matched_slug      TEXT,
    PRIMARY KEY (chain_id, address)
);

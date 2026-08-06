-- Track the opcodes the analyzer encountered but does not yet simulate.
-- Rows with a non-empty set produce inflated savings (e.g. CREATE returns
-- 0 gas in our model), so all aggregate queries filter them out by default.
--
-- No backfill — historicals stay at the default '{}' and remain in
-- aggregates. Per agreed scope: live with the existing skew, refactor
-- ensures going-forward rows are correctly flagged.

ALTER TABLE analysis
    ADD COLUMN IF NOT EXISTS skipped_opcodes TEXT[] NOT NULL DEFAULT '{}';

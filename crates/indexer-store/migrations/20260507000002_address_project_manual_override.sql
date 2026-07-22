-- Adds a sticky-bit on address_project rows so manual edits from the admin
-- UI survive the next resolver / auto-labeler upsert cycle. Without it, a
-- human override gets clobbered the moment DefiLlama or the labeler runs.
ALTER TABLE address_project
    ADD COLUMN IF NOT EXISTS manual_override BOOLEAN NOT NULL DEFAULT FALSE;

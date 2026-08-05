-- Organizations group one or more projects under a parent brand (e.g.
-- "Uniswap" with project rows for v2/v3/x). The grouped leaderboard
-- gives BD a single "$X saved across N contracts" line per protocol
-- instead of N partial lines.

CREATE TABLE IF NOT EXISTS organizations (
    org_slug    TEXT        PRIMARY KEY,
    org_name    TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Nullable so existing projects don't need an immediate assignment.
ALTER TABLE projects
    ADD COLUMN IF NOT EXISTS org_slug TEXT REFERENCES organizations(org_slug) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS projects_org_idx ON projects (org_slug);

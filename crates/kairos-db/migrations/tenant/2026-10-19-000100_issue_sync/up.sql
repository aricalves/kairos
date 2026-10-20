-- KAIROS-T-0670 (COLLIERY-I-0623): a repository takes its open issues on a
-- cadence, with no webhook. The cadence is a sync: each run lists the open
-- issues and compares them with `forge_issue_intake`. An open issue with no
-- row there becomes one support task.
--
--   repositories.issue_import_interval_secs   NULL (off, the default) or
--       the seconds between two runs: at least 60, at most 30 days.
--   forge_issue_sync   One row for each repository that has had a run or a
--       schedule: when the next run is due, and what the last run did.
--   item_links.repository_id   The repository of a link that has no
--       webhook connection. The link of an issue that the cadence took has
--       no connection, because the cadence needs none.
--   item_links.connection_id   May be NULL now. A link has a connection or
--       a repository.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table. 1 nullable column on `repositories`, 1 on `item_links`: no
-- row is rewritten, and every repository keeps the cadence off. No row is
-- changed. `item_links` and `repositories` are small tables, so the plain
-- FOREIGN KEY and CHECK take a short lock.
ALTER TABLE repositories
    ADD COLUMN IF NOT EXISTS issue_import_interval_secs INTEGER;
ALTER TABLE repositories
    DROP CONSTRAINT IF EXISTS repositories_issue_import_interval_check;
ALTER TABLE repositories
    ADD CONSTRAINT repositories_issue_import_interval_check
    CHECK (issue_import_interval_secs IS NULL
           OR (issue_import_interval_secs >= 60 AND issue_import_interval_secs <= 2592000));

CREATE TABLE IF NOT EXISTS forge_issue_sync (
    repository_id       UUID PRIMARY KEY REFERENCES repositories (id) ON DELETE CASCADE,
    -- When the next cadence run is due. NULL: due now.
    next_run_at         TIMESTAMPTZ,
    -- When the last run started, and when it ended. A run that started and
    -- did not end (a crash) shows as not complete.
    last_run_started_at TIMESTAMPTZ,
    last_run_at         TIMESTAMPTZ,
    -- True: the last run listed every page. False: it stopped half way, or
    -- it has not ended.
    last_run_complete   BOOLEAN NOT NULL DEFAULT false,
    -- The end of the last complete run.
    last_complete_at    TIMESTAMPTZ,
    -- Open issues that the last run listed, and tasks that it made.
    last_run_listed     INTEGER NOT NULL DEFAULT 0,
    last_run_imported   INTEGER NOT NULL DEFAULT 0,
    last_error          TEXT,
    last_error_at       TIMESTAMPTZ,
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now()
);

ALTER TABLE item_links ALTER COLUMN connection_id DROP NOT NULL;
ALTER TABLE item_links
    ADD COLUMN IF NOT EXISTS repository_id UUID REFERENCES repositories (id);
ALTER TABLE item_links DROP CONSTRAINT IF EXISTS item_links_owner_check;
ALTER TABLE item_links ADD CONSTRAINT item_links_owner_check
    CHECK (connection_id IS NOT NULL OR repository_id IS NOT NULL);

-- KAIROS-T-0482 (COLLIERY-I-0623, KAIROS-A-0028): an opened GitHub or GitLab
-- issue that the forge filter admits becomes one support task on the
-- delivery board of the team of the repository.
--
-- `forge_issue_intake` is the key of the intake: one row per issue of a
-- repository, by the forge's own id of the issue. A redelivered event, or
-- an import of an issue that a webhook took already, finds the row and
-- makes nothing. After intake nothing is synced.
--
-- `item_links.kind` gets the value `issue`: the task links to its issue.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table. The CHECK on `item_links.kind` gets one more value. No row
-- is changed.
ALTER TABLE item_links DROP CONSTRAINT IF EXISTS item_links_kind_check;
ALTER TABLE item_links ADD CONSTRAINT item_links_kind_check
    CHECK (kind IN ('branch', 'pull_request', 'issue'));

CREATE TABLE IF NOT EXISTS forge_issue_intake (
    repository_id UUID NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    -- GitHub `issue.id`, GitLab `id`: stable, unlike the number in a move.
    issue_id      TEXT NOT NULL,
    -- The number that people see (#12; GitLab `iid`).
    number        BIGINT NOT NULL,
    -- The task that the intake made. No FK: item ids span the entity
    -- tables (the `item_links` convention).
    item_id       UUID NOT NULL,
    url           TEXT NOT NULL,
    author        TEXT NOT NULL DEFAULT '',
    -- `webhook` or `import` (KAIROS-T-0483).
    source        TEXT NOT NULL CHECK (source IN ('webhook', 'import')),
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repository_id, issue_id)
);

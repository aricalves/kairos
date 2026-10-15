-- KAIROS-T-0480 (COLLIERY-I-0623): every branch and pull request that the
-- forge filter of a repository admits is recorded for the repository, with
-- or without a short code. A row with a short code is also linked to its
-- items in `item_links`, as before. Like `item_links`, the rows mirror the
-- forge: `forge_updated_at` is the ordering guard of the upsert.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table. No row is changed. The branches and pull requests linked
-- before this migration are not copied: the next event of each records it.
CREATE TABLE IF NOT EXISTS forge_refs (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    repository_id     UUID NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    kind              TEXT NOT NULL CHECK (kind IN ('branch', 'pull_request')),
    -- PR/MR number, or the branch name.
    external_id       TEXT NOT NULL,
    title             TEXT NOT NULL,
    url               TEXT NOT NULL,
    state             TEXT NOT NULL CHECK (state IN ('open', 'merged', 'closed', 'draft')),
    author            TEXT NOT NULL DEFAULT '',
    -- The head branch of a pull request, or the branch itself.
    head_branch       TEXT NOT NULL DEFAULT '',
    forge_updated_at  TIMESTAMPTZ NOT NULL,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repository_id, kind, external_id)
);

CREATE INDEX IF NOT EXISTS idx_forge_refs_repository_state
    ON forge_refs (repository_id, state, forge_updated_at DESC);

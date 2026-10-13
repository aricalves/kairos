-- KAIROS-T-0406 (COLLIERY-I-0619, KAIROS-D-0012): a repository can publish a
-- built static site.
--
--   repositories.site_enabled     The site switch of the repository. Off by
--                                 default: a publish is refused, and the site
--                                 is not served.
--   repositories.site_publish_id  The live publish, or NULL.
--   site_publishes                One row for each publish: where it came
--                                 from (the repository slug at that time),
--                                 the base path it was built for, the MD5 of
--                                 the archive, the sizes, and who sent it.
--   site_files                    The files of each publish: the manifest
--                                 that serving reads, so it does not list
--                                 the store.
--
-- `state`: 'live' (the one that `site_publish_id` names), 'replaced' (a
-- newer publish took its place), 'unpublished'. The files of a publish
-- that is not live are deleted from the store by a job; its rows stay as
-- the history.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 2 new columns on repositories (each repository gets site_enabled false
-- and no live publish) and 2 new tables, empty. No row is deleted.
--
-- Re-runnable: each statement has a guard.
ALTER TABLE repositories ADD COLUMN IF NOT EXISTS site_enabled BOOLEAN NOT NULL DEFAULT false;

CREATE TABLE IF NOT EXISTS site_publishes (
    id               UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    repository_id    UUID NOT NULL REFERENCES repositories (id),
    repository_slug  TEXT NOT NULL,
    base_path        TEXT NOT NULL,
    md5              TEXT NOT NULL CHECK (md5 ~ '^[0-9a-f]{32}$'),
    size             BIGINT NOT NULL CHECK (size >= 0),
    unpacked_size    BIGINT NOT NULL CHECK (unpacked_size >= 0),
    files            INTEGER NOT NULL CHECK (files >= 0),
    principal_id     UUID NOT NULL,
    agent_key_id     UUID,
    published_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    state            TEXT NOT NULL CHECK (state IN ('live', 'replaced', 'unpublished'))
);

-- The history of a repository reads the newest publishes first.
CREATE INDEX IF NOT EXISTS site_publishes_repository_published
    ON site_publishes (repository_id, published_at DESC);

CREATE TABLE IF NOT EXISTS site_files (
    publish_id    UUID NOT NULL REFERENCES site_publishes (id) ON DELETE CASCADE,
    path          TEXT NOT NULL,
    size          BIGINT NOT NULL CHECK (size >= 0),
    sha256        TEXT NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    content_type  TEXT NOT NULL,
    PRIMARY KEY (publish_id, path)
);

ALTER TABLE repositories ADD COLUMN IF NOT EXISTS site_publish_id UUID REFERENCES site_publishes (id);

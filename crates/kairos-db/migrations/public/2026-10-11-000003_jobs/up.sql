-- KAIROS-T-0373 (COLLIERY-I-0613): the job queue of the background work.
--
-- PUBLIC, with a tenant column, not one table in each tenant schema. Each
-- server runs one worker loop, and the worker takes the next job of ALL the
-- organizations with one query (priority, then age). With a table in each
-- org_* schema, the worker would have to read each schema in turn, and a
-- job of one organization could not be ordered against a job of another.
-- A job of the deployment (a periodic pass over all the organizations) has
-- no tenant.
--
-- A worker takes a job with SELECT ... FOR UPDATE SKIP LOCKED. A leased job
-- has an owner (leased_by: the random id of the server start, KAIROS-T-0354)
-- and a lease. The progress of the job moves heartbeat_at and lease_until.
-- When the lease runs out, any server can take the job again. tries counts
-- the leases; after max_tries the job is failed, with the reason in
-- last_error. Re-runnable.
CREATE TABLE IF NOT EXISTS jobs (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    kind          TEXT NOT NULL,
    -- The slug of the organization; NULL for a job of the deployment.
    tenant        TEXT,
    payload       JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- A higher priority runs first.
    priority      INTEGER NOT NULL DEFAULT 0,
    -- A job with the key of a queued or leased job is not added again.
    dedupe_key    TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Not taken before this time. Inside one priority, the oldest goes first.
    run_after     TIMESTAMPTZ NOT NULL DEFAULT now(),
    state         TEXT NOT NULL DEFAULT 'queued'
                  CHECK (state IN ('queued', 'leased', 'done', 'failed')),
    leased_by     UUID,
    lease_until   TIMESTAMPTZ,
    heartbeat_at  TIMESTAMPTZ,
    tries         INTEGER NOT NULL DEFAULT 0,
    max_tries     INTEGER NOT NULL DEFAULT 3 CHECK (max_tries >= 1),
    last_error    TEXT,
    finished_at   TIMESTAMPTZ
);

-- One queued or leased job for each key.
CREATE UNIQUE INDEX IF NOT EXISTS jobs_dedupe_key
    ON jobs (dedupe_key)
    WHERE state IN ('queued', 'leased') AND dedupe_key IS NOT NULL;

-- The take query: the open jobs of some kinds, by priority and age.
CREATE INDEX IF NOT EXISTS jobs_open
    ON jobs (kind, priority DESC, run_after, created_at)
    WHERE state IN ('queued', 'leased');

-- The removal of old finished jobs.
CREATE INDEX IF NOT EXISTS jobs_finished
    ON jobs (finished_at)
    WHERE state IN ('done', 'failed');

-- KAIROS-T-0399 (COLLIERY-I-0620, GitHub #34): a per-repository breaker of
-- the code index builder.
--
--   code_index_breakers   One row for each repository whose breaker is
--                         tripped. No row: the breaker is not tripped.
--
-- A build job whose lease ran out (its server stopped, ran out of memory,
-- or stalled) is not run again: the breaker of its repository trips. While
-- it is tripped, the sweep adds no build job for the repository (no update,
-- no first build), and a build job of the repository that waits does not
-- build. A build that a person asks for removes the row.
--
-- `tripped_at`: when the breaker tripped. `reason`: the error of the job.
-- `job_id`: the job that tripped it; the job queue removes old jobs, so it
-- has no foreign key. `repository_id` has no foreign key, as
-- `code_index_builds.repository_id` has none: a repository is never
-- removed from its table.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table, empty. No row of a table that the tenant has is changed or
-- deleted.
--
-- Re-runnable: the table is added once.
CREATE TABLE IF NOT EXISTS code_index_breakers (
    repository_id   UUID PRIMARY KEY,
    tripped_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    reason          TEXT NOT NULL,
    job_id          UUID
);

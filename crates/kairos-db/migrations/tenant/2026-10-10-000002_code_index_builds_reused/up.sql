-- KAIROS-T-0335: each run of the builder records the summaries that it
-- reused: from the pool of the repository, or from the work file of a run
-- that did not end (a restart). NULL for a run that ended before this
-- version, or that has no counts (an upload, a failure). Re-runnable.
ALTER TABLE code_index_builds ADD COLUMN IF NOT EXISTS summaries_reused INTEGER;

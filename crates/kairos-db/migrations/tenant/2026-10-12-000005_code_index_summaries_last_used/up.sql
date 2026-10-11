-- KAIROS-T-0378: a build writes each summary to the pool of the
-- repository when the model makes it, before an index uses it. So the
-- pool can hold rows that no stored index uses: the summaries of a build
-- that did not end, or of code that went away before the build ended.
--
-- `last_used_at`: the time of the last write of a stored index that uses
-- the row (an upload or a build). NULL: no stored index has used the row.
-- The retention pass deletes a row with NULL whose `created_at` is older
-- than KAIROS_CODE_INDEX_POOL_DAYS (30).
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- Before this version, the pool held only the rows of stored indexes. So
-- each row that is there now gets the time of this migration: it is in
-- use. No row is deleted.
--
-- Re-runnable: the column is added once. The DEFAULT fills the rows that
-- are there when the column is added, and is then dropped, so a second
-- run does not mark a new row as used.
ALTER TABLE code_index_summaries
    ADD COLUMN IF NOT EXISTS last_used_at TIMESTAMPTZ DEFAULT now();
ALTER TABLE code_index_summaries ALTER COLUMN last_used_at DROP DEFAULT;

-- The retention pass reads the rows that no index used, by age.
CREATE INDEX IF NOT EXISTS code_index_summaries_unused_idx
    ON code_index_summaries (created_at)
    WHERE last_used_at IS NULL;

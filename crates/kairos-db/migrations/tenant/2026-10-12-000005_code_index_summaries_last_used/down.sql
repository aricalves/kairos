-- Reverse of KAIROS-T-0378: removes the time of the last use of each pool
-- row. The rows stay.
DROP INDEX IF EXISTS code_index_summaries_unused_idx;
ALTER TABLE code_index_summaries DROP COLUMN IF EXISTS last_used_at;

-- Reverse of KAIROS-T-0495: removes the missing summaries of each run.
ALTER TABLE code_index_builds DROP COLUMN IF EXISTS summaries_missing;

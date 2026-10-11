-- Reverse of KAIROS-T-0335: removes the reused summaries of each run.
ALTER TABLE code_index_builds DROP COLUMN IF EXISTS summaries_reused;

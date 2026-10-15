-- Reverse of KAIROS-T-0478: removes the SCIP warnings of each run.
ALTER TABLE code_index_builds DROP COLUMN IF EXISTS scip_warnings;

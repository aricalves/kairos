-- KAIROS-T-0478: the SCIP warnings of a run of the builder: each language
-- that fell back to the edges by name, and why (COLLIERY-I-0622). A JSON
-- array of {"language", "reason"}; NULL for a run with no warning, or from
-- before this version. Re-runnable.
ALTER TABLE code_index_builds ADD COLUMN IF NOT EXISTS scip_warnings TEXT;

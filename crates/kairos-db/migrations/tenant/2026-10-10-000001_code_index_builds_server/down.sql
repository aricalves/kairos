-- Reverse of KAIROS-T-0354: removes the server start of each run.
ALTER TABLE code_index_builds DROP COLUMN IF EXISTS server_instance;

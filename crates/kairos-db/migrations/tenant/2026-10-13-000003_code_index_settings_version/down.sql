-- Reverse of KAIROS-T-0415: removes the version of the code index settings.
ALTER TABLE code_index_settings DROP COLUMN IF EXISTS version;

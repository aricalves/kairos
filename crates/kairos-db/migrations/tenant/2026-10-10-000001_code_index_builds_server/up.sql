-- KAIROS-T-0354: each run of the code index builder records the start of
-- the server process that runs it, as a random id that the process makes
-- when it starts. At its start, the builder ends only the `running` runs of
-- another id: the server of those runs stopped. A run of the server that
-- runs now is never ended as stopped. NULL for a run that started before
-- this version: such a run is of a server that stopped. Re-runnable.
ALTER TABLE code_index_builds ADD COLUMN IF NOT EXISTS server_instance UUID;

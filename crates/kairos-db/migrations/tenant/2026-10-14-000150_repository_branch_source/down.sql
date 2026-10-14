-- Reverse of KAIROS-T-0430: removes the source of the default branch and
-- the branch of the forge.
ALTER TABLE repositories DROP COLUMN IF EXISTS forge_branch;
ALTER TABLE repositories DROP COLUMN IF EXISTS default_branch_source;

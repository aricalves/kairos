-- KAIROS-T-0430: the code index builder indexes the default branch of the
-- forge, read on each fetch. A branch set by hand wins.
--
--   repositories.default_branch_source  'forge': the builder reads the
--                                       default branch of the remote on
--                                       each fetch and writes it to
--                                       default_branch. 'hand': a person
--                                       set default_branch, and the
--                                       builder keeps it.
--   repositories.forge_branch           The default branch that the remote
--                                       gave at the last fetch, or NULL.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 2 new columns on repositories. Each existing repository gets 'forge' and
-- no forge branch: it follows the forge from the next fetch. Its
-- default_branch stays until then, and stays when the remote gives no
-- default branch. A person sets the branch again to keep it by hand. A new
-- row that does not name the column gets 'hand': the API names it.
-- No row is deleted.
--
-- Re-runnable: each statement has a guard or is idempotent.
ALTER TABLE repositories
    ADD COLUMN IF NOT EXISTS default_branch_source TEXT NOT NULL DEFAULT 'forge'
        CHECK (default_branch_source IN ('forge', 'hand'));
ALTER TABLE repositories ALTER COLUMN default_branch_source SET DEFAULT 'hand';
ALTER TABLE repositories ADD COLUMN IF NOT EXISTS forge_branch TEXT;

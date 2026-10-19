-- KAIROS-T-0536: an agent key has a scope, a list of capabilities and an
-- optional list of boards. A request with the key can do only what the
-- person can do AND what the scope lets it do.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- api_keys gets 2 columns, NULL in each row. NULL is "no scope": the key
-- keeps each right of its principal, as before. No row is changed. Only a
-- key that is made after this migration gets a scope.
--
-- Re-runnable: each column is added IF NOT EXISTS.
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS scope_capabilities TEXT[];
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS scope_boards UUID[];

-- KAIROS-T-0536, down. The scope of each key is lost: each key then acts
-- with each right of its principal.
ALTER TABLE api_keys DROP COLUMN IF EXISTS scope_boards;
ALTER TABLE api_keys DROP COLUMN IF EXISTS scope_capabilities;

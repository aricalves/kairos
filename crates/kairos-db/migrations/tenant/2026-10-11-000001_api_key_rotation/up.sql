-- KAIROS-T-0370: the rotation of an API key or an agent key.
--
--   api_keys.replaced_by   The key that a rotation made in place of this
--                          key. NULL for a key that no rotation replaced.
--                          The rotation also sets `expires_at` of the old
--                          key to the end of the grace time.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new column, NULL in each row. No row is changed. The expiry of the
-- keys that exist does not change.
--
-- Re-runnable: the statement has a guard.
ALTER TABLE api_keys
    ADD COLUMN IF NOT EXISTS replaced_by UUID NULL
        REFERENCES api_keys(id) ON DELETE SET NULL;

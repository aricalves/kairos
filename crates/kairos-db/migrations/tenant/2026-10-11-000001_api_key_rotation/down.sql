-- KAIROS-T-0370, down. The link from a rotated key to its new key is lost.
-- The keys and their expiry stay.
ALTER TABLE api_keys DROP COLUMN IF EXISTS replaced_by;

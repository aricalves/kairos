DELETE FROM organization_members
 WHERE user_id IN (SELECT id FROM users WHERE kind = 'system');
DELETE FROM users WHERE kind = 'system';
ALTER TABLE users DROP CONSTRAINT IF EXISTS users_system_has_no_password;
ALTER TABLE users DROP CONSTRAINT IF EXISTS users_kind_check;
ALTER TABLE users ADD CONSTRAINT users_kind_check
    CHECK (kind IN ('human', 'service_account'));

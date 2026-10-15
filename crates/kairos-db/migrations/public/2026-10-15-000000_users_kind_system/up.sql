-- KAIROS-T-0481 (COLLIERY-I-0623): the forge system actor. A `users` row with
-- kind 'system' acts for a forge: it creates the items of issue intake and
-- it is the actor of webhook events. It is not a login: it has no password
-- (the CHECK below), and the server refuses it an OIDC login and an API key.
--
-- WHAT THIS DOES TO THE DATA
--
-- No row is changed. The CHECK on `kind` gets one more value, and one new
-- CHECK holds that a system row has no password.
ALTER TABLE users DROP CONSTRAINT IF EXISTS users_kind_check;
ALTER TABLE users ADD CONSTRAINT users_kind_check
    CHECK (kind IN ('human', 'service_account', 'system'));
ALTER TABLE users ADD CONSTRAINT users_system_has_no_password
    CHECK (kind <> 'system' OR password_hash IS NULL);

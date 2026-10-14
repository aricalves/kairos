-- KAIROS-T-0431: one read token of the organization for each forge host.
-- The builder of the code index uses it to fetch each repository on that
-- host. A repository token (repository_credentials) is used only for a host
-- with no token here.
--
--   org_read_tokens   One row for each host that has a token. The row holds
--                     the AES-256-GCM ciphertext of the token, never the
--                     token. The key is KAIROS_SECRETS_KEY; it is not in
--                     the database.
--
-- `host` is the host of the forge in lowercase, with the port only when it
-- is not the default port (for example github.com, git.example.org:8443).
-- The associated data of the encryption is the tenant slug and the host, so
-- a ciphertext that is copied to a different row does not decrypt. `key_id`
-- is a fingerprint of the key that encrypted the row. `set_by` is a user of
-- public.users; it has no foreign key, as in repository_credentials.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table, empty. No row of a table that the tenant has is changed or
-- deleted.
--
-- Re-runnable: the statement has a guard, and the second run changes
-- nothing.
CREATE TABLE IF NOT EXISTS org_read_tokens (
    host        TEXT PRIMARY KEY CHECK (host <> '' AND host = lower(host)),
    ciphertext  BYTEA NOT NULL CHECK (octet_length(ciphertext) > 16),
    nonce       BYTEA NOT NULL CHECK (octet_length(nonce) = 12),
    key_id      TEXT NOT NULL,
    set_by      UUID NOT NULL,
    set_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

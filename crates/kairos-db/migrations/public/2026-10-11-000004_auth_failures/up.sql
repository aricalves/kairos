-- KAIROS-T-0377 (COLLIERY-I-0613): the counts of the login throttle, shared
-- by every server.
--
-- PUBLIC, not one table in each tenant schema. The throttle runs before the
-- server knows the organization: /api/login names a person (users are
-- public), and an API key names its organization only after it is
-- verified. So the counts cannot live in an org_* schema.
--
-- One row for each subject: kind 'identity' (the email of a login) or
-- 'source' (the address of the client). subject is the SHA-256 (hex) of
-- the value, not the value: an email or an address in a table outlives the
-- attack it was counted for. A failed login is one upsert; a check is one
-- read of the primary key. expires_at is the time after which the row can
-- change no decision: the retention pass removes the rows past it.
-- Re-runnable.
CREATE TABLE IF NOT EXISTS auth_failures (
    kind            TEXT NOT NULL CHECK (kind IN ('identity', 'source')),
    subject         TEXT NOT NULL,
    -- The failures in the current window.
    failures        INTEGER NOT NULL DEFAULT 0,
    window_started  TIMESTAMPTZ NOT NULL,
    locked_until    TIMESTAMPTZ,
    expires_at      TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (kind, subject)
);

-- The removal of the rows that can change no decision.
CREATE INDEX IF NOT EXISTS auth_failures_expires ON auth_failures (expires_at);

-- KAIROS-T-0628 (COLLIERY-I-0649): the sweep of old sessions deletes by
-- expires_at (local_auth.rs), and without an index it scans the table.
--
-- Not CONCURRENTLY, for the same reason as the tenant migration of this
-- task: the build holds a SHARE lock on local_sessions, so a login waits
-- for it; the table holds one row per session.
--
-- No row is changed. Re-runnable.
CREATE INDEX IF NOT EXISTS idx_local_sessions_expires_at
    ON local_sessions (expires_at);

-- KAIROS-T-0531: a claim ends when its person leaves the organization, and
-- a claim with no write for some days is reported. Two new kinds of
-- notification: claim_dropped and claim_stale. The column
-- task_claims.stale_notified_at records the report, so that Kairos sends
-- it once for each quiet period.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- The CHECK of the kind of the 2 notification tables accepts two more
-- values. task_claims gets 1 column, NULL in each row. No row is changed.
--
-- Re-runnable: each constraint is dropped before it is added.
ALTER TABLE notifications DROP CONSTRAINT IF EXISTS notifications_kind_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing',
     'agent_key_revoked', 'blocked_again', 'blocker_cancelled', 'claim_dropped',
     'claim_stale'));
ALTER TABLE notification_settings DROP CONSTRAINT IF EXISTS notification_settings_kind_check;
ALTER TABLE notification_settings ADD CONSTRAINT notification_settings_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing',
     'agent_key_revoked', 'blocked_again', 'blocker_cancelled', 'claim_dropped',
     'claim_stale'));
ALTER TABLE task_claims ADD COLUMN IF NOT EXISTS stale_notified_at TIMESTAMPTZ;

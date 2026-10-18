-- KAIROS-T-0540: a key that expires within 7 days gets one notification
-- of the new kind key_expiring. The column api_keys.expiring_notified_at
-- records the notification, so that Kairos sends it once for each key.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- The CHECK of the kind of the 2 notification tables accepts one more
-- value. api_keys gets 1 column, NULL in each row. No row is changed.
--
-- Re-runnable: each constraint is dropped before it is added.
ALTER TABLE notifications DROP CONSTRAINT IF EXISTS notifications_kind_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing',
     'agent_key_revoked', 'blocked_again', 'blocker_cancelled', 'claim_dropped',
     'claim_stale', 'key_expiring'));
ALTER TABLE notification_settings DROP CONSTRAINT IF EXISTS notification_settings_kind_check;
ALTER TABLE notification_settings ADD CONSTRAINT notification_settings_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing',
     'agent_key_revoked', 'blocked_again', 'blocker_cancelled', 'claim_dropped',
     'claim_stale', 'key_expiring'));
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS expiring_notified_at TIMESTAMPTZ;

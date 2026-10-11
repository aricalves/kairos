-- KAIROS-T-0388: an organization admin can revoke the agent key of a
-- person. Kairos then tells the person: a notification of the new kind
-- agent_key_revoked, which names the key.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- The CHECK of the kind of the 2 notification tables accepts one more
-- value. No row is changed.
--
-- Re-runnable: each constraint is dropped before it is added.
ALTER TABLE notifications DROP CONSTRAINT IF EXISTS notifications_kind_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing',
     'agent_key_revoked'));
ALTER TABLE notification_settings DROP CONSTRAINT IF EXISTS notification_settings_kind_check;
ALTER TABLE notification_settings ADD CONSTRAINT notification_settings_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing',
     'agent_key_revoked'));

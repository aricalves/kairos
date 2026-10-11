-- KAIROS-T-0388, down. The notifications of the kind agent_key_revoked,
-- and the settings of that kind, are lost.
DELETE FROM notifications WHERE kind = 'agent_key_revoked';
DELETE FROM notification_settings WHERE kind = 'agent_key_revoked';
ALTER TABLE notifications DROP CONSTRAINT IF EXISTS notifications_kind_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing'));
ALTER TABLE notification_settings DROP CONSTRAINT IF EXISTS notification_settings_kind_check;
ALTER TABLE notification_settings ADD CONSTRAINT notification_settings_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing'));

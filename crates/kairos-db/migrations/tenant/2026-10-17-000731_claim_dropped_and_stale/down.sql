-- KAIROS-T-0531, down. The notifications of the kinds claim_dropped and
-- claim_stale, the settings of those kinds, and the record of each stale
-- report are lost.
DELETE FROM notifications WHERE kind IN ('claim_dropped', 'claim_stale');
DELETE FROM notification_settings WHERE kind IN ('claim_dropped', 'claim_stale');
ALTER TABLE notifications DROP CONSTRAINT IF EXISTS notifications_kind_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing',
     'agent_key_revoked', 'blocked_again', 'blocker_cancelled'));
ALTER TABLE notification_settings DROP CONSTRAINT IF EXISTS notification_settings_kind_check;
ALTER TABLE notification_settings ADD CONSTRAINT notification_settings_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing',
     'agent_key_revoked', 'blocked_again', 'blocker_cancelled'));
ALTER TABLE task_claims DROP COLUMN IF EXISTS stale_notified_at;

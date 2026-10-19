-- KAIROS-T-0558, down. The notifications of the kind proposal_decided and
-- the settings of that kind are lost.
DELETE FROM notifications WHERE kind = 'proposal_decided';
DELETE FROM notification_settings WHERE kind = 'proposal_decided';
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

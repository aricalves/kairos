-- KAIROS-T-0384, down. The webhook targets, their secrets and the delivery
-- log are lost, and so are the notifications of the kind delivery_failing.
DROP TABLE IF EXISTS webhook_delivery_tries;
DROP TABLE IF EXISTS webhook_deliveries;
DROP TABLE IF EXISTS webhook_targets;
DELETE FROM notifications WHERE kind = 'delivery_failing';
DELETE FROM notification_settings WHERE kind = 'delivery_failing';
ALTER TABLE notifications DROP CONSTRAINT IF EXISTS notifications_kind_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug'));
ALTER TABLE notification_settings DROP CONSTRAINT IF EXISTS notification_settings_kind_check;
ALTER TABLE notification_settings ADD CONSTRAINT notification_settings_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug'));

-- KAIROS-T-0638: drop the 2 indexes. The back-fill of the relationship rows
-- of activity_log stays: the old value was NULL, and the new value is
-- right.
DROP INDEX IF EXISTS webhook_deliveries_finished_at_idx;
DROP INDEX IF EXISTS notifications_unread_age_idx;

-- KAIROS-T-0550, down. The read state of each agent key is lost. The read
-- state of the person (`notifications.read_at`) stays.
DROP TABLE IF EXISTS notification_reads;

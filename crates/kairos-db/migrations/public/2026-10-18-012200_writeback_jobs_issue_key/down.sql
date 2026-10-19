-- KAIROS-T-0637: drop the index. No row is changed.
DROP INDEX CONCURRENTLY IF EXISTS jobs_issue_writeback_queued_issue_key;

-- KAIROS-T-0637 (COLLIERY-I-0650): one queued issue_writeback job for each
-- issue (KAIROS-T-0575 has the same race as the pull request). queue_issue
-- (kairos-db, issue_writeback.rs) adds the job with ON CONFLICT on this
-- index DO NOTHING. A leased job is not in the index.
--
-- CONCURRENTLY, outside a transaction (KAIROS-A-0062): jobs is a large
-- table. After a failure Postgres keeps an INVALID index: drop it with
-- DROP INDEX CONCURRENTLY and run the migrations again.
CREATE UNIQUE INDEX CONCURRENTLY jobs_issue_writeback_queued_issue_key
    ON jobs ((payload->>'issue_key'))
    WHERE kind = 'issue_writeback' AND state = 'queued';

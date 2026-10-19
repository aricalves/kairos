-- KAIROS-T-0637 (COLLIERY-I-0650): one queued github_writeback job for each
-- pull request. queue_pr (kairos-db, writeback.rs) adds the job with
-- ON CONFLICT on this index DO NOTHING, so 2 transitions at the same time
-- add one job. A leased job is not in the index: a change while a job runs
-- adds a job, because the running job may have read the items already.
--
-- CONCURRENTLY, outside a transaction (KAIROS-A-0062): jobs is a large
-- table. Writes go on during the build. After a failure Postgres keeps an
-- INVALID index: drop it with DROP INDEX CONCURRENTLY and run the
-- migrations again.
CREATE UNIQUE INDEX CONCURRENTLY jobs_github_writeback_queued_pr_key
    ON jobs ((payload->>'pr_key'))
    WHERE kind = 'github_writeback' AND state = 'queued';

-- KAIROS-T-0637 (COLLIERY-I-0650): one queued write-back job for each pull
-- request and for each issue.
--
-- The next two migrations add a unique index on the key of a queued job of
-- the kinds github_writeback (payload->>'pr_key') and issue_writeback
-- (payload->>'issue_key'). Until now two transactions could each add a job
-- for one key. This migration ends those twins, so that the index can be
-- built: of the queued jobs of one key, the oldest stays queued, and the
-- others end 'done'. The job that stays reads the items when it runs, so it
-- writes the change of each twin too.
--
-- WHAT THIS DOES TO THE DATA
--
-- Each queued twin ends 'done' with a reason in last_error. No row is
-- deleted. A leased job is not changed.
--
-- Re-runnable: a second run finds no twin.
UPDATE jobs j
   SET state = 'done', finished_at = now(), lease_until = NULL,
       last_error = 'An older queued job of the same pull request does this write (KAIROS-T-0637).'
 WHERE j.kind = 'github_writeback' AND j.state = 'queued'
   AND EXISTS (SELECT 1 FROM jobs o
                WHERE o.kind = j.kind AND o.state = 'queued'
                  AND o.payload->>'pr_key' = j.payload->>'pr_key'
                  AND (o.created_at, o.id) < (j.created_at, j.id));

UPDATE jobs j
   SET state = 'done', finished_at = now(), lease_until = NULL,
       last_error = 'An older queued job of the same issue does this write (KAIROS-T-0637).'
 WHERE j.kind = 'issue_writeback' AND j.state = 'queued'
   AND EXISTS (SELECT 1 FROM jobs o
                WHERE o.kind = j.kind AND o.state = 'queued'
                  AND o.payload->>'issue_key' = j.payload->>'issue_key'
                  AND (o.created_at, o.id) < (j.created_at, j.id));

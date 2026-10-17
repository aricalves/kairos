-- KAIROS-T-0529: the claim survives Active and Blocked.
--
--   board_columns.keeps_claims   A task with a claim keeps it in this
--                                column, and a task with no claim gets
--                                none here. The flow metric does not
--                                count the column as a start, and a
--                                move into it gives no claim: that is
--                                what sets it apart from `claims`.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new column, false for each row. Each live column named 'Blocked' on
-- a live delivery board gets keeps_claims = true. No task gets or loses
-- a claim: a task in Blocked now has no claim until a person moves it
-- into Active or claims it there and blocks it again.
--
-- Re-runnable: each statement has a guard.
ALTER TABLE board_columns
    ADD COLUMN IF NOT EXISTS keeps_claims BOOLEAN NOT NULL DEFAULT false;

UPDATE board_columns AS c
   SET keeps_claims = true
  FROM boards AS b
 WHERE b.id = c.board_id
   AND b.board_level = 'delivery'
   AND b.deleted_at IS NULL
   AND c.deleted_at IS NULL
   AND c.name = 'Blocked'
   AND NOT c.keeps_claims;

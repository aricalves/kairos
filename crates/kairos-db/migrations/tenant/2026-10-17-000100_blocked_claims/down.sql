-- KAIROS-T-0529, down. The claims of the tasks in a column that kept
-- them end, and no column keeps a claim.
DELETE FROM task_claims AS t
 USING tasks AS k, board_columns AS c
 WHERE t.task_id = k.id
   AND k.column_id = c.id
   AND c.keeps_claims
   AND NOT c.claims;

ALTER TABLE board_columns DROP COLUMN IF EXISTS keeps_claims;

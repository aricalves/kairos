-- KAIROS-T-0371: the WIP limit of a column.
--
--   board_columns.wip_limit   The most live items that the column should
--                             hold. NULL: no limit. A move into a column
--                             at or over its limit is allowed; the answer
--                             has a warning. The board shows the count
--                             against the limit.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new column, NULL in each row: no column has a limit after the
-- migration. No row is changed.
--
-- Re-runnable: the statement has a guard.
ALTER TABLE board_columns
    ADD COLUMN IF NOT EXISTS wip_limit INT NULL
        CONSTRAINT board_columns_wip_limit_positive CHECK (wip_limit > 0);

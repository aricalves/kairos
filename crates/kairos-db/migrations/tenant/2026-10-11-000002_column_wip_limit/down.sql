-- KAIROS-T-0371, down. The WIP limits of the columns are lost.
ALTER TABLE board_columns DROP COLUMN IF EXISTS wip_limit;

-- KAIROS-T-0427: a dispatch rule fires when a task enters a column (the
-- trigger `entered_column`, as before) or when the last open blocker of a
-- task is done, archived or cancelled (the trigger `unblocked`).
--
-- An `unblocked` rule can name a column: the task must be in it. With no
-- column, a task in each column that is not done matches. An
-- `entered_column` rule still needs its column.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new column, `trigger`, with `entered_column` on each rule. No other row is changed.
--
-- Re-runnable: each statement has a guard.
ALTER TABLE dispatch_rules
    ADD COLUMN IF NOT EXISTS trigger TEXT NOT NULL DEFAULT 'entered_column';

ALTER TABLE dispatch_rules ALTER COLUMN column_id DROP NOT NULL;

ALTER TABLE dispatch_rules DROP CONSTRAINT IF EXISTS dispatch_rules_trigger_check;
ALTER TABLE dispatch_rules ADD CONSTRAINT dispatch_rules_trigger_check
    CHECK (trigger IN ('entered_column', 'unblocked')
           AND (trigger = 'unblocked' OR column_id IS NOT NULL));

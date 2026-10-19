-- KAIROS-T-0575: a task born from an issue writes back to the issue. When
-- the task is done, Kairos comments on the issue and closes it. When the
-- task is cancelled, Kairos writes the reason and closes the issue as not
-- planned. When the task is reopened, Kairos reopens the issue.
--
-- `forge_issue_intake` keeps the state of the write-back of its issue:
--
-- - `written_state`: the state that Kairos last wrote on the issue
--   (`open`, `completed`, `not_planned`). An intake issue is open.
-- - `comment_state`: the state whose comment is on the issue. A retry of a
--   failed close does not comment again.
-- - `busy_until`: the lease of the issue while one job writes it.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- forge_issue_intake gets 3 columns. Each row gets `open` and 2 NULLs.
-- No row is changed otherwise, and nothing is written to a forge.
ALTER TABLE forge_issue_intake
    ADD COLUMN IF NOT EXISTS written_state TEXT NOT NULL DEFAULT 'open';
ALTER TABLE forge_issue_intake
    ADD COLUMN IF NOT EXISTS comment_state TEXT;
ALTER TABLE forge_issue_intake
    ADD COLUMN IF NOT EXISTS busy_until TIMESTAMPTZ;
ALTER TABLE forge_issue_intake DROP CONSTRAINT IF EXISTS forge_issue_intake_written_state_check;
ALTER TABLE forge_issue_intake ADD CONSTRAINT forge_issue_intake_written_state_check
    CHECK (written_state IN ('open', 'completed', 'not_planned'));
ALTER TABLE forge_issue_intake DROP CONSTRAINT IF EXISTS forge_issue_intake_comment_state_check;
ALTER TABLE forge_issue_intake ADD CONSTRAINT forge_issue_intake_comment_state_check
    CHECK (comment_state IN ('completed', 'not_planned'));

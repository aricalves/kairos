-- KAIROS-T-0575, down. The state of the write-back of each issue is lost.
-- The comments and the closed issues stay on the forge.
ALTER TABLE forge_issue_intake DROP CONSTRAINT IF EXISTS forge_issue_intake_comment_state_check;
ALTER TABLE forge_issue_intake DROP CONSTRAINT IF EXISTS forge_issue_intake_written_state_check;
ALTER TABLE forge_issue_intake DROP COLUMN IF EXISTS busy_until;
ALTER TABLE forge_issue_intake DROP COLUMN IF EXISTS comment_state;
ALTER TABLE forge_issue_intake DROP COLUMN IF EXISTS written_state;

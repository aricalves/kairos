-- KAIROS-T-0386, down. The write tokens and the state of the write-back are
-- lost: the comments that Kairos wrote stay on GitHub, and a new write-back
-- makes new ones.
DROP TABLE IF EXISTS github_pr_comments;
DROP TABLE IF EXISTS github_pr_writebacks;
DROP TABLE IF EXISTS repository_write_tokens;

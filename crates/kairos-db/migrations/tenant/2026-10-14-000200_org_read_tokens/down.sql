-- Reverse of KAIROS-T-0431: removes the read tokens of the organization.
-- The builder then fetches each repository with its own token, or with no
-- credential.
DROP TABLE IF EXISTS org_read_tokens;

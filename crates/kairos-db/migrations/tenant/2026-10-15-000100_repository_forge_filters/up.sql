-- KAIROS-T-0479 (COLLIERY-I-0623): the forge filters of a repository. An org
-- admin sets one include and one exclude regular expression per field. A
-- field with no row has the default: include `.*`, exclude empty (nothing).
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table. No row is changed.
CREATE TABLE IF NOT EXISTS repository_forge_filters (
    repository_id   UUID NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    field           TEXT NOT NULL CHECK (field IN ('branch_name', 'pr_head_branch',
                        'pr_title', 'pr_author', 'issue_title', 'issue_author')),
    include_pattern TEXT NOT NULL DEFAULT '.*',
    exclude_pattern TEXT NOT NULL DEFAULT '',
    updated_by      UUID NOT NULL,
    updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repository_id, field)
);

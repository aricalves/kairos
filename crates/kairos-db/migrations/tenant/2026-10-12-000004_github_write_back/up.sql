-- KAIROS-T-0386 (COLLIERY-I-0614): the GitHub write-back of a repository.
--
--   repository_write_tokens   The write token of a repository: sealed with
--                             KAIROS_SECRETS_KEY like the read token
--                             (repository_credentials), with its own
--                             associated data. No row: no write-back. The
--                             result of the last write is kept, with no
--                             token in it.
--   github_pr_writebacks      One row for each pull request that Kairos
--                             writes to: the head commit, the state and
--                             the description of the last `kairos` status,
--                             and a short lease (busy_until), so 2 servers
--                             do not write one pull request at a time.
--   github_pr_comments        The comment of one item on one pull request:
--                             its id on GitHub, so the next change edits it
--                             in place, and the SHA-256 of its body, so an
--                             unchanged body is not sent again.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 3 new tables. No row is changed.
--
-- Re-runnable: each statement has a guard.
CREATE TABLE IF NOT EXISTS repository_write_tokens (
    repository_id    UUID PRIMARY KEY REFERENCES repositories (id) ON DELETE CASCADE,
    ciphertext       BYTEA NOT NULL,
    nonce            BYTEA NOT NULL,
    key_id           TEXT NOT NULL,
    set_by           UUID NOT NULL,
    set_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_write_at    TIMESTAMPTZ NULL,
    last_write_ok    BOOLEAN NULL,
    last_write_error TEXT NULL
);

CREATE TABLE IF NOT EXISTS github_pr_writebacks (
    repository_id      UUID NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    pr_number          BIGINT NOT NULL,
    head_sha           TEXT NULL,
    status_state       TEXT NULL,
    status_description TEXT NULL,
    busy_until         TIMESTAMPTZ NULL,
    updated_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repository_id, pr_number)
);

CREATE TABLE IF NOT EXISTS github_pr_comments (
    repository_id UUID NOT NULL REFERENCES repositories (id) ON DELETE CASCADE,
    pr_number     BIGINT NOT NULL,
    item_id       UUID NOT NULL,
    comment_id    BIGINT NOT NULL,
    body_sha256   TEXT NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (repository_id, pr_number, item_id)
);

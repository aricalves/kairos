-- KAIROS-T-0506 (COLLIERY-I-0629): a task has one structured live state,
-- separate from the append-only notes.
--
--   task_state   The state of a task: the branch, the last commit, pushed
--                or not, the PR, the next step, and who wrote it when. One
--                row for each task at most, replaced on each write. No
--                row: the task has no state.
--
-- The row stays in each column, Completed too: it is useful after the
-- work. No history: the notes of the task hold the record. No foreign key
-- on updated_by: users are in the public schema (the rule of
-- 2026-10-09-000003_task_claims). The state goes with the task when the
-- retention sweep deletes the row of the task (ON DELETE CASCADE).
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table, empty: each task has no state. No row of a table that the
-- tenant has is changed or deleted.
--
-- Re-runnable: the table is added once.
CREATE TABLE IF NOT EXISTS task_state (
    task_id    UUID PRIMARY KEY REFERENCES tasks(id) ON DELETE CASCADE,
    branch     TEXT NULL,
    commit     TEXT NULL,
    pushed     BOOLEAN NOT NULL DEFAULT false,
    pr_url     TEXT NULL,
    next_step  TEXT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by UUID NOT NULL
);

-- KAIROS-T-0337 (amends KAIROS-A-0006 and KAIROS-A-0017): the grants of
-- the structure capability `manage_structure`.
--
--   structure_grants   One row for each grant. `board_id` NULL is a grant
--                      at the organization: teams, team members, boards,
--                      the configuration, the members and the grants of
--                      each board, and the repositories. A `board_id` is a
--                      grant on that board only: its columns, its
--                      transitions, its members and its grants.
--
-- The grants are not rows of board_member_capabilities. That table holds
-- the board capabilities, and its globs (`*`, `manage_*`) must not reach
-- this capability. The check reads this table by name.
--
-- No foreign key on user_id and granted_by: users are in the public
-- schema (the rule of api_keys and task_claims). A grant on a board goes
-- with the board when the retention sweep deletes the row of the board
-- (ON DELETE CASCADE).
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table and 2 indexes. No row is written and no row is changed. No
-- principal has the capability after the migration.
--
-- Re-runnable: each statement has a guard.
CREATE TABLE IF NOT EXISTS structure_grants (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    UUID NOT NULL,
    board_id   UUID NULL REFERENCES boards(id) ON DELETE CASCADE,
    granted_by UUID NOT NULL,
    granted_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- One grant at the organization for each principal, and one grant on each
-- board for each principal.
CREATE UNIQUE INDEX IF NOT EXISTS structure_grants_organization_key
    ON structure_grants (user_id) WHERE board_id IS NULL;
CREATE UNIQUE INDEX IF NOT EXISTS structure_grants_board_key
    ON structure_grants (user_id, board_id) WHERE board_id IS NOT NULL;

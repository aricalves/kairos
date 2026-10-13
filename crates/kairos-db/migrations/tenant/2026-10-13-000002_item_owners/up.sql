-- KAIROS-T-0411 (KAIROS-A-0025, decisions 2 to 7): a strategy or an
-- initiative has one owner at most, a person, by a claim.
--
--   item_owners   The owner of a strategy or an initiative: the person
--                 (`user_id`) and the time of the claim. One row for each
--                 item at most. No row: the item has no owner.
--
-- The owner is not tied to a column: the row stays in each column,
-- Completed too, and when the item is archived. The row goes when the
-- owner leaves the organization (the server removes it in the transaction
-- that removes the membership).
--
-- No foreign key on item_id: the item is a row of `strategies` or of
-- `initiatives`, and a strategy or an initiative is never deleted (an
-- archive sets deleted_at). No foreign key on user_id: users are in the
-- public schema (the rule of 2026-10-09-000003_task_claims).
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table, empty: each strategy and each initiative has no owner. No
-- row of a table that the tenant has is changed or deleted.
--
-- Re-runnable: the table and the index are added once.
CREATE TABLE IF NOT EXISTS item_owners (
    item_id    UUID PRIMARY KEY,
    user_id    UUID NOT NULL,
    claimed_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS item_owners_user_id_idx ON item_owners (user_id);

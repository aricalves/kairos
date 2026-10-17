-- KAIROS-T-0628 (COLLIERY-I-0649): indexes for the reads that sort, and
-- the indexes that only cost writes.
--
-- New:
--   idx_activity_log_entity_time   activity_log (entity_id, occurred_at):
--                                  flow, rename history and the feed read
--                                  one entity in time order, and sorted
--                                  before. It also serves entity_id alone,
--                                  so idx_activity_log_entity goes.
--   notifications_user_read_idx    notifications (user_id, read_at): the
--                                  read list of a person, in read_at order.
--
-- Dropped, each covered by a unique index that starts with the same
-- columns:
--   idx_item_history_item            UNIQUE (item_id, version)
--   idx_item_metadata_item           UNIQUE (item_id, metadata_definition_id)
--   idx_board_member_cap_board_user  PRIMARY KEY (board_id, user_id, capability)
--   idx_activity_log_entity          idx_activity_log_entity_time
--
-- Not CONCURRENTLY: a new tenant runs every tenant migration inside one
-- transaction (tenant.rs, create_tenant), and CREATE INDEX CONCURRENTLY
-- cannot run in a transaction or a multi-statement string. A plain CREATE
-- INDEX holds a SHARE lock on the table while it builds: writes to
-- activity_log and notifications wait for the build, reads go on.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- No row is changed. 2 indexes added, 4 dropped.
--
-- Re-runnable: each statement has a guard.
CREATE INDEX IF NOT EXISTS idx_activity_log_entity_time
    ON activity_log (entity_id, occurred_at);
CREATE INDEX IF NOT EXISTS notifications_user_read_idx
    ON notifications (user_id, read_at);

DROP INDEX IF EXISTS idx_activity_log_entity;
DROP INDEX IF EXISTS idx_item_history_item;
DROP INDEX IF EXISTS idx_item_metadata_item;
DROP INDEX IF EXISTS idx_board_member_cap_board_user;

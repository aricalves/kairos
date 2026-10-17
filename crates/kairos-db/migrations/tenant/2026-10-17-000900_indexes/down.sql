-- Reverse of KAIROS-T-0628: the four dropped indexes come back, the two
-- new ones go. No row is changed.
CREATE INDEX IF NOT EXISTS idx_board_member_cap_board_user
    ON board_member_capabilities (board_id, user_id);
CREATE INDEX IF NOT EXISTS idx_item_metadata_item ON item_metadata (item_id);
CREATE INDEX IF NOT EXISTS idx_item_history_item ON item_history (item_id);
CREATE INDEX IF NOT EXISTS idx_activity_log_entity ON activity_log (entity_id);

DROP INDEX IF EXISTS notifications_user_read_idx;
DROP INDEX IF EXISTS idx_activity_log_entity_time;

-- KAIROS-T-0651: back to full rows only. A diff row has no text in
-- `content`, so the down migration stops while a diff row exists.
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM item_history WHERE base_version IS NOT NULL) THEN
        RAISE EXCEPTION 'item_history has diff rows (KAIROS-T-0651). Restore their text before this down migration.';
    END IF;
END
$$;

ALTER TABLE item_history DROP CONSTRAINT IF EXISTS item_history_diff_shape;
ALTER TABLE item_history DROP COLUMN IF EXISTS content_diff;
ALTER TABLE item_history DROP COLUMN IF EXISTS base_version;

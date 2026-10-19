-- KAIROS-T-0651 (COLLIERY-I-0650): item_history keeps a diff, with a full
-- row at most every 10 versions.
--
--   base_version   NULL: a full row. `content` holds the text.
--                  Set: a diff row. The diff is from the full row of this
--                  version of the same item.
--   content_diff   The diff of a diff row (kairos_core::history_diff), or
--                  NULL. `content` of a diff row is empty.
--
-- Each existing row stays a full row: this migration rewrites no row. The
-- two ADD COLUMN steps change only the catalog. The CHECK is NOT VALID, so
-- PostgreSQL does not scan the table. New rows must obey it.
ALTER TABLE item_history ADD COLUMN IF NOT EXISTS base_version INTEGER;
ALTER TABLE item_history ADD COLUMN IF NOT EXISTS content_diff TEXT;

ALTER TABLE item_history DROP CONSTRAINT IF EXISTS item_history_diff_shape;
ALTER TABLE item_history ADD CONSTRAINT item_history_diff_shape CHECK (
    (base_version IS NULL AND content_diff IS NULL)
    OR (base_version IS NOT NULL AND content_diff IS NOT NULL AND base_version < version)
) NOT VALID;

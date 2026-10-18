-- KAIROS-T-0572: the edges that the up migration removed do not come back.
-- Their activity_log rows name them.
DROP INDEX IF EXISTS item_relationships_one_parent;

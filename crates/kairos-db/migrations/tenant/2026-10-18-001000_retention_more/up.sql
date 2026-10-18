-- KAIROS-T-0638 (COLLIERY-I-0650): retention for more tables.
--
-- New:
--   notifications_unread_age_idx       notifications (created_at) WHERE
--                                      read_at IS NULL: the pass deletes
--                                      the unread notifications by age, in
--                                      each tenant, a page at a time.
--   webhook_deliveries_finished_at_idx webhook_deliveries (finished_at)
--                                      WHERE finished_at IS NOT NULL: the
--                                      pass deletes the old deliveries of
--                                      each target, also of a quiet one.
--
-- Back-fill: a relationship row of activity_log (relationship_add,
-- relationship_remove) from graph.rs had entity_id NULL. The new rows name
-- the source of the edge there. This sets the source in the old rows when
-- the label of the source in details (a short code, or the UUID of an
-- archived item) names an item of entity_directory. A row whose label
-- names no item keeps NULL: the activity window removes it in time.
--
-- Not CONCURRENTLY: a new tenant runs every tenant migration inside one
-- transaction (tenant.rs, create_tenant). A plain CREATE INDEX holds a
-- SHARE lock while it builds: writes to notifications and
-- webhook_deliveries wait for the build, reads go on. The UPDATE locks only
-- the rows that it changes.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 2 indexes added. The relationship rows of activity_log with no entity get
-- the source of the edge as entity_id and entity_type. No row is deleted.
--
-- Re-runnable: each statement has a guard.
CREATE INDEX IF NOT EXISTS notifications_unread_age_idx
    ON notifications (created_at) WHERE read_at IS NULL;
CREATE INDEX IF NOT EXISTS webhook_deliveries_finished_at_idx
    ON webhook_deliveries (finished_at) WHERE finished_at IS NOT NULL;

UPDATE activity_log a
   SET entity_id = d.id, entity_type = d.entity_type
  FROM entity_directory d
 WHERE a.entity_id IS NULL
   AND a.action IN ('relationship_add', 'relationship_remove')
   AND a.details LIKE 'relationship:%'
   AND d.short_code = split_part(split_part(a.details, ':', 3), '->', 1);

UPDATE activity_log a
   SET entity_id = d.id, entity_type = d.entity_type
  FROM entity_directory d
 WHERE a.entity_id IS NULL
   AND a.action IN ('relationship_add', 'relationship_remove')
   AND a.details LIKE 'relationship:%'
   AND d.id::text = split_part(split_part(a.details, ':', 3), '->', 1);

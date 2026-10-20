-- KAIROS-T-0670, down. The sync state is lost. A link with no connection
-- (an issue that the cadence took) is deleted, because it cannot have a
-- NOT NULL connection. The tasks stay.
ALTER TABLE item_links DROP CONSTRAINT IF EXISTS item_links_owner_check;
DELETE FROM item_links WHERE connection_id IS NULL;
ALTER TABLE item_links DROP COLUMN IF EXISTS repository_id;
ALTER TABLE item_links ALTER COLUMN connection_id SET NOT NULL;
DROP TABLE IF EXISTS forge_issue_sync;
ALTER TABLE repositories DROP CONSTRAINT IF EXISTS repositories_issue_import_interval_check;
ALTER TABLE repositories DROP COLUMN IF EXISTS issue_import_interval_secs;

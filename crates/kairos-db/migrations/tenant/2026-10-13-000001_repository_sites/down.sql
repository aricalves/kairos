-- Reverse of KAIROS-T-0406: removes the site switch, the publishes and the
-- manifests. The stored files of the sites stay in the site store.
ALTER TABLE repositories DROP COLUMN IF EXISTS site_publish_id;
DROP TABLE IF EXISTS site_files;
DROP TABLE IF EXISTS site_publishes;
ALTER TABLE repositories DROP COLUMN IF EXISTS site_enabled;

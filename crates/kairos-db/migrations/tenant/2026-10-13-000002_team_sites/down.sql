-- Reverse of KAIROS-T-0404: removes the site switch of the teams.
ALTER TABLE teams DROP COLUMN IF EXISTS site_enabled;

-- KAIROS-T-0436: a team grows its own tree. Remove the folder set that
-- `2026-08-29-000000_team_pages` seeded for each team (Support Processes with
-- its Overview page, Documentation with six section folders), where the team
-- never used it.
--
-- A seeded node goes only when it is still as seeded: version 1, and nothing
-- under it (live or deleted). A node that a person edited, renamed, moved or
-- filled stays, and so does its parent folder. The charter is not touched.
-- History rows of a removed page go with it (ON DELETE CASCADE).

-- 1. The six section folders under Documentation.
DELETE FROM team_pages section
USING team_pages docs
WHERE section.parent_id = docs.id
  AND docs.parent_id IS NULL
  AND docs.slug = 'documentation'
  AND docs.kind = 'folder'
  AND section.kind = 'folder'
  AND section.slug IN ('planning', 'tutorials', 'how-to-guides', 'reference',
                       'explanation', 'design-docs')
  AND section.version = 1
  AND NOT EXISTS (SELECT 1 FROM team_pages c WHERE c.parent_id = section.id);

-- 2. The Overview page under Support Processes.
DELETE FROM team_pages overview
USING team_pages support
WHERE overview.parent_id = support.id
  AND support.parent_id IS NULL
  AND support.slug = 'support-processes'
  AND support.kind = 'folder'
  AND overview.kind = 'page'
  AND overview.slug = 'overview'
  AND overview.version = 1;

-- 3. The two root folders, when nothing is left under them.
DELETE FROM team_pages root
WHERE root.parent_id IS NULL
  AND root.kind = 'folder'
  AND root.slug IN ('documentation', 'support-processes')
  AND root.version = 1
  AND NOT EXISTS (SELECT 1 FROM team_pages c WHERE c.parent_id = root.id);

-- KAIROS-T-0459: every live team has its charter.
--
-- A team made through SCIM (POST /scim/v2/Groups) got its delivery board
-- and no charter: the 2026-08-29 migration gave a charter to the teams of
-- that day, and the SCIM path never called seed_team_charter. Nothing can
-- make a protected root page later. This gives the charter to each live
-- team that has none, as the 2026-08-29 migration did: the protected page
-- `charter` at root position 0, with its v1 history row. The actor is the
-- nil UUID, as there.
DO $$
DECLARE
    team RECORD;
    charter_id UUID;
    actor CONSTANT UUID := '00000000-0000-0000-0000-000000000000';
BEGIN
    FOR team IN SELECT id FROM teams WHERE deleted_at IS NULL LOOP
        IF NOT EXISTS (
            SELECT 1 FROM team_pages p
             WHERE p.team_id = team.id AND p.parent_id IS NULL AND p.slug = 'charter'
               AND p.deleted_at IS NULL
        ) THEN
            INSERT INTO team_pages (team_id, parent_id, kind, slug, title, content, position, is_protected, created_by, updated_by)
            VALUES (team.id, NULL, 'page', 'charter', 'Team Charter',
                    E'# Team Charter\n\n## Mission\n\n## Scope\n\n## Ways of Working\n\n## Success Measures\n',
                    0, true, actor, actor)
            RETURNING id INTO charter_id;
            INSERT INTO team_page_history (page_id, version, title, content, edited_by)
            SELECT charter_id, 1, p.title, p.content, actor FROM team_pages p WHERE p.id = charter_id;
        END IF;
    END LOOP;
END $$;

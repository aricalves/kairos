-- KAIROS-T-0440 (KAIROS-A-0026): a strategy or initiative board has a team,
-- as a delivery board does. This gives each live strategy and initiative
-- board with no team its own empty team: the name of the board, the slug
-- `<level>-<board slug>` (with `-2`, `-3`, ... when a live team has it), the
-- type stream_aligned, and its charter. It adds no member: an admin adds
-- people when ready. Board members and board grants do not change.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- For each such board: 1 team, 1 charter page with its v1 history row, and
-- boards.team_id set. Nothing is deleted.
DO $$
DECLARE
    board RECORD;
    base TEXT;
    candidate TEXT;
    n INTEGER;
    new_team UUID;
    charter_id UUID;
    actor CONSTANT UUID := '00000000-0000-0000-0000-000000000000';
BEGIN
    FOR board IN
        SELECT id, name, slug, board_level::text AS level
          FROM boards
         WHERE deleted_at IS NULL
           AND team_id IS NULL
           AND board_level::text IN ('strategy', 'initiative')
         ORDER BY created_at, id
    LOOP
        base := rtrim(left(board.level || '-' || board.slug, 60), '-');
        candidate := base;
        n := 2;
        WHILE EXISTS (SELECT 1 FROM teams t WHERE t.slug = candidate AND t.deleted_at IS NULL) LOOP
            candidate := base || '-' || n;
            n := n + 1;
        END LOOP;

        INSERT INTO teams (name, slug, team_type)
        VALUES (board.name, candidate, 'stream_aligned')
        RETURNING id INTO new_team;

        INSERT INTO team_pages (team_id, parent_id, kind, slug, title, content, position, is_protected, created_by, updated_by)
        VALUES (new_team, NULL, 'page', 'charter', 'Team Charter',
                E'# Team Charter\n\n## Mission\n\n## Scope\n\n## Ways of Working\n\n## Success Measures\n',
                0, true, actor, actor)
        RETURNING id INTO charter_id;
        INSERT INTO team_page_history (page_id, version, title, content, edited_by)
        SELECT charter_id, 1, p.title, p.content, actor FROM team_pages p WHERE p.id = charter_id;

        UPDATE boards SET team_id = new_team WHERE id = board.id;
    END LOOP;
END $$;

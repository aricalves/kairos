-- KAIROS-T-0630: back to notes in the content. Each note goes back to the
-- end of the content of its item as a `## Note n-3 (date, name)` section.
-- The vector of each item is the vector of 2026-10-17-001500 again.

-- 1. The trigger function of 2026-10-17-001500: the title and the content.
CREATE OR REPLACE FUNCTION item_search_write() RETURNS trigger AS $$
BEGIN
    IF TG_OP = 'DELETE' THEN
        EXECUTE format(
            'DELETE FROM %I.item_search WHERE item_id = $1 AND entity_type = $2',
            TG_TABLE_SCHEMA
        ) USING OLD.id, TG_ARGV[0];
        RETURN OLD;
    END IF;
    EXECUTE format(
        'INSERT INTO %I.item_search (item_id, entity_type, tsv) '
        || 'VALUES ($1, $2, setweight(to_tsvector(''english'', coalesce($3, '''')), ''A'') '
        || '|| setweight(to_tsvector(''english'', coalesce($4, '''')), ''B'')) '
        || 'ON CONFLICT (item_id) DO UPDATE '
        || 'SET entity_type = EXCLUDED.entity_type, tsv = EXCLUDED.tsv',
        TG_TABLE_SCHEMA
    ) USING NEW.id, TG_ARGV[0], NEW.title, NEW.content;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- 2. The notes back into the content. The update writes each vector again.
DROP TRIGGER IF EXISTS item_notes_search ON item_notes;
UPDATE strategies SET content = rtrim(content, E' \t\r\n') || item_notes_markdown(id)
    WHERE id IN (SELECT item_id FROM item_notes);
UPDATE initiatives SET content = rtrim(content, E' \t\r\n') || item_notes_markdown(id)
    WHERE id IN (SELECT item_id FROM item_notes);
UPDATE tasks SET content = rtrim(content, E' \t\r\n') || item_notes_markdown(id)
    WHERE id IN (SELECT item_id FROM item_notes);
UPDATE documents SET content = rtrim(content, E' \t\r\n') || item_notes_markdown(id)
    WHERE id IN (SELECT item_id FROM item_notes);
UPDATE adrs SET content = rtrim(content, E' \t\r\n') || item_notes_markdown(id)
    WHERE id IN (SELECT item_id FROM item_notes);

-- 3. The table and the functions.
DROP FUNCTION IF EXISTS item_notes_search_write();
DROP FUNCTION IF EXISTS item_notes_markdown(UUID);
DROP FUNCTION IF EXISTS item_search_vector(TEXT, UUID, TEXT, TEXT);
DROP TABLE IF EXISTS item_notes;

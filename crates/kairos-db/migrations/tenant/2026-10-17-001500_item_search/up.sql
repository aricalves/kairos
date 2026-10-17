-- KAIROS-T-0624 (COLLIERY-I-0649): a stored tsvector for each item.
--
--   item_search   One row for each item of the 5 item tables: its id, its
--                 type, and the weighted tsvector of its title and content.
--
-- Before this, the 5 idx_*_tsv GIN indexes were expression indexes. An
-- expression index stores no vector. So ts_rank_cd ran to_tsvector again
-- over the full content of each row it ranked, and a broad term (one that
-- matches most items) made the planner tokenize every row twice. Also, a
-- change to the content re-tokenized the item and inserted into every index
-- of its table, and a transition re-indexed the tsvector too.
--
-- The vector is the expression of migration 2026-09-23-000004, character
-- for character: the title at weight A, the content at weight B. So each
-- match and each rank is the same as before.
--
-- Triggers write the row, in the transaction of the write:
--   AFTER INSERT                        the new item.
--   AFTER UPDATE OF title, content      only when the title or the content
--                                       changes. A transition, an archive
--                                       or a restore does not write it.
--   AFTER DELETE                        a purge removes the row.
-- The function writes to item_search of the schema of the table
-- (TG_TABLE_SCHEMA), not of the search_path, as the retired-code trigger
-- does (2026-10-03-000002). The delete names the type, so that an item that
-- moves to another table with the same id keeps the row of its new table.
--
-- Not CONCURRENTLY: a new tenant runs every tenant migration inside one
-- transaction (tenant.rs, provision_tenant), and CREATE INDEX CONCURRENTLY
-- cannot run in a transaction. The order keeps the locks short:
--   1. the table and the triggers: from here, writes to the 5 item tables
--      wait for the end of this migration (SHARE ROW EXCLUSIVE);
--   2. the backfill and the GIN index: one to_tsvector for each item;
--   3. the view, then the drop of the 5 expression indexes. A drop takes
--      ACCESS EXCLUSIVE until the commit, so reads wait only for this tail.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table with one row for each item (archived items too, as the old
-- indexes covered them), 1 function, 15 triggers, 1 GIN index. The view
-- searchable_items keeps its columns; its tsv comes from item_search. 5
-- indexes dropped. No row of an item table is changed.
--
-- Re-runnable: each statement has a guard, and the second run changes
-- nothing.
CREATE TABLE IF NOT EXISTS item_search (
    item_id     UUID PRIMARY KEY,
    entity_type TEXT NOT NULL
                CHECK (entity_type IN ('strategy', 'initiative', 'task', 'document', 'adr')),
    tsv         TSVECTOR NOT NULL
);

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

DROP TRIGGER IF EXISTS strategies_item_search_insert ON strategies;
CREATE TRIGGER strategies_item_search_insert AFTER INSERT ON strategies
    FOR EACH ROW EXECUTE FUNCTION item_search_write('strategy');
DROP TRIGGER IF EXISTS strategies_item_search_update ON strategies;
CREATE TRIGGER strategies_item_search_update AFTER UPDATE OF title, content ON strategies
    FOR EACH ROW
    WHEN (OLD.title IS DISTINCT FROM NEW.title OR OLD.content IS DISTINCT FROM NEW.content)
    EXECUTE FUNCTION item_search_write('strategy');
DROP TRIGGER IF EXISTS strategies_item_search_delete ON strategies;
CREATE TRIGGER strategies_item_search_delete AFTER DELETE ON strategies
    FOR EACH ROW EXECUTE FUNCTION item_search_write('strategy');

DROP TRIGGER IF EXISTS initiatives_item_search_insert ON initiatives;
CREATE TRIGGER initiatives_item_search_insert AFTER INSERT ON initiatives
    FOR EACH ROW EXECUTE FUNCTION item_search_write('initiative');
DROP TRIGGER IF EXISTS initiatives_item_search_update ON initiatives;
CREATE TRIGGER initiatives_item_search_update AFTER UPDATE OF title, content ON initiatives
    FOR EACH ROW
    WHEN (OLD.title IS DISTINCT FROM NEW.title OR OLD.content IS DISTINCT FROM NEW.content)
    EXECUTE FUNCTION item_search_write('initiative');
DROP TRIGGER IF EXISTS initiatives_item_search_delete ON initiatives;
CREATE TRIGGER initiatives_item_search_delete AFTER DELETE ON initiatives
    FOR EACH ROW EXECUTE FUNCTION item_search_write('initiative');

DROP TRIGGER IF EXISTS tasks_item_search_insert ON tasks;
CREATE TRIGGER tasks_item_search_insert AFTER INSERT ON tasks
    FOR EACH ROW EXECUTE FUNCTION item_search_write('task');
DROP TRIGGER IF EXISTS tasks_item_search_update ON tasks;
CREATE TRIGGER tasks_item_search_update AFTER UPDATE OF title, content ON tasks
    FOR EACH ROW
    WHEN (OLD.title IS DISTINCT FROM NEW.title OR OLD.content IS DISTINCT FROM NEW.content)
    EXECUTE FUNCTION item_search_write('task');
DROP TRIGGER IF EXISTS tasks_item_search_delete ON tasks;
CREATE TRIGGER tasks_item_search_delete AFTER DELETE ON tasks
    FOR EACH ROW EXECUTE FUNCTION item_search_write('task');

DROP TRIGGER IF EXISTS documents_item_search_insert ON documents;
CREATE TRIGGER documents_item_search_insert AFTER INSERT ON documents
    FOR EACH ROW EXECUTE FUNCTION item_search_write('document');
DROP TRIGGER IF EXISTS documents_item_search_update ON documents;
CREATE TRIGGER documents_item_search_update AFTER UPDATE OF title, content ON documents
    FOR EACH ROW
    WHEN (OLD.title IS DISTINCT FROM NEW.title OR OLD.content IS DISTINCT FROM NEW.content)
    EXECUTE FUNCTION item_search_write('document');
DROP TRIGGER IF EXISTS documents_item_search_delete ON documents;
CREATE TRIGGER documents_item_search_delete AFTER DELETE ON documents
    FOR EACH ROW EXECUTE FUNCTION item_search_write('document');

DROP TRIGGER IF EXISTS adrs_item_search_insert ON adrs;
CREATE TRIGGER adrs_item_search_insert AFTER INSERT ON adrs
    FOR EACH ROW EXECUTE FUNCTION item_search_write('adr');
DROP TRIGGER IF EXISTS adrs_item_search_update ON adrs;
CREATE TRIGGER adrs_item_search_update AFTER UPDATE OF title, content ON adrs
    FOR EACH ROW
    WHEN (OLD.title IS DISTINCT FROM NEW.title OR OLD.content IS DISTINCT FROM NEW.content)
    EXECUTE FUNCTION item_search_write('adr');
DROP TRIGGER IF EXISTS adrs_item_search_delete ON adrs;
CREATE TRIGGER adrs_item_search_delete AFTER DELETE ON adrs
    FOR EACH ROW EXECUTE FUNCTION item_search_write('adr');

-- The backfill: every item, archived ones too. A second run writes the same
-- vectors again and adds no row.
INSERT INTO item_search (item_id, entity_type, tsv)
    SELECT id, 'strategy', setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B')
    FROM strategies
    UNION ALL
    SELECT id, 'initiative', setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B')
    FROM initiatives
    UNION ALL
    SELECT id, 'task', setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B')
    FROM tasks
    UNION ALL
    SELECT id, 'document', setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B')
    FROM documents
    UNION ALL
    SELECT id, 'adr', setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B')
    FROM adrs
ON CONFLICT (item_id) DO UPDATE SET entity_type = EXCLUDED.entity_type, tsv = EXCLUDED.tsv;

CREATE INDEX IF NOT EXISTS idx_item_search_tsv ON item_search USING GIN (tsv);

-- The view keeps its columns, names and types (CREATE OR REPLACE checks
-- that). The 5 arms stay one table each, so the planner can still probe
-- each primary key; the LEFT JOIN gives tsv, and the planner removes it
-- when a query does not read tsv.
CREATE OR REPLACE VIEW searchable_items AS
    SELECT u.id, u.short_code, u.entity_type, u.title, u.content, s.tsv, u.deleted_at
    FROM (
        SELECT id, short_code, 'strategy'::text AS entity_type, title, content, deleted_at
        FROM strategies
        UNION ALL
        SELECT id, short_code, 'initiative', title, content, deleted_at
        FROM initiatives
        UNION ALL
        SELECT id, short_code, 'task', title, content, deleted_at
        FROM tasks
        UNION ALL
        SELECT id, short_code, 'document', title, content, deleted_at
        FROM documents
        UNION ALL
        SELECT id, short_code, 'adr', title, content, deleted_at
        FROM adrs
    ) u
    LEFT JOIN item_search s ON s.item_id = u.id;

DROP INDEX IF EXISTS idx_strategies_tsv;
DROP INDEX IF EXISTS idx_initiatives_tsv;
DROP INDEX IF EXISTS idx_tasks_tsv;
DROP INDEX IF EXISTS idx_documents_tsv;
DROP INDEX IF EXISTS idx_adrs_tsv;

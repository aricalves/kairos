-- KAIROS-T-0624: back to the expression indexes of 2026-09-23-000004.
CREATE INDEX IF NOT EXISTS idx_strategies_tsv ON strategies USING GIN ((setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B')));
CREATE INDEX IF NOT EXISTS idx_initiatives_tsv ON initiatives USING GIN ((setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B')));
CREATE INDEX IF NOT EXISTS idx_tasks_tsv ON tasks USING GIN ((setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B')));
CREATE INDEX IF NOT EXISTS idx_documents_tsv ON documents USING GIN ((setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B')));
CREATE INDEX IF NOT EXISTS idx_adrs_tsv ON adrs USING GIN ((setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B')));

DROP VIEW IF EXISTS searchable_items;

CREATE VIEW searchable_items AS
    SELECT id, short_code, 'strategy' AS entity_type, title, content,
           setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B') AS tsv,
           deleted_at
    FROM strategies
    UNION ALL
    SELECT id, short_code, 'initiative', title, content,
           setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B'),
           deleted_at
    FROM initiatives
    UNION ALL
    SELECT id, short_code, 'task', title, content,
           setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B'),
           deleted_at
    FROM tasks
    UNION ALL
    SELECT id, short_code, 'document', title, content,
           setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B'),
           deleted_at
    FROM documents
    UNION ALL
    SELECT id, short_code, 'adr', title, content,
           setweight(to_tsvector('english', coalesce(title, '')), 'A') || setweight(to_tsvector('english', coalesce(content, '')), 'B'),
           deleted_at
    FROM adrs;

DROP TRIGGER IF EXISTS strategies_item_search_insert ON strategies;
DROP TRIGGER IF EXISTS strategies_item_search_update ON strategies;
DROP TRIGGER IF EXISTS strategies_item_search_delete ON strategies;
DROP TRIGGER IF EXISTS initiatives_item_search_insert ON initiatives;
DROP TRIGGER IF EXISTS initiatives_item_search_update ON initiatives;
DROP TRIGGER IF EXISTS initiatives_item_search_delete ON initiatives;
DROP TRIGGER IF EXISTS tasks_item_search_insert ON tasks;
DROP TRIGGER IF EXISTS tasks_item_search_update ON tasks;
DROP TRIGGER IF EXISTS tasks_item_search_delete ON tasks;
DROP TRIGGER IF EXISTS documents_item_search_insert ON documents;
DROP TRIGGER IF EXISTS documents_item_search_update ON documents;
DROP TRIGGER IF EXISTS documents_item_search_delete ON documents;
DROP TRIGGER IF EXISTS adrs_item_search_insert ON adrs;
DROP TRIGGER IF EXISTS adrs_item_search_update ON adrs;
DROP TRIGGER IF EXISTS adrs_item_search_delete ON adrs;
DROP FUNCTION IF EXISTS item_search_write();
DROP TABLE IF EXISTS item_search;

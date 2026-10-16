-- KAIROS-T-0508 (COLLIERY-I-0630): an item links to a URI with a label, an
-- EXTERNAL REFERENCE. An RFC in a wiki, a design page or an issue that
-- Kairos does not mirror: the tools (`get_item`, `related_work`) and the
-- web item page show it next to the Kairos items.
--
-- A table of its own, as `item_impacts` is (COLLIERY-T-0269), and not rows
-- of `item_relationships`. Each end of an `item_relationships` row is an
-- item, and the graph view, the traversal of the search, `related_work`
-- and the cascade of an archive read that table with that rule. A URI is
-- not an item.
--
-- `item_id` has no foreign key, for the reason that `item_relationships`
-- has none: an item is a row in one of five tables. `item_type` says
-- which. Every item type can have a reference.
--
-- The server applies the rule of the URI (`kairos_core::external_ref`:
-- the schemes http, https and mailto). The table keeps the text.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table, empty. No row is changed. Re-runnable.
CREATE TABLE IF NOT EXISTS item_external_refs (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    item_id     UUID NOT NULL,
    item_type   TEXT NOT NULL
                CHECK (item_type IN ('strategy', 'initiative', 'task', 'document', 'adr')),
    uri         TEXT NOT NULL CHECK (uri <> ''),
    label       TEXT,
    created_by  UUID NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (item_id, uri)
);

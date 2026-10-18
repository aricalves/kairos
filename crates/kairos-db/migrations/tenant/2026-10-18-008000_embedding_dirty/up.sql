-- KAIROS-T-0636 (COLLIERY-I-0650): the embedding sweep reads the items that
-- a write marked, not every item.
--
-- New:
--   embedding_dirty             one row for each item whose vectors can be
--                               behind its text. The transaction of the
--                               write adds the row, or gives it a new mark
--                               (kairos-db events.rs and embeddings.rs).
--                               The sweep reads the rows, oldest first, and
--                               deletes a row only when its mark is still
--                               the mark that it read: an item changed in
--                               between stays dirty.
--   embedding_dirty_oldest_idx  embedding_dirty (dirty_at, item_id): the
--                               order of the sweep.
--
-- No foreign key on item_id: an item is a row of one of five tables, as in
-- item_embeddings. The sweep deletes the row of an item that is gone.
--
-- Back-fill: each item of searchable_items, archived items too. That is the
-- set that the sweep before this migration walked. SQL cannot compute the
-- hash of the text, so it cannot tell which items the old sweep would embed.
-- The first runs after the migration plan each item once. The hash check
-- sends only changed text to the provider.
--
-- The table is new, so its index is not CONCURRENTLY.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 table and 1 index added, with one row for each item. No
-- other row changes.
--
-- Re-runnable: each statement has a guard.
CREATE TABLE IF NOT EXISTS embedding_dirty (
    item_id  uuid        PRIMARY KEY,
    dirty_at timestamptz NOT NULL DEFAULT now(),
    -- The microseconds of the clock at the mark. A new mark of a marked
    -- item is also at least the old mark plus 1, so it always differs.
    mark     bigint      NOT NULL
             DEFAULT (extract(epoch FROM clock_timestamp()) * 1000000)::bigint
);

CREATE INDEX IF NOT EXISTS embedding_dirty_oldest_idx
    ON embedding_dirty (dirty_at, item_id);

INSERT INTO embedding_dirty (item_id)
SELECT id FROM searchable_items
ON CONFLICT (item_id) DO NOTHING;

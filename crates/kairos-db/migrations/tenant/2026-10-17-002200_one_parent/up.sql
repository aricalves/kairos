-- KAIROS-T-0572: an item has one parent.
--
-- A `parent` edge goes from a strategy to an initiative, or from an
-- initiative to a task. Until now nothing stopped a second edge to the same
-- item. A task under two initiatives then counted in both rollups and in the
-- `my_work` of both owners, and `get_item` showed the oldest parent only.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1. For each item with two or more parent edges, the oldest edge stays
--    (created_at, then id). The migration removes each other parent edge.
-- 2. For each removed edge, one `relationship_remove` row goes to
--    activity_log. The actor is the nil UUID: the system, as the retention
--    sweep. The details have the usual form, and name the parent that
--    stays: "relationship:parent:<parent>-><item> (KAIROS-T-0572: one parent
--    per item; <kept> stays the parent)".
-- 3. A unique index on the target of a parent edge. A second parent is then
--    refused by the database too.
--
-- An item with one parent or none is not changed. Edges to archived items
-- count as edges: the oldest edge stays, also when its parent is archived.
--
-- Re-runnable: a second run finds no item with two parents.

WITH ranked AS (
    SELECT r.id,
           r.source_id,
           r.target_id,
           row_number() OVER w AS n,
           first_value(r.source_id) OVER w AS kept_id
      FROM item_relationships r
     WHERE r.relationship = 'parent'
    WINDOW w AS (PARTITION BY r.target_id ORDER BY r.created_at, r.id)
),
removed AS (
    DELETE FROM item_relationships r
     USING ranked
     WHERE r.id = ranked.id
       AND ranked.n > 1
    RETURNING r.source_id, r.target_id, ranked.kept_id
)
INSERT INTO activity_log (actor_id, action, entity_id, entity_type, details)
SELECT '00000000-0000-0000-0000-000000000000'::uuid,
       'relationship_remove',
       NULL,
       NULL,
       'relationship:parent:'
           || COALESCE(s.short_code, removed.source_id::text)
           || '->'
           || COALESCE(t.short_code, removed.target_id::text)
           || ' (KAIROS-T-0572: one parent per item; '
           || COALESCE(k.short_code, removed.kept_id::text)
           || ' stays the parent)'
  FROM removed
  LEFT JOIN entity_directory s ON s.id = removed.source_id
  LEFT JOIN entity_directory t ON t.id = removed.target_id
  LEFT JOIN entity_directory k ON k.id = removed.kept_id;

CREATE UNIQUE INDEX IF NOT EXISTS item_relationships_one_parent
    ON item_relationships (target_id)
 WHERE relationship = 'parent';

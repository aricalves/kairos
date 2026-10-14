-- KAIROS-T-0437: the documents and ADRs of a team are nodes in its tree. The
-- board still owns each item and shows where it is in its lifecycle; this
-- table only says where the item sits in the team page. An item with no row
-- is at the root. One row per item: an item has one place.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table. No row is changed.
CREATE TABLE IF NOT EXISTS team_page_placements (
    -- A document (documents.id) or an ADR (adrs.id); the code checks which.
    item_id    UUID PRIMARY KEY,
    team_id    UUID NOT NULL REFERENCES teams (id) ON DELETE CASCADE,
    -- The folder (team_pages.id); NULL is the root. An archived or deleted
    -- folder shows the item at the root, and a restore puts it back.
    parent_id  UUID REFERENCES team_pages (id) ON DELETE SET NULL,
    position   INTEGER NOT NULL DEFAULT 0,
    placed_by  UUID NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS team_page_placements_team
    ON team_page_placements (team_id);

DROP TABLE IF EXISTS forge_issue_intake;
DELETE FROM item_links WHERE kind = 'issue';
ALTER TABLE item_links DROP CONSTRAINT IF EXISTS item_links_kind_check;
ALTER TABLE item_links ADD CONSTRAINT item_links_kind_check
    CHECK (kind IN ('branch', 'pull_request'));

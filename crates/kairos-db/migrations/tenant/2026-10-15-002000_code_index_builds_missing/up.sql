-- KAIROS-T-0495: each run of the builder records the summaries that its
-- index does not have: a summary call failed (for example the provider
-- refused it for the rate), or a child of the item has no summary. The next
-- build makes them. NULL for a run with no counts, or from before this
-- version. Re-runnable.
ALTER TABLE code_index_builds ADD COLUMN IF NOT EXISTS summaries_missing INTEGER;

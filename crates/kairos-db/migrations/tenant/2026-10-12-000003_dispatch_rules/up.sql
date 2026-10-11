-- KAIROS-T-0385 (COLLIERY-I-0614): the agent dispatch rules of a board.
--
--   dispatch_rules   A rule on a board: when a task enters the column
--                    `column_id` and matches the filter, Kairos queues one
--                    signed delivery to the webhook target `target_id`
--                    (KAIROS-T-0384). The filter has 3 optional parts, and
--                    each part that is set must match: the task type, a
--                    metadata value (the slug of the definition and the
--                    value), and the repository.
--
-- webhook_deliveries.rule_id names the rule of a delivery (NULL for a test
-- delivery), so the log of a rule can be read.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table and 1 new column with NULL. No row is changed.
--
-- Re-runnable: each statement has a guard.
CREATE TABLE IF NOT EXISTS dispatch_rules (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    board_id       UUID NOT NULL REFERENCES boards (id) ON DELETE CASCADE,
    column_id      UUID NOT NULL REFERENCES board_columns (id) ON DELETE CASCADE,
    target_id      UUID NOT NULL REFERENCES webhook_targets (id) ON DELETE CASCADE,
    task_type      TEXT NULL CHECK (task_type IN ('task', 'bug', 'tech_debt', 'support')),
    metadata_slug  TEXT NULL,
    metadata_value TEXT NULL,
    repository_id  UUID NULL REFERENCES repositories (id) ON DELETE CASCADE,
    created_by     UUID NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    CHECK ((metadata_slug IS NULL) = (metadata_value IS NULL))
);

CREATE INDEX IF NOT EXISTS dispatch_rules_column_idx ON dispatch_rules (board_id, column_id);

ALTER TABLE webhook_deliveries
    ADD COLUMN IF NOT EXISTS rule_id UUID NULL REFERENCES dispatch_rules (id) ON DELETE SET NULL;

CREATE INDEX IF NOT EXISTS webhook_deliveries_rule_idx
    ON webhook_deliveries (rule_id, created_at DESC) WHERE rule_id IS NOT NULL;

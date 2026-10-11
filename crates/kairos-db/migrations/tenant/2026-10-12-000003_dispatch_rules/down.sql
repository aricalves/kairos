-- KAIROS-T-0385, down. The dispatch rules are lost; the deliveries stay,
-- with no rule.
DROP INDEX IF EXISTS webhook_deliveries_rule_idx;
ALTER TABLE webhook_deliveries DROP COLUMN IF EXISTS rule_id;
DROP TABLE IF EXISTS dispatch_rules;

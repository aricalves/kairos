-- KAIROS-T-0427 down: the rules that fire on `unblocked` go.
DELETE FROM dispatch_rules WHERE trigger = 'unblocked';
ALTER TABLE dispatch_rules DROP CONSTRAINT IF EXISTS dispatch_rules_trigger_check;
ALTER TABLE dispatch_rules ALTER COLUMN column_id SET NOT NULL;
ALTER TABLE dispatch_rules DROP COLUMN IF EXISTS trigger;

-- KAIROS-T-0384 (COLLIERY-I-0614): signed webhook deliveries.
--
--   webhook_targets          A URL that gets deliveries, on one board. The
--                            secret is made by Kairos, shown one time, and
--                            kept sealed with KAIROS_SECRETS_KEY
--                            (secret_ciphertext, secret_nonce,
--                            secret_key_id; the associated data names the
--                            tenant and the target). consecutive_failures
--                            counts the failed tries in a row; at the limit
--                            the target is turned off (enabled false,
--                            disabled_at, disabled_reason).
--   webhook_deliveries       One event to one target: the body that is
--                            signed and sent, and the status (pending,
--                            delivered, failed, skipped). A delivery is a job
--                            of public.jobs (kind webhook_delivery).
--   webhook_delivery_tries   The log: one row for each try (the time, the
--                            response code or the error, the duration).
--
-- notifications and notification_settings get the kind delivery_failing:
-- a target was turned off after failures.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 3 new tables. The CHECK of the kind of the 2 notification tables accepts
-- one more value. No row is changed.
--
-- Re-runnable: each statement has a guard.
CREATE TABLE IF NOT EXISTS webhook_targets (
    id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    board_id             UUID NOT NULL REFERENCES boards (id) ON DELETE CASCADE,
    name                 TEXT NOT NULL CHECK (btrim(name) <> ''),
    url                  TEXT NOT NULL CHECK (url ~* '^https?://'),
    secret_ciphertext    BYTEA NOT NULL,
    secret_nonce         BYTEA NOT NULL,
    secret_key_id        TEXT NOT NULL,
    enabled              BOOLEAN NOT NULL DEFAULT true,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    disabled_at          TIMESTAMPTZ NULL,
    disabled_reason      TEXT NULL,
    created_by           UUID NOT NULL,
    created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS webhook_targets_board_idx ON webhook_targets (board_id);

CREATE TABLE IF NOT EXISTS webhook_deliveries (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    target_id     UUID NOT NULL REFERENCES webhook_targets (id) ON DELETE CASCADE,
    event         TEXT NOT NULL CHECK (btrim(event) <> ''),
    item_id       UUID NULL,
    body          TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'delivered', 'failed', 'skipped')),
    tries         INTEGER NOT NULL DEFAULT 0,
    response_code INTEGER NULL,
    last_error    TEXT NULL,
    next_try_at   TIMESTAMPTZ NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at   TIMESTAMPTZ NULL
);

CREATE INDEX IF NOT EXISTS webhook_deliveries_target_idx
    ON webhook_deliveries (target_id, created_at DESC);
CREATE INDEX IF NOT EXISTS webhook_deliveries_finished_idx
    ON webhook_deliveries (target_id, finished_at) WHERE finished_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS webhook_delivery_tries (
    delivery_id   UUID NOT NULL REFERENCES webhook_deliveries (id) ON DELETE CASCADE,
    try           INTEGER NOT NULL,
    at            TIMESTAMPTZ NOT NULL DEFAULT now(),
    ok            BOOLEAN NOT NULL,
    response_code INTEGER NULL,
    error         TEXT NULL,
    duration_ms   INTEGER NOT NULL,
    PRIMARY KEY (delivery_id, try)
);

ALTER TABLE notifications DROP CONSTRAINT IF EXISTS notifications_kind_check;
ALTER TABLE notifications ADD CONSTRAINT notifications_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing'));
ALTER TABLE notification_settings DROP CONSTRAINT IF EXISTS notification_settings_kind_check;
ALTER TABLE notification_settings ADD CONSTRAINT notification_settings_kind_check CHECK (kind IN
    ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug', 'delivery_failing'));

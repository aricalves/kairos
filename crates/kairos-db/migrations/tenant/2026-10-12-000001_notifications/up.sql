-- KAIROS-T-0381 (COLLIERY-I-0614): the notifications of a person.
--
--   notifications           One notification for one person (`user_id`):
--                           the kind, the item it is about (`item_id`, or
--                           NULL), the text, the person or the service
--                           account that made the change (`actor_id`), the
--                           time, and the time the person read it
--                           (`read_at`, NULL while unread).
--   notification_settings   The kinds that a person turned off or on. No
--                           row for a kind: the kind is on.
--
-- The kinds: hand_off, blocker_cleared, cancelled, mention, p0_bug.
--
-- No foreign key on user_id and actor_id: users are in the public schema.
-- No foreign key on item_id: an item is in one of 5 tables. A purge of a
-- task deletes its notifications; an archive keeps them.
--
-- The partial index on the unread rows serves the read of each MCP answer
-- (KAIROS-T-0382): the newest unread notifications of one person.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 2 new tables. No row is changed.
--
-- Re-runnable: each statement has a guard.
CREATE TABLE IF NOT EXISTS notifications (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id    UUID NOT NULL,
    kind       TEXT NOT NULL CHECK (kind IN
                   ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug')),
    item_id    UUID NULL,
    text       TEXT NOT NULL CHECK (btrim(text) <> ''),
    actor_id   UUID NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    read_at    TIMESTAMPTZ NULL
);

CREATE INDEX IF NOT EXISTS notifications_unread_idx
    ON notifications (user_id, created_at DESC) WHERE read_at IS NULL;
CREATE INDEX IF NOT EXISTS notifications_user_idx
    ON notifications (user_id, created_at DESC);
CREATE INDEX IF NOT EXISTS notifications_read_at_idx
    ON notifications (read_at) WHERE read_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS notifications_item_idx
    ON notifications (item_id) WHERE item_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS notification_settings (
    user_id UUID NOT NULL,
    kind    TEXT NOT NULL CHECK (kind IN
                ('hand_off', 'blocker_cleared', 'cancelled', 'mention', 'p0_bug')),
    enabled BOOLEAN NOT NULL,
    PRIMARY KEY (user_id, kind)
);

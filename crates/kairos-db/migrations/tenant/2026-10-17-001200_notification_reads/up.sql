-- KAIROS-T-0550: each agent key of a person keeps its own read state of
-- the notifications of the person.
--
--   notification_reads   One row for each notification that one agent
--                        key read (`api_key_id`), and the time. The read
--                        state of the person stays `notifications.read_at`.
--
-- A notification is read for a key when a row of the key, or of a key that
-- the key replaced (a rotation), exists. It is read for the key also when
-- the person read it before the first key of that chain was made. So a new
-- key does not start with the old history unread.
--
-- A delete of a notification (retention, a purge of its item) deletes its
-- rows here.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table. No row is changed.
--
-- Re-runnable: each statement has a guard.
CREATE TABLE IF NOT EXISTS notification_reads (
    notification_id UUID NOT NULL REFERENCES notifications (id) ON DELETE CASCADE,
    api_key_id      UUID NOT NULL REFERENCES api_keys (id) ON DELETE CASCADE,
    read_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (notification_id, api_key_id)
);

CREATE INDEX IF NOT EXISTS notification_reads_key_idx
    ON notification_reads (api_key_id);

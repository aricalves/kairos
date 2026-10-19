-- KAIROS-T-0632 (COLLIERY-I-0650): the outbox of the change events.
--
-- A write inserts its events here, in its own transaction. It sends no
-- NOTIFY. At commit, PostgreSQL takes one database-wide lock for each
-- transaction that sent a NOTIFY, and holds it through the flush of the
-- commit. Every tenant is a schema of one database, so every write of the
-- deployment waited on that lock.
--
-- The emitter of each server takes the unsent rows in id order with
-- SELECT ... FOR UPDATE SKIP LOCKED, sends one NOTIFY kairos_events for
-- each batch, and sets sent_at. Two servers thus never send one row twice.
-- A sent row is deleted one minute later.
--
-- PUBLIC, with the schema of the tenant in a column, as the job queue: one
-- query of the emitter reads the events of every tenant.
CREATE TABLE IF NOT EXISTS event_outbox (
    id          BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    -- The schema of the write (org_<slug>), from current_schema().
    schema_name TEXT NOT NULL,
    -- The thin event, without the routing fields tenant and blocks_boards.
    payload     JSONB NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    sent_at     TIMESTAMPTZ
);

-- The emitter reads only the unsent rows. The table is new and empty.
CREATE INDEX IF NOT EXISTS event_outbox_unsent
    ON event_outbox (id) WHERE sent_at IS NULL;

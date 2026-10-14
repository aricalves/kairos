-- KAIROS-T-0428: a board setting that lets an agent decide the triage of
-- intake (a task in the support lane), not only propose it. A board with no
-- row has the setting off: an agent proposes, a person decides.
--
-- WHAT THIS DOES TO THE DATA OF A TENANT
--
-- 1 new table. No row is changed.
CREATE TABLE IF NOT EXISTS board_agent_triage (
    board_id      UUID PRIMARY KEY REFERENCES boards (id) ON DELETE CASCADE,
    agents_decide BOOLEAN NOT NULL DEFAULT false,
    updated_by    UUID NOT NULL,
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

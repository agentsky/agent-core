-- Agent-to-agent hand-off.
--
-- `message_refs.hands_off` marks a post a turn made in its own thread, as
-- the agent, outside a private task: the only posts whose mentions of other
-- agents start their turns. A post the same turn made in another thread or
-- conversation, a private task's result, and a notice of agentd's own are
-- 0, so a mention there starts nothing, whoever delivers it.
--
-- `hand_offs` holds the hand-offs agentd owes: one row for each agent a
-- turn's posts mention, written as the post is recorded, with the event
-- agentd built for it (`event_json`, JSON agentd owns). A row is deleted
-- once that agent's job for it has settled it, and is otherwise taken again
-- once `due_at` passes: the instance whose job holds it keeps pushing
-- `due_at` on, and a shutdown makes it due at once, so a hand-off a shutdown
-- or a crash cut is delivered by the next instance to look. The hop claim
-- in `processed_events` makes a second delivery do nothing. Rows recorded
-- over an hour ago are dropped by `created_at`, unless a job holds them. Ids
-- are never reused, since instances hold rows by id in memory.
ALTER TABLE message_refs ADD COLUMN hands_off INTEGER NOT NULL DEFAULT 0
    CHECK (hands_off IN (0, 1));

CREATE TABLE hand_offs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    agent_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    event_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    due_at INTEGER NOT NULL
) STRICT;

CREATE INDEX hand_offs_by_due ON hand_offs (due_at);

CREATE INDEX hand_offs_by_age ON hand_offs (created_at);

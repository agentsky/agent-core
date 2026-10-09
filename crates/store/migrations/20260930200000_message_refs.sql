-- Message refs: every message agentd posts as an agent, and every inbound
-- message shown to a session's model, with the short id the model knows it
-- by in that session.
--
-- `short_id` counts from 1 in each session. `platform_ref` is the
-- message's id on the surface (a Slack `ts`, a Rocket.Chat `_id`), unique
-- only within its conversation, so a message is named by `surface`,
-- `team_id`, `conversation` and `platform_ref` together. `thread_root` is
-- the thread the message is in, `''` for a conversation's top level, as in
-- `sessions`.
--
-- A message agentd posted has `agent_id` set: the agent it posted as, whose
-- turn's requester and hop the row records, so a later mention in it is
-- billed to that requester. Each posted message has one such row. The same
-- message may also be shown to other sessions, which record it as an
-- inbound row of their own (`agent_id` NULL) with their own short id. An
-- inbound row's requester is the message's sender, with hop 0.

CREATE TABLE message_refs (
    session_id TEXT NOT NULL,
    short_id INTEGER NOT NULL CHECK (short_id > 0),
    surface TEXT NOT NULL CHECK (surface IN ('slack', 'rocketchat')),
    team_id TEXT NOT NULL,
    conversation TEXT NOT NULL,
    thread_root TEXT NOT NULL,
    platform_ref TEXT NOT NULL,
    agent_id TEXT,
    turn_id TEXT,
    requester_member TEXT,
    requester_key TEXT NOT NULL,
    hop INTEGER NOT NULL CHECK (hop BETWEEN 0 AND 255),
    posted_at INTEGER NOT NULL,
    PRIMARY KEY (session_id, short_id)
) STRICT;

CREATE UNIQUE INDEX message_refs_session_message
    ON message_refs (session_id, surface, team_id, conversation, platform_ref);

CREATE UNIQUE INDEX message_refs_posted
    ON message_refs (surface, team_id, conversation, platform_ref)
    WHERE agent_id IS NOT NULL;

CREATE INDEX message_refs_agent_thread
    ON message_refs (agent_id, surface, team_id, conversation, thread_root);

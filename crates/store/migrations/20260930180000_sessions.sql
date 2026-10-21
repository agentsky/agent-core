-- Claude Code sessions: one row per agent and thread, plus private tasks.
--
-- A normal session is found by its agent and thread: `surface`, `team_id`,
-- `conversation` (the conversation's id on the surface) and `thread_root`.
-- A DM's one continuous session has `thread_root = ''`, because SQLite
-- treats NULLs as distinct in a unique index. A private task's session is
-- never looked up by thread: its thread columns record where its result is
-- posted, and `consent_id` the consent it runs under.
--
-- `started` is set once the CLI has read a turn's message, so the session
-- has a transcript and the next process must `--resume` it.
-- `maybe_started` is set before a turn goes to the CLI of a session that
-- hasn't started, and cleared once the outcome says whether the CLI read
-- it; if agentd dies in between, the next process tries `--resume` first.
--
-- A reset keeps the row with `reset_at` set and inserts a new row with a
-- new id, so the unique index covers live normal rows only.

CREATE TABLE sessions (
    id TEXT PRIMARY KEY NOT NULL,
    agent_id TEXT NOT NULL,
    surface TEXT NOT NULL CHECK (surface IN ('slack', 'rocketchat')),
    team_id TEXT NOT NULL,
    conversation TEXT NOT NULL,
    thread_root TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('normal', 'private')),
    consent_id TEXT,
    started INTEGER NOT NULL DEFAULT 0 CHECK (started IN (0, 1)),
    maybe_started INTEGER NOT NULL DEFAULT 0 CHECK (maybe_started IN (0, 1)),
    created_at INTEGER NOT NULL,
    last_turn_at INTEGER,
    reset_at INTEGER,
    CHECK ((kind = 'private') = (consent_id IS NOT NULL))
) STRICT;

CREATE UNIQUE INDEX sessions_live_thread
    ON sessions (agent_id, surface, team_id, conversation, thread_root)
    WHERE kind = 'normal' AND reset_at IS NULL;

CREATE INDEX sessions_agent_id ON sessions (agent_id);

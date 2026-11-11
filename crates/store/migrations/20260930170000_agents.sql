-- Agents and the chat identities they are exposed as.
--
-- An agent's name is unique among its owner's agents that aren't deleted.
-- Deleted agents keep their row, since volumes, sessions and message refs
-- name them.
--
-- A binding is one bot identity on one surface and team. `bot_user_id` is
-- set once the platform has created the bot user, which for a Slack app
-- comes after the binding exists. The Slack columns are empty for
-- Rocket.Chat bindings.
--
-- A `disabled` binding whose bot user still exists owes its retirement:
-- deactivating the bot user on the platform. `retired_at` records that it
-- was done. The claim, lease and backoff columns follow the relink
-- notices' pattern: a claim counts an attempt and sets
-- `retire_next_attempt_at` to the lease's end, a failure moves it to the
-- next retry.

CREATE TABLE agents (
    id TEXT PRIMARY KEY NOT NULL,
    owner_id TEXT NOT NULL REFERENCES members (id),
    name TEXT NOT NULL,
    persona TEXT NOT NULL,
    visibility TEXT NOT NULL CHECK (visibility IN ('public', 'private')),
    state TEXT NOT NULL CHECK (state IN ('active', 'paused', 'deleted')),
    created_at INTEGER NOT NULL
) STRICT;

CREATE UNIQUE INDEX agents_owner_name ON agents (owner_id, name) WHERE state <> 'deleted';

CREATE TABLE agent_bindings (
    id TEXT PRIMARY KEY NOT NULL,
    agent_id TEXT NOT NULL REFERENCES agents (id),
    surface TEXT NOT NULL CHECK (surface IN ('slack', 'rocketchat')),
    team_id TEXT NOT NULL,
    bot_user_id TEXT,
    bot_username TEXT,
    bot_token_enc BLOB,
    state TEXT NOT NULL
        CHECK (state IN ('creating', 'pending_install', 'active', 'disabled')),
    state_changed_at INTEGER NOT NULL,
    app_id TEXT,
    client_id TEXT,
    client_secret_enc BLOB,
    signing_secret_enc BLOB,
    retired_at INTEGER,
    retire_attempts INTEGER NOT NULL DEFAULT 0,
    retire_next_attempt_at INTEGER
) STRICT;

CREATE UNIQUE INDEX agent_bindings_bot_user ON agent_bindings (surface, team_id, bot_user_id)
    WHERE bot_user_id IS NOT NULL;
CREATE INDEX agent_bindings_agent_id ON agent_bindings (agent_id);
CREATE INDEX agent_bindings_state ON agent_bindings (surface, team_id, state);

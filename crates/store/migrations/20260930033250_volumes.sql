-- One row per volume: the host directory that holds an agent's files for
-- one scope. The directory name is a digest of the scope key, which can't
-- be reversed, so this table records which key each directory holds.
--
-- `path` is relative to agentd's data directory
-- (`volumes/<agent id>/<hex SHA-256 of scope_key>`), so moving the data
-- directory doesn't invalidate it. `scope_key` is core-types' string form.

CREATE TABLE volumes (
    agent_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (agent_id, scope_key)
) STRICT;

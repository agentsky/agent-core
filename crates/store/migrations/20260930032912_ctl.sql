-- agentctl: per-process tokens and the scope locks agentctl takes for
-- writes to a volume's shared/ directory.
--
-- Neither table outlives a restart: agentd deletes every row at startup,
-- because the containers the rows name are reaped then and Docker can give
-- their addresses to new containers.

-- One row per running `claude` process. `hash` is the SHA-256 of the token;
-- the token itself is never stored. The turn columns are set while a turn
-- runs and NULL between turns, when the token authorizes nothing.
CREATE TABLE ctl_tokens (
    hash BLOB PRIMARY KEY NOT NULL CHECK (length(hash) = 32),
    session_id TEXT NOT NULL UNIQUE,
    agent_id TEXT NOT NULL,
    volume_key TEXT NOT NULL,
    container_ip TEXT NOT NULL,
    turn_id TEXT,
    requester_member TEXT,
    requester_key TEXT,
    hop INTEGER CHECK (hop BETWEEN 0 AND 255),
    kind TEXT CHECK (kind IN ('normal', 'private_task')),
    consent_id TEXT,
    side TEXT CHECK (side IN ('owner', 'public')),
    conversation TEXT,
    thread_root TEXT,
    trigger_message TEXT,
    CHECK (
        (turn_id IS NULL AND requester_key IS NULL AND hop IS NULL AND kind IS NULL
            AND side IS NULL AND conversation IS NULL AND requester_member IS NULL
            AND consent_id IS NULL AND thread_root IS NULL AND trigger_message IS NULL)
        OR (turn_id IS NOT NULL AND requester_key IS NOT NULL AND hop IS NOT NULL
            AND kind IS NOT NULL AND side IS NOT NULL AND conversation IS NOT NULL
            AND (kind = 'private_task') = (consent_id IS NOT NULL))
    )
) STRICT;

-- One row per held lease. `volume_key` is unique, so a volume has at most
-- one lease; an expired row is taken over by the next acquire.
CREATE TABLE scope_locks (
    lease_id TEXT PRIMARY KEY NOT NULL,
    volume_key TEXT NOT NULL UNIQUE,
    holder_session TEXT NOT NULL,
    expires_at INTEGER NOT NULL
) STRICT;

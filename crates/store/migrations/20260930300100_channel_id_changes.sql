-- channel_id_changes holds the channel_id_changed events agents' apps
-- received: a channel the binding's bot is in says it changed its id from
-- old_channel to new_channel, as a private channel does when it is shared
-- with another organization. A change waits (settled_at NULL) until Slack
-- confirms where the channel is now and the binding's agent's rules on the
-- old id move there. While it waits, the agent's denies on the old id
-- apply to the new one too.
--
-- A try is claimed like a configuration token's rotation: a conditional
-- UPDATE moves next_attempt_at past the try, so a try that fails, or whose
-- process dies, is made again once it has passed. A settled change keeps
-- the id Slack said the channel had then (settled_to), which its chain
-- goes on from as from new_channel, and is kept for a day after it was
-- received, so a later change in a chain (old to new, then new to newer)
-- finds where the channel went, and a replay is known. A change still waiting a day after it was received is given up:
-- its denies on the old id are copied to the new one, and it is deleted.

CREATE TABLE channel_id_changes (
    binding_id TEXT NOT NULL REFERENCES agent_bindings (id) ON DELETE CASCADE,
    old_channel TEXT NOT NULL CHECK (
        length(old_channel) BETWEEN 2 AND 65 AND old_channel GLOB '[CDG]*'
        AND substr(old_channel, 2) NOT GLOB '*[^A-Z0-9]*'
    ),
    new_channel TEXT NOT NULL CHECK (
        length(new_channel) BETWEEN 2 AND 65 AND new_channel GLOB '[CDG]*'
        AND substr(new_channel, 2) NOT GLOB '*[^A-Z0-9]*'
    ),
    received_at INTEGER NOT NULL,
    next_attempt_at INTEGER NOT NULL,
    settled_at INTEGER,
    settled_to TEXT CHECK (
        length(settled_to) BETWEEN 2 AND 65 AND settled_to GLOB '[CDG]*'
        AND substr(settled_to, 2) NOT GLOB '*[^A-Z0-9]*'
    ),
    PRIMARY KEY (binding_id, old_channel, new_channel),
    CHECK (old_channel <> new_channel),
    CHECK ((settled_at IS NULL) = (settled_to IS NULL))
) STRICT;

CREATE INDEX channel_id_changes_next_attempt_at ON channel_id_changes (next_attempt_at)
    WHERE settled_at IS NULL;
CREATE INDEX channel_id_changes_received_at ON channel_id_changes (received_at);

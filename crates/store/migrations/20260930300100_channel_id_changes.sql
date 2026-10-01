-- channel_id_changes holds the channel_id_changed events agents' apps
-- received and agentd hasn't settled yet: a channel the binding's bot is in
-- says it changed its id from old_channel to new_channel, as a private
-- channel does when it is shared with another organization. Settling one
-- confirms the new id with Slack and moves the binding's agent's rules on
-- the old id to it, then deletes the row.
--
-- A try is claimed like a configuration token's rotation: a conditional
-- UPDATE moves next_attempt_at past the try, so a try that fails, or whose
-- process dies, is made again once it has passed. A row is given up a day
-- after it was received.

CREATE TABLE channel_id_changes (
    binding_id TEXT NOT NULL REFERENCES agent_bindings (id) ON DELETE CASCADE,
    old_channel TEXT NOT NULL,
    new_channel TEXT NOT NULL,
    received_at INTEGER NOT NULL,
    next_attempt_at INTEGER NOT NULL,
    PRIMARY KEY (binding_id, old_channel, new_channel)
) STRICT;

CREATE INDEX channel_id_changes_next_attempt_at ON channel_id_changes (next_attempt_at);

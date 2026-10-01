-- The manifest each Slack agent app has: manifest_version is the version of
-- agentd's agent app manifest the app was made, or last updated, from
-- (surface-slack's MANIFEST_VERSION). Every binding already here gets 0,
-- below every version agentd writes, so each Slack app made before the
-- column existed is updated; a new app is stored with the version it was
-- made from.
--
-- An update with apps.manifest.update is claimed like a configuration
-- token's rotation: a conditional UPDATE sets manifest_lease_until, and the
-- update is tried again once it has passed. Registering a configuration
-- token clears the leases of its member's bindings in that workspace, so
-- their apps are updated at once. manifest_blocked_version is the version
-- agentd found it can't update the app to, as when Slack says the app is
-- gone: no update to that version is tried again.

ALTER TABLE agent_bindings ADD COLUMN manifest_version INTEGER NOT NULL DEFAULT 0
    CHECK (manifest_version >= 0);
ALTER TABLE agent_bindings ADD COLUMN manifest_lease_until INTEGER;
ALTER TABLE agent_bindings ADD COLUMN manifest_blocked_version INTEGER
    CHECK (manifest_blocked_version > 0);

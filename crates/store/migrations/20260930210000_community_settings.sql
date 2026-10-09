-- Community-wide settings: one row, whose id is always 1.
--
-- api_key_enc is the community API key a community admin set with
-- `/agent admin api-key set`, sealed like every other secret column, with
-- `community_settings/api_key_enc/1` as its associated data. It is the only
-- place the key lives: NULL when none is set. api_key_changed_by (a member
-- key's string form) and api_key_changed_at record the admin and the time
-- of the last `set` or `clear`, for `/agent me`.

CREATE TABLE community_settings (
    id INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
    api_key_enc BLOB,
    api_key_changed_by TEXT,
    api_key_changed_at INTEGER
) STRICT;

INSERT INTO community_settings (id) VALUES (1);

-- Members, their surface identities, Claude links, pending logins and
-- processed events.
--
-- Conventions for every store migration:
-- - IDs (members, agents, sessions, ...) are TEXT in the lowercase
--   hyphenated UUID form that core-types writes.
-- - Timestamps are INTEGER Unix seconds, UTC.
-- - Encrypted columns end in `_enc` and hold a Sealer value:
--   version(1) || nonce(12) || ciphertext and tag.
-- - Tables are STRICT.

CREATE TABLE members (
    id TEXT PRIMARY KEY NOT NULL,
    display_name TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;

CREATE TABLE surface_identities (
    surface TEXT NOT NULL CHECK (surface IN ('slack', 'rocketchat')),
    team_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    member_id TEXT NOT NULL REFERENCES members (id) ON DELETE CASCADE,
    PRIMARY KEY (surface, team_id, user_id)
) STRICT;

CREATE INDEX surface_identities_member_id ON surface_identities (member_id);

CREATE TABLE claude_links (
    member_id TEXT PRIMARY KEY NOT NULL REFERENCES members (id) ON DELETE CASCADE,
    access_token_enc BLOB NOT NULL,
    refresh_token_enc BLOB NOT NULL,
    expires_at INTEGER NOT NULL,
    plan TEXT,
    rate_limit_tier TEXT,
    broken_at INTEGER,
    updated_at INTEGER NOT NULL
) STRICT;

CREATE TABLE pending_logins (
    state TEXT PRIMARY KEY NOT NULL,
    member_id TEXT NOT NULL REFERENCES members (id) ON DELETE CASCADE,
    verifier_enc BLOB NOT NULL,
    expires_at INTEGER NOT NULL
) STRICT;

CREATE INDEX pending_logins_member_id ON pending_logins (member_id);
CREATE INDEX pending_logins_expires_at ON pending_logins (expires_at);

CREATE TABLE processed_events (
    source TEXT NOT NULL,
    event_id TEXT NOT NULL,
    seen_at INTEGER NOT NULL,
    PRIMARY KEY (source, event_id)
) STRICT, WITHOUT ROWID;

CREATE INDEX processed_events_seen_at ON processed_events (seen_at);

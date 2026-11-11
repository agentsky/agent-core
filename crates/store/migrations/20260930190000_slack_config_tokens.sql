-- Members' Slack app configuration tokens, one per member and workspace,
-- registered with `/agent slack-token` and renewed with
-- tooling.tokens.rotate before they expire.
--
-- version is a random value replaced on every write of the tokens, so a
-- rotation that read the row writes nothing if the member registered a new
-- token meanwhile. lease_until keeps other callers off the row while one
-- rotates it or sends its notice. broken_at is set when Slack refused the
-- refresh token; the member is then owed one notice, which notified_at
-- records, tried at most a bounded number of times (notice_attempts).

CREATE TABLE slack_config_tokens (
    member_id TEXT NOT NULL REFERENCES members (id) ON DELETE CASCADE,
    team_id TEXT NOT NULL,
    token_enc BLOB NOT NULL,
    refresh_token_enc BLOB NOT NULL,
    expires_at INTEGER NOT NULL,
    version TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    lease_until INTEGER,
    broken_at INTEGER,
    notified_at INTEGER,
    notice_attempts INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (member_id, team_id)
) STRICT;

CREATE INDEX slack_config_tokens_expires_at ON slack_config_tokens (expires_at);

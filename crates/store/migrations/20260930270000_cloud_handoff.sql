-- Cloud hand-off: members' routines and the hand-offs that fired them.
--
-- `cloud_routines` holds the routines a member registered with
-- `/agent cloud add`: the member's label for it, the routine's id (with its
-- `trig_` prefix), the origin of the fire URL it was registered with
-- (`url_origin`, as `url::Origin::ascii_serialization` writes it, so a fire
-- can be refused once `[cloud] base_url` points elsewhere), and its API
-- trigger's token, sealed with
-- `cloud_routines/token_enc/<member>:<id>:<routine id>:<label>:<url
-- origin>` as associated data, so a row moved to another member, or given
-- another routine id, label or origin, no longer opens. A label holds no
-- `:`, so that key reads one way. `added_by` is the identity that
-- registered it, a member key's string form. A member holds a label once
-- and a routine id once; registering an existing label again replaces
-- that row in place, keeping its id.
--
-- `cloud_handoffs` records each `/agent cloud run`: the routine's label and
-- id, copied so the record outlives the routine's row, the identity that
-- asked (`requested_by`) and the kind of place it asked from (`origin`), and
-- the task, sealed with `cloud_handoffs/task_enc/<member>:<id>`. `state` is
-- `sending` from before the request until its outcome is recorded: `fired`
-- with the session's id and link, `rejected` with the HTTP status, error
-- type and `Retry-After` the answer gave, if any, or `unknown` with its
-- status, if any, and why it isn't known (`unknown_reason`; `no_answer`
-- when a pass marked it). `answered_at` is when the outcome was recorded,
-- or when a pass gave up waiting for one and marked the row `unknown`.
--
-- An `unknown` row a pass marked owes its member a notice, sent at least
-- once like the relink notices: `notice_attempts` counts claims, and
-- `notice_next_attempt_at` holds a claim's lease or a failed send's
-- backoff. `notified_at` says the member was told the outcome, by the
-- command's reply or by the notice, so a `fired` or `rejected` row always
-- has it. A row the pass marked (`unknown_reason` `no_answer`) still takes
-- one late answer.

CREATE TABLE cloud_routines (
    id TEXT PRIMARY KEY NOT NULL,
    member_id TEXT NOT NULL REFERENCES members (id) ON DELETE CASCADE,
    label TEXT NOT NULL CHECK (label NOT GLOB '*:*'),
    routine_id TEXT NOT NULL CHECK (routine_id GLOB 'trig_?*'),
    url_origin TEXT NOT NULL,
    token_enc BLOB NOT NULL,
    added_by TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    UNIQUE (member_id, label),
    UNIQUE (member_id, routine_id)
) STRICT;

CREATE TABLE cloud_handoffs (
    id TEXT PRIMARY KEY NOT NULL,
    member_id TEXT NOT NULL REFERENCES members (id) ON DELETE CASCADE,
    routine_label TEXT NOT NULL,
    routine_id TEXT NOT NULL CHECK (routine_id GLOB 'trig_?*'),
    requested_by TEXT NOT NULL,
    origin TEXT NOT NULL
        CHECK (origin IN ('slack_slash', 'slack_dm', 'rocketchat_dm')),
    task_enc BLOB NOT NULL,
    state TEXT NOT NULL
        CHECK (state IN ('sending', 'fired', 'rejected', 'unknown')),
    http_status INTEGER CHECK (http_status BETWEEN 100 AND 999),
    error_type TEXT,
    retry_after_secs INTEGER CHECK (retry_after_secs >= 0),
    session_id TEXT,
    session_url TEXT,
    created_at INTEGER NOT NULL,
    answered_at INTEGER,
    notice_attempts INTEGER NOT NULL DEFAULT 0 CHECK (notice_attempts >= 0),
    notice_next_attempt_at INTEGER,
    notified_at INTEGER,
    unknown_reason TEXT CHECK (unknown_reason IN (
        'server_error', 'other_status', 'timeout', 'connection_lost',
        'redirect', 'unreadable_answer', 'no_answer'
    )),
    CHECK ((state = 'unknown') = (unknown_reason IS NOT NULL)),
    CHECK ((state = 'sending') = (answered_at IS NULL)),
    CHECK ((state = 'fired') = (session_id IS NOT NULL)),
    CHECK (session_id IS NULL OR session_id <> ''),
    CHECK (session_url IS NULL OR state = 'fired'),
    CHECK (
        state = 'rejected' OR (error_type IS NULL AND retry_after_secs IS NULL)
    ),
    CHECK (state <> 'sending' OR notified_at IS NULL),
    CHECK (state NOT IN ('fired', 'rejected') OR notified_at IS NOT NULL)
) STRICT;

CREATE INDEX cloud_handoffs_by_member
    ON cloud_handoffs (member_id, created_at);

CREATE INDEX cloud_handoffs_by_created_at ON cloud_handoffs (created_at);

CREATE INDEX cloud_handoffs_sending ON cloud_handoffs (created_at)
    WHERE state = 'sending';

CREATE INDEX cloud_handoffs_notices ON cloud_handoffs (notice_next_attempt_at)
    WHERE state = 'unknown' AND notified_at IS NULL;

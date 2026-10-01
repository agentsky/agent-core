-- Consents: requests for private tasks on an agent owner's private
-- resources, made with `agentctl private` during a turn.
--
-- A consent names its agent, the requester and hop of the turn that asked
-- (`requester_member` and `requester_key`, as in `message_refs`), the
-- exact task text, and the files that turn attached: `attachments_json` is
-- JSON agentd owns, each file's display name and its staged copy under
-- `<data dir>/consents/<id>/`. The result goes to the thread named by
-- `reply_surface`, `reply_team_id`, `reply_conversation` and
-- `reply_thread_root` (`''` for a conversation's top level, as in
-- `sessions`); `origin_session_id` is the session that asked.
--
-- `state` is the owner's decision: `pending` until the owner answers the
-- consent card, `approved`, `declined`, or `expired` once `expires_at`
-- passed unanswered or the card couldn't reach the owner. `approval` says
-- how an approved consent was approved: `asked` at once, because the owner
-- asked for it in their own DM with the agent (a turn at hop 0, so never at
-- a later hop), or `card` by the owner on the consent card. `decided_by`
-- is the identity that decided, a member key's string form (none for an
-- expiry), and `decided_at` when.
--
-- The card is sent at least once, retried until the consent expires, like
-- the relink notices: `card_attempts` counts claims and
-- `card_next_attempt_at` holds a claim's lease or a failed send's backoff. `card_conversation` and
-- `card_message` say where it is once posted, and `card_closed_at` that it
-- was updated with the outcome.
--
-- A decided consent then owes work: running an approved task, or posting a
-- declined or expired outcome to the thread. It is leased the same way,
-- with `work_attempts` and `work_next_attempt_at`, so an instance that dies
-- mid-task leaves it to be claimed again; `work_failures` counts the claims
-- that failed. `private_session_id` is the session the task last ran in,
-- and `finished_at` is set once the work is done.
--
-- `message_refs.consent_id` names the consent a private task's result or
-- outcome reports, so a mention in that post never starts another agent's
-- turn.

CREATE TABLE consents (
    id TEXT PRIMARY KEY NOT NULL,
    agent_id TEXT NOT NULL REFERENCES agents (id),
    requester_member TEXT,
    requester_key TEXT NOT NULL,
    hop INTEGER NOT NULL CHECK (hop BETWEEN 0 AND 255),
    task_text TEXT NOT NULL,
    attachments_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'approved', 'declined', 'expired')),
    approval TEXT CHECK (approval IN ('asked', 'card')),
    reply_surface TEXT NOT NULL CHECK (reply_surface IN ('slack', 'rocketchat')),
    reply_team_id TEXT NOT NULL,
    reply_conversation TEXT NOT NULL,
    reply_thread_root TEXT NOT NULL,
    origin_session_id TEXT NOT NULL,
    private_session_id TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    decided_by TEXT,
    decided_at INTEGER,
    card_conversation TEXT,
    card_message TEXT,
    card_attempts INTEGER NOT NULL DEFAULT 0,
    card_next_attempt_at INTEGER,
    card_closed_at INTEGER,
    work_attempts INTEGER NOT NULL DEFAULT 0,
    work_next_attempt_at INTEGER,
    work_failures INTEGER NOT NULL DEFAULT 0,
    finished_at INTEGER,
    CHECK ((state = 'pending') = (decided_at IS NULL)),
    CHECK ((state = 'approved') = (approval IS NOT NULL)),
    CHECK (approval IS NOT 'asked' OR hop = 0),
    CHECK ((card_conversation IS NULL) = (card_message IS NULL))
) STRICT;

CREATE INDEX consents_unfinished ON consents (state, decided_at)
    WHERE finished_at IS NULL;

CREATE INDEX consents_unfinished_by_requester ON consents (agent_id, requester_key)
    WHERE finished_at IS NULL;

CREATE INDEX consents_cards_to_close ON consents (decided_at)
    WHERE card_message IS NOT NULL AND card_closed_at IS NULL;

ALTER TABLE message_refs ADD COLUMN consent_id TEXT;

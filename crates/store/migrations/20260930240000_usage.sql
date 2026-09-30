-- The usage meter, agents' limits, allow and deny rules, per-thread usage
-- and bans.
--
-- Days and hours are UTC: `day` counts days since 1970-01-01, and `hour` is
-- the hour of that day, 0 to 23.
--
-- usage is each member's turns per day, billed to them as a turn's
-- requester: turns, tokens and the CLI's cost. input_tokens holds the
-- input the model read fresh (uncached input and cache writes);
-- cache reads, which every call of a turn repeats for the whole
-- conversation, aren't counted. A turn whose cost isn't known is billed
-- no cost, in a row of its own whose cost_unknown says why (the store's
-- CostUnknown names the reasons); a turn with a known cost has
-- cost_unknown ''. What went unbilled is then the turns and tokens of the
-- rows with a reason.
--
-- agent_policies holds an owner's settings for one agent: turns_per_day
-- (NULL for no cap), max_hops (NULL to leave the global cap), and the allow
-- and deny rules as JSON lists agentd reads. An agent without a row has no
-- limits and no rules.
--
-- thread_usage counts agents' turns and tokens per thread, agent and hour,
-- for the per-thread caps, and others_turns, the turns requested by anyone
-- but the agent's owner, for its daily cap. A thread is named as in
-- message_refs: thread_root is '' for a conversation without threads. Rows
-- older than a couple of days are swept.
--
-- limit_notices records that an agent told a thread it reached a limit, once
-- per window (the day or the hour the limit counts), so a capped agent
-- doesn't answer every message with the same line. Swept like thread_usage.
--
-- bans holds the members a community admin banned: banned_by is the admin's
-- member key string form.

CREATE TABLE usage (
    member_id TEXT NOT NULL REFERENCES members (id) ON DELETE CASCADE,
    day INTEGER NOT NULL,
    turns INTEGER NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cost_usd REAL NOT NULL,
    cost_unknown TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (member_id, day, cost_unknown)
) STRICT;

CREATE TABLE agent_policies (
    agent_id TEXT PRIMARY KEY NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    turns_per_day INTEGER CHECK (turns_per_day >= 0),
    max_hops INTEGER CHECK (max_hops BETWEEN 0 AND 255),
    allow_json TEXT NOT NULL DEFAULT '[]',
    deny_json TEXT NOT NULL DEFAULT '[]'
) STRICT;

CREATE TABLE thread_usage (
    surface TEXT NOT NULL,
    team_id TEXT NOT NULL,
    conversation TEXT NOT NULL,
    thread_root TEXT NOT NULL,
    day INTEGER NOT NULL,
    hour INTEGER NOT NULL CHECK (hour BETWEEN 0 AND 23),
    agent_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    agent_turns INTEGER NOT NULL,
    others_turns INTEGER NOT NULL,
    tokens INTEGER NOT NULL,
    PRIMARY KEY (surface, team_id, conversation, thread_root, day, hour, agent_id)
) STRICT;

CREATE INDEX thread_usage_agent_day ON thread_usage (agent_id, day);

CREATE TABLE limit_notices (
    agent_id TEXT NOT NULL REFERENCES agents (id) ON DELETE CASCADE,
    surface TEXT NOT NULL,
    team_id TEXT NOT NULL,
    conversation TEXT NOT NULL,
    thread_root TEXT NOT NULL,
    kind TEXT NOT NULL,
    window_start INTEGER NOT NULL,
    PRIMARY KEY (agent_id, surface, team_id, conversation, thread_root, kind, window_start)
) STRICT;

CREATE TABLE bans (
    member_id TEXT PRIMARY KEY NOT NULL REFERENCES members (id) ON DELETE CASCADE,
    banned_by TEXT NOT NULL,
    reason TEXT,
    created_at INTEGER NOT NULL
) STRICT;

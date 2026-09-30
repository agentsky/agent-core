-- Skills an agent's owner added with `/agent skill add`.
--
-- The skill's files live in agentd's data directory, `skills/<agent>/<name>/`
-- for an active skill; this table records what agentd reads back: which
-- skills an agent has, where each came from, and the hosts its sandboxes
-- may reach for it through the egress proxy.
--
-- A skill whose `SKILL.md` declares `allowed-hosts` is `pending` until the
-- owner confirms those hosts; only `active` rows extend the egress
-- allowlist. An agent has at most one row of each state per name: a
-- pending row may wait next to the active skill it would replace.
--
-- `hosts` holds the host rules one per line, `''` for none. `source` is
-- the Git URL the skill was cloned from, or `upload:` and the name of the
-- file the owner attached.

CREATE TABLE agent_skills (
    agent_id TEXT NOT NULL REFERENCES agents (id),
    name TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'active')),
    source TEXT NOT NULL,
    hosts TEXT NOT NULL,
    added_by TEXT NOT NULL REFERENCES members (id),
    added_at INTEGER NOT NULL,
    PRIMARY KEY (agent_id, name, state)
) STRICT;

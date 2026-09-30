-- When a requester was last sent the manager bot's direct message about a
-- turn that failed on the credential it ran on, per kind of failure.
--
-- requester is the member key's string form, <surface>:<team>:<user>, since
-- a turn on the community key may have no member. kind names the failure
-- and whose credential it was, such as `usage_limit/member`. sent_at is when
-- the last such message was claimed; agentd claims a new one only once the
-- last is old enough, so a credential that keeps failing doesn't message the
-- requester on every turn, whichever instance runs it.

CREATE TABLE failure_notices (
    requester TEXT NOT NULL,
    kind TEXT NOT NULL,
    sent_at INTEGER NOT NULL,
    PRIMARY KEY (requester, kind)
) STRICT;

-- How many times an agentd instance claimed the relink notice of the link's
-- current break, and when it may be claimed next: the end of a claim's lease
-- while a send is under way, or the end of the backoff after a send failed.
-- An instance that dies mid-send leaves a lease that simply runs out. Both
-- are cleared with relink_notified_at whenever the link is stored or
-- refreshed again, so each break starts over.

ALTER TABLE claude_links ADD COLUMN relink_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE claude_links ADD COLUMN relink_next_attempt_at INTEGER;

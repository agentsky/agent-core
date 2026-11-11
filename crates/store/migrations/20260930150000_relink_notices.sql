-- When the member was sent the notice that their broken Claude link needs a
-- new login. Set by the one agentd instance that claims the notice, and
-- cleared whenever the link is stored or refreshed again, so each time
-- broken_at goes from NULL to set is announced exactly once, whichever
-- instance marked it and even across restarts.

ALTER TABLE claude_links ADD COLUMN relink_notified_at INTEGER;

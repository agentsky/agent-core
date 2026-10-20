-- `files_requester` is the surface identity (`MemberKey`) of the requester
-- the session's files were last cleared for: whose turns may have left
-- what is in its `home/`, `tmp/` and `claude/`, with the requesters they
-- trust. The runner clears those directories before starting a container
-- for a turn of anyone else, and NULL, for a new session or a row from
-- before this column, clears for everyone.

ALTER TABLE sessions ADD COLUMN files_requester TEXT;

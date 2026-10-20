-- `files_requesters` holds the surface identities (`MemberKey`s) of the
-- requesters whose turns have used the session's `home/`, `tmp/` and
-- `claude/` files since the runner last cleared them: a JSON array of their
-- string forms, sorted and without duplicates. The runner clears those
-- directories before starting a container for a turn whose requester isn't
-- the same party as every one of them. NULL, for a new session or a row
-- from before this column, and a value that doesn't parse clear for
-- everyone.

ALTER TABLE sessions ADD COLUMN files_requesters TEXT;

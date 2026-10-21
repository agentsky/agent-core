-- `files_requesters` holds the surface identities (`MemberKey`s) of the
-- requesters whose turns have used the session's files since the runner
-- last cleared them: a JSON array of their string forms, sorted and without
-- duplicates. A clear removes every entry of the session directory but its
-- four subdirectories, empties `home/` and `tmp/`, and removes everything
-- in `claude/` but the transcript file and its directory. The runner clears
-- before starting a container for a turn whose requester isn't the same
-- party as every one of them, or whose session's warm container was
-- started for another party. NULL, for a new session or a row from before
-- this column, and a value that doesn't parse clear for everyone.

ALTER TABLE sessions ADD COLUMN files_requesters TEXT;

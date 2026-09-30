-- Slack agent apps: the scopes and redirect URL an app was created with, and
-- the reminder a member gets when their agent's app is still waiting to be
-- installed.
--
-- A Slack binding is created in state `creating`, gets its app's `app_id`,
-- `client_id`, `client_secret_enc`, `signing_secret_enc`, `app_scopes` (the
-- bot scopes its manifest asks for, comma-separated, which its install link
-- must ask for too) and `app_redirect_url` (the OAuth redirect URL its
-- manifest names, which its install link and the code exchange must name
-- too) and moves to `pending_install`, and becomes
-- `active` with its bot user and token once the member installed the app.
-- A binding still `pending_install` a while after it got there owes its
-- owner one reminder, which `install_reminded_at` records. The claim, lease and
-- attempt columns follow the relink notices' pattern: a claim counts an
-- attempt and sets `install_reminder_next_at` to the lease's end.

ALTER TABLE agent_bindings ADD COLUMN app_scopes TEXT;
ALTER TABLE agent_bindings ADD COLUMN app_redirect_url TEXT;
ALTER TABLE agent_bindings ADD COLUMN install_reminded_at INTEGER;
ALTER TABLE agent_bindings ADD COLUMN install_reminder_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE agent_bindings ADD COLUMN install_reminder_next_at INTEGER;

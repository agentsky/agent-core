//! Slack agent apps on `agent_bindings`: each agent's Slack app, from its
//! creation to its installation.
//!
//! A Slack binding is created with its agent in state
//! [`creating`](BindingState::Creating) ([`Store::create_agent`]), so the
//! ingress knows it and answers the `url_verification` challenge Slack sends
//! while `apps.manifest.create` runs. The app Slack created is then
//! [stored](Store::set_slack_app), with its client and signing secrets
//! sealed, and the binding waits in
//! [`pending_install`](BindingState::PendingInstall) until the member
//! installs the app; the OAuth callback [installs](Store::install_slack_app)
//! the bot token and makes it [`active`](BindingState::Active).
//!
//! The install link's `state` is [sealed](Store::install_state) with the
//! master key, naming the binding, so the callback can trust which binding
//! it installs. A binding still waiting for its install a while after it
//! started owes its owner one reminder, claimed with a lease like the relink
//! notices.
//!
//! Each app records the version of agentd's manifest it was made from
//! (`manifest_version`). An installed app made from an older one is
//! [updated](Store::due_manifest_updates) with its owner's configuration
//! token, each update claimed with a lease like a configuration token's
//! rotation, unless agentd found it can't update the app to that version
//! ([`block_manifest_update`](Store::block_manifest_update)).

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use core_types::{AgentId, BindingId, MemberId, TeamId, UserId};
use secrecy::{ExposeSecret as _, SecretString};
use time::OffsetDateTime;

use crate::agents::BindingState;
use crate::seal::Aad;
use crate::{Result, Store, StoreError, parse_column, to_unix};

const BINDINGS: &str = "agent_bindings";
const CLIENT_SECRET: &str = "client_secret_enc";
const SIGNING_SECRET: &str = "signing_secret_enc";
const TOKEN: &str = "bot_token_enc";
const INSTALL_STATE: &str = "install_state";
const INSTALL_STATE_TEXT: &str = "slack-install";
/// The longest `state` [`Store::binding_of_install_state`] reads.
const MAX_INSTALL_STATE_LEN: usize = 256;

/// The app `apps.manifest.create` made for a binding, for
/// [`Store::set_slack_app`]. `Debug` redacts the secrets.
#[derive(Debug)]
pub struct NewSlackApp {
    /// The app's id (`A…`).
    pub app_id: String,
    /// The app's OAuth client id.
    pub client_id: String,
    /// The app's OAuth client secret.
    pub client_secret: SecretString,
    /// The secret the app's requests are signed with.
    pub signing_secret: SecretString,
    /// The bot scopes its manifest asks for, comma-separated, which its
    /// install link must ask for too.
    pub scopes: String,
    /// The OAuth redirect URL its manifest names, which its install link
    /// and the code exchange must name too.
    pub redirect_url: String,
    /// The version of agentd's manifest it was made from.
    pub manifest_version: u32,
}

/// What the ingress needs to verify a Slack binding's requests, from
/// [`Store::slack_app_keys`]. `Debug` redacts the secret.
#[derive(Debug, Clone)]
pub struct SlackAppKeys {
    /// The app's signing secret, once `apps.manifest.create` returned it.
    pub signing_secret: Option<SecretString>,
    /// The app's bot user, once the app is installed.
    pub bot_user: Option<UserId>,
    /// The owner of the binding's agent.
    pub owner: MemberId,
}

/// A Slack binding and its app, without secrets, from
/// [`Store::slack_app`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackAppBinding {
    /// The binding.
    pub binding: BindingId,
    /// Its agent.
    pub agent: AgentId,
    /// The workspace.
    pub team: TeamId,
    /// Where the binding is in its life.
    pub state: BindingState,
    /// The app's id, once created.
    pub app_id: Option<String>,
    /// The app's OAuth client id, once created.
    pub client_id: Option<String>,
    /// The app's bot scopes, comma-separated, once created.
    pub scopes: Option<String>,
    /// The app's OAuth redirect URL, once created.
    pub redirect_url: Option<String>,
}

/// An installed Slack app made, or last updated, from a manifest older than
/// the one asked for, whose owner has a configuration token to update it
/// with, from [`Store::due_manifest_updates`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestUpdate {
    /// The binding.
    pub binding: BindingId,
    /// The agent's owner, whose configuration token updates the app.
    pub owner: MemberId,
    /// The app's id.
    pub app_id: String,
}

/// An agent whose installed Slack app is on a manifest older than the one
/// asked for, from [`Store::outdated_slack_apps`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutdatedSlackApp {
    /// The agent's name.
    pub agent_name: String,
    /// Whether agentd found it can't update the app to the version asked
    /// for.
    pub blocked: bool,
}

/// A Slack binding whose owner is owed the reminder to install its app,
/// from [`Store::due_install_reminders`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReminder {
    /// The binding.
    pub binding: BindingId,
    /// The agent's name.
    pub agent_name: String,
    /// The agent's owner, who is reminded.
    pub owner: MemberId,
    /// The app's OAuth client id, for the install link.
    pub client_id: String,
    /// The app's bot scopes, comma-separated, for the install link.
    pub scopes: String,
    /// The app's OAuth redirect URL, for the install link.
    pub redirect_url: String,
}

fn aad<'a>(column: &'static str, binding: &'a str) -> Aad<'a> {
    Aad {
        table: BINDINGS,
        column,
        key: binding,
    }
}

fn attempt(value: i64) -> Result<u32> {
    u32::try_from(value).map_err(|_| StoreError::Corrupt {
        table: BINDINGS,
        column: "install_reminder_attempts",
    })
}

/// The conditions under which a reminder may be claimed, binding the time
/// the binding must have been waiting since, then `now`, then the most
/// attempts, in that order.
macro_rules! reminder_due {
    () => {
        "b.surface = 'slack' AND b.state = 'pending_install' AND b.state_changed_at <= ? \
         AND b.install_reminded_at IS NULL \
         AND (b.install_reminder_next_at IS NULL OR b.install_reminder_next_at <= ?) \
         AND b.install_reminder_attempts < ?"
    };
}

/// The conditions under which a manifest update may be claimed, binding the
/// version asked for, twice, then `now`.
macro_rules! manifest_due {
    () => {
        "b.surface = 'slack' AND b.state = 'active' AND b.manifest_version < ? \
         AND (b.manifest_blocked_version IS NULL OR b.manifest_blocked_version < ?) \
         AND (b.manifest_lease_until IS NULL OR b.manifest_lease_until <= ?)"
    };
}

impl Store {
    /// The signing secret, bot user and agent's owner of the Slack binding
    /// `binding` in `team`, while it is `creating`, `pending_install` or
    /// `active`: the bindings whose requests the ingress answers. `None` for any other
    /// binding, one in another workspace, or one that is disabled.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Seal`] if
    /// the secret doesn't decrypt.
    pub async fn slack_app_keys(
        &self,
        binding: BindingId,
        team: &TeamId,
    ) -> Result<Option<SlackAppKeys>> {
        let key = binding.to_string();
        let row: Option<(Option<Vec<u8>>, Option<String>, String)> = sqlx::query_as(
            "SELECT b.signing_secret_enc, b.bot_user_id, a.owner_id \
             FROM agent_bindings b JOIN agents a ON a.id = b.agent_id \
             WHERE b.id = ? AND b.surface = 'slack' AND b.team_id = ? \
             AND b.state IN ('creating', 'pending_install', 'active')",
        )
        .bind(&key)
        .bind(team.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(secret, bot_user, owner)| {
            Ok(SlackAppKeys {
                signing_secret: secret
                    .map(|sealed| self.open_sealed(aad(SIGNING_SECRET, &key), &sealed))
                    .transpose()?,
                bot_user: bot_user.map(UserId::from),
                owner: parse_column(&owner, "agents", "owner_id")?,
            })
        })
        .transpose()
    }

    /// Stores the app Slack created for the `creating` Slack binding
    /// `binding`, with its bot user's username `bot_username`, and moves the
    /// binding to `pending_install` at `now`. Returns false, storing
    /// nothing, if the binding isn't a `creating` Slack binding any more, as
    /// when its creation was abandoned meanwhile.
    ///
    /// # Errors
    ///
    /// [`StoreError::Seal`] if a secret can't be sealed,
    /// [`StoreError::Database`] if the query fails.
    pub async fn set_slack_app(
        &self,
        binding: BindingId,
        app: &NewSlackApp,
        bot_username: &str,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let key = binding.to_string();
        let client_secret = self.seal(aad(CLIENT_SECRET, &key), &app.client_secret)?;
        let signing_secret = self.seal(aad(SIGNING_SECRET, &key), &app.signing_secret)?;
        let result = sqlx::query(
            "UPDATE agent_bindings SET app_id = ?, client_id = ?, client_secret_enc = ?, \
             signing_secret_enc = ?, app_scopes = ?, app_redirect_url = ?, bot_username = ?, \
             manifest_version = ?, state = 'pending_install', state_changed_at = ? \
             WHERE id = ? AND surface = 'slack' AND state = 'creating'",
        )
        .bind(&app.app_id)
        .bind(&app.client_id)
        .bind(client_secret)
        .bind(signing_secret)
        .bind(&app.scopes)
        .bind(&app.redirect_url)
        .bind(bot_username)
        .bind(i64::from(app.manifest_version))
        .bind(to_unix(now))
        .bind(&key)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The Slack binding `binding` and its app, in any state, without its
    /// secrets. `None` if there is no such Slack binding.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the row doesn't parse.
    pub async fn slack_app(&self, binding: BindingId) -> Result<Option<SlackAppBinding>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            agent_id: String,
            team_id: String,
            state: String,
            app_id: Option<String>,
            client_id: Option<String>,
            app_scopes: Option<String>,
            app_redirect_url: Option<String>,
        }
        let row: Option<Row> = sqlx::query_as(
            "SELECT agent_id, team_id, state, app_id, client_id, app_scopes, app_redirect_url \
             FROM agent_bindings WHERE id = ? AND surface = 'slack'",
        )
        .bind(binding.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok(SlackAppBinding {
                binding,
                agent: parse_column(&row.agent_id, BINDINGS, "agent_id")?,
                team: row.team_id.into(),
                state: BindingState::parse(&row.state)?,
                app_id: row.app_id,
                client_id: row.client_id,
                scopes: row.app_scopes,
                redirect_url: row.app_redirect_url,
            })
        })
        .transpose()
    }

    /// The OAuth client secret of the Slack binding `binding`'s app, while
    /// the binding is `pending_install`: the only time it is needed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Seal`] if
    /// the secret doesn't decrypt.
    pub async fn slack_client_secret(&self, binding: BindingId) -> Result<Option<SecretString>> {
        let key = binding.to_string();
        let sealed: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT client_secret_enc FROM agent_bindings \
             WHERE id = ? AND surface = 'slack' AND state = 'pending_install' \
             AND client_secret_enc IS NOT NULL",
        )
        .bind(&key)
        .fetch_optional(&self.pool)
        .await?;
        sealed
            .map(|sealed| self.open_sealed(aad(CLIENT_SECRET, &key), &sealed))
            .transpose()
    }

    /// Installs the app `app_id` of the `pending_install` Slack binding
    /// `binding`: stores its bot user and sealed bot token, forgets the
    /// client secret, which nothing needs any more, and makes the binding
    /// `active` at `now`. Returns false, storing nothing, if the binding
    /// isn't waiting for that app's install.
    ///
    /// # Errors
    ///
    /// [`StoreError::Seal`] if the token can't be sealed,
    /// [`StoreError::Database`] if another binding already has that bot user
    /// in the workspace, or the query fails.
    pub async fn install_slack_app(
        &self,
        binding: BindingId,
        app_id: &str,
        bot_user: &UserId,
        token: &SecretString,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let key = binding.to_string();
        let sealed = self.seal(aad(TOKEN, &key), token)?;
        let result = sqlx::query(
            "UPDATE agent_bindings SET bot_user_id = ?, bot_token_enc = ?, \
             client_secret_enc = NULL, state = 'active', state_changed_at = ? \
             WHERE id = ? AND surface = 'slack' AND state = 'pending_install' AND app_id = ?",
        )
        .bind(bot_user.as_str())
        .bind(sealed)
        .bind(to_unix(now))
        .bind(&key)
        .bind(app_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The `state` of an install link for `binding`: the binding id and a
    /// value sealed with the master key for that binding, so only agentd
    /// can make one and it names one binding.
    /// [`binding_of_install_state`](Self::binding_of_install_state) reads
    /// it back.
    ///
    /// # Errors
    ///
    /// [`StoreError::Seal`] if the value can't be sealed.
    pub fn install_state(&self, binding: BindingId) -> Result<String> {
        let key = binding.to_string();
        let sealed = self.seal(
            aad(INSTALL_STATE, &key),
            &SecretString::from(INSTALL_STATE_TEXT),
        )?;
        Ok(format!("{key}.{}", URL_SAFE_NO_PAD.encode(sealed)))
    }

    /// The binding an install link's `state` names, if agentd made it with
    /// [`install_state`](Self::install_state); `None` for anything else.
    /// Whether the binding still waits for its install is the caller's to
    /// check.
    pub fn binding_of_install_state(&self, state: &str) -> Option<BindingId> {
        if state.len() > MAX_INSTALL_STATE_LEN {
            return None;
        }
        let (key, sealed) = state.split_once('.')?;
        let binding: BindingId = key.parse().ok()?;
        let sealed = URL_SAFE_NO_PAD.decode(sealed).ok()?;
        let opened = self.open_sealed(aad(INSTALL_STATE, key), &sealed).ok()?;
        (opened.expose_secret() == INSTALL_STATE_TEXT).then_some(binding)
    }

    /// The Slack bindings in `team` whose owners are owed the reminder to
    /// install their app at `now`: `pending_install` since `pending_since`
    /// or earlier, not reminded yet, with no lease running and fewer than
    /// `max_attempts` claims, oldest first.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn due_install_reminders(
        &self,
        team: &TeamId,
        pending_since: OffsetDateTime,
        now: OffsetDateTime,
        max_attempts: u32,
    ) -> Result<Vec<InstallReminder>> {
        let rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(concat!(
            "SELECT b.id, a.name, a.owner_id, b.client_id, b.app_scopes, b.app_redirect_url \
             FROM agent_bindings b JOIN agents a ON a.id = b.agent_id \
             WHERE b.team_id = ? AND a.state <> 'deleted' AND b.client_id IS NOT NULL \
             AND b.app_scopes IS NOT NULL AND b.app_redirect_url IS NOT NULL AND ",
            reminder_due!(),
            " ORDER BY b.state_changed_at, b.rowid"
        ))
        .bind(team.as_str())
        .bind(to_unix(pending_since))
        .bind(to_unix(now))
        .bind(i64::from(max_attempts))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(
                |(binding, agent_name, owner, client_id, scopes, redirect_url)| {
                    Ok(InstallReminder {
                        binding: parse_column(&binding, BINDINGS, "id")?,
                        agent_name,
                        owner: parse_column(&owner, "agents", "owner_id")?,
                        client_id,
                        scopes,
                        redirect_url,
                    })
                },
            )
            .collect()
    }

    /// Claims the install reminder of `binding` at `now`, with a lease until
    /// `lease_until`, if it is due (as for
    /// [`due_install_reminders`](Self::due_install_reminders)). Returns
    /// which attempt this is, counting from 1, only for the one call that
    /// claims it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the attempt count is negative.
    pub async fn claim_install_reminder(
        &self,
        binding: BindingId,
        pending_since: OffsetDateTime,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
        max_attempts: u32,
    ) -> Result<Option<u32>> {
        let claimed: Option<i64> = sqlx::query_scalar(concat!(
            "UPDATE agent_bindings AS b \
             SET install_reminder_attempts = install_reminder_attempts + 1, \
             install_reminder_next_at = ? WHERE b.id = ? AND ",
            reminder_due!(),
            " RETURNING install_reminder_attempts"
        ))
        .bind(to_unix(lease_until))
        .bind(binding.to_string())
        .bind(to_unix(pending_since))
        .bind(to_unix(now))
        .bind(i64::from(max_attempts))
        .fetch_optional(&self.pool)
        .await?;
        claimed.map(attempt).transpose()
    }

    /// Up to `limit` installed Slack apps in `team` whose manifest update
    /// may be claimed at `now`: made, or last updated, from a manifest
    /// older than `version`, not blocked at `version`, with no lease
    /// running, of an agent that isn't deleted, whose owner has a
    /// configuration token that isn't broken and still works at
    /// `token_until`. The ones never tried come first, then those whose
    /// lease ended longest ago, so a failing app doesn't hold up the rest.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn due_manifest_updates(
        &self,
        team: &TeamId,
        version: u32,
        now: OffsetDateTime,
        token_until: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<ManifestUpdate>> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(concat!(
            "SELECT b.id, a.owner_id, b.app_id \
             FROM agent_bindings b JOIN agents a ON a.id = b.agent_id \
             WHERE b.team_id = ? AND a.state <> 'deleted' AND b.app_id IS NOT NULL \
             AND EXISTS (SELECT 1 FROM slack_config_tokens t WHERE t.member_id = a.owner_id \
             AND t.team_id = b.team_id AND t.broken_at IS NULL AND t.expires_at > ?) AND ",
            manifest_due!(),
            " ORDER BY b.manifest_lease_until, b.state_changed_at, b.rowid LIMIT ?"
        ))
        .bind(team.as_str())
        .bind(to_unix(token_until))
        .bind(i64::from(version))
        .bind(i64::from(version))
        .bind(to_unix(now))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(binding, owner, app_id)| {
                Ok(ManifestUpdate {
                    binding: parse_column(&binding, BINDINGS, "id")?,
                    owner: parse_column(&owner, "agents", "owner_id")?,
                    app_id,
                })
            })
            .collect()
    }

    /// Claims the manifest update of `binding` at `now`, with a lease until
    /// `lease_until`, if it is due as for
    /// [`due_manifest_updates`](Self::due_manifest_updates) says, for
    /// `version`. Returns true only for the one call that claims it.
    ///
    /// Follow a claim with
    /// [`set_manifest_version`](Self::set_manifest_version) once the app is
    /// updated. A claim that doesn't, because the update failed or its
    /// caller died, lets the update be claimed again once the lease ends.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn claim_manifest_update(
        &self,
        binding: BindingId,
        version: u32,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(concat!(
            "UPDATE agent_bindings AS b SET manifest_lease_until = ? WHERE b.id = ? AND ",
            manifest_due!()
        ))
        .bind(to_unix(lease_until))
        .bind(binding.to_string())
        .bind(i64::from(version))
        .bind(i64::from(version))
        .bind(to_unix(now))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Records that agentd can't update `binding`'s app to the manifest of
    /// `version`, and ends its lease: no update to `version` is claimed
    /// again. Returns false, changing nothing, if the app has that version
    /// already.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn block_manifest_update(&self, binding: BindingId, version: u32) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE agent_bindings SET manifest_blocked_version = ?, manifest_lease_until = NULL \
             WHERE id = ? AND manifest_version < ?",
        )
        .bind(i64::from(version))
        .bind(binding.to_string())
        .bind(i64::from(version))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Records that `binding`'s app has the manifest of `version` now, and
    /// ends its lease. Returns false, changing nothing, if it had that
    /// version or a later one already.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn set_manifest_version(&self, binding: BindingId, version: u32) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE agent_bindings SET manifest_version = ?, manifest_lease_until = NULL \
             WHERE id = ? AND manifest_version < ?",
        )
        .bind(i64::from(version))
        .bind(binding.to_string())
        .bind(i64::from(version))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// `owner`'s agents whose installed Slack apps in `team` were made, or
    /// last updated, from a manifest older than `version`, by name, each
    /// saying whether it is blocked at `version`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn outdated_slack_apps(
        &self,
        owner: MemberId,
        team: &TeamId,
        version: u32,
    ) -> Result<Vec<OutdatedSlackApp>> {
        let rows: Vec<(String, bool)> = sqlx::query_as(
            "SELECT a.name, COALESCE(b.manifest_blocked_version >= ?, 0) \
             FROM agent_bindings b JOIN agents a ON a.id = b.agent_id \
             WHERE a.owner_id = ? AND a.state <> 'deleted' AND b.team_id = ? \
             AND b.surface = 'slack' AND b.state = 'active' AND b.manifest_version < ? \
             ORDER BY a.name, b.rowid",
        )
        .bind(i64::from(version))
        .bind(owner.to_string())
        .bind(team.as_str())
        .bind(i64::from(version))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(agent_name, blocked)| OutdatedSlackApp {
                agent_name,
                blocked,
            })
            .collect())
    }

    /// Records that the owner of `binding` was reminded at `now`. Returns
    /// false if it was recorded already.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn mark_install_reminded(
        &self,
        binding: BindingId,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE agent_bindings SET install_reminded_at = ?, install_reminder_next_at = NULL \
             WHERE id = ? AND install_reminded_at IS NULL",
        )
        .bind(to_unix(now))
        .bind(binding.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests;

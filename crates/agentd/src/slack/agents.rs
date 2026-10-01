//! [`SlackAgents`]: agents' Slack apps, from `/agent create` to
//! `/agent delete`.
//!
//! Every agent on Slack is its own app, created from a manifest with its
//! owner's app configuration token:
//!
//! 1. [`create`](SlackAgents::create) stores the agent with a binding in
//!    state `creating`, so the ingress answers the `url_verification`
//!    challenge Slack sends for it, calls `apps.manifest.create` with a
//!    manifest whose URLs name that binding, stores the app's credentials
//!    sealed, with the scopes and the OAuth redirect URL its manifest names
//!    (`pending_install`), and DMs the owner an install link whose `state`
//!    is sealed for the binding. If creating the app fails, the creation is
//!    abandoned, which frees the name.
//! 2. The install link leads to Slack's consent page, which redirects to
//!    [`OAUTH_CALLBACK_PATH`](surface_slack::manifest::OAUTH_CALLBACK_PATH).
//!    [`callback`](SlackAgents::callback) checks the `state`, exchanges the
//!    code with `oauth.v2.access` and the app's own credentials, checks
//!    that the install granted no scope the app doesn't ask for, stores the
//!    bot token sealed, makes the binding `active` and tells the owner.
//! 3. When the workspace requires app approval, the click becomes a request
//!    and Slack never calls back, so the sweeper ([`run`](SlackAgents::run))
//!    reminds the owner once when an app still waits after
//!    `[slack] install_reminder_secs`. It also abandons creations that
//!    stopped halfway, as after a crash.
//! 4. [`delete_apps`](SlackAgents::delete_apps) deletes a deleted agent's
//!    apps with `apps.manifest.delete`, which needs the owner's
//!    configuration token; without one the owner is told to delete the app
//!    at api.slack.com.
//!
//! # Manifest updates
//!
//! An app keeps the manifest it was made from. Each installed app made
//! from a manifest older than [`MANIFEST_VERSION`] is updated with
//! `apps.manifest.update` and its owner's configuration token by the
//! sweeper, from the agent's name and the public URL and scopes the app was
//! made with, so its scopes, request URLs and install never change. Each
//! update is claimed for [`MANIFEST_UPDATE_LEASE`], an hour, so one that
//! fails is tried again an hour later, and one whose owner has no usable
//! token waits for one; registering one ends the leases. An app whose
//! scopes aren't the ones this agentd asks for isn't updated, since that
//! would take a new install. `/agent me` lists the owner's agents still on
//! an older manifest.
//!
//! # Channels that change id
//!
//! Sharing a private channel (`G…`) with another organization gives it a
//! new id, and each agent whose bot is in it gets `channel_id_changed`
//! ([`channel_id_changed`](SlackAgents::channel_id_changed)). The change is
//! recorded in the store, then settled in a task of its own, and by the
//! sweeper when that fails: Slack must confirm the new id with
//! `conversations.info` on the binding's bot token (the channel exists, its
//! id is exactly the new one, and the bot is a member), and then the
//! receiving agent's own rules on the old id move to the new one in one
//! transaction ([`Rules::move_room`]), and the workspace forgets what it
//! cached about the old id. A change Slack doesn't confirm moves nothing.
//! One that can't be confirmed for now (a rate limit, Slack unreachable) is
//! tried again every [`CHANNEL_CHANGE_RETRY`], for a day. Sessions,
//! volumes, thread counts, limit notices and message references stay under
//! the old id: agentd can't confirm that the two ids are one channel.
//!
//! agentd serves one workspace, the manager app's: agent apps are created
//! there, and an install in any other workspace is refused. No token,
//! secret or code reaches a log line, an error or a reply.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS};
use axum::response::{IntoResponse, Response};
use core_types::{
    BindingId, ConvRef, MemberId, MemberKey, SurfaceError, SurfaceKind, TeamId, Throttle, UserId,
};
use secrecy::SecretString;
use serde::Deserialize;
use store::{
    AgentBinding, AgentCreation, BindingState, ChannelIdChange, ChannelIdChangeRecord,
    ManifestUpdate, NewAgent, NewSlackApp, SlackConfigToken, Store, StoreError, Visibility,
};
use surface_slack::ChannelIdChanged;
use surface_slack::ingress::WARNING_INTERVAL;
use surface_slack::manifest::{
    AgentApp, MANIFEST_VERSION, PUBLIC_POSTING_SCOPE, agent_manifest, install_url,
};
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use super::bots::SlackBots;
use super::manager::SlackManager;
use crate::agents::CREATION_LEASE;
use crate::config::Config;
use crate::policy::Rules;

/// How often the sweeper looks for creations to abandon and reminders to
/// send.
pub const INSTALL_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// How long a claim keeps other instances from sending a reminder, and so
/// how long after a failed attempt the next one comes.
pub const REMINDER_LEASE: Duration = Duration::from_secs(10 * 60);

/// How many times the install reminder is tried.
pub const REMINDER_MAX_ATTEMPTS: u32 = 5;

/// How long creating or deleting an app may take, rate-limit waits
/// included: longer than one `apps.manifest.*` request may take
/// ([`MANIFEST_TIMEOUT`](surface_slack::web::MANIFEST_TIMEOUT)), and well
/// within [`CREATION_LEASE`], so the sweeper never abandons a creation still
/// waiting on Slack.
pub const APP_CALL_TIMEOUT: Duration = Duration::from_secs(3 * 60);

/// How long a claimed manifest update keeps other instances off its app,
/// and so how long after a failed update the next one comes.
pub const MANIFEST_UPDATE_LEASE: Duration = Duration::from_secs(60 * 60);

/// The most manifest updates one sweeper pass makes, so the install
/// reminders and channel id changes after them never wait long.
pub const MANIFEST_UPDATES_PER_PASS: u32 = 16;

/// How long after a try at settling a channel id change the next may come,
/// should that one not settle it.
pub const CHANNEL_CHANGE_RETRY: Duration = Duration::from_secs(5 * 60);

/// How long after it was received a channel id change is given up.
pub const CHANNEL_CHANGE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// The most channel id changes one binding may have waiting. Past that,
/// another is dropped: only the app's owner can send so many.
pub const MAX_CHANNEL_CHANGES_WAITING: u32 = 16;

/// The most channel id changes one sweeper pass tries.
const CHANNEL_CHANGES_PER_PASS: u32 = 64;

/// What the install callback's pages call agentd when the manager app has
/// no name.
const DEFAULT_SERVICE_NAME: &str = "agent-core";

/// Where members manage their apps.
pub const APPS_PAGE: &str = "https://api.slack.com/apps";

/// How agent apps are made, from `[slack]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAppSettings {
    /// agentd's public HTTPS URL, without a trailing slash. Without it,
    /// agent apps can't be created.
    pub public_url: Option<String>,
    /// Whether apps ask for `chat:write.public`.
    pub public_posting: bool,
    /// How long an app waits for its install before its owner is reminded.
    pub reminder_after: Duration,
    /// How many agents that aren't deleted one member may have.
    pub max_per_owner: u32,
}

impl AgentAppSettings {
    /// The settings `[slack]` and `[agents]` give.
    pub fn from_config(config: &Config) -> Self {
        Self {
            public_url: config.slack.public_url(),
            public_posting: config.slack.public_posting,
            reminder_after: config.slack.install_reminder(),
            max_per_owner: config.agents.max_per_owner,
        }
    }
}

/// What [`SlackAgents::create`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Creation {
    /// The app exists and waits for its install. The link went to the
    /// owner in a DM, unless `dm_sent` is false.
    Created {
        /// The install link.
        install_url: String,
        /// Whether the DM with the link was sent.
        dm_sent: bool,
    },
    /// The owner already has an agent of that name.
    NameTaken,
    /// The owner already has as many agents as they may.
    LimitReached,
    /// agentd's public URL isn't set, so apps can't be created.
    NoPublicUrl,
    /// The owner has no configuration token that can be used: none, or one
    /// Slack refused to renew.
    NoConfigToken,
    /// The owner's configuration token expired, and the rotator is about to
    /// renew it, as after agentd was down for a while.
    TokenRenewing,
    /// Slack refused the configuration token, which is now marked broken.
    TokenRefused,
    /// Slack refused to create the app, with this code.
    Refused(String),
    /// Something else failed, such as Slack answering with a server error
    /// or not in time; it was logged.
    Failed,
}

/// What [`SlackAgents::delete_apps`] did with one app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppDeletion {
    /// The app is deleted.
    Deleted,
    /// The app `app_id` is still there: the owner has no configuration
    /// token that can be used, or Slack refused it.
    NoToken(String),
    /// Deleting the app failed; it was logged. The app's id, if the store
    /// could say.
    Failed(Option<String>),
}

/// What one sweeper pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepPass {
    /// Creations abandoned because they stopped halfway.
    pub abandoned: usize,
    /// Owners reminded to install their agent's app.
    pub reminded: usize,
    /// Apps updated to the current manifest.
    pub updated: usize,
    /// Channel id changes settled: confirmed or not, or given up.
    pub settled: usize,
}

/// What a try at settling a channel id change did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelChange {
    /// Slack confirmed the new id, and the agent's rules on the old id,
    /// if it had any, moved to it.
    Moved {
        /// Whether the agent had rules on the old id.
        rules: bool,
    },
    /// Slack doesn't confirm the new id, or the binding isn't active; no
    /// rule moved.
    Unconfirmed,
    /// Nothing is settled yet: another try holds it, or this one couldn't
    /// ask Slack, and it is tried again later.
    Waiting,
}

/// Agents' Slack apps in the workspace agentd serves: creating them,
/// installing them, reminding their owners, and deleting them.
///
/// Cloning is cheap and shares everything.
#[derive(Clone)]
pub struct SlackAgents {
    inner: Arc<Inner>,
}

struct Inner {
    store: Store,
    manager: SlackManager,
    bots: SlackBots,
    settings: AgentAppSettings,
    notes: Throttle<(BindingId, &'static str)>,
}

impl fmt::Debug for SlackAgents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackAgents")
            .field("team", self.team())
            .field("settings", &self.inner.settings)
            .finish_non_exhaustive()
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

impl SlackAgents {
    /// Agents' apps in the manager app's workspace, over `store`, acting
    /// through `manager` and `bots`, made as `settings` say.
    pub fn new(
        store: Store,
        manager: SlackManager,
        bots: SlackBots,
        settings: AgentAppSettings,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                manager,
                bots,
                settings,
                notes: Throttle::new(WARNING_INTERVAL),
            }),
        }
    }

    /// The workspace.
    pub fn team(&self) -> &TeamId {
        &self.inner.manager.identity().team
    }

    /// The agents' bots.
    pub fn bots(&self) -> &SlackBots {
        &self.inner.bots
    }

    /// How many agents that aren't deleted one member may have.
    pub fn max_per_owner(&self) -> u32 {
        self.inner.settings.max_per_owner
    }

    /// How `user` is named in the workspace: their display name, full name
    /// or username from `users.info`, or their id if Slack can't tell.
    pub async fn display_name(&self, user: &UserId) -> String {
        match self.inner.manager.surface().api().user_info(user).await {
            Ok(found) => [
                found.profile.display_name,
                found.profile.real_name,
                found.real_name,
                found.name,
            ]
            .into_iter()
            .flatten()
            .map(|name| name.trim().to_owned())
            .find(|name| !name.is_empty())
            .unwrap_or_else(|| user.to_string()),
            Err(err) => {
                tracing::debug!(%user, error = %err, "couldn't look a member's name up");
                user.to_string()
            }
        }
    }

    /// Creates the agent `name` with `persona` for `owner`, whose identity
    /// here is `key`, and its Slack app, and DMs `key` the install link.
    ///
    /// # Errors
    ///
    /// If the store fails before anything was created at Slack. Failures
    /// after that are handled here: the app is deleted again and the
    /// creation abandoned.
    pub async fn create(
        &self,
        owner: MemberId,
        key: &MemberKey,
        name: &str,
        persona: &str,
    ) -> Result<Creation, StoreError> {
        let store = &self.inner.store;
        let Some(public_url) = self.inner.settings.public_url.as_deref() else {
            return Ok(Creation::NoPublicUrl);
        };
        let Some(token) = store
            .usable_slack_config_token(owner, self.team(), now())
            .await?
        else {
            let renewing = store
                .slack_config_token_status(owner, self.team())
                .await?
                .is_some_and(|status| !status.broken);
            return Ok(if renewing {
                Creation::TokenRenewing
            } else {
                Creation::NoConfigToken
            });
        };
        let new = NewAgent {
            owner,
            name,
            persona,
            visibility: Visibility::Public,
            surface: SurfaceKind::Slack,
            team: self.team(),
        };
        let max = self.inner.settings.max_per_owner;
        let binding = match store.create_agent(&new, max, now()).await? {
            AgentCreation::Created(_, binding) => binding,
            AgentCreation::NameTaken => return Ok(Creation::NameTaken),
            AgentCreation::LimitReached => return Ok(Creation::LimitReached),
        };
        let state = match store.install_state(binding) {
            Ok(state) => state,
            Err(err) => {
                self.abandon(binding).await;
                return Err(err);
            }
        };
        let app = AgentApp {
            name,
            public_url,
            binding,
            public_posting: self.inner.settings.public_posting,
        };
        let client = self.inner.manager.client();
        let created = tokio::time::timeout(
            APP_CALL_TIMEOUT,
            client.create_app(&token.token, &agent_manifest(&app)),
        )
        .await
        .unwrap_or_else(|_| {
            Err(SurfaceError::Transport(
                "apps.manifest.create took too long".into(),
            ))
        });
        let created = match created {
            Ok(created) => created,
            Err(err) => {
                tracing::info!(%owner, %binding, error = %err, "Slack didn't create an agent's app");
                self.abandon(binding).await;
                return Ok(match err {
                    SurfaceError::Unauthorized => {
                        self.token_refused(&token).await;
                        Creation::TokenRefused
                    }
                    SurfaceError::Api(code)
                    | SurfaceError::Forbidden(code)
                    | SurfaceError::NotFound(code) => Creation::Refused(code),
                    _ => Creation::Failed,
                });
            }
        };
        let app_id = created.app_id.clone();
        let scopes = app.scopes();
        let redirect_url = app.redirect_url();
        let install = install_url(&created.client_id, &scopes, &redirect_url, &state);
        let stored = store
            .set_slack_app(
                binding,
                &NewSlackApp {
                    app_id: created.app_id,
                    client_id: created.client_id,
                    client_secret: created.client_secret,
                    signing_secret: created.signing_secret,
                    scopes: scopes.join(","),
                    redirect_url,
                    manifest_version: MANIFEST_VERSION,
                },
                name,
                now(),
            )
            .await;
        match stored {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(%binding, "an agent's creation was abandoned while Slack created its app");
                self.delete_created(&token.token, &app_id).await;
                return Ok(Creation::Failed);
            }
            Err(err) => {
                tracing::warn!(%binding, error = %err, "couldn't store an agent's new Slack app");
                self.delete_created(&token.token, &app_id).await;
                self.abandon(binding).await;
                return Ok(Creation::Failed);
            }
        }
        tracing::info!(%owner, %binding, app_id, "created an agent's Slack app");
        let text = format!(
            "[Install {name}]({install}), your new agent, in this workspace.\n\nIf your \
             workspace requires app approval, your click sends an admin a request instead, and \
             I'll remind you if `{name}` still isn't installed in a while. Once it is installed, \
             I'll tell you how to invite it to a channel."
        );
        let dm_sent = match self.inner.manager.manager_bot().dm(key, &text).await {
            Ok(()) => true,
            Err(err) => {
                tracing::warn!(%owner, %binding, error = %err, "couldn't DM an install link");
                false
            }
        };
        Ok(Creation::Created {
            install_url: install,
            dm_sent,
        })
    }

    /// Marks `token` broken because Slack refused it, so its owner is told
    /// to register a new one and nothing uses it again.
    async fn token_refused(&self, token: &SlackConfigToken) {
        if let Err(err) = self
            .inner
            .store
            .mark_slack_config_token_broken(&token.row, now())
            .await
        {
            tracing::warn!(member = %token.row.member, error = %err, "couldn't mark a refused configuration token broken");
        }
    }

    /// Abandons the creation of `binding`, freeing the agent's name.
    async fn abandon(&self, binding: BindingId) {
        let at = now();
        if let Err(err) = self.inner.store.abandon_creation(binding, at, at).await {
            tracing::warn!(%binding, error = %err, "couldn't abandon an agent's creation; the sweeper will");
        }
    }

    /// Deletes an app created for a creation that can't go on.
    async fn delete_created(&self, token: &SecretString, app_id: &str) {
        let deleted = tokio::time::timeout(
            APP_CALL_TIMEOUT,
            self.inner.manager.client().delete_app(token, app_id),
        )
        .await;
        match deleted {
            Ok(Ok(())) => {}
            Ok(Err(err)) => {
                tracing::warn!(app_id, error = %err, "couldn't delete the Slack app of an abandoned creation");
            }
            Err(_) => tracing::warn!(
                app_id,
                "deleting the Slack app of an abandoned creation took too long"
            ),
        }
    }

    /// Deletes the Slack apps of `owner`'s deleted agent, whose `bindings`
    /// were read before it was deleted, with `apps.manifest.delete` and the
    /// owner's configuration token. Each app deleted, or found gone
    /// already, is recorded as retired. Bindings on another surface or team,
    /// without an app, or already retired are left alone.
    ///
    /// It never fails: the agent is deleted already, so whatever goes wrong
    /// for one app is logged and reported in its [`AppDeletion`], with the
    /// app's id whenever the store could read it, for the owner to delete
    /// it by hand.
    pub async fn delete_apps(
        &self,
        owner: MemberId,
        bindings: &[AgentBinding],
    ) -> Vec<AppDeletion> {
        let store = &self.inner.store;
        let mut done = Vec::new();
        for binding in bindings {
            if binding.surface != SurfaceKind::Slack
                || binding.team != *self.team()
                || binding.retired_at.is_some()
            {
                continue;
            }
            let app_id = match store.slack_app(binding.id).await {
                Ok(app) => app.and_then(|app| app.app_id),
                Err(err) => {
                    tracing::warn!(binding = %binding.id, error = %err, "couldn't read a deleted agent's Slack app");
                    done.push(AppDeletion::Failed(None));
                    continue;
                }
            };
            let Some(app_id) = app_id else {
                continue;
            };
            done.push(self.delete_app(owner, binding.id, app_id).await);
        }
        self.name_managed().await;
        done
    }

    /// Deletes the app `app_id` of the deleted agent's `binding`.
    async fn delete_app(&self, owner: MemberId, binding: BindingId, app_id: String) -> AppDeletion {
        let store = &self.inner.store;
        let token = match store
            .usable_slack_config_token(owner, self.team(), now())
            .await
        {
            Ok(Some(token)) => token,
            Ok(None) => return AppDeletion::NoToken(app_id),
            Err(err) => {
                tracing::warn!(%binding, app_id, error = %err, "couldn't read the configuration token to delete an app");
                return AppDeletion::Failed(Some(app_id));
            }
        };
        let deleted = tokio::time::timeout(
            APP_CALL_TIMEOUT,
            self.inner
                .manager
                .client()
                .delete_app(&token.token, &app_id),
        )
        .await
        .unwrap_or_else(|_| {
            Err(SurfaceError::Transport(
                "apps.manifest.delete took too long".into(),
            ))
        });
        match deleted {
            Ok(()) | Err(SurfaceError::NotFound(_)) => {
                if let Err(err) = store.mark_retired(binding, now()).await {
                    tracing::warn!(%binding, app_id, error = %err, "deleted a deleted agent's Slack app, but couldn't record it");
                } else {
                    tracing::info!(%binding, app_id, "deleted a deleted agent's Slack app");
                }
                AppDeletion::Deleted
            }
            Err(SurfaceError::Unauthorized) => {
                tracing::info!(%binding, app_id, "Slack refused the configuration token deleting an app");
                self.token_refused(&token).await;
                AppDeletion::NoToken(app_id)
            }
            Err(err) => {
                tracing::warn!(%binding, app_id, error = %err, "couldn't delete a deleted agent's Slack app");
                AppDeletion::Failed(Some(app_id))
            }
        }
    }

    /// Gives the workspace's directory its active agents' bot users
    /// ([`SlackBots::name_managed`]), logging a failure.
    pub async fn name_managed(&self) {
        if let Err(err) = self.inner.bots.name_managed().await {
            tracing::warn!(error = %err, "couldn't read the workspace's agent bots");
        }
    }

    /// The identity of `member` in this workspace, if they have one.
    async fn identity_here(&self, member: MemberId) -> Result<Option<MemberKey>, StoreError> {
        Ok(self
            .inner
            .store
            .member_identities(member)
            .await?
            .into_iter()
            .find(|key| key.surface == SurfaceKind::Slack && key.team == *self.team()))
    }

    /// Answers the OAuth redirect of an install, whose query string is
    /// `query`. See the [module docs](self).
    pub async fn callback(&self, query: Option<&str>) -> Response {
        let (status, text) = self.install(query.unwrap_or_default()).await;
        (
            status,
            [
                (CONTENT_TYPE, "text/plain; charset=utf-8"),
                (X_CONTENT_TYPE_OPTIONS, "nosniff"),
                (CACHE_CONTROL, "no-store"),
                (REFERRER_POLICY, "no-referrer"),
            ],
            text,
        )
            .into_response()
    }

    /// What agentd calls itself on the callback's pages: the manager app's
    /// name, if it has one.
    fn service_name(&self) -> &str {
        self.inner
            .manager
            .identity()
            .app_name
            .as_deref()
            .unwrap_or(DEFAULT_SERVICE_NAME)
    }

    async fn install(&self, query: &str) -> (StatusCode, String) {
        const TRY_AGAIN: &str = "Something went wrong finishing the install. Please try the link \
                                 again in a minute.";
        const NOT_WAITING: &str = "This agent isn't waiting to be installed: it is installed \
                                   already, or was deleted.";
        #[derive(Deserialize)]
        struct Callback {
            code: Option<String>,
            state: Option<String>,
            error: Option<String>,
        }
        let service = self.service_name();
        let invalid = || {
            (
                StatusCode::BAD_REQUEST,
                format!("This install link isn't valid. Use the link {service} sent you in Slack."),
            )
        };
        let page = |status: StatusCode, text: &str| (status, text.to_owned());
        let Ok(callback) = serde_urlencoded::from_str::<Callback>(query) else {
            return invalid();
        };
        let store = &self.inner.store;
        let Some(binding) = callback
            .state
            .as_deref()
            .and_then(|state| store.binding_of_install_state(state))
        else {
            tracing::debug!("refused an install callback whose state isn't agentd's");
            return invalid();
        };
        let app = match store.slack_app(binding).await {
            Ok(Some(app)) if app.team == *self.team() => app,
            Ok(_) => return invalid(),
            Err(err) => {
                tracing::warn!(%binding, error = %err, "couldn't read a binding for its install");
                return page(StatusCode::INTERNAL_SERVER_ERROR, TRY_AGAIN);
            }
        };
        if app.state != BindingState::PendingInstall {
            tracing::info!(%binding, state = app.state.as_str(), "refused an install callback for a binding that isn't waiting for one");
            return page(StatusCode::CONFLICT, NOT_WAITING);
        }
        if callback.error.is_some() {
            tracing::info!(%binding, "an install was cancelled or refused at Slack");
            return page(
                StatusCode::OK,
                "The install was cancelled. The link in your Slack DM still works.",
            );
        }
        let (Some(code), Some(app_id), Some(client_id), Some(scopes), Some(redirect)) = (
            callback.code,
            app.app_id,
            app.client_id,
            app.scopes,
            app.redirect_url,
        ) else {
            return invalid();
        };
        let secret = match store.slack_client_secret(binding).await {
            Ok(Some(secret)) => secret,
            Ok(None) => return page(StatusCode::CONFLICT, NOT_WAITING),
            Err(err) => {
                tracing::warn!(%binding, error = %err, "couldn't read an app's client secret");
                return page(StatusCode::INTERNAL_SERVER_ERROR, TRY_AGAIN);
            }
        };
        let installed = tokio::time::timeout(
            APP_CALL_TIMEOUT,
            self.inner.manager.client().install_app(
                &client_id,
                &secret,
                &SecretString::from(code),
                &redirect,
            ),
        )
        .await
        .unwrap_or_else(|_| {
            Err(SurfaceError::Transport(
                "oauth.v2.access took too long".into(),
            ))
        });
        let installed = match installed {
            Ok(installed) => installed,
            Err(err) => {
                tracing::warn!(%binding, error = %err, "Slack didn't complete an install");
                return page(
                    StatusCode::BAD_GATEWAY,
                    "Slack didn't complete the install. Please try the link again.",
                );
            }
        };
        if installed.app_id != app_id || installed.team != *self.team() {
            tracing::warn!(%binding, team = %installed.team, "refused an install of another app or in another workspace");
            return (
                StatusCode::BAD_REQUEST,
                format!("That install is for another app or workspace than {service} serves."),
            );
        }
        let asked: Vec<&str> = scopes.split(',').collect();
        if let Some(extra) = installed
            .scopes
            .iter()
            .find(|scope| !asked.contains(&scope.as_str()))
        {
            tracing::warn!(%binding, scope = extra.as_str(), "refused an install granting a scope the app doesn't ask for");
            return page(
                StatusCode::BAD_REQUEST,
                "That install granted permissions the agent's app doesn't ask for, so it wasn't \
                 stored.",
            );
        }
        match store
            .install_slack_app(
                binding,
                &app_id,
                &installed.bot_user,
                &installed.bot_token,
                now(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return page(StatusCode::CONFLICT, NOT_WAITING),
            Err(err) => {
                tracing::warn!(%binding, error = %err, "couldn't store an installed app's bot token");
                return page(StatusCode::INTERNAL_SERVER_ERROR, TRY_AGAIN);
            }
        }
        tracing::info!(%binding, app_id, bot_user = %installed.bot_user, "installed an agent's Slack app");
        self.name_managed().await;
        self.tell_installed(binding, app.agent, &installed.bot_user)
            .await;
        page(
            StatusCode::OK,
            "Installed. You can close this page and go back to Slack.",
        )
    }

    /// Tells the owner of `agent` that its app, whose bot user is `bot`, is
    /// installed. The bot is named by its user id, which the manager's
    /// renderer turns into a mention of it, since several agents can share
    /// a name.
    async fn tell_installed(&self, binding: BindingId, agent: core_types::AgentId, bot: &UserId) {
        let told = async {
            let Some(agent) = self.inner.store.agent(agent).await? else {
                return Ok(());
            };
            let Some(owner) = self.identity_here(agent.owner).await? else {
                return Ok(());
            };
            let name = &agent.name;
            let text = format!(
                "`{name}` is installed as @{bot}. To use it in a channel, invite it there (run \
                 `/invite` in the channel and pick @{bot}) and mention it: it only hears the \
                 channels it's in. You can also send it a direct message."
            );
            if let Err(err) = self.inner.manager.manager_bot().dm(&owner, &text).await {
                tracing::warn!(%binding, error = %err, "couldn't tell an owner their agent is installed");
            }
            Ok::<_, StoreError>(())
        };
        if let Err(err) = told.await {
            tracing::warn!(%binding, error = %err, "couldn't look an installed agent's owner up");
        }
    }

    /// Settles the channel id changes due, abandons the creations that
    /// stopped halfway, sends the install reminders owed and updates the
    /// apps on an older manifest.
    ///
    /// # Errors
    ///
    /// If what is due can't be listed; what was done stays done.
    pub async fn pass(&self) -> Result<SweepPass, StoreError> {
        self.pass_at(now).await
    }

    /// [`pass`](Self::pass), reading the time from `now`.
    pub async fn pass_at(&self, now: impl Fn() -> OffsetDateTime) -> Result<SweepPass, StoreError> {
        let store = &self.inner.store;
        let mut pass = SweepPass {
            settled: self.settle_channel_changes(&now).await?,
            ..SweepPass::default()
        };
        let at = now();
        let stale = at - CREATION_LEASE;
        for binding in store
            .stale_creations(SurfaceKind::Slack, self.team(), stale)
            .await?
        {
            match store.abandon_creation(binding, stale, now()).await {
                Ok(true) => {
                    tracing::warn!(%binding, "abandoned an agent's creation that stopped halfway");
                    pass.abandoned += 1;
                }
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(%binding, error = %err, "couldn't abandon a stale creation")
                }
            }
        }
        let since = at - self.inner.settings.reminder_after;
        for due in store
            .due_install_reminders(self.team(), since, at, REMINDER_MAX_ATTEMPTS)
            .await?
        {
            match self.remind(&due, since, &now).await {
                Ok(true) => pass.reminded += 1,
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(binding = %due.binding, error = %err, "sending an install reminder failed in the store");
                }
            }
        }
        for due in store
            .due_manifest_updates(
                self.team(),
                MANIFEST_VERSION,
                now(),
                MANIFEST_UPDATES_PER_PASS,
            )
            .await?
        {
            match self.update_manifest(&due, &now).await {
                Ok(true) => pass.updated += 1,
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(binding = %due.binding, error = %err, "updating an app's manifest failed in the store");
                }
            }
        }
        Ok(pass)
    }

    /// Updates the app of `due` to the current manifest, if this call
    /// claims it; true if it did.
    async fn update_manifest(
        &self,
        due: &ManifestUpdate,
        now: &impl Fn() -> OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let store = &self.inner.store;
        let binding = due.binding;
        let at = now();
        if !store
            .claim_manifest_update(binding, MANIFEST_VERSION, at, at + MANIFEST_UPDATE_LEASE)
            .await?
        {
            return Ok(false);
        }
        let Some(public_url) = AgentApp::public_url_of(&due.redirect_url) else {
            tracing::warn!(%binding, "an app's redirect URL names no public URL agentd could have made it with; not updating its manifest");
            return Ok(false);
        };
        let app = AgentApp {
            name: &due.agent_name,
            public_url,
            binding,
            public_posting: due
                .scopes
                .split(',')
                .any(|scope| scope == PUBLIC_POSTING_SCOPE),
        };
        if app.scopes().join(",") != due.scopes {
            tracing::warn!(%binding, "an app asks for other scopes than this agentd would; not updating its manifest, which would take a new install");
            return Ok(false);
        }
        let Some(token) = store
            .usable_slack_config_token(due.owner, self.team(), now())
            .await?
        else {
            return Ok(false);
        };
        let updated = tokio::time::timeout(
            APP_CALL_TIMEOUT,
            self.inner.manager.client().update_app(
                &token.token,
                &due.app_id,
                &agent_manifest(&app),
            ),
        )
        .await
        .unwrap_or_else(|_| {
            Err(SurfaceError::Transport(
                "apps.manifest.update took too long".into(),
            ))
        });
        let app_id = &due.app_id;
        match updated {
            Ok(permissions_updated) => {
                if permissions_updated {
                    tracing::warn!(%binding, app_id, "Slack says updating an app's manifest changed its permissions, which take a new install");
                }
                store
                    .set_manifest_version(binding, MANIFEST_VERSION)
                    .await?;
                tracing::info!(%binding, app_id, version = MANIFEST_VERSION, "updated an agent's Slack app to the current manifest");
                Ok(true)
            }
            Err(SurfaceError::Unauthorized) => {
                tracing::info!(%binding, app_id, "Slack refused the configuration token updating an app's manifest");
                self.token_refused(&token).await;
                Ok(false)
            }
            Err(err) => {
                tracing::warn!(%binding, app_id, error = %err, "couldn't update an agent's Slack app to the current manifest; trying again in an hour");
                Ok(false)
            }
        }
    }

    /// Takes the channel id change the app of its binding was told of:
    /// records it, and tries to settle it at once in a task of its own,
    /// which leaves it to the sweeper when it can't. A change that waits
    /// already is left to that try, and one past the binding's
    /// [`MAX_CHANNEL_CHANGES_WAITING`] is dropped.
    pub async fn channel_id_changed(&self, changed: ChannelIdChanged) {
        let binding = changed.binding;
        if !changed.old.as_str().starts_with('G') {
            self.note(binding, "an unexpected old id", |quiet| {
                tracing::warn!(%binding, old = %changed.old, new = %changed.new, since_last_warning = quiet, "a channel id change from an id that doesn't start with G; handling it all the same");
            });
        }
        let change = ChannelIdChange {
            binding,
            old: changed.old,
            new: changed.new,
            received_at: changed.received_at,
        };
        match self
            .inner
            .store
            .record_channel_id_change(&change, MAX_CHANNEL_CHANGES_WAITING)
            .await
        {
            Ok(ChannelIdChangeRecord::Recorded) => {
                let agents = self.clone();
                tokio::spawn(async move {
                    if let Err(err) = agents.settle_channel_change(&change, &now).await {
                        tracing::warn!(binding = %change.binding, error = %err, "settling a channel id change failed in the store; the sweeper tries again");
                    }
                });
            }
            Ok(ChannelIdChangeRecord::Known) => {
                tracing::debug!(%binding, "a channel id change waits already");
            }
            Ok(ChannelIdChangeRecord::Full) => {
                self.note(binding, "too many channel id changes", |quiet| {
                    tracing::warn!(%binding, waiting = MAX_CHANNEL_CHANGES_WAITING, dropped_since_last_warning = quiet, "dropped a channel id change: the binding has as many waiting as it may");
                });
            }
            Err(err) => {
                tracing::warn!(%binding, error = %err, "couldn't record a channel id change; dropped it");
            }
        }
    }

    /// Calls `log` with how many went quiet when `binding`'s `note` is due,
    /// at most once per [`WARNING_INTERVAL`] for each.
    fn note(&self, binding: BindingId, note: &'static str, log: impl FnOnce(u64)) {
        match self
            .inner
            .notes
            .record((binding, note), std::time::Instant::now())
        {
            Some(quiet) => log(quiet),
            None => tracing::debug!(%binding, note, "a Slack agent app note went quiet"),
        }
    }

    /// Gives up the channel id changes received over
    /// [`CHANNEL_CHANGE_TTL`] ago, and tries to settle those due. Returns
    /// how many were settled or given up.
    async fn settle_channel_changes(
        &self,
        now: &impl Fn() -> OffsetDateTime,
    ) -> Result<usize, StoreError> {
        let store = &self.inner.store;
        let stale = store
            .drop_stale_channel_id_changes(now() - CHANNEL_CHANGE_TTL)
            .await?;
        for change in &stale {
            tracing::warn!(binding = %change.binding, old = %change.old, new = %change.new, "gave up a channel id change Slack couldn't be asked about for a day; the agent's rules on the old id stay there");
        }
        let mut settled = stale.len();
        for change in store
            .due_channel_id_changes(now(), CHANNEL_CHANGES_PER_PASS)
            .await?
        {
            match self.settle_channel_change(&change, now).await {
                Ok(ChannelChange::Waiting) => {}
                Ok(_) => settled += 1,
                Err(err) => {
                    tracing::warn!(binding = %change.binding, error = %err, "settling a channel id change failed in the store; trying again later");
                }
            }
        }
        Ok(settled)
    }

    /// Tries to settle `change`, if this call claims the try: asks Slack
    /// to confirm the new id with the binding's bot token, and if it does,
    /// moves the agent's rules on the old id to it.
    ///
    /// # Errors
    ///
    /// If the store fails; the change is then tried again later.
    pub async fn settle_channel_change(
        &self,
        change: &ChannelIdChange,
        now: &impl Fn() -> OffsetDateTime,
    ) -> Result<ChannelChange, StoreError> {
        let store = &self.inner.store;
        let binding = change.binding;
        let at = now();
        if !store
            .claim_channel_id_change(change, at, at + CHANNEL_CHANGE_RETRY)
            .await?
        {
            return Ok(ChannelChange::Waiting);
        }
        let row = store.binding(binding).await?;
        let surface = self.inner.bots.surface(binding).await?;
        let (Some(row), Some(surface)) = (row, surface) else {
            tracing::info!(%binding, "a channel id change for an app that isn't active; dropped it");
            store.finish_channel_id_change(change).await?;
            return Ok(ChannelChange::Unconfirmed);
        };
        let (old, new) = (&change.old, &change.new);
        match surface.confirms_channel(new).await {
            Ok(true) => {}
            Ok(false) => {
                self.note(binding, "an unconfirmed channel id change", |quiet| {
                    tracing::warn!(%binding, %old, %new, since_last_warning = quiet, "Slack doesn't confirm a channel id change: no such channel under the new id with the agent's bot in it; moved no rules");
                });
                store.finish_channel_id_change(change).await?;
                return Ok(ChannelChange::Unconfirmed);
            }
            Err(err) => {
                self.note(binding, "a channel id change waiting for Slack", |quiet| {
                    tracing::warn!(%binding, %old, %new, error = %err, since_last_warning = quiet, "couldn't ask Slack to confirm a channel id change; trying again later");
                });
                return Ok(ChannelChange::Waiting);
            }
        }
        let room = |conversation: &core_types::ConversationId| ConvRef {
            surface: SurfaceKind::Slack,
            team: row.team.clone(),
            conversation: conversation.clone(),
        };
        let (from, to) = (room(old), room(new));
        let moved = store
            .update_agent_settings(row.agent, |settings| {
                let mut rules = Rules::read(settings).ok()?;
                let moved = rules.move_room(&from, &to);
                if moved {
                    rules.write(settings);
                }
                Some(moved)
            })
            .await?;
        surface.directory().forget_conv(old);
        store.finish_channel_id_change(change).await?;
        match moved {
            Some(true) => {
                tracing::info!(%binding, agent = %row.agent, %old, %new, "a channel changed its id; moved the agent's rules on it to the new id");
            }
            Some(false) => {
                tracing::debug!(%binding, %old, %new, "a channel changed its id; the agent had no rules on it");
            }
            None => {
                tracing::warn!(%binding, agent = %row.agent, %old, %new, "a channel changed its id, but the agent's rules don't read, so none moved; they refuse everyone until its owner sets them again");
            }
        }
        Ok(ChannelChange::Moved {
            rules: moved == Some(true),
        })
    }

    /// Sends the reminder `due`, if its owner is reachable here; true if it
    /// was sent.
    async fn remind(
        &self,
        due: &store::InstallReminder,
        since: OffsetDateTime,
        now: &impl Fn() -> OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let store = &self.inner.store;
        let Some(owner) = self.identity_here(due.owner).await? else {
            tracing::debug!(binding = %due.binding, "an install reminder's owner has no identity here");
            return Ok(false);
        };
        let state = store.install_state(due.binding)?;
        let scopes: Vec<&str> = due.scopes.split(',').collect();
        let link = install_url(&due.client_id, &scopes, &due.redirect_url, &state);
        let at = now();
        let Some(attempt) = store
            .claim_install_reminder(
                due.binding,
                since,
                at,
                at + REMINDER_LEASE,
                REMINDER_MAX_ATTEMPTS,
            )
            .await?
        else {
            return Ok(false);
        };
        let name = &due.agent_name;
        let text = format!(
            "`{name}` still isn't installed. If your workspace requires app approval, an admin has \
             to approve it first; once they have, or if you haven't tried yet, \
             [install {name}]({link})."
        );
        match self.inner.manager.manager_bot().dm(&owner, &text).await {
            Ok(()) => {
                store.mark_install_reminded(due.binding, now()).await?;
                tracing::info!(binding = %due.binding, "sent an install reminder");
                Ok(true)
            }
            Err(err) => {
                tracing::warn!(binding = %due.binding, attempt, error = %err, "couldn't send an install reminder");
                Ok(false)
            }
        }
    }

    /// Runs a pass now and then every `every`, until `stopping` becomes
    /// true or its sender is dropped. A pass in progress finishes first.
    pub async fn run(self, every: Duration, mut stopping: watch::Receiver<bool>) {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = stopping.wait_for(|stop| *stop) => break,
                _ = ticks.tick() => {}
            }
            match self.pass().await {
                Ok(pass) if pass == SweepPass::default() => {}
                Ok(pass) => tracing::debug!(
                    abandoned = pass.abandoned,
                    reminded = pass.reminded,
                    "Slack agent app pass"
                ),
                Err(err) => tracing::warn!(error = %err, "the Slack agent app pass failed"),
            }
        }
    }
}

/// `GET /slack/oauth/callback`: an agent app's install redirect.
pub async fn oauth_callback(
    State(agents): State<SlackAgents>,
    RawQuery(query): RawQuery,
) -> Response {
    agents.callback(query.as_deref()).await
}

#[cfg(test)]
mod tests;

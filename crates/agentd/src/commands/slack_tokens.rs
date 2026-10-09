//! Slack app configuration tokens: `/agent slack-token`, and the
//! [`ConfigTokenRotator`] that renews them.
//!
//! Slack issues configuration tokens only in the api.slack.com UI, for 12
//! hours, with a refresh token. A member hands both to agentd with
//! `/agent slack-token <token> <refresh token>`, and agentd uses the token
//! to create and edit their agent apps (T31). The command checks them by
//! rotating at once with `tooling.tokens.rotate`, which proves the refresh
//! token works and gives a fresh pair, then stores that pair sealed. Only a
//! linked member on Slack may register one, and only one that Slack says
//! is their own in the workspace they sent it from.
//!
//! The rotator renews each token when it has less than [`RENEW_BEFORE`]
//! left. When Slack refuses a refresh token, the token is marked broken
//! and the member gets one DM from the manager app, tried at most
//! [`NOTICE_MAX_ATTEMPTS`] times. Any other failure, a renewal that takes
//! longer than [`ROTATE_TIMEOUT`] included, is retried once the claim's
//! [`ROTATION_LEASE`] ends. A rotated pair's store write is tried a few
//! times, since the rotation used up the refresh token it replaces. Tokens
//! are deleted by `/agent logout`, and when the member leaves the workspace
//! (a `user_change` event whose user is `deleted`).
//!
//! Neither token ever reaches a log line, an error or a reply.

use std::fmt;
use std::time::Duration;

use core_types::{MemberKey, SurfaceError, SurfaceKind};
use secrecy::SecretString;
use store::{NewSlackConfigToken, SlackConfigTokenRef, Store, StoreError};
use surface_slack::{ConfigToken, SlackClient};
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use super::{Commands, Failure, Origin, Replies, now};

/// Tokens with less than this left are renewed.
pub const RENEW_BEFORE: Duration = Duration::from_secs(2 * 60 * 60);

/// How often the rotator looks for tokens to renew and notices to send.
pub const ROTATION_INTERVAL: Duration = Duration::from_secs(60);

/// How long a claim keeps other instances off a token while one renews it.
/// A renewal that failed for a reason other than a refused refresh token
/// is tried again once it ends.
pub const ROTATION_LEASE: Duration = Duration::from_secs(5 * 60);

/// How long a renewal waits on Slack, rate limits included, before it
/// gives up until the lease ends. Well within [`ROTATION_LEASE`], so that
/// the new pair is stored before another instance may claim the token.
pub const ROTATE_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// How many times `retry_store` tries store work before it gives up.
pub const STORE_ATTEMPTS: u32 = 4;

/// The wait before `retry_store`'s second try, doubled before each one
/// after it.
const STORE_RETRY_WAIT: Duration = Duration::from_millis(250);

/// How long a claim keeps other instances from sending a notice, and so how
/// long after a failed attempt the next one comes.
pub const NOTICE_LEASE: Duration = Duration::from_secs(10 * 60);

/// How many times the notice about a refused refresh token is tried.
pub const NOTICE_MAX_ATTEMPTS: u32 = 20;

/// Where members generate configuration tokens.
const TOKENS_PAGE: &str = "https://api.slack.com/apps";

/// The DM a member gets when Slack refused to renew their configuration
/// token.
pub fn broken_token_notice() -> String {
    format!(
        "Slack refused to renew your configuration token, so I can no longer create or change \
         agent apps as you. Generate a new one at {TOKENS_PAGE} (\"Your App Configuration \
         Tokens\") and send it with `/agent slack-token <token> <refresh token>`."
    )
}

impl Commands {
    /// `slack-token`: checks `refresh` by rotating it, and stores the new
    /// pair for `key`'s workspace.
    pub(super) async fn slack_token(
        &self,
        key: &MemberKey,
        refresh: &SecretString,
        origin: &Origin,
    ) -> Result<String, Failure> {
        let Some(slack) = &self.inner.slack else {
            return Ok("Slack isn't set up on this agentd.".to_owned());
        };
        if key.surface != SurfaceKind::Slack {
            return Ok(
                "`slack-token` registers a Slack configuration token, so send it on Slack, with \
                 `/agent slack-token <token> <refresh token>`."
                    .to_owned(),
            );
        }
        let member = match self.member(key).await? {
            Some(member) if self.inner.auth.status(member).await?.linked => member,
            _ => {
                return Ok(format!(
                    "Link your Claude account first with {}, then send the token again. I \
                     didn't use it.",
                    origin.command("login")
                ));
            }
        };
        let rotated = match slack.client().rotate_config_token(refresh).await {
            Ok(rotated) => rotated,
            Err(SurfaceError::Unauthorized) => {
                tracing::info!(%member, "Slack refused a configuration refresh token");
                return Ok(format!(
                    "Slack didn't accept that refresh token. Generate a new configuration token \
                     at {TOKENS_PAGE} (\"Your App Configuration Tokens\") and send the token and \
                     its refresh token again."
                ));
            }
            Err(err) => {
                tracing::warn!(%member, error = %err, "couldn't check a configuration token");
                return Ok(
                    "I couldn't reach Slack to check that token. Please try again in a minute."
                        .to_owned(),
                );
            }
        };
        if rotated.team != key.team || rotated.user != key.user {
            tracing::info!(%member, "refused a configuration token of another member or workspace");
            return Ok(format!(
                "That configuration token belongs to another member or workspace, so I didn't \
                 keep it. Checking it used up its refresh token: generate a new one of your own \
                 at {TOKENS_PAGE} and send it from this workspace."
            ));
        }
        let rotated = stored(rotated);
        let kept = retry_store("storing a checked configuration token", member, || {
            self.inner
                .store
                .put_slack_config_token(member, &key.team, &rotated, now())
        })
        .await;
        if let Err(err) = kept {
            tracing::warn!(%member, error = %err, "couldn't store a checked configuration token");
            return Ok(format!(
                "I couldn't save that configuration token, and checking it used up its refresh \
                 token. Generate a new one at {TOKENS_PAGE} and send it again in a few minutes."
            ));
        }
        tracing::info!(%member, team = %key.team, "registered a Slack configuration token");
        Ok(format!(
            "Your Slack configuration token is registered. I renew it before it expires, and \
             use it only to create and change your agent apps. {} deletes it.",
            origin.command("logout"),
        ))
    }

    /// The `me` line about `key`'s configuration token in their workspace.
    pub(super) async fn slack_token_status(
        &self,
        key: &MemberKey,
        origin: &Origin,
    ) -> Result<String, Failure> {
        let register = origin.command("slack-token <token> <refresh token>");
        let status = match self.member(key).await? {
            Some(member) => {
                self.inner
                    .store
                    .slack_config_token_status(member, &key.team)
                    .await?
            }
            None => None,
        };
        Ok(match status {
            None => format!(
                "Slack configuration token: not registered. Send {register} to let me create \
                 agent apps as you."
            ),
            Some(status) if status.broken => format!(
                "Slack configuration token: Slack refused to renew it. Send a new one with \
                 {register}."
            ),
            Some(status) if status.expires_at <= now() => format!(
                "Slack configuration token: expired, because renewing it keeps failing. I keep \
                 trying; if this lasts, send a new one with {register}."
            ),
            Some(_) => "Slack configuration token: registered, renewed automatically.".to_owned(),
        })
    }
}

/// Runs `op` until it succeeds or has failed [`STORE_ATTEMPTS`] times,
/// waiting a little longer after each failure, for store work a passing
/// error mustn't lose: a pair `tooling.tokens.rotate` returned, which
/// used up the old refresh token, or the deletion of a departed member's
/// token, whose Slack event isn't delivered again. Each failure but the
/// last is logged with `what`, naming the work, and `member`, an id of
/// whose it is; the last is returned for the caller to log.
pub(crate) async fn retry_store<T, F>(
    what: &'static str,
    member: impl fmt::Display,
    mut op: impl FnMut() -> F,
) -> Result<T, StoreError>
where
    F: Future<Output = Result<T, StoreError>>,
{
    let mut attempt = 1;
    let mut wait = STORE_RETRY_WAIT;
    loop {
        match op().await {
            Err(err) if attempt < STORE_ATTEMPTS => {
                tracing::warn!(what, %member, attempt, error = %err, "a store operation failed; trying again");
                tokio::time::sleep(wait).await;
                attempt += 1;
                wait *= 2;
            }
            result => return result,
        }
    }
}

fn stored(token: ConfigToken) -> NewSlackConfigToken {
    NewSlackConfigToken {
        token: token.token,
        refresh_token: token.refresh_token,
        expires_at: token.expires_at,
    }
}

/// What one [`ConfigTokenRotator`] pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RotationPass {
    /// Tokens renewed.
    pub renewed: usize,
    /// Tokens whose refresh token Slack refused.
    pub broken: usize,
    /// Members told that their token broke.
    pub notified: usize,
}

/// What renewing one token did.
enum Renewal {
    Renewed,
    Broken,
    Nothing,
}

/// Renews configuration tokens before they expire, and tells members whose
/// refresh token Slack refused.
#[derive(Debug, Clone)]
pub struct ConfigTokenRotator {
    store: Store,
    client: SlackClient,
    replies: Replies,
}

impl ConfigTokenRotator {
    /// A rotator over `store`, renewing with `client` and sending notices
    /// through `replies`.
    pub fn new(store: Store, client: SlackClient, replies: Replies) -> Self {
        Self {
            store,
            client,
            replies,
        }
    }

    /// Renews every token that is due and sends every notice owed.
    ///
    /// A store failure on one token is logged and the pass goes on with
    /// the next, so a row that can't be read doesn't hold the others up.
    ///
    /// # Errors
    ///
    /// A [`StoreError`] if the tokens due or the notices owed can't be
    /// listed; what was done stays done.
    pub async fn pass(&self) -> Result<RotationPass, StoreError> {
        self.pass_at(OffsetDateTime::now_utc).await
    }

    /// [`pass`](Self::pass), reading the time from `now`.
    pub(super) async fn pass_at(
        &self,
        now: impl Fn() -> OffsetDateTime,
    ) -> Result<RotationPass, StoreError> {
        let mut pass = RotationPass::default();
        let at = now();
        for row in self
            .store
            .due_slack_config_tokens(at + RENEW_BEFORE, at)
            .await?
        {
            match self.renew(&row, &now).await {
                Ok(Renewal::Renewed) => pass.renewed += 1,
                Ok(Renewal::Broken) => pass.broken += 1,
                Ok(Renewal::Nothing) => {}
                Err(err) => {
                    tracing::warn!(member = %row.member, error = %err, "renewing a configuration token failed in the store");
                }
            }
        }
        let at = now();
        for row in self
            .store
            .pending_slack_config_token_notices(at, NOTICE_MAX_ATTEMPTS)
            .await?
        {
            match self.notify(&row, &now).await {
                Ok(true) => pass.notified += 1,
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(member = %row.member, error = %err, "sending a configuration token notice failed in the store");
                }
            }
        }
        Ok(pass)
    }

    /// Claims `row` and renews it.
    async fn renew(
        &self,
        row: &SlackConfigTokenRef,
        now: &impl Fn() -> OffsetDateTime,
    ) -> Result<Renewal, StoreError> {
        let claimed_at = now();
        let Some(token) = self
            .store
            .claim_slack_config_token(row, claimed_at, claimed_at + ROTATION_LEASE)
            .await?
        else {
            return Ok(Renewal::Nothing);
        };
        let rotation = tokio::time::timeout(
            ROTATE_TIMEOUT,
            self.client.rotate_config_token(&token.refresh_token),
        )
        .await
        .unwrap_or_else(|_| {
            Err(SurfaceError::Transport(
                "tooling.tokens.rotate took too long".into(),
            ))
        });
        match rotation {
            Ok(rotated) => {
                let rotated = stored(rotated);
                let kept = retry_store("storing a renewed configuration token", row.member, || {
                    self.store
                        .update_rotated_slack_config_token(row, &rotated, now())
                })
                .await?;
                if kept.is_some() {
                    return Ok(Renewal::Renewed);
                }
                tracing::info!(member = %row.member, "the configuration token was replaced or deleted while it was renewed; dropped the new pair");
            }
            Err(SurfaceError::Unauthorized) => {
                tracing::warn!(member = %row.member, team = %row.team, "Slack refused to renew a configuration token");
                if self
                    .store
                    .mark_slack_config_token_broken(row, now())
                    .await?
                {
                    return Ok(Renewal::Broken);
                }
            }
            Err(err) => {
                tracing::warn!(member = %row.member, error = %err, "renewing a configuration token failed; trying again later");
            }
        }
        Ok(Renewal::Nothing)
    }

    /// Sends the notice owed for `row`, if a manager bot reaches its
    /// member in that workspace; true if it was sent.
    async fn notify(
        &self,
        row: &SlackConfigTokenRef,
        now: &impl Fn() -> OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let identity = self
            .store
            .member_identities(row.member)
            .await?
            .into_iter()
            .find(|key| {
                key.surface == SurfaceKind::Slack
                    && key.team == row.team
                    && self.replies.can_dm(key)
            });
        let Some(identity) = identity else {
            tracing::debug!(member = %row.member, "no manager bot reaches this member; the token notice waits");
            return Ok(false);
        };
        let claimed_at = now();
        let Some(attempt) = self
            .store
            .claim_slack_config_token_notice(
                row,
                claimed_at,
                claimed_at + NOTICE_LEASE,
                NOTICE_MAX_ATTEMPTS,
            )
            .await?
        else {
            return Ok(false);
        };
        match self.replies.dm(&identity, &broken_token_notice()).await {
            Ok(()) => {
                self.store
                    .mark_slack_config_token_notice_sent(row, now())
                    .await?;
                tracing::info!(member = %row.member, "sent the configuration token notice");
                Ok(true)
            }
            Err(err) => {
                tracing::warn!(member = %row.member, attempt, error = %err, "couldn't send the configuration token notice");
                if attempt >= NOTICE_MAX_ATTEMPTS {
                    tracing::warn!(member = %row.member, attempts = attempt, "giving up on the configuration token notice");
                }
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
                Ok(pass) if pass == RotationPass::default() => {}
                Ok(pass) => tracing::debug!(
                    renewed = pass.renewed,
                    broken = pass.broken,
                    notified = pass.notified,
                    "configuration token pass"
                ),
                Err(err) => tracing::warn!(error = %err, "renewing configuration tokens failed"),
            }
        }
    }
}

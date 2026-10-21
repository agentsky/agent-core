//! `/agent` command dispatch, and the account commands.
//!
//! Every surface turns a command into `(MemberKey, text, Origin)` and hands
//! it to [`Commands::handle_text`], which parses it with
//! [`commands::parse`] and runs it. The reply always goes privately to the
//! member through [`Replies::reply_private`], wherever the command came from.
//!
//! - [`rocketchat`]: which Rocket.Chat messages are commands. A DM to the
//!   manager bot is a command as a whole; a message elsewhere is one when it
//!   starts with `!agent`.
//! - [`relink`]: the notice a member gets, once, when their Claude link
//!   breaks.
//! - [`reply`]: private replies through each surface's manager bot.
//!
//! # Secrets
//!
//! A secret-bearing command (`login <code>`, `admin api-key set`,
//! `slack-token`) is refused when it comes from a room others can read
//! ([`Origin::is_private`] is false), and the member is told privately that
//! the secret is now public: a login code cancels the member's pending
//! logins and the one its `state` names, whoever started it; an API key or
//! token has to be revoked. Text that fails to parse but looks
//! secret-bearing gets the same treatment. Commands are logged by name only,
//! never with their text or arguments.

pub mod relink;
pub mod reply;
pub mod rocketchat;

#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::Arc;

use auth::{Auth, AuthError, LinkStatus, PENDING_LOGIN_TTL, Plan};
use commands::{AdminCommand, ApiKeyCommand, Command, ParseError};
use core_types::{ConversationId, MemberId, MemberKey, SurfaceKind};
use secrecy::SecretString;
use store::{Store, StoreError};
use time::OffsetDateTime;

pub use reply::{ManagerBot, OpenDm, Replies, ReplyError};

/// Where a command came from. It decides where the reply goes and whether
/// the command may carry a secret.
pub enum Origin {
    /// A Slack slash command. Its text isn't posted to the channel, so it
    /// is private. The reply goes to `response_url` (T30).
    SlackSlash {
        /// Where Slack takes the ephemeral reply. Anyone holding it can post
        /// there for a while, so it is kept secret.
        response_url: SecretString,
    },
    /// A direct message with the Rocket.Chat manager bot, in `room`.
    RocketChatDm {
        /// The DM's room.
        room: ConversationId,
    },
    /// An `!agent` message in any other Rocket.Chat room, which others can
    /// read.
    RocketChatChannel {
        /// The room.
        room: ConversationId,
    },
}

impl fmt::Debug for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SlackSlash { .. } => f.write_str("SlackSlash"),
            Self::RocketChatDm { room } => {
                f.debug_struct("RocketChatDm").field("room", room).finish()
            }
            Self::RocketChatChannel { room } => f
                .debug_struct("RocketChatChannel")
                .field("room", room)
                .finish(),
        }
    }
}

impl Origin {
    /// Whether only the member (and the manager bot) can read what they sent.
    pub fn is_private(&self) -> bool {
        !matches!(self, Self::RocketChatChannel { .. })
    }

    /// A short name for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SlackSlash { .. } => "slack_slash",
            Self::RocketChatDm { .. } => "rocketchat_dm",
            Self::RocketChatChannel { .. } => "rocketchat_channel",
        }
    }

    /// How the member types `command` where their reply lands: after
    /// `/agent` on Slack, bare in the manager bot's DM on Rocket.Chat.
    fn command(&self, command: &str) -> String {
        match self {
            Self::SlackSlash { .. } => format!("`/agent {command}`"),
            Self::RocketChatDm { .. } | Self::RocketChatChannel { .. } => format!("`{command}`"),
        }
    }

    /// Where the member should send secrets: the place their reply lands.
    fn private_place(&self) -> &'static str {
        match self {
            Self::SlackSlash { .. } => "with the `/agent` command, which no one else sees",
            Self::RocketChatDm { .. } | Self::RocketChatChannel { .. } => {
                "here, in this direct message"
            }
        }
    }
}

/// The reply when something on agentd's side failed. The cause is logged.
const FAILED: &str = "Something went wrong on my side. Please try again in a minute.";

/// Runs `/agent` commands and sends their replies.
///
/// Cloning is cheap and shares everything.
#[derive(Debug, Clone)]
pub struct Commands {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: Store,
    auth: Arc<Auth>,
    replies: Replies,
}

/// Why a handler couldn't produce its reply. Logged, never shown.
#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Auth(#[from] AuthError),
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

impl Commands {
    /// Commands over `store` and `auth`, replying through `replies`.
    pub fn new(store: Store, auth: Arc<Auth>, replies: Replies) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                auth,
                replies,
            }),
        }
    }

    /// The private reply plumbing.
    pub fn replies(&self) -> &Replies {
        &self.inner.replies
    }

    /// Parses `text` from `member` and runs it, replying privately. Text
    /// that doesn't parse gets the parser's message (help or usage).
    pub async fn handle_text(&self, member: &MemberKey, text: &str, origin: &Origin) {
        match commands::parse(text) {
            Ok(command) => self.dispatch(member, command, origin).await,
            Err(err) => {
                tracing::info!(
                    %member,
                    origin = origin.kind(),
                    kind = ?err.kind(),
                    secret_bearing = err.is_secret_bearing(),
                    "command text didn't parse"
                );
                let reply = self.unparsed(member, &err, origin).await;
                self.reply(member, origin, &reply).await;
            }
        }
    }

    /// Runs `command` from `member` and replies privately.
    pub async fn dispatch(&self, member: &MemberKey, command: Command, origin: &Origin) {
        tracing::info!(
            %member,
            origin = origin.kind(),
            command = command.name(),
            "running a command"
        );
        let reply = self.run(member, command, origin).await;
        self.reply(member, origin, &reply).await;
    }

    async fn reply(&self, member: &MemberKey, origin: &Origin, text: &str) {
        if let Err(err) = self.inner.replies.reply_private(member, origin, text).await {
            tracing::warn!(%member, origin = origin.kind(), error = %err, "couldn't send a command reply");
        }
    }

    /// The reply to `command`.
    async fn run(&self, member: &MemberKey, command: Command, origin: &Origin) -> String {
        let name = command.name();
        let result = if command.is_secret_bearing() && !origin.is_private() {
            Ok(self.refuse_public_secret(member, &command, origin).await)
        } else {
            match command {
                Command::Login { code: None } => self.login_start(member, origin).await,
                Command::Login { code: Some(code) } => {
                    self.login_complete(member, &code, origin).await
                }
                Command::Logout => self.logout(member, origin).await,
                Command::Me => self.me(member, origin).await,
                _ => Ok(format!("`{name}` isn't available yet.")),
            }
        };
        result.unwrap_or_else(|err| {
            tracing::warn!(%member, command = name, error = %err, "a command failed");
            FAILED.to_owned()
        })
    }

    async fn member(&self, key: &MemberKey) -> Result<Option<MemberId>, Failure> {
        Ok(self.inner.store.member_for_identity(key).await?)
    }

    async fn login_start(&self, key: &MemberKey, origin: &Origin) -> Result<String, Failure> {
        let member = self
            .inner
            .store
            .ensure_member(key, key.user.as_str(), now())
            .await?;
        let start = self.inner.auth.start_login(member).await?;
        Ok(format!(
            "To link your Claude account, open this link and approve:\n{}\n\n\
             The page then shows a code. Send it {} as {}. The link works once, \
             for {} minutes.",
            start.url,
            origin.private_place(),
            origin.command("login <code>"),
            PENDING_LOGIN_TTL.as_secs() / 60,
        ))
    }

    async fn login_complete(
        &self,
        key: &MemberKey,
        code: &SecretString,
        origin: &Origin,
    ) -> Result<String, Failure> {
        let again = format!("Start again with {}.", origin.command("login"));
        let Some(member) = self.member(key).await? else {
            return Ok(format!("No login is waiting for a code. {again}"));
        };
        let reply = match self.inner.auth.complete_login(member, code).await {
            Ok(linked) => {
                let plan = linked
                    .plan
                    .and_then(|info| info.plan)
                    .map_or_else(String::new, |plan| format!(" Plan: {}.", plan_name(&plan)));
                format!("Your Claude account is linked.{plan}")
            }
            Err(AuthError::MalformedCode) => format!(
                "That isn't a login code. Send the whole code the page shows, with the `#` in \
                 the middle, as {}.",
                origin.command("login <code>")
            ),
            Err(AuthError::UnknownLogin) => format!("No login is waiting for that code. {again}"),
            Err(AuthError::LoginExpired) => format!("That login expired. {again}"),
            Err(AuthError::CodeRejected { .. }) => {
                format!("Anthropic didn't accept that code. {again}")
            }
            Err(err) => {
                tracing::warn!(%member, error = %err, "couldn't complete a login");
                format!("I couldn't finish linking your account. {again}")
            }
        };
        Ok(reply)
    }

    async fn logout(&self, key: &MemberKey, origin: &Origin) -> Result<String, Failure> {
        let unlinked = match self.member(key).await? {
            Some(member) => {
                self.inner.store.invalidate_pending_logins(member).await?;
                self.inner.auth.logout(member).await?
            }
            None => false,
        };
        Ok(if unlinked {
            format!(
                "Your Claude account is unlinked. Send {} to link one again.",
                origin.command("login")
            )
        } else {
            "No Claude account is linked.".to_owned()
        })
    }

    async fn me(&self, key: &MemberKey, origin: &Origin) -> Result<String, Failure> {
        let status = match self.member(key).await? {
            Some(member) => self.inner.auth.status(member).await?,
            None => LinkStatus::default(),
        };
        let login = origin.command("login");
        Ok(match status {
            LinkStatus { linked: false, .. } => {
                format!("Claude account: not linked. Send {login} to link one.")
            }
            LinkStatus { broken: true, .. } => format!(
                "Claude account: linked, but it stopped working. Send {login} to link it again."
            ),
            LinkStatus { plan, .. } => match plan.plan {
                Some(plan) => format!("Claude account: linked. Plan: {}.", plan_name(&plan)),
                None => "Claude account: linked. Plan: unknown.".to_owned(),
            },
        })
    }

    /// The reply to a secret-bearing `command` sent where others can read
    /// it. The secret isn't used. A store failure while cancelling logins is
    /// logged, and the member is still told the secret is public.
    async fn refuse_public_secret(
        &self,
        key: &MemberKey,
        command: &Command,
        origin: &Origin,
    ) -> String {
        let place = origin.private_place();
        let advice = match command {
            Command::Login { code: Some(code) } => {
                let own = self.cancel_pending_logins(key).await;
                let pasted = match self.inner.auth.cancel_pasted_login(code).await {
                    Ok(true) => {
                        tracing::info!(
                            member = %key,
                            "cancelled the pending login a public code belongs to"
                        );
                        true
                    }
                    Ok(false) => false,
                    Err(err) => {
                        tracing::warn!(
                            member = %key,
                            error = %err,
                            "couldn't cancel the pending login a public code belongs to"
                        );
                        false
                    }
                };
                let and_cancelled = match (own.is_some_and(|count| count > 0), pasted) {
                    (true, true) => " and cancelled your pending login and the one it belongs to",
                    (true, false) => " and cancelled your pending login",
                    (false, true) => " and cancelled the pending login it belongs to",
                    (false, false) => "",
                };
                format!(
                    "That login code is no longer secret, so I didn't use it{and_cancelled}. \
                     Start again with {}, and send the code only {place}.",
                    origin.command("login")
                )
            }
            Command::Admin(AdminCommand::ApiKey(ApiKeyCommand::Set { .. })) => format!(
                "That API key is no longer secret, so I didn't store it. Revoke it in the \
                 Anthropic Console now, create a new one, and set that one {place}."
            ),
            Command::SlackToken { .. } => format!(
                "That token is no longer secret, so I didn't store it. Revoke it at \
                 api.slack.com now, and send a new one only {place}."
            ),
            Command::Login { code: None }
            | Command::Logout
            | Command::Me
            | Command::Create { .. }
            | Command::Persona { .. }
            | Command::Skill(_)
            | Command::Allow { .. }
            | Command::Deny { .. }
            | Command::Limits { .. }
            | Command::Pause { .. }
            | Command::Resume { .. }
            | Command::Delete { .. }
            | Command::Sessions { .. }
            | Command::Reset { .. }
            | Command::List { .. }
            | Command::Admin(
                AdminCommand::ApiKey(ApiKeyCommand::Clear)
                | AdminCommand::Ban { .. }
                | AdminCommand::Unban { .. }
                | AdminCommand::Slack,
            )
            | Command::Approve { .. }
            | Command::Decline { .. } => format!(
                "Whatever secret it held is no longer secret, so I didn't use it. Revoke it now, \
                 and send a new one only {place}."
            ),
        };
        format!(
            "You posted a secret in a room others can read. {advice} You may also want to \
             delete your message there."
        )
    }

    /// The reply to text that didn't parse. If it looks like it held a
    /// secret and others could read it, the member's pending logins are
    /// cancelled and they are told to revoke what they posted.
    async fn unparsed(&self, key: &MemberKey, err: &ParseError, origin: &Origin) -> String {
        if !err.is_secret_bearing() || origin.is_private() {
            return err.to_string();
        }
        let cancelled = if self.cancel_pending_logins(key).await.is_some() {
            "I cancelled any pending login, so "
        } else {
            ""
        };
        format!(
            "Your message looked like it held a secret (a login code, an API key or a token), \
             and others can read the room you posted it in. If it did, that secret is no longer \
             private: {cancelled}start again with {}, and revoke any key or token you posted. \
             Send secrets only {}.\n\n{err}",
            origin.command("login"),
            origin.private_place(),
        )
    }

    /// Cancels the pending logins of the member `key` belongs to, after a
    /// secret was posted publicly. A store failure is logged rather than
    /// returned, so the member still hears that the secret is public.
    /// Returns how many pending logins it cancelled, or `None` if the
    /// cancellation didn't go through.
    async fn cancel_pending_logins(&self, key: &MemberKey) -> Option<u64> {
        let cancelled = async {
            let Some(member) = self.member(key).await? else {
                return Ok(0);
            };
            let cancelled = self.inner.store.invalidate_pending_logins(member).await?;
            tracing::info!(%member, cancelled, "cancelled pending logins after a public secret");
            Ok::<_, Failure>(cancelled)
        };
        match cancelled.await {
            Ok(cancelled) => Some(cancelled),
            Err(failure) => {
                tracing::warn!(member = %key, error = %failure, "couldn't cancel pending logins");
                None
            }
        }
    }
}

/// How a plan is named in replies.
fn plan_name(plan: &Plan) -> String {
    match plan {
        Plan::Pro => "Claude Pro".to_owned(),
        Plan::Max => "Claude Max".to_owned(),
        Plan::Team => "Claude Team".to_owned(),
        Plan::Enterprise => "Claude Enterprise".to_owned(),
        Plan::Unknown(other) => format!("`{}`", other.replace('`', "")),
    }
}

/// How the member types `login` on `surface`, for notices that answer no
/// command.
fn login_command(surface: SurfaceKind) -> &'static str {
    match surface {
        SurfaceKind::Slack => "`/agent login`",
        SurfaceKind::RocketChat => "`login`",
    }
}

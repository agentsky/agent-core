//! `/agent` command dispatch, the account commands, the agent commands
//! (`create`, `persona`, `list`, `pause`, `resume`, `delete`), the skill
//! commands (`skill add`, `skill confirm`, `skill rm`), the session
//! commands (`sessions`, `reset`), `approve` and `decline`, which decide a
//! private task's consent for the agent's owner
//! ([`Commands::with_consents`]), and the community admins' `admin api-key
//! set` and `clear`, which only the identities [`Commands::with_admins`]
//! names may run.
//!
//! Every surface turns a command into `(MemberKey, text, Origin, files)` and hands
//! it to [`Commands::handle_text`], which parses it with
//! [`commands::parse`] and runs it. The reply always goes privately to the
//! member through [`Replies::reply_private`], wherever the command came from.
//!
//! - [`intake`]: runs the commands every surface hears, one member's in
//!   order.
//! - [`rocketchat`]: which Rocket.Chat messages are commands. A DM to the
//!   manager bot is a command as a whole; a message elsewhere is one when it
//!   starts with `!agent`.
//! - [`slack`]: which Slack requests are commands: `/agent`, a DM to the
//!   manager app as a whole, and a consent card's Approve or Decline
//!   button, as `approve <id>` or `decline <id>`.
//! - [`slack_tokens`]: members' Slack app configuration tokens, which
//!   `/agent slack-token` registers and a background loop renews.
//! - [`relink`]: the notice a member gets, once, when their Claude link
//!   breaks.
//! - [`cloud`]: `cloud add`, `cloud run`, `cloud list` and `cloud rm`,
//!   which hand work to a Claude Code cloud session on the member's own
//!   account ([`Commands::with_cloud`]), and the notice a member gets for a
//!   hand-off whose answer was never recorded.
//! - [`reply`]: private replies through each surface's manager bot.
//! - `sessions`: `sessions` and `reset`, which reach the runner through a
//!   [`SessionControl`].
//!
//! # Secrets
//!
//! A secret-bearing command (`login <code>`, `admin api-key set`,
//! `slack-token`, `cloud add`) is refused when it comes from a room others can read
//! ([`Origin::is_private`] is false), and the member is told privately that
//! the secret is now public: a login code cancels the member's pending
//! logins and the one its `state` names, whoever started it; an API key or
//! token has to be revoked. Text that fails to parse but looks
//! secret-bearing gets the same treatment. Commands are logged by name only,
//! never with their text or arguments.
//!
//! # Who may run them
//!
//! A DM to the Slack manager app runs only when its sender is home: before
//! the text is even parsed, [`Commands::answer_text`] asks the manager
//! surface's [`home_user`](surface_slack::SlackSurface::home_user), without
//! waiting for a used-up quota. It runs in the member's own task of the
//! [`intake`], never in the Slack queue every app's requests pass through,
//! so a slow lookup holds up only that member's commands. A sender who
//! isn't home is dropped. A lookup that couldn't reach Slack, that Slack
//! couldn't answer this time, or that ran out of quota, gets
//! [`UNCONFIRMED_TEXT`] in the DM the event named, which opens nothing; any
//! other failed lookup drops the command, and the directory logs it as a
//! warning at most once per
//! [`LOOKUP_WARNING_INTERVAL`](surface_slack::directory::LOOKUP_WARNING_INTERVAL).
//! Slash commands and clicks are checked before they get here
//! (`slack::Inbound`).

mod admin;
mod agents;
pub mod cloud;
pub mod intake;
mod limits;
pub mod relink;
pub mod reply;
pub mod rocketchat;
mod sessions;
mod skills;
pub mod slack;
mod slack_agents;
pub mod slack_tokens;

#[cfg(test)]
mod slack_tests;
#[cfg(test)]
mod tests;

use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};

use auth::{Auth, AuthError, LinkStatus, PENDING_LOGIN_TTL, Plan};
use commands::{AdminCommand, ApiKeyCommand, CloudCommand, Command, ParseError};
use core_types::{
    ConsentId, ConvRef, ConversationId, InFile, MemberId, MemberKey, SurfaceError, SurfaceKind,
};
use secrecy::SecretString;
use store::{CloudDeleted, ConsentState, MemberUsage, Store, StoreError, UsageTotals};
use time::OffsetDateTime;

use crate::agents::RocketChatAgents;
use crate::cloud::FireClient;
use crate::consents::{Consents, Decided};
use crate::pipeline::UNCONFIRMED_TEXT;
use crate::policy::Limits;
use crate::skills::Skills;
use crate::slack::agents::SlackAgents;
use crate::slack::manager::SlackManager;

pub use agents::PERSONA_MAX_BYTES;
pub use reply::{ManagerBot, OpenDm, Replies, ReplyError, Rich};
pub use sessions::{MAX_LISTED, SessionControl};

/// Where a command came from. It decides where the reply goes and whether
/// the command may carry a secret.
pub enum Origin {
    /// A Slack slash command. Its text isn't posted to the channel, so it
    /// is private. The reply goes to `response_url`.
    SlackSlash {
        /// Where Slack takes the ephemeral reply. Anyone holding it can post
        /// there for a while, so it is kept secret.
        response_url: SecretString,
        /// The conversation it was run in.
        conv: ConvRef,
    },
    /// A direct message with the Slack manager app, in `channel`.
    SlackDm {
        /// The DM's channel.
        channel: ConversationId,
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
            Self::SlackSlash { conv, .. } => f
                .debug_struct("SlackSlash")
                .field("conv", conv)
                .finish_non_exhaustive(),
            Self::SlackDm { channel } => {
                f.debug_struct("SlackDm").field("channel", channel).finish()
            }
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

    /// The conversation the command was sent in, where `reset <name>
    /// here` acts, for `member`'s command: the room of an `!agent` message
    /// or the conversation of a slash command. `None` in a direct message
    /// with the manager bot, where no agent answers.
    pub fn conversation(&self, member: &MemberKey) -> Option<ConvRef> {
        match self {
            Self::SlackSlash { conv, .. } => Some(conv.clone()),
            Self::RocketChatChannel { room } => Some(ConvRef {
                surface: member.surface,
                team: member.team.clone(),
                conversation: room.clone(),
            }),
            Self::SlackDm { .. } | Self::RocketChatDm { .. } => None,
        }
    }

    /// Whether the surface delivered the command's text with `&`, `<` and
    /// `>` as entities and mentions, channels and links as `<…>` tokens,
    /// as Slack does.
    fn is_slack(&self) -> bool {
        matches!(self, Self::SlackSlash { .. } | Self::SlackDm { .. })
    }

    /// Command `text` as the member typed it: Slack's entities decoded
    /// ([`unescape`](surface_slack::normalize::unescape)), so a persona
    /// reads as typed, and other surfaces' text as it is. Mention and link
    /// tokens parse the same either way.
    fn decoded<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if self.is_slack() {
            std::borrow::Cow::Owned(surface_slack::normalize::unescape(text))
        } else {
            std::borrow::Cow::Borrowed(text)
        }
    }

    /// A short name for logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SlackSlash { .. } => "slack_slash",
            Self::SlackDm { .. } => "slack_dm",
            Self::RocketChatDm { .. } => "rocketchat_dm",
            Self::RocketChatChannel { .. } => "rocketchat_channel",
        }
    }

    /// How the member types `command` where their reply lands: after
    /// `/agent` in a Slack slash command, bare in the manager bot's DM.
    fn command(&self, command: &str) -> String {
        match self {
            Self::SlackSlash { .. } => format!("`/agent {command}`"),
            Self::SlackDm { .. } | Self::RocketChatDm { .. } | Self::RocketChatChannel { .. } => {
                format!("`{command}`")
            }
        }
    }

    /// Where the member should send secrets: the place their reply lands.
    fn private_place(&self) -> &'static str {
        match self {
            Self::SlackSlash { .. } => "with the `/agent` command, which no one else sees",
            Self::SlackDm { .. } | Self::RocketChatDm { .. } | Self::RocketChatChannel { .. } => {
                "here, in this direct message"
            }
        }
    }
}

/// What a command still has to do once its reply is sent. It no longer
/// holds up the member's later commands.
#[must_use = "a follow-up does nothing unless it is run"]
#[derive(Default)]
pub struct FollowUp(Option<Pin<Box<dyn Future<Output = ()> + Send>>>);

impl fmt::Debug for FollowUp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FollowUp")
            .field(&self.0.as_ref().map(|_| ".."))
            .finish()
    }
}

impl FollowUp {
    fn new(work: impl Future<Output = ()> + Send + 'static) -> Self {
        Self(Some(Box::pin(work)))
    }

    /// Does what is left, if anything.
    pub async fn run(self) {
        if let Some(work) = self.0 {
            work.await;
        }
    }
}

/// The reply when something on agentd's side failed. The cause is logged.
const FAILED: &str = "Something went wrong on my side. Please try again in a minute.";

/// The reply to a banned member's commands, those that only take
/// something away aside.
const BANNED: &str = "A community admin banned you, so agents won't take your requests. You \
                      can still run `me`, `logout`, `cloud rm`, and `pause` or `delete` your \
                      agents.";

/// Runs `/agent` commands and sends their replies. Agents are created on
/// Rocket.Chat through [`RocketChatAgents`], and on Slack, as apps, through
/// [`SlackAgents`] ([`with_slack_agents`](Self::with_slack_agents)).
///
/// Cloning is cheap and shares everything.
#[derive(Debug, Clone)]
pub struct Commands {
    inner: Arc<Inner>,
    admins: Arc<[MemberKey]>,
    slack_agents: Option<SlackAgents>,
    limits: Limits,
    consents: Option<Consents>,
    cloud: Option<FireClient>,
}

#[derive(Debug)]
struct Inner {
    store: Store,
    auth: Arc<Auth>,
    replies: Replies,
    rocketchat: Option<RocketChatAgents>,
    slack: Option<SlackManager>,
    skills: Skills,
    sessions: Mutex<Option<Weak<dyn SessionControl>>>,
}

/// Why a handler couldn't produce its reply. Logged, never shown.
#[derive(Debug, thiserror::Error)]
enum Failure {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Surface(#[from] core_types::SurfaceError),
    #[error(transparent)]
    Skill(#[from] crate::skills::SkillError),
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

impl Commands {
    /// Commands over `store` and `auth`, replying through `replies`,
    /// managing agents on Rocket.Chat through `rocketchat` if agentd serves
    /// Rocket.Chat, with the Slack manager app `slack` if agentd serves
    /// Slack, and managing agents' skills through `skills`.
    pub fn new(
        store: Store,
        auth: Arc<Auth>,
        replies: Replies,
        rocketchat: Option<RocketChatAgents>,
        slack: Option<SlackManager>,
        skills: Skills,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                auth,
                replies,
                rocketchat,
                slack,
                skills,
                sessions: Mutex::new(None),
            }),
            admins: Arc::new([]),
            slack_agents: None,
            limits: Limits::default(),
            consents: None,
            cloud: None,
        }
    }

    /// These commands, firing members' routines with `fire` for `cloud
    /// run`, as with `[cloud]`. Without it cloud hand-off is off: `cloud
    /// add` and `cloud run` are refused, and `cloud list` and `cloud rm`
    /// still work.
    #[must_use]
    pub fn with_cloud(mut self, fire: FireClient) -> Self {
        self.cloud = Some(fire);
        self
    }

    /// These commands, deciding private tasks' consents in `consents` with
    /// `approve` and `decline`. Without it those aren't available.
    #[must_use]
    pub fn with_consents(mut self, consents: Consents) -> Self {
        self.consents = Some(consents);
        self
    }

    /// Has the consents look at what an agent's new state owes, as an
    /// approved task waiting for its paused agent.
    fn wake_consents(&self) {
        if let Some(consents) = &self.consents {
            consents.wake();
        }
    }

    /// These commands, creating and deleting agents on Slack through
    /// `agents`.
    #[must_use]
    pub fn with_slack_agents(mut self, agents: SlackAgents) -> Self {
        self.slack_agents = Some(agents);
        self
    }

    /// The same commands, with `admins` as the community admins: the only
    /// members whose `/agent admin …` commands run. Without it nobody is an
    /// admin.
    #[must_use]
    pub fn with_admins(mut self, admins: impl IntoIterator<Item = MemberKey>) -> Self {
        self.admins = admins.into_iter().collect();
        self
    }

    /// The same commands, with the community's caps from `[limits]`, which
    /// `limits` replies with. Without it, the defaults.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Whether `member` is a community admin.
    fn is_admin(&self, member: &MemberKey) -> bool {
        self.admins.contains(member)
    }

    /// The private reply plumbing.
    pub fn replies(&self) -> &Replies {
        &self.inner.replies
    }

    /// Parses `text` from `member`, sent with `files` attached, and runs
    /// it to the end, replying privately. Text that doesn't parse gets the
    /// parser's message (help or usage). `text` is as the surface delivered
    /// it: Slack's entities are decoded here, before it is parsed.
    pub async fn handle_text(
        &self,
        member: &MemberKey,
        text: &str,
        origin: &Origin,
        files: &[InFile],
    ) {
        self.answer_text(member, text, origin, files)
            .await
            .run()
            .await;
    }

    /// As [`handle_text`](Self::handle_text), but returns once the reply
    /// is sent, with what the command still has to do. A manager DM from a
    /// sender who isn't home runs nothing ([Who may run
    /// them](self#who-may-run-them)).
    pub async fn answer_text(
        &self,
        member: &MemberKey,
        text: &str,
        origin: &Origin,
        files: &[InFile],
    ) -> FollowUp {
        if !self.admits(member, origin).await {
            return FollowUp::default();
        }
        match commands::parse(&origin.decoded(text)) {
            Ok(command) => self.answer(member, command, origin, files, text).await,
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
                FollowUp::default()
            }
        }
    }

    /// Whether a command `member` sent from `origin` may run: always, but
    /// for a DM to the Slack manager app, which runs only when the manager
    /// surface says its sender is home ([Who may run
    /// them](self#who-may-run-them)).
    async fn admits(&self, member: &MemberKey, origin: &Origin) -> bool {
        if !matches!(origin, Origin::SlackDm { .. }) {
            return true;
        }
        let Some(slack) = self.inner.slack.as_ref().filter(|slack| {
            member.surface == SurfaceKind::Slack && member.team == slack.identity().team
        }) else {
            tracing::debug!(%member, "dropped a Slack DM command no manager app serves");
            return false;
        };
        match slack.surface().home_user(&member.user).await {
            Ok(true) => true,
            Ok(false) => {
                tracing::debug!(%member, "dropped a DM command from outside the workspace");
                false
            }
            Err(SurfaceError::Transport(_) | SurfaceError::RateLimited { .. }) => {
                self.reply(member, origin, UNCONFIRMED_TEXT).await;
                false
            }
            Err(err) => {
                tracing::debug!(%member, error = %err, "couldn't check whether a DM command's sender is home; dropped it");
                false
            }
        }
    }

    /// Runs `command` from `member`, sent with `files` attached, to the
    /// end, and replies privately, skipping the parser and [who may run
    /// them](self#who-may-run-them): for tests only.
    #[cfg(test)]
    pub(crate) async fn dispatch(
        &self,
        member: &MemberKey,
        command: Command,
        origin: &Origin,
        files: &[InFile],
    ) {
        self.answer(member, command, origin, files, "")
            .await
            .run()
            .await;
    }

    /// Runs `command` until its reply is sent, and returns what it still
    /// has to do. `delivered` is the command's text as the surface
    /// delivered it, which `cloud run` reads its task from on Slack.
    async fn answer(
        &self,
        member: &MemberKey,
        command: Command,
        origin: &Origin,
        files: &[InFile],
        delivered: &str,
    ) -> FollowUp {
        tracing::info!(
            %member,
            origin = origin.kind(),
            command = command.name(),
            files = files.len(),
            "running a command"
        );
        let (reply, follow_up) = self.run(member, command, origin, files, delivered).await;
        self.reply(member, origin, &reply).await;
        follow_up
    }

    async fn reply(&self, member: &MemberKey, origin: &Origin, text: &str) {
        if let Err(err) = self.inner.replies.reply_private(member, origin, text).await {
            tracing::warn!(%member, origin = origin.kind(), error = %err, "couldn't send a command reply");
        }
    }

    /// The reply to `command`, and what it still has to do after it.
    async fn run(
        &self,
        member: &MemberKey,
        command: Command,
        origin: &Origin,
        files: &[InFile],
        delivered: &str,
    ) -> (String, FollowUp) {
        let name = command.name();
        match self.banned(member, &command, origin).await {
            Ok(false) => {}
            Ok(true) => {
                tracing::info!(%member, command = name, "refused a banned member's command");
                return (BANNED.to_owned(), FollowUp::default());
            }
            Err(err) => {
                tracing::warn!(%member, command = name, error = %err, "couldn't read whether a member is banned");
                return (FAILED.to_owned(), FollowUp::default());
            }
        }
        let result = match command {
            Command::Reset { name, here } => self.reset(member, name.as_str(), here, origin).await,
            command => self
                .reply_to(member, command, origin, files, delivered)
                .await
                .map(|reply| (reply, FollowUp::default())),
        };
        result.unwrap_or_else(|err| {
            tracing::warn!(%member, command = name, error = %err, "a command failed");
            (FAILED.to_owned(), FollowUp::default())
        })
    }

    /// Whether `command` from `key` is refused because a community admin
    /// banned them. An admin never is, nor are the commands that only take
    /// something away from the member (`me`, `logout`, `cloud rm`, and
    /// `pause` and `delete` of their own agents), nor a secret-bearing
    /// command sent where others can read it, whose refusal tells them to
    /// revoke the secret.
    async fn banned(
        &self,
        key: &MemberKey,
        command: &Command,
        origin: &Origin,
    ) -> Result<bool, Failure> {
        let reduces = matches!(
            command,
            Command::Me
                | Command::Logout
                | Command::Pause { .. }
                | Command::Delete { .. }
                | Command::Cloud(CloudCommand::Rm { .. })
        );
        if reduces || self.is_admin(key) || (command.is_secret_bearing() && !origin.is_private()) {
            return Ok(false);
        }
        match self.member(key).await? {
            Some(member) => Ok(self.inner.store.is_banned(member).await?),
            None => Ok(false),
        }
    }

    /// The reply to `command`, for a command that is done once it has one.
    async fn reply_to(
        &self,
        member: &MemberKey,
        command: Command,
        origin: &Origin,
        files: &[InFile],
        delivered: &str,
    ) -> Result<String, Failure> {
        let name = command.name();
        if command.is_secret_bearing() && !origin.is_private() {
            self.refuse_public_secret(member, &command, origin).await
        } else {
            match command {
                Command::Login { code: None } => self.login_start(member, origin).await,
                Command::Login { code: Some(code) } => {
                    self.login_complete(member, &code, origin).await
                }
                Command::Logout => self.logout(member, origin).await,
                Command::Me => self.me(member, origin).await,
                Command::SlackToken { refresh, .. } => {
                    self.slack_token(member, &refresh, origin).await
                }
                Command::Create { name, persona } => {
                    self.create(member, name.as_str(), persona, origin).await
                }
                Command::Persona { name, text } => {
                    self.persona(member, name.as_str(), text, origin, files)
                        .await
                }
                Command::List { user } => self.list(member, user.as_ref(), origin).await,
                Command::Pause { name } => {
                    self.set_paused(member, name.as_str(), true, origin).await
                }
                Command::Resume { name } => {
                    self.set_paused(member, name.as_str(), false, origin).await
                }
                Command::Delete { name } => self.delete(member, name.as_str()).await,
                Command::Skill(command) => self.skill(member, command, origin, files).await,
                Command::Sessions { name } => self.sessions(member, name.as_str(), origin).await,
                Command::Admin(AdminCommand::ApiKey(command)) => {
                    self.api_key(member, command).await
                }
                Command::Admin(AdminCommand::Ban { user, reason }) => {
                    self.ban(member, &user, reason).await
                }
                Command::Admin(AdminCommand::Unban { user }) => self.unban(member, &user).await,
                Command::Limits {
                    name,
                    turns_per_day,
                    hops,
                } => {
                    self.limits(member, name.as_str(), turns_per_day, hops)
                        .await
                }
                Command::Allow { name, target } => {
                    self.allow_or_deny(member, name.as_str(), &target, true)
                        .await
                }
                Command::Deny { name, target } => {
                    self.allow_or_deny(member, name.as_str(), &target, false)
                        .await
                }
                Command::Approve { consent } => self.decide(member, consent, true).await,
                Command::Decline { consent } => self.decide(member, consent, false).await,
                Command::Cloud(command) => self.cloud(member, command, origin, delivered).await,
                _ => Ok(format!("`{name}` isn't available yet.")),
            }
        }
    }

    /// `approve` or `decline`: `key`'s decision on a private task's
    /// consent, which only the agent's owner may make.
    async fn decide(
        &self,
        key: &MemberKey,
        consent: ConsentId,
        approve: bool,
    ) -> Result<String, Failure> {
        let Some(consents) = &self.consents else {
            let name = if approve { "approve" } else { "decline" };
            return Ok(format!("`{name}` isn't available yet."));
        };
        Ok(match consents.decide(key, consent, approve).await? {
            Decided::Recorded if approve => {
                format!(
                    "Approved private task `{consent}`. Its result goes to the thread that asked."
                )
            }
            Decided::Recorded => {
                format!("Declined private task `{consent}`. The thread that asked is told.")
            }
            Decided::NotYours => format!(
                "No private task `{consent}` is waiting for you: only the owner of the agent that \
                 asked can approve or decline it."
            ),
            Decided::Settled(state) => format!(
                "Private task `{consent}` was already {}.",
                match state {
                    ConsentState::Approved => "approved",
                    ConsentState::Declined => "declined",
                    ConsentState::Expired | ConsentState::Pending => "expired",
                }
            ),
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
            "To link your Claude account, [open the Claude login page]({}) and approve.\n\n\
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
            Err(AuthError::ScopeRefused) => format!(
                "Anthropic granted this login more access than agentd uses, so nothing was \
                 linked. Open the login link exactly as it is sent, without changing it. {again}"
            ),
            Err(AuthError::ScopeUnstated) => "Anthropic's answer didn't say what access it \
                granted, so nothing was linked, and logging in again won't help until that \
                changes. Tell an admin."
                .to_owned(),
            Err(err) => {
                tracing::warn!(%member, error = %err, "couldn't complete a login");
                format!("I couldn't finish linking your account. {again}")
            }
        };
        Ok(reply)
    }

    async fn logout(&self, key: &MemberKey, origin: &Origin) -> Result<String, Failure> {
        let (unlinked, tokens, cloud) = match self.member(key).await? {
            Some(member) => {
                self.inner.store.invalidate_pending_logins(member).await?;
                let unlinked = self.inner.auth.logout(member).await?;
                let tokens = self.inner.store.delete_slack_config_tokens(member).await?;
                if tokens > 0 {
                    tracing::info!(%member, tokens, "deleted Slack configuration tokens at logout");
                }
                let cloud = self.inner.store.delete_cloud_routines_of(member).await?;
                if cloud != CloudDeleted::default() {
                    tracing::info!(
                        %member,
                        routines = cloud.routines,
                        handoffs = cloud.handoffs,
                        "deleted cloud routines and hand-offs at logout"
                    );
                }
                (unlinked, tokens, cloud)
            }
            None => (false, 0, CloudDeleted::default()),
        };
        let mut reply = if unlinked {
            format!(
                "Your Claude account is unlinked. Send {} to link one again.",
                origin.command("login")
            )
        } else {
            "No Claude account is linked.".to_owned()
        };
        if tokens > 0 {
            reply.push_str(
                " I also deleted your Slack configuration token, so I can no longer create or \
                 change apps as you.",
            );
        }
        let forgot: Vec<String> = [
            (cloud.routines > 0).then(|| cloud::routines_counted(cloud.routines)),
            (cloud.handoffs > 0).then(|| cloud::handoffs_counted(cloud.handoffs)),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !forgot.is_empty() {
            reply.push_str(&format!(" I also forgot your {}.", forgot.join(" and ")));
        }
        if cloud.routines > 0 {
            reply.push_str(
                " I can't revoke a routine's token: revoke each with **Revoke** on the \
                 routine's API trigger at claude.ai/code/routines.",
            );
        }
        Ok(reply)
    }

    async fn me(&self, key: &MemberKey, origin: &Origin) -> Result<String, Failure> {
        let member = self.member(key).await?;
        let status = match member {
            Some(member) => self.inner.auth.status(member).await?,
            None => LinkStatus::default(),
        };
        let login = origin.command("login");
        let mut reply = match status {
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
        };
        reply.push('\n');
        reply.push_str(&self.usage(member).await?);
        if let Some(member) = member.filter(|_| !self.is_admin(key))
            && let Some(ban) = self.inner.store.ban(member).await?
        {
            reply.push_str("\nA community admin banned you: agents won't take your requests, and you can only run `me`, `logout`, `cloud rm`, and `pause` or `delete` your agents.");
            if let Some(reason) = ban.reason.filter(|reason| !reason.trim().is_empty()) {
                let reason: String = reason
                    .chars()
                    .map(|c| if c.is_control() { ' ' } else { c })
                    .collect();
                reply.push_str(&format!(" Reason: {}", reason.trim()));
            }
        }
        if self.is_admin(key) {
            reply.push('\n');
            reply.push_str(&self.community_key_status().await?);
        }
        if key.surface == SurfaceKind::Slack
            && let Some(slack) = &self.inner.slack
        {
            reply.push('\n');
            reply.push_str(&self.slack_token_status(key, origin).await?);
            if let Some(outdated) = self.outdated_apps_status(key, origin).await? {
                reply.push('\n');
                reply.push_str(&outdated);
            }
            let manager = slack.identity();
            let name = manager.app_name.as_deref().unwrap_or("(no name)");
            reply.push_str(&format!(
                "\nCommands here are answered by the Slack app `{}` (`{}`). If `/agent` ever \
                 answers as another app, that app has taken the command over: don't send it \
                 codes or tokens, and tell your workspace admins.",
                name.replace('`', ""),
                manager.app_id.replace('`', ""),
            ));
        }
        Ok(reply)
    }

    /// `member`'s usage line for `me`: the turns and tokens billed to them
    /// today and this month, UTC.
    async fn usage(&self, member: Option<MemberId>) -> Result<String, Failure> {
        let billed = match member {
            Some(member) => self.inner.store.member_usage(member, now()).await?,
            None => MemberUsage::default(),
        };
        let describe = |usage: UsageTotals| {
            format!(
                "{} {}, {} tokens",
                usage.turns,
                if usage.turns == 1 { "turn" } else { "turns" },
                usage.tokens()
            )
        };
        Ok(format!(
            "Usage billed to you today: {}. This month: {} (days start at midnight UTC).",
            describe(billed.today),
            describe(billed.month)
        ))
    }

    /// The reply to a secret-bearing `command` sent where others can read
    /// it. The secret isn't used.
    async fn refuse_public_secret(
        &self,
        key: &MemberKey,
        command: &Command,
        origin: &Origin,
    ) -> Result<String, Failure> {
        let place = origin.private_place();
        let advice = match command {
            Command::Login { code: Some(code) } => {
                self.cancel_pending_logins(key).await?;
                if self.inner.auth.cancel_pasted_login(code).await? {
                    tracing::info!(member = %key, "cancelled the pending login a public code belongs to");
                }
                format!(
                    "That login code is no longer secret, so I didn't use it and cancelled your \
                     pending login. Start again with {}, and send the code only {place}.",
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
            Command::Cloud(CloudCommand::Add { .. }) => format!(
                "That routine token is no longer secret, so I didn't store it. Revoke it now \
                 with **Regenerate** or **Revoke** on the routine's API trigger at \
                 claude.ai/code/routines, and register the new one only {place}."
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
            | Command::Decline { .. }
            | Command::Cloud(
                CloudCommand::Run { .. } | CloudCommand::List | CloudCommand::Rm { .. },
            ) => format!(
                "Whatever secret it held is no longer secret, so I didn't use it. Revoke it now, \
                 and send a new one only {place}."
            ),
        };
        Ok(format!(
            "You posted a secret in a room others can read. {advice} You may also want to \
             delete your message there."
        ))
    }

    /// The reply to text that didn't parse. If it looks like it held a
    /// secret and others could read it, the member's pending logins are
    /// cancelled and they are told to revoke what they posted.
    async fn unparsed(&self, key: &MemberKey, err: &ParseError, origin: &Origin) -> String {
        if !err.is_secret_bearing() || origin.is_private() {
            return err.to_string();
        }
        if let Err(failure) = self.cancel_pending_logins(key).await {
            tracing::warn!(member = %key, error = %failure, "couldn't cancel pending logins");
        }
        format!(
            "Your message looked like it held a secret (a login code, an API key or a token), \
             and others can read the room you posted it in. If it did, that secret is no longer \
             private: I cancelled any pending login, so start again with {}, and revoke any key \
             or token you posted (a routine token with **Regenerate** or **Revoke** at \
             claude.ai/code/routines). Send secrets only {}.\n\n{err}",
            origin.command("login"),
            origin.private_place(),
        )
    }

    async fn cancel_pending_logins(&self, key: &MemberKey) -> Result<(), Failure> {
        if let Some(member) = self.member(key).await? {
            let cancelled = self.inner.store.invalidate_pending_logins(member).await?;
            tracing::info!(%member, cancelled, "cancelled pending logins after a public secret");
        }
        Ok(())
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

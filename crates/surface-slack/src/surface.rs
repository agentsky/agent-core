//! [`SlackSurface`]: the [`Surface`] for one Slack binding.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use core_types::{
    Binding, Caps, ConvRef, ConversationId, Cursor, InboundEvent, MemberKey, Msg, MsgRef, OutFile,
    Outside, Posted, ReplyTarget, Sender, Surface, SurfaceError, SurfaceKind, ThreadKey, UserId,
};
use render::MentionDirectory;
use render::slack::{MESSAGE_LIMIT, to_mrkdwn};
use time::OffsetDateTime;

use crate::directory::{MemberDirectory, Membership, TeamDirectory};
use crate::normalize::{self, Context, KEPT_SUBTYPES};
use crate::web::{Message, PageRequest, Result, WebApi};

/// The page size history reads ask for.
const PAGE_SIZE: usize = 200;

/// The most pages a history read follows.
const MAX_PAGES: usize = 1000;

/// How long before its event arrived a message may have been posted and
/// still be read back by [`Surface::confirm`]. Slack's last retry of a
/// delivery comes about five minutes after the first; deduplication
/// forgets a message after [`DEDUP_RETENTION`](crate::ingress::DEDUP_RETENTION),
/// and a message older than the bot's membership never had one, so without
/// it either could be replayed. The ingress acknowledges and drops an
/// agent's message this old before it records it.
pub const CONFIRM_WINDOW: Duration = Duration::from_secs(15 * 60);

/// What [`SlackSurface::caps`] returns: a 3,000-character message limit
/// ([`MESSAGE_LIMIT`]), edits, buttons and threads, and a copy of every
/// event for each agent's app.
pub const CAPS: Caps = Caps {
    message_limit: MESSAGE_LIMIT,
    supports_edit: true,
    supports_buttons: true,
    supports_threads: true,
    per_binding_delivery: true,
};

/// Slack's web client link to a conversation, or to a thread in it when
/// `thread` has a root: `https://app.slack.com/client/<team>/<channel>`, then
/// `/thread/<channel>-<ts>`. It names the workspace by id, so it needs no
/// Web API call, and opens the conversation for anyone who may see it.
/// `None` only if [`CLIENT_URL`] stopped being a base URL.
pub fn thread_link(thread: &ThreadKey) -> Option<String> {
    let conv = &thread.conv;
    let mut url = reqwest::Url::parse(CLIENT_URL).ok()?;
    {
        let mut segments = url.path_segments_mut().ok()?;
        segments
            .pop_if_empty()
            .extend([conv.team.as_str(), conv.conversation.as_str()]);
        if let Some(root) = &thread.root {
            let reply = format!("{}-{}", conv.conversation, root);
            segments.extend(["thread", reply.as_str()]);
        }
    }
    Some(url.into())
}

/// Where [`thread_link`] points: Slack's web client.
pub const CLIENT_URL: &str = "https://app.slack.com/client/";

/// The [`Surface`] for one Slack binding: an agent's app, or the manager
/// app, acting with that app's bot token.
///
/// - Events don't come through [`Surface::events`]: Slack pushes them to
///   the HTTPS [`ingress`](crate::ingress()), so `events` returns
///   [`SurfaceError::Unsupported`].
/// - [`render`](Surface::render) converts with [`to_mrkdwn`], resolving
///   `@Name` through the workspace's member cache as last read, and splits
///   at [`MESSAGE_LIMIT`]. A stale cache is refreshed in the background for
///   the next render; await [`refresh_members`](Self::refresh_members) to
///   have it current now, as agentd does when it starts a binding, or pass a
///   directory of your own to [`render_with`](Self::render_with). agentd
///   names its agents' bot users with
///   [`TeamDirectory::set_managed_bots`], so an agent keeps a name a human
///   shares.
/// - [`post`](Surface::post) sends one chunk with `chat.postMessage`.
/// - A bot message without a `user` names its sender by bot id until
///   [`fill_bot_sender`](Self::fill_bot_sender) looks the bot up.
/// - [`confirm`](Surface::confirm) decides whether Slack's copy of a
///   message is from outside the workspace with
///   [`fill_sender_team`](Self::fill_sender_team).
///
/// Every conversation it is given must be a Slack conversation in its
/// workspace; any other is refused with [`SurfaceError::Api`] before
/// anything is sent.
///
/// `Debug` shows the workspace and cache sizes, never member names.
#[derive(Clone)]
pub struct SlackSurface {
    api: WebApi,
    members_api: WebApi,
    directory: Arc<TeamDirectory>,
    bot_user: Option<UserId>,
    member_of: Arc<Mutex<HashMap<ConversationId, Instant>>>,
}

/// How long [`Surface::can_post`] trusts that the bot is a member of a
/// conversation, once Slack said so.
pub const MEMBERSHIP_TTL: Duration = Duration::from_secs(5 * 60);

/// The most conversations whose membership one surface remembers.
const MAX_MEMBERSHIPS: usize = 10_000;

impl SlackSurface {
    /// A surface acting through `api` (the binding's bot token) in the
    /// workspace `directory` belongs to. Share one [`TeamDirectory`] among
    /// the bindings of a workspace.
    pub fn new(api: WebApi, directory: Arc<TeamDirectory>) -> Self {
        Self {
            members_api: api.clone(),
            api,
            directory,
            bot_user: None,
            member_of: Arc::default(),
        }
    }

    /// Reads the workspace's member list, which every binding in it
    /// shares, through `api` rather than the binding's own token. agentd
    /// gives agents' surfaces the manager app's, which its operators hold:
    /// an agent's owner holds the agent's token, and could revoke it or use
    /// up its quota to keep the shared list stale for everyone.
    pub fn with_members_api(mut self, api: WebApi) -> Self {
        self.members_api = api;
        self
    }

    /// Sets the binding's bot user, which [`Surface::confirm`] needs to
    /// tell, as the ingress does, whether a message outside a one-to-one DM
    /// mentions the bot or replies in a thread under the bot's root.
    /// Without one, only thread replies and DMs are confirmed.
    pub fn with_bot_user(mut self, bot_user: Option<UserId>) -> Self {
        self.bot_user = bot_user;
        self
    }

    /// The Web API client, for the calls the [`Surface`] trait doesn't
    /// cover.
    pub fn api(&self) -> &WebApi {
        &self.api
    }

    /// The workspace's caches.
    pub fn directory(&self) -> &Arc<TeamDirectory> {
        &self.directory
    }

    /// Reads the workspace's members again when the cache is older than its
    /// TTL, through the [members API](Self::with_members_api), and returns
    /// the snapshot [`render`](Surface::render) will use.
    ///
    /// # Errors
    ///
    /// The `users.list` error, when there is no older list to fall back on.
    pub async fn refresh_members(&self) -> Result<Arc<MemberDirectory>> {
        self.directory.refresh_members(&self.members_api).await
    }

    /// Starts a member refresh on the current Tokio runtime when the cache
    /// is missing or stale and no refresh is running, without waiting for
    /// it. Outside a runtime it does nothing.
    pub fn refresh_in_background(&self) {
        if !self.directory.needs_refresh() {
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let surface = self.clone();
        runtime.spawn(async move {
            if let Err(err) = surface.refresh_members().await {
                tracing::warn!(team = %surface.directory.team(), error = %err, "reading the Slack member list failed");
            }
        });
    }

    /// Converts `markdown` to mrkdwn, resolving `@Name` through
    /// `directory`, and splits it into chunks of at most
    /// [`MESSAGE_LIMIT`].
    pub fn render_with(&self, markdown: &str, directory: &dyn MentionDirectory) -> Vec<String> {
        render::split(&to_mrkdwn(markdown, directory), MESSAGE_LIMIT)
    }

    /// Fills in the sender of a bot message that carried only a `bot_id`:
    /// looks the bot up with `bots.info` (cached per bot id) and, when it
    /// has a user, puts that user id in `sender.user` and
    /// `sender_bot_user`. A bot without one keeps its bot id and no
    /// `sender_bot_user`, so the router ignores it as an unmanaged bot.
    ///
    /// Events from a human, from a bot whose user is already known, or from
    /// another workspace are left alone.
    ///
    /// The lookup [never waits](WebApi::without_waiting) for the token's
    /// `bots.info` quota, so events with made-up bot ids can't stall the
    /// caller behind it.
    ///
    /// # Errors
    ///
    /// A `bots.info` failure other than `bot_not_found`, including
    /// [`SurfaceError::RateLimited`] when the quota is used up. The event is
    /// left unchanged.
    pub async fn fill_bot_sender(&self, event: &mut InboundEvent) -> Result<()> {
        if !event.sender_is_bot
            || event.sender_bot_user.is_some()
            || event.sender.surface != SurfaceKind::Slack
            || event.sender.team != *self.directory.team()
        {
            return Ok(());
        }
        let bot_id = event.sender.user.as_str().to_owned();
        let api = self.api.without_waiting();
        if let Some(user) = self.directory.bot_user(&api, &bot_id).await? {
            event.sender.user = user.clone();
            event.sender_bot_user = Some(user);
        }
        Ok(())
    }

    /// Whether `user` belongs to the workspace, as
    /// [`TeamDirectory::home_user`] says, asking `users.info` with the
    /// [members API](Self::with_members_api), the manager app's token in
    /// agentd, and never waiting for its used-up quota.
    ///
    /// # Errors
    ///
    /// As for [`TeamDirectory::home_user`]; past the quota,
    /// [`SurfaceError::RateLimited`].
    pub async fn home_user(&self, user: &UserId) -> Result<bool> {
        self.directory
            .home_user(&self.members_api.without_waiting(), user)
            .await
    }

    /// Decides whether `event`'s sender is from outside the workspace when
    /// its team fields left [`outside`](InboundEvent::outside) `None`: a
    /// sender the directory doesn't say is home is set outside, of the
    /// organization `users.info` named for them
    /// ([`directory::organization`](crate::directory::organization)), or of
    /// none known. A failed lookup that says
    /// nothing about the user does that too ([`SurfaceError::Api`],
    /// [`SurfaceError::Unauthorized`], [`SurfaceError::Forbidden`]), so
    /// the fields alone never make a sender home; the directory logs it
    /// ([Who is home](crate::directory#who-is-home)). So is a sender
    /// keyed by another surface or workspace, which this surface can't
    /// vouch for.
    ///
    /// A bot sender is never looked up: its `outside` decides nothing,
    /// since the router takes no bot for a requester. Neither is a sender
    /// already outside. The only caller is [`confirm`](Surface::confirm),
    /// on Slack's copy of a message, so a made-up user id in a forged event
    /// costs no lookup.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::Transport`] (Slack unreachable, or saying it
    /// couldn't answer this time) and [`SurfaceError::RateLimited`], as
    /// they came, leaving `outside` alone: Slack may answer later, and the
    /// confirmation fails as any lookup it can't make does.
    pub async fn fill_sender_team(&self, event: &mut InboundEvent) -> Result<()> {
        if event.outside.is_some() || event.sender_is_bot || event.sender_bot_user.is_some() {
            return Ok(());
        }
        if event.sender.surface == SurfaceKind::Slack && event.sender.team == *self.directory.team()
        {
            match self
                .directory
                .membership(&self.members_api.without_waiting(), &event.sender.user)
                .await
            {
                Ok(Membership::Home) => return Ok(()),
                Ok(Membership::Outside(team)) => {
                    event.outside = Some(Outside { team });
                    return Ok(());
                }
                Err(err @ (SurfaceError::Transport(_) | SurfaceError::RateLimited { .. })) => {
                    return Err(err);
                }
                Err(err) => {
                    tracing::debug!(binding = %event.binding, error = %err, "Slack wouldn't say whether a sender is home; taking them as outside");
                }
            }
        }
        event.outside = Some(Outside { team: None });
        Ok(())
    }

    /// Shows `text` (mrkdwn) to `user` alone, in `to`'s conversation and
    /// thread, with `chat.postEphemeral`.
    ///
    /// # Errors
    ///
    /// As for [`WebApi::post_ephemeral`].
    pub async fn post_ephemeral(&self, to: &ReplyTarget, user: &UserId, text: &str) -> Result<()> {
        let channel = self.channel(&to.conv)?;
        self.api
            .post_ephemeral(channel, user, to.thread_root.as_ref(), text)
            .await
            .map(drop)
    }

    /// The channel id of `conv`, which must be in this workspace.
    fn lock_member_of(&self) -> MutexGuard<'_, HashMap<ConversationId, Instant>> {
        self.member_of
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether Slack says the bot may post in `channel`: the conversation
    /// asked about, not archived, and the bot a member, or a DM. Slack not
    /// finding the conversation for the bot, refusing the bot, or no longer
    /// accepting its token is a no too; only a failure that may pass, as a
    /// rate limit or a network error, is an error. Such a no is logged as
    /// a warning with Slack's error, since a revoked token or a missing
    /// scope looks like it. A yes is kept for [`MEMBERSHIP_TTL`], and a no
    /// drops it.
    async fn member(&self, channel: &ConversationId) -> Result<bool> {
        let info = match self.api.conversation_info(channel).await {
            Ok(info) => Some(info),
            Err(
                err @ (SurfaceError::NotFound(_)
                | SurfaceError::Forbidden(_)
                | SurfaceError::Unauthorized),
            ) => {
                tracing::warn!(%channel, error = %err, "Slack refused to say whether the bot is in a conversation; taking it as no");
                None
            }
            Err(err) => return Err(err),
        };
        let member = info.is_some_and(|info| {
            info.id == *channel
                && !info.is_archived
                && (info.is_member || info.is_im || info.is_mpim)
        });
        let mut member_of = self.lock_member_of();
        if member {
            if member_of.len() >= MAX_MEMBERSHIPS {
                member_of.clear();
            }
            member_of.insert(channel.clone(), Instant::now() + MEMBERSHIP_TTL);
        } else {
            member_of.remove(channel);
        }
        Ok(member)
    }

    fn channel<'a>(&self, conv: &'a ConvRef) -> Result<&'a ConversationId> {
        if conv.surface != SurfaceKind::Slack || conv.team != *self.directory.team() {
            return Err(SurfaceError::Api(
                "the conversation is not in this binding's Slack workspace".into(),
            ));
        }
        Ok(&conv.conversation)
    }

    /// Turns a message read back into a [`Msg`], or `None` for one that
    /// isn't [content](is_content).
    async fn msg(&self, message: Message) -> Option<Msg> {
        if !is_content(&message) {
            return None;
        }
        let sent_at = ts_time(message.ts.as_str())?;
        let user = match (message.user, message.bot_id) {
            (Some(user), _) => user,
            (None, Some(bot_id)) => match self.directory.bot_user(&self.api, &bot_id).await {
                Ok(Some(user)) => user,
                Ok(None) => bot_id.into(),
                Err(err) => {
                    tracing::debug!(error = %err, "bots.info failed; naming a sender by bot id");
                    bot_id.into()
                }
            },
            (None, None) => return None,
        };
        Some(Msg {
            id: message.ts,
            sender: MemberKey {
                surface: SurfaceKind::Slack,
                team: self.directory.team().clone(),
                user,
            },
            sender_is_bot: message.is_bot,
            text: message.text,
            files: message.files,
            sent_at,
        })
    }

    /// The newest `limit` messages of a thread older than `before`, oldest
    /// first. `conversations.replies` pages from the oldest message, so
    /// every page is read and only the last `limit` kept. A `ts` seen
    /// before, such as the root repeated on a later page, is skipped.
    async fn thread_history(
        &self,
        channel: &ConversationId,
        root: &core_types::MessageId,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Msg>> {
        let mut kept: VecDeque<Message> = VecDeque::new();
        let mut seen = HashSet::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let page = PageRequest {
                latest: before,
                cursor: cursor.as_deref(),
                limit: PAGE_SIZE,
            };
            let page = self.api.replies(channel, root, page).await?;
            for message in page.messages {
                if is_content(&message)
                    && before.is_none_or(|before| ts_before(message.ts.as_str(), before))
                    && seen.insert(message.ts.clone())
                {
                    kept.push_back(message);
                    if kept.len() > limit {
                        kept.pop_front();
                    }
                }
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(self.msgs(kept).await),
            }
        }
        Err(SurfaceError::Api(
            "conversations.replies returned too many pages".into(),
        ))
    }

    /// The newest `limit` top-level messages older than `before`, oldest
    /// first. `conversations.history` pages from the newest message.
    async fn channel_history(
        &self,
        channel: &ConversationId,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Msg>> {
        let mut newest_first = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let page = PageRequest {
                latest: before,
                cursor: cursor.as_deref(),
                limit: PAGE_SIZE,
            };
            let page = self.api.history(channel, page).await?;
            for message in page.messages {
                if newest_first.len() < limit
                    && before.is_none_or(|before| ts_before(message.ts.as_str(), before))
                    && let Some(msg) = self.msg(message).await
                {
                    newest_first.push(msg);
                }
            }
            match page.next_cursor {
                Some(next) if newest_first.len() < limit => cursor = Some(next),
                _ => {
                    newest_first.reverse();
                    return Ok(newest_first);
                }
            }
        }
        Err(SurfaceError::Api(
            "conversations.history returned too many pages".into(),
        ))
    }

    async fn msgs(&self, messages: impl IntoIterator<Item = Message>) -> Vec<Msg> {
        let mut out = Vec::new();
        for message in messages {
            if let Some(msg) = self.msg(message).await {
                out.push(msg);
            }
        }
        out
    }
}

impl fmt::Debug for SlackSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackSurface")
            .field("directory", &self.directory)
            .finish_non_exhaustive()
    }
}

#[async_trait::async_trait]
impl Surface for SlackSurface {
    /// Slack delivers events to the HTTPS ingress, not through here.
    async fn events(&self, _binding: &Binding, _tx: Sender<InboundEvent>) -> Result<()> {
        Err(SurfaceError::Unsupported("events"))
    }

    /// Posts the mrkdwn `text`. Its mentions are read as Slack's copy of it
    /// is ([`normalize::mentions`]), so both deliveries of a post hand off
    /// to the same agents.
    async fn post(&self, to: &ReplyTarget, text: &str) -> Result<Posted> {
        let channel = self.channel(&to.conv)?;
        let ts = self
            .api
            .post_message(channel, to.thread_root.as_ref(), text)
            .await?;
        Ok(Posted {
            msg: MsgRef {
                conv: to.conv.clone(),
                id: ts,
            },
            mentions: normalize::mentions(text, None),
        })
    }

    async fn edit(&self, msg: &MsgRef, text: &str) -> Result<()> {
        let channel = self.channel(&msg.conv)?;
        self.api.update_message(channel, &msg.id, text).await
    }

    async fn react(&self, msg: &MsgRef, emoji: &str) -> Result<()> {
        let channel = self.channel(&msg.conv)?;
        self.api.add_reaction(channel, &msg.id, emoji).await
    }

    async fn unreact(&self, msg: &MsgRef, emoji: &str) -> Result<()> {
        let channel = self.channel(&msg.conv)?;
        self.api.remove_reaction(channel, &msg.id, emoji).await
    }

    /// Slack never joins a bot to a conversation it posts in; it refuses
    /// the post with `not_in_channel` instead. So this checks that the
    /// conversation is in this surface's workspace and, with
    /// `conversations.info`, that the bot is a member, or that it is a DM,
    /// and that it isn't archived. Slack's yes is trusted for
    /// [`MEMBERSHIP_TTL`]; a no is asked again each time, so a bot just
    /// added posts at once.
    async fn can_post(&self, conv: &ConvRef) -> Result<bool> {
        let channel = self.channel(conv)?;
        if self
            .lock_member_of()
            .get(channel)
            .is_some_and(|until| *until > Instant::now())
        {
            return Ok(true);
        }
        self.member(channel).await
    }

    /// Asks Slack as [`can_post`](Self::can_post) does, whatever it said
    /// lately, and keeps the answer: a no forgets an earlier yes.
    async fn can_post_now(&self, conv: &ConvRef) -> Result<bool> {
        let channel = self.channel(conv)?;
        self.member(channel).await
    }

    async fn upload(&self, to: &ReplyTarget, files: &[OutFile]) -> Result<()> {
        let channel = self.channel(&to.conv)?;
        self.api
            .upload_files(channel, to.thread_root.as_ref(), files)
            .await
            .map(drop)
    }

    /// Reads a thread with `conversations.replies`, or a conversation's top
    /// level with `conversations.history`. Joins, edits and other
    /// non-content subtypes are skipped, and a bot known only by its bot id
    /// is named by its user through `bots.info` when it has one.
    async fn history(
        &self,
        thread: &ThreadKey,
        before: Option<Cursor>,
        limit: usize,
    ) -> Result<Vec<Msg>> {
        let channel = self.channel(&thread.conv)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let before = before.as_ref().map(Cursor::as_str);
        match &thread.root {
            Some(root) => self.thread_history(channel, root, before, limit).await,
            None => self.channel_history(channel, before, limit).await,
        }
    }

    /// Slack's copy of the message, with this binding's bot token, so that
    /// nothing a forger put in the event decides anything:
    ///
    /// 1. A message whose `ts` is more than [`CONFIRM_WINDOW`] before the
    ///    event arrived is refused without a lookup.
    /// 2. The conversation's kind comes from `conversations.info`, cached
    ///    per channel ([`TeamDirectory::conv_kind`]), never from the event's
    ///    `channel_type`; a channel id Slack gives back spelled otherwise
    ///    is refused, so each message has one spelling to deduplicate by.
    /// 3. The message is read back whole with [`WebApi::message`], in the
    ///    thread the event names, and normalized with
    ///    [`normalize::read_back`], the ingress's rules, with this binding's
    ///    bot user: subtypes, sender, mentions (from `text` and `blocks`,
    ///    a bot's from `text` alone, none with a backtick on each side),
    ///    thread, files and whether the message addresses the bot: outside
    ///    a one-to-one DM, a mention of the bot or a thread reply under its
    ///    root. A bot known only by its bot id is named by its user, as
    ///    [`fill_bot_sender`](Self::fill_bot_sender) names it.
    /// 4. Whether the sender is from outside the workspace: its team fields,
    ///    then, unless they say so already,
    ///    [`fill_sender_team`](Self::fill_sender_team), so a sender is home
    ///    only when the member list or `users.info` agrees.
    ///
    /// None of the lookups [waits](WebApi::without_waiting) for its
    /// token's quota: past it, or while a 429 holds it, confirming fails
    /// at once with [`SurfaceError::RateLimited`], so forged events can't
    /// hold the caller's place behind the owner's token.
    ///
    /// The binding, event id and arrival time are the event's. `None` when
    /// Slack doesn't have the message, or has it in a form the ingress
    /// would drop.
    async fn confirm(&self, event: &InboundEvent) -> Result<Option<InboundEvent>> {
        let channel = self.channel(&event.conv)?;
        if !within_window(event.message.id.as_str(), event.received_at) {
            tracing::debug!(binding = %event.binding, message = %event.message.id, "a message older than the confirmation window; not reading it back");
            return Ok(None);
        }
        let api = self.api.without_waiting();
        let conv_kind = self.directory.conv_kind(&api, channel).await?;
        let Some(raw) = api
            .message(channel, event.thread_root.as_ref(), &event.message.id)
            .await?
        else {
            return Ok(None);
        };
        let context = Context {
            binding: event.binding,
            bot_user: self.bot_user.as_ref(),
            team: self.directory.team(),
            home_org: self.directory.home_org(),
            event_id: &event.event_id,
            received_at: event.received_at,
        };
        let mut copy = match normalize::read_back(&context, channel, conv_kind, &raw) {
            Ok(copy) => copy,
            Err(skip) => {
                tracing::debug!(binding = %event.binding, message = %event.message.id, reason = %skip, "Slack's copy of a message isn't one the ingress keeps");
                return Ok(None);
            }
        };
        self.fill_bot_sender(&mut copy).await?;
        self.fill_sender_team(&mut copy).await?;
        Ok(Some(copy))
    }

    /// Converts with the member cache as last read, and, when that is
    /// older than its TTL, starts a refresh in the background for the next
    /// call.
    fn render(&self, markdown: &str) -> Vec<String> {
        self.refresh_in_background();
        self.render_with(markdown, self.directory.members().as_ref())
    }

    fn caps(&self) -> Caps {
        CAPS
    }
}

/// Whether a message read back is content for [`Surface::history`]: a
/// message with no subtype, or with `file_share`, `thread_broadcast` or
/// `bot_message`, that has a sender and a valid `ts`. Joins, edits,
/// tombstones and the like are not.
fn is_content(message: &Message) -> bool {
    let subtype_kept = message
        .subtype
        .as_deref()
        .is_none_or(|subtype| KEPT_SUBTYPES.contains(&subtype) || subtype == "bot_message");
    subtype_kept
        && (message.user.is_some() || message.bot_id.is_some())
        && ts_parts(message.ts.as_str()).is_some()
}

/// A Slack `ts` (`"1727697600.000100"`) as seconds and microseconds.
fn ts_parts(ts: &str) -> Option<(i64, u32)> {
    let (seconds, fraction) = ts.split_once('.').unwrap_or((ts, ""));
    if seconds.is_empty()
        || fraction.len() > 6
        || !seconds
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let micros = format!("{fraction:0<6}").parse().ok()?;
    Some((seconds.parse().ok()?, micros))
}

/// Whether a message with this `ts` was posted at most [`CONFIRM_WINDOW`]
/// before `received_at`, when its event arrived.
pub(crate) fn within_window(ts: &str, received_at: OffsetDateTime) -> bool {
    ts_time(ts).is_some_and(|sent| sent >= received_at - CONFIRM_WINDOW)
}

/// When a message with this `ts` was sent.
fn ts_time(ts: &str) -> Option<OffsetDateTime> {
    let (seconds, micros) = ts_parts(ts)?;
    let nanos = i128::from(seconds) * 1_000_000_000 + i128::from(micros) * 1_000;
    OffsetDateTime::from_unix_timestamp_nanos(nanos).ok()
}

/// Whether `ts` is older than `before`. Unparsable values compare as text.
fn ts_before(ts: &str, before: &str) -> bool {
    match (ts_parts(ts), ts_parts(before)) {
        (Some(ts), Some(before)) => ts < before,
        _ => ts < before,
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn thread_links_open_slacks_web_client() {
        let conv = ConvRef {
            surface: SurfaceKind::Slack,
            team: "T012".into(),
            conversation: "C345".into(),
        };
        let channel = ThreadKey {
            conv: conv.clone(),
            root: None,
        };
        assert_eq!(
            thread_link(&channel).as_deref(),
            Some("https://app.slack.com/client/T012/C345")
        );
        let thread = ThreadKey {
            conv,
            root: Some("1727697600.000100".into()),
        };
        assert_eq!(
            thread_link(&thread).as_deref(),
            Some("https://app.slack.com/client/T012/C345/thread/C345-1727697600.000100")
        );
    }

    #[test]
    fn ts_values_parse_to_seconds_and_microseconds() {
        assert_eq!(ts_parts("1727697600.000100"), Some((1_727_697_600, 100)));
        assert_eq!(ts_parts("1727697600.5"), Some((1_727_697_600, 500_000)));
        assert_eq!(ts_parts("1727697600"), Some((1_727_697_600, 0)));
        for bad in ["", ".5", "1.2.3", "1.1234567", "x.1", "-1.0"] {
            assert_eq!(ts_parts(bad), None, "{bad}");
        }
        assert_eq!(
            ts_time("1727697600.000100"),
            Some(datetime!(2024-09-30 12:00:00.000100 UTC))
        );
    }

    #[test]
    fn ts_order_is_numeric() {
        assert!(ts_before("9.000001", "10.000000"));
        assert!(ts_before("10.000001", "10.000002"));
        assert!(!ts_before("10.000002", "10.000002"));
        assert!(ts_before("a", "b"));
    }
}

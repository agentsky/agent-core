//! [`RocketChatSurface`]: the [`Surface`] for one Rocket.Chat bot user.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use core_types::surface_trait::Result;
use core_types::{
    Binding, Caps, ConvRef, ConversationId, Cursor, InboundEvent, Limit, MemberKey, MessageId, Msg,
    MsgRef, OutFile, ReplyTarget, Sender, Surface, SurfaceError, SurfaceKind, TeamId, ThreadKey,
    UserId,
};
use render::MentionDirectory;
use time::OffsetDateTime;
use tokio::sync::mpsc;

use crate::normalize::{self, Context};
use crate::realtime::{RealtimeClient, RealtimeOptions, websocket_url};
use crate::rest::{Credentials, FileRef, Message, RestClient, RoomInfo};

/// The `source` under which Rocket.Chat events are recorded with
/// [`Dedup::mark_event_processed`].
pub const DEDUP_SOURCE: &str = "rocketchat";

/// How many raw messages may wait between the realtime connection and
/// normalization.
const RAW_BUFFER: usize = 256;

/// How many thread replies one `chat.getThreadMessages` call asks for:
/// the server's default `API_Upper_Count_Limit`.
const THREAD_PAGE: usize = 100;

/// The most `chat.getThreadMessages` pages one `history` call reads.
const MAX_THREAD_PAGES: usize = 50;

/// The most users [`BotRoles`] remembers.
const MAX_CACHED_USERS: usize = 10_000;

/// The most rooms a [`RocketChatSurface`] remembers `rooms.info` for.
const MAX_CACHED_ROOMS: usize = 10_000;

/// A map whose entries expire after `ttl` and which holds at most `cap` of
/// them: making room drops the expired entries, then the oldest.
struct Cache<K, V> {
    ttl: Duration,
    cap: usize,
    entries: HashMap<K, (V, Instant)>,
}

impl<K: Eq + Hash + Clone, V: Clone> Cache<K, V> {
    fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap,
            entries: HashMap::new(),
        }
    }

    fn get(&self, key: &K) -> Option<V> {
        self.entries
            .get(key)
            .filter(|(_, at)| at.elapsed() < self.ttl)
            .map(|(value, _)| value.clone())
    }

    fn insert(&mut self, key: K, value: V) {
        if !self.entries.contains_key(&key) && self.entries.len() >= self.cap {
            let ttl = self.ttl;
            self.entries.retain(|_, (_, at)| at.elapsed() < ttl);
            while self.entries.len() >= self.cap {
                let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, (_, at))| *at)
                    .map(|(key, _)| key.clone())
                else {
                    break;
                };
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(key, (value, Instant::now()));
    }
}

/// Records which events were already handled, so that each message is
/// delivered once although every bot connection in a room receives it.
///
/// agentd implements it over the store, whose
/// `Store::mark_event_processed(source, event_id)` has exactly this
/// contract. surface-rocketchat doesn't depend on the store crate, so the
/// store reaches the surface through this trait.
///
/// ```
/// use std::collections::HashSet;
/// use std::sync::Mutex;
/// use core_types::SurfaceError;
/// use surface_rocketchat::Dedup;
///
/// #[derive(Default)]
/// struct InMemory(Mutex<HashSet<(String, String)>>);
///
/// #[async_trait::async_trait]
/// impl Dedup for InMemory {
///     async fn mark_event_processed(
///         &self,
///         source: &str,
///         event_id: &str,
///     ) -> Result<bool, SurfaceError> {
///         let mut seen = self.0.lock().unwrap();
///         Ok(seen.insert((source.to_owned(), event_id.to_owned())))
///     }
/// }
/// ```
#[async_trait]
pub trait Dedup: Send + Sync {
    /// Records that the event `event_id` from `source` is being handled.
    /// Returns true the first time, and false when it was already recorded.
    /// Under concurrent callers exactly one gets true.
    async fn mark_event_processed(&self, source: &str, event_id: &str) -> Result<bool>;
}

/// Which senders are bots: users with the `bot` role.
///
/// Rocket.Chat messages don't carry the sender's roles, so they are read
/// with `users.info` and remembered for a while. `users.info` returns
/// another user's roles only to a caller with `view-full-other-user-info`,
/// so agentd builds one `BotRoles` from the manager's client and shares it
/// between every surface: whichever connection records a message first,
/// the sender is classified the same way. It remembers at most 10,000
/// users.
///
/// Cloning is cheap and shares the cache.
#[derive(Clone)]
pub struct BotRoles {
    inner: Arc<BotRolesInner>,
}

struct BotRolesInner {
    rest: RestClient,
    ttl: Duration,
    cache: Mutex<Cache<UserId, bool>>,
}

impl fmt::Debug for BotRoles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BotRoles")
            .field("user", self.inner.rest.user_id())
            .field("ttl", &self.inner.ttl)
            .finish_non_exhaustive()
    }
}

impl BotRoles {
    /// How long a lookup is remembered by default.
    pub const DEFAULT_TTL: Duration = Duration::from_secs(600);

    /// Looks roles up with `rest`, remembering each for
    /// [`DEFAULT_TTL`](Self::DEFAULT_TTL).
    pub fn new(rest: RestClient) -> Self {
        Self::with_ttl(rest, Self::DEFAULT_TTL)
    }

    /// Looks roles up with `rest`, remembering each for `ttl`.
    pub fn with_ttl(rest: RestClient, ttl: Duration) -> Self {
        Self {
            inner: Arc::new(BotRolesInner {
                rest,
                ttl,
                cache: Mutex::new(Cache::new(ttl, MAX_CACHED_USERS)),
            }),
        }
    }

    fn cache(&self) -> MutexGuard<'_, Cache<UserId, bool>> {
        self.inner
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `user` has the `bot` role.
    ///
    /// Every Rocket.Chat user has at least one role, so `users.info`
    /// answering with none means the caller may not see them. That fails
    /// with [`SurfaceError::Forbidden`] naming the missing
    /// `view-full-other-user-info` permission, instead of classifying every
    /// bot as a person.
    pub async fn is_bot(&self, user: &UserId) -> Result<bool> {
        if let Some(is_bot) = self.cache().get(user) {
            return Ok(is_bot);
        }
        let info = self.inner.rest.user_info(user).await?;
        if info.roles.is_empty() {
            return Err(SurfaceError::Forbidden(format!(
                "users.info showed no roles for {user}; the roles of {} need the \
                 view-full-other-user-info permission",
                self.inner.rest.user_id()
            )));
        }
        let is_bot = info.roles.iter().any(|role| role == "bot");
        self.cache().insert(user.clone(), is_bot);
        Ok(is_bot)
    }
}

/// How to reach one Rocket.Chat server as one bot user.
#[derive(Debug, Clone)]
pub struct RocketChatConfig {
    /// The server's base URL, such as `https://chat.example.com`.
    pub base_url: String,
    /// The realtime endpoint. `None` derives it from `base_url` with
    /// [`websocket_url`].
    pub websocket_url: Option<String>,
    /// The id agentd gives the server, which every [`MemberKey`] and
    /// [`ConvRef`] of this surface carries.
    pub team: TeamId,
    /// The bot user's id and personal access token.
    pub credentials: Credentials,
    /// The server's `Message_MaxAllowedSize`, in UTF-16 units.
    pub message_limit: Limit,
    /// Timing of the realtime connection.
    pub realtime: RealtimeOptions,
}

impl RocketChatConfig {
    /// A configuration with the default message limit
    /// ([`render::rocketchat::DEFAULT_MESSAGE_LIMIT`], 5,000 UTF-16 units)
    /// and default realtime timing.
    pub fn new(base_url: impl Into<String>, team: TeamId, credentials: Credentials) -> Self {
        Self {
            base_url: base_url.into(),
            websocket_url: None,
            team,
            credentials,
            message_limit: render::rocketchat::DEFAULT_MESSAGE_LIMIT,
            realtime: RealtimeOptions::default(),
        }
    }
}

/// The [`Surface`] for one Rocket.Chat bot user: an agent's, or the
/// manager's.
///
/// - [`events`](Surface::events) runs one realtime connection as the bot
///   (see [`realtime`](crate::realtime)), skips system messages, edits and
///   changes to old messages, normalizes the rest, and delivers each
///   message only if [`Dedup`] records it first. A bot's own messages are
///   delivered too; the pipeline and the router decide what to ignore.
/// - Posting, editing, reacting, uploading and reading history go through
///   the REST client as the bot.
/// - [`render`](Surface::render) converts with
///   [`render::rocketchat::to_markdown`] and splits to the message limit.
///   `@Name` mentions are left as written: `Surface::render` has no
///   directory to resolve them with, and on Rocket.Chat an `@username` in
///   the text is already a mention.
pub struct RocketChatSurface {
    rest: RestClient,
    realtime: RealtimeClient,
    team: TeamId,
    limit: Limit,
    dedup: Arc<dyn Dedup>,
    bots: BotRoles,
    rooms: Mutex<Cache<ConversationId, RoomInfo>>,
}

impl fmt::Debug for RocketChatSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RocketChatSurface")
            .field("team", &self.team)
            .field("user", self.rest.user_id())
            .finish_non_exhaustive()
    }
}

impl RocketChatSurface {
    /// A surface for the bot in `config`, deduplicating with `dedup` and
    /// telling bots apart with `bots`.
    pub fn new(config: RocketChatConfig, dedup: Arc<dyn Dedup>, bots: BotRoles) -> Result<Self> {
        let rest = RestClient::new(&config.base_url, config.credentials)?;
        let url = match config.websocket_url {
            Some(url) => url,
            None => websocket_url(&config.base_url)?,
        };
        let realtime = RealtimeClient::new(&url, rest.clone(), config.realtime)?;
        Ok(Self {
            rest,
            realtime,
            team: config.team,
            limit: config.message_limit,
            dedup,
            bots,
            rooms: Mutex::new(Cache::new(Duration::MAX, MAX_CACHED_ROOMS)),
        })
    }

    /// The REST client, acting as the bot.
    pub fn rest(&self) -> &RestClient {
        &self.rest
    }

    fn room<'c>(&self, conv: &'c ConvRef) -> Result<&'c ConversationId> {
        if conv.surface == SurfaceKind::RocketChat && conv.team == self.team {
            Ok(&conv.conversation)
        } else {
            Err(SurfaceError::NotFound(
                "conversation on another surface or server".into(),
            ))
        }
    }

    fn cached_rooms(&self) -> MutexGuard<'_, Cache<ConversationId, RoomInfo>> {
        self.rooms.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `rooms.info`, remembered for the surface's lifetime, for at most
    /// 10,000 rooms: a room's type and whether a DM has more than two
    /// members don't change.
    async fn room_info(&self, room: &ConversationId) -> Result<RoomInfo> {
        if let Some(info) = self.cached_rooms().get(room) {
            return Ok(info);
        }
        let info = self.rest.room_info(room).await?;
        self.cached_rooms().insert(room.clone(), info.clone());
        Ok(info)
    }

    /// Whether the sender is a bot. When its roles can't be read, the
    /// sender counts as a person: [`BotRoles`] is shared by every surface,
    /// so skipping the message would lose it on all of them, and the router
    /// looks every sender up as a managed agent whatever this says.
    async fn sender_is_bot(&self, message: &Message) -> bool {
        if message.bot {
            return true;
        }
        match self.bots.is_bot(&message.sender.id).await {
            Ok(is_bot) => is_bot,
            Err(err) => {
                tracing::warn!(
                    room = %message.room,
                    message = %message.id,
                    error = %err,
                    "could not read the sender's roles, treating the sender as a person"
                );
                false
            }
        }
    }

    /// Normalizes one message and delivers it if it is the first copy.
    /// Only a closed receiver is an error. A room that can't be read or a
    /// failure to record skips the message without recording it, so another
    /// connection can still deliver it.
    async fn deliver(
        &self,
        binding: &Binding,
        message: Message,
        tx: &Sender<InboundEvent>,
    ) -> Result<()> {
        let room = &message.room;
        if let Some(skip) = normalize::skip_reason(&message) {
            tracing::debug!(%room, message = %message.id, reason = skip.as_str(), "skipping message");
            return Ok(());
        }
        let conv_kind = match self.room_info(room).await {
            Ok(info) => match normalize::conv_kind(&info) {
                Some(kind) => kind,
                None => {
                    tracing::debug!(%room, "skipping a message in an unsupported room type");
                    return Ok(());
                }
            },
            Err(err) => {
                tracing::warn!(%room, message = %message.id, error = %err, "could not read the room, skipping message");
                return Ok(());
            }
        };
        let sender_is_bot = self.sender_is_bot(&message).await;
        match self
            .dedup
            .mark_event_processed(DEDUP_SOURCE, message.id.as_str())
            .await
        {
            Ok(true) => {}
            Ok(false) => return Ok(()),
            Err(err) => {
                tracing::warn!(%room, message = %message.id, error = %err, "could not record the message, skipping it");
                return Ok(());
            }
        }
        let file_url = |file: &FileRef| self.rest.file_url(file);
        let event = normalize::to_event(
            &message,
            &Context {
                binding: binding.id,
                team: &self.team,
                conv_kind,
                sender_is_bot,
                received_at: OffsetDateTime::now_utc(),
                file_url: &file_url,
            },
        );
        tx.send(event).await?;
        Ok(())
    }

    async fn thread_history(
        &self,
        root: &MessageId,
        before: Option<(&MessageId, OffsetDateTime)>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let older = |m: &Message| before.is_none_or(|(id, at)| m.id != *id && m.sent_at < at);
        let mut newest_first = Vec::new();
        let mut offset = 0;
        for _ in 0..MAX_THREAD_PAGES {
            let page = self.rest.thread_messages(root, offset, THREAD_PAGE).await?;
            if page.is_empty() {
                let root = self.rest.get_message(root).await?;
                if older(&root) {
                    newest_first.push(root);
                }
                break;
            }
            offset += page.len();
            newest_first.extend(page.into_iter().filter(|m| older(m)));
            if newest_first.len() >= limit {
                break;
            }
        }
        Ok(newest_first)
    }

    async fn to_msg(&self, message: Message) -> Msg {
        let sender_is_bot = self.sender_is_bot(&message).await;
        let file_url = |file: &FileRef| self.rest.file_url(file);
        Msg {
            files: message
                .files
                .iter()
                .map(|file| normalize::in_file(file, &file_url))
                .collect(),
            id: message.id,
            sender: MemberKey {
                surface: SurfaceKind::RocketChat,
                team: self.team.clone(),
                user: message.sender.id,
            },
            sender_is_bot,
            text: message.text,
            sent_at: message.sent_at,
        }
    }
}

/// A directory that resolves nothing, so `@Name` stays as written.
struct NoDirectory;

impl MentionDirectory for NoDirectory {
    fn resolve(&self, _name: &str) -> Option<String> {
        None
    }
}

#[async_trait]
impl Surface for RocketChatSurface {
    async fn events(&self, binding: &Binding, tx: Sender<InboundEvent>) -> Result<()> {
        let bot = &binding.bot;
        if bot.surface != SurfaceKind::RocketChat
            || bot.team != self.team
            || bot.user != *self.rest.user_id()
        {
            return Err(SurfaceError::Api(
                "the binding's bot user is not this surface's".into(),
            ));
        }
        let (raw_tx, mut raw_rx) = mpsc::channel(RAW_BUFFER);
        let deliver = async {
            while let Some(message) = raw_rx.recv().await {
                self.deliver(binding, message, &tx).await?;
            }
            Ok::<(), SurfaceError>(())
        };
        tokio::select! {
            result = self.realtime.run(raw_tx) => result?,
            result = deliver => result?,
        }
        Err(SurfaceError::Closed)
    }

    async fn post(&self, to: &ReplyTarget, text: &str) -> Result<MsgRef> {
        let room = self.room(&to.conv)?;
        let posted = self
            .rest
            .post_message(room, text, to.thread_root.as_ref())
            .await?;
        Ok(MsgRef {
            conv: to.conv.clone(),
            id: posted.id,
        })
    }

    async fn edit(&self, msg: &MsgRef, text: &str) -> Result<()> {
        let room = self.room(&msg.conv)?;
        self.rest.update_message(room, &msg.id, text).await
    }

    async fn react(&self, msg: &MsgRef, emoji: &str) -> Result<()> {
        self.room(&msg.conv)?;
        self.rest.react(&msg.id, emoji).await
    }

    async fn upload(&self, to: &ReplyTarget, files: &[OutFile]) -> Result<()> {
        let room = self.room(&to.conv)?;
        for file in files {
            self.rest
                .upload(room, to.thread_root.as_ref(), file)
                .await?;
        }
        Ok(())
    }

    async fn history(
        &self,
        thread: &ThreadKey,
        before: Option<Cursor>,
        limit: usize,
    ) -> Result<Vec<Msg>> {
        let room = self.room(&thread.conv)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        let before_id = before.map(|cursor| MessageId::new(cursor.as_str()));
        let before_at = match &before_id {
            Some(id) => Some(self.rest.get_message(id).await?.sent_at),
            None => None,
        };
        let newest_first = match &thread.root {
            None => {
                let info = self.room_info(room).await?;
                self.rest
                    .room_history(room, &info.room_type, before_at, limit)
                    .await?
            }
            Some(root) => {
                let before = before_id.as_ref().zip(before_at);
                self.thread_history(root, before, limit).await?
            }
        };
        let mut messages = Vec::with_capacity(limit.min(newest_first.len()));
        for message in newest_first {
            if messages.len() == limit {
                break;
            }
            if !normalize::is_system(&message) {
                messages.push(self.to_msg(message).await);
            }
        }
        messages.reverse();
        Ok(messages)
    }

    fn render(&self, markdown: &str) -> Vec<String> {
        let text = render::rocketchat::to_markdown(markdown, &NoDirectory);
        render::split(&text, self.limit)
    }

    fn caps(&self) -> Caps {
        Caps {
            message_limit: self.limit,
            supports_edit: true,
            supports_buttons: false,
            supports_threads: true,
            per_binding_delivery: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface() -> RocketChatSurface {
        let creds = Credentials {
            user_id: "bot".into(),
            token: "t".into(),
        };
        let config = RocketChatConfig::new("http://chat.example", "chat.example".into(), creds);
        let rest = RestClient::new(&config.base_url, config.credentials.clone()).unwrap();
        struct Never;
        #[async_trait]
        impl Dedup for Never {
            async fn mark_event_processed(&self, _: &str, _: &str) -> Result<bool> {
                Ok(false)
            }
        }
        RocketChatSurface::new(config, Arc::new(Never), BotRoles::new(rest)).unwrap()
    }

    #[test]
    fn caps_match_the_plan() {
        let caps = surface().caps();
        assert_eq!(
            caps.message_limit,
            render::rocketchat::DEFAULT_MESSAGE_LIMIT
        );
        assert!(caps.supports_edit);
        assert!(caps.supports_threads);
        assert!(!caps.supports_buttons);
        assert!(!caps.per_binding_delivery);
    }

    #[test]
    fn conversations_of_other_surfaces_or_servers_are_refused() {
        let surface = surface();
        let mut conv = ConvRef {
            surface: SurfaceKind::RocketChat,
            team: "chat.example".into(),
            conversation: "R1".into(),
        };
        assert_eq!(surface.room(&conv).unwrap().as_str(), "R1");
        conv.team = "other.example".into();
        assert!(matches!(
            surface.room(&conv),
            Err(SurfaceError::NotFound(_))
        ));
        conv.team = "chat.example".into();
        conv.surface = SurfaceKind::Slack;
        assert!(matches!(
            surface.room(&conv),
            Err(SurfaceError::NotFound(_))
        ));
    }

    #[test]
    fn the_cache_forgets_expired_entries_and_holds_at_most_its_cap() {
        let mut cache = Cache::new(Duration::from_secs(60), 2);
        cache.insert("a", 1);
        std::thread::sleep(Duration::from_millis(2));
        cache.insert("b", 2);
        cache.insert("b", 3);
        cache.insert("c", 4);
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.get(&"a"), None);
        assert_eq!(cache.get(&"b"), Some(3));
        assert_eq!(cache.get(&"c"), Some(4));

        let mut expiring = Cache::new(Duration::ZERO, 2);
        expiring.insert("a", 1);
        assert_eq!(expiring.get(&"a"), None);
        expiring.insert("b", 2);
        expiring.insert("c", 3);
        assert_eq!(expiring.entries.len(), 1);
    }

    #[test]
    fn debug_output_names_the_bot_but_no_token() {
        let surface = surface();
        let text = format!("{surface:?} {:?}", surface.bots);
        assert!(text.contains("bot"));
        assert!(!text.contains("\"t\""));
    }
}

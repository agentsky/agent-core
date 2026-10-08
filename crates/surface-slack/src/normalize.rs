//! Turning a Slack `message` event into an [`InboundEvent`].
//!
//! Agent apps subscribe to `message.channels`, `message.groups`,
//! `message.im` and `message.mpim`, not `app_mention`, so every message in
//! every conversation the bot is in arrives here. [`message`] keeps what the
//! router can use and says why it drops the rest:
//!
//! - Only plain messages and the `file_share` and `thread_broadcast`
//!   subtypes are kept. Edits, deletions, joins, `bot_message` posts from
//!   classic integrations and the other subtypes are dropped.
//! - What is kept is bounded, since an agent's owner can sign an event
//!   with anything in it (see [Bounds](#bounds)).
//! - Outside a one-to-one DM (`channel_type` other than `im`), a message
//!   is kept only if it mentions the binding's bot user among those, or
//!   replies in a thread whose root the bot may have posted: one whose
//!   `parent_user_id` is the bot user, or whose root's author isn't known,
//!   because the reply has no `parent_user_id` shaped like a user id or the
//!   bot user isn't known. The router answers a person in a channel or a
//!   group DM only for a mention of the agent or a reply under one of the
//!   agent's own messages, and another agent only for a mention, so what is
//!   dropped here is what it would ignore. Whether the root is the agent's
//!   own message stays the router's question, answered from `reply_to` and
//!   the messages agentd recorded; `parent_user_id` only drops replies
//!   under a root that can't be.
//! - In every kind of conversation, the bot's own posts are dropped, and so
//!   is another bot's message that doesn't mention the bot user, when the
//!   bot user is known: the router ignores an agent's own posts, and
//!   answers a bot only when it is another agent that mentions this one.
//! - `thread_ts` becomes both `thread_root` and `reply_to`, unless it equals
//!   the message's own `ts`, which makes the message the root itself.
//! - A bot sender (`bot_id` or `bot_profile`) with a `user` field has that
//!   user id in `sender.user` and `sender_bot_user`. Without one, its
//!   `bot_id` is `sender.user` and `sender_bot_user` is `None`, until the Web
//!   API's `bots.info` lookup fills both (see
//!   [`InboundEvent`]'s "Bot senders").
//! - Mentions are the `<@U…>` tokens in `text`, then the `user` elements of
//!   `rich_text` blocks and the tokens in `mrkdwn` text objects, each user
//!   once, in order of first appearance. Text typed inside a `rich_text`
//!   block is not scanned: a literal `<@U…>` there is not a mention. A
//!   token with a backtick somewhere before it and another after it in the
//!   same text is not one either, since Slack might show it as code
//!   ([`mentions`]); a person's mention there is still read from the
//!   `user` element their client sends. A bot's mentions are read from
//!   `text` alone, as agentd reads what it posted, so the copy of an
//!   agent's post Slack delivers, with whatever blocks Slack makes of its
//!   text, hands off to the agents agentd's own delivery did: agentd's
//!   agents post text, and the router ignores every other bot.
//! - The team is the workspace the event came through, its installation's
//!   `authorizations[0].team_id` ([`Context::team`]), for the sender and
//!   the conversation alike, whoever sent it: an outside member is keyed
//!   by the workspace too.
//! - Whether the sender is from outside that workspace comes from the
//!   message's own team fields (see [Who is outside](#who-is-outside)).
//! - A bot's message that was `edited` is dropped: agentd never edits its
//!   agents' posts, so someone else holding the bot's token did.
//!
//! # Who is outside
//!
//! A message names its sender's team in up to four fields, read in this
//! order by `MessageEvent::sender_teams`, the one place that lists them:
//! `user_team`, `source_team`, `user_profile.team` and `team`. Bolt reads
//! `user_team` before `team`, and in one of its fixtures an outside actor
//! has the installing team in `team` and their own only in the others.
//! One that is present and not shaped like a team id ([`is_team_id`])
//! makes the message [`Skip::Malformed`]. When a field
//! names neither the workspace ([`Context::team`]) nor its Enterprise Grid
//! organization ([`Context::home_org`]), the sender is outside, with the
//! first such field, in that order, as their organization. Otherwise
//! [`InboundEvent::outside`] is `None`, which only the event's own first
//! routing takes as home: the fields can make a sender outside, never
//! home, and Slack's copy of the message, when its own fields leave the
//! sender home too, is kept only when the home check agrees
//! ([`SlackSurface::copy_sender_is_home`](crate::SlackSurface::copy_sender_is_home)).
//! Slack's fixtures disagree on which field names an outside actor, so
//! every field counts and none alone.
//!
//! [`read_back`] runs the same rules on a message read back from
//! `conversations.history` or `conversations.replies`, which carries no
//! `channel` or `channel_type`: the caller names the channel and its kind,
//! as `conversations.info` gives it.
//!
//! # Bounds
//!
//! Every id a message is kept with must be shaped like Slack's, or the
//! message is [`Skip::Malformed`]: its `channel` ([`is_channel_id`]), its
//! `ts` and `thread_ts` ([`is_ts`]), its `user` ([`is_user_id`]) and its
//! `bot_id` ([`is_bot_id`]). A `parent_user_id` isn't kept, only compared
//! with the bot user, so one that isn't a string shaped like a user id is
//! taken as missing, which keeps the reply. Each shape leaves room for
//! Slack's ids to grow, up to [`MAX_ID_TAIL`] characters after the
//! prefix. The ingress refuses an event whose `channel`, `ts` or
//! `thread_ts` isn't shaped so with 400 before it is acknowledged, since
//! they make up its deduplication key; a sender that isn't is dropped
//! here, after the 200, before any row. The rest is cut to Slack's own
//! limits:
//!
//! - `text` to at most [`MAX_TEXT_BYTES`], at a character boundary: as
//!   many bytes as Slack's limit of 40,000 characters can take. Slack's
//!   escaping of `&`, `<` and `>` lengthens the text a member typed, `&`
//!   to five characters, so a cut in characters of the escaped text could
//!   cut a real message short; a cut in bytes keeps the same bound and
//!   more of the message. A person's mention past the cut is still read
//!   from `blocks`, where Slack's clients put each one too; a bot's
//!   mentions are read from `text` alone.
//! - The sender's team fields are kept only as the [`Outside`] they make,
//!   each a team id [`is_team_id`] accepts.
//! - Mentions to the first [`MAX_MENTIONS`] different users, each an id
//!   [`is_user_id`] accepts; the router looks each one up.
//! - Files to the first [`MAX_FILES`] the bot can download, each with an id
//!   [`is_file_id`] accepts and a URL of at most [`MAX_FILE_URL_BYTES`],
//!   its name cut to [`MAX_FILE_NAME_CHARS`] characters and a MIME type
//!   longer than [`MAX_MIME_TYPE_BYTES`] left out.
//!
//! So a kept message is at most about 225 KB, whatever the event held:
//! 160 KB of text, 55 KB of files and 7 KB of mentions.

use std::collections::HashSet;
use std::fmt;

use core_types::{
    BindingId, ConvKind, ConvRef, ConversationId, InFile, InboundEvent, MAX_MENTIONS, MemberKey,
    MsgRef, Outside, SurfaceKind, TeamId, UserId,
};
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use time::OffsetDateTime;

/// The `message` subtypes that are kept. A message with no subtype is kept
/// too.
pub const KEPT_SUBTYPES: [&str; 2] = ["file_share", "thread_broadcast"];

/// The most bytes of a message's `text` kept: Slack's own limit of 40,000
/// characters, at the 4 bytes each that UTF-8 takes at most.
pub const MAX_TEXT_BYTES: usize = 4 * 40_000;

/// The most files a message is kept with: Slack's own limit.
pub const MAX_FILES: usize = 10;

/// The most characters of a file's name kept.
pub const MAX_FILE_NAME_CHARS: usize = 255;

/// The longest download URL a kept file may have, in bytes. Slack's hold
/// the team, the file id and the file's name, URL-encoded.
pub const MAX_FILE_URL_BYTES: usize = 4096;

/// The longest MIME type a kept file carries, in bytes.
pub const MAX_MIME_TYPE_BYTES: usize = 255;

/// The most bytes of an ignored subtype that [`Skip::Subtype`] carries.
const MAX_SUBTYPE_BYTES: usize = 64;

/// What [`message`] needs besides the event.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The binding whose app received the event.
    pub binding: BindingId,
    /// The binding's bot user, if known. Without it, messages outside
    /// one-to-one DMs are kept only when they reply in a thread, whoever
    /// posted its root, and bots' messages are kept whatever they mention.
    pub bot_user: Option<&'a UserId>,
    /// The workspace the event came through: its installation's,
    /// `authorizations[0].team_id`, never the envelope's `team_id`; for a
    /// message read back, the binding's. The sender and the conversation
    /// are keyed by it.
    pub team: &'a TeamId,
    /// The workspace's Enterprise Grid organization, the `enterprise_id`
    /// `auth.test` gave at startup, if it has one. A sender team field
    /// naming it doesn't make the sender outside.
    pub home_org: Option<&'a TeamId>,
    /// The envelope's `event_id`.
    pub event_id: &'a str,
    /// When agentd received the request.
    pub received_at: OffsetDateTime,
}

/// Why [`message`] dropped an event. Logged by kind; none of the variants
/// carry message content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Skip {
    /// The event isn't shaped like a message event.
    #[error("the event is not a well-formed message")]
    Malformed,
    /// The message has a subtype other than those in [`KEPT_SUBTYPES`].
    #[error("message subtype {0} is ignored")]
    Subtype(String),
    /// The event names neither a `user` nor a `bot_id`.
    #[error("the message has no sender")]
    NoSender,
    /// A message the router would ignore: the bot's own post, another
    /// bot's that doesn't mention the bot, or one outside a one-to-one DM
    /// that neither mentions the bot nor replies in a thread whose root the
    /// bot may have posted.
    #[error("a message that doesn't address the bot")]
    NotAddressed,
    /// A bot's message that was edited.
    #[error("a bot's message was edited")]
    EditedByBot,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct MessageEvent {
    subtype: Option<String>,
    channel: Option<String>,
    channel_type: Option<String>,
    user: Option<String>,
    bot_id: Option<String>,
    bot_profile: Option<IgnoredAny>,
    text: Option<String>,
    ts: Option<String>,
    thread_ts: Option<String>,
    #[serde(rename = "parent_user_id", deserialize_with = "user_id_or_nothing")]
    parent_user: Option<UserId>,
    blocks: Option<Value>,
    files: Option<Vec<SlackFile>>,
    edited: Option<IgnoredAny>,
    team: Option<String>,
    user_team: Option<String>,
    source_team: Option<String>,
    user_profile: Option<UserProfile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct UserProfile {
    team: Option<String>,
}

impl MessageEvent {
    /// The sender's team fields, `user_team`, `source_team`,
    /// `user_profile.team` and `team`, each that is present, in the order
    /// the first that names another organization is taken as theirs.
    fn sender_teams(&self) -> impl Iterator<Item = &str> {
        [
            self.user_team.as_deref(),
            self.source_team.as_deref(),
            self.user_profile
                .as_ref()
                .and_then(|profile| profile.team.as_deref()),
            self.team.as_deref(),
        ]
        .into_iter()
        .flatten()
    }

    /// Whether the sender is outside the workspace, as [Who is
    /// outside](self#who-is-outside) says, or `Err` when a field isn't
    /// shaped like a team id.
    fn outside(&self, context: &Context<'_>) -> Result<Option<Outside>, Skip> {
        if !self.sender_teams().all(is_team_id) {
            return Err(Skip::Malformed);
        }
        Ok(self
            .sender_teams()
            .find(|team| {
                *team != context.team.as_str()
                    && context.home_org.is_none_or(|org| *team != org.as_str())
            })
            .map(|team| Outside { team: team.into() }))
    }
}

/// A string [`is_user_id`] accepts, or `None` for any other value, read
/// without keeping what it skips.
fn user_id_or_nothing<'de, D: Deserializer<'de>>(value: D) -> Result<Option<UserId>, D::Error> {
    Ok(shaped_or_nothing(value, is_user_id)?.map(UserId::from))
}

/// A string [`is_team_id`] accepts, or `None` for any other value, read
/// without keeping what it skips: how an Events API payload's
/// `authorizations[0].team_id` is read, so one in another form is no
/// installation rather than an unreadable body.
pub(crate) fn team_id_or_nothing<'de, D: Deserializer<'de>>(
    value: D,
) -> Result<Option<TeamId>, D::Error> {
    Ok(shaped_or_nothing(value, is_team_id)?.map(TeamId::from))
}

/// A string [`is_enterprise_id`] accepts, or `None` for any other value,
/// read without keeping what it skips.
pub(crate) fn enterprise_id_or_nothing<'de, D: Deserializer<'de>>(
    value: D,
) -> Result<Option<TeamId>, D::Error> {
    Ok(shaped_or_nothing(value, is_enterprise_id)?.map(TeamId::from))
}

/// A string `shape` accepts, or `None` for any other value, read without
/// keeping what it skips.
pub(crate) fn shaped_or_nothing<'de, D: Deserializer<'de>>(
    value: D,
    shape: fn(&str) -> bool,
) -> Result<Option<String>, D::Error> {
    value.deserialize_any(ShapedOrNothing(shape))
}

struct ShapedOrNothing(fn(&str) -> bool);

impl<'de> Visitor<'de> for ShapedOrNothing {
    type Value = Option<String>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any value")
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Self::Value, E> {
        Ok((self.0)(text).then(|| text.to_owned()))
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_some<D: Deserializer<'de>>(self, value: D) -> Result<Self::Value, D::Error> {
        value.deserialize_any(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut items: A) -> Result<Self::Value, A::Error> {
        while items.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Self::Value, A::Error> {
        while entries.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(None)
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(crate) struct SlackFile {
    id: Option<String>,
    name: Option<String>,
    title: Option<String>,
    mimetype: Option<String>,
    size: Option<u64>,
    url_private: Option<String>,
    url_private_download: Option<String>,
}

/// Normalizes the `event` object of an `event_callback` whose type is
/// `message`.
///
/// # Errors
///
/// A [`Skip`] saying why the message is not for the router.
pub fn message(context: &Context<'_>, event: &Value) -> Result<InboundEvent, Skip> {
    let event = MessageEvent::deserialize(event).map_err(|_| Skip::Malformed)?;
    let conv_kind = match event.channel_type.as_deref() {
        Some("im") => ConvKind::Dm,
        Some("mpim") => ConvKind::GroupDm,
        _ => ConvKind::Channel,
    };
    let channel = event
        .channel
        .clone()
        .filter(|channel| is_channel_id(channel))
        .ok_or(Skip::Malformed)?;
    normalized(context, event, channel.into(), conv_kind)
}

/// Normalizes `message`, read back from `conversations.history` or
/// `conversations.replies` in `channel`, a conversation of kind
/// `conv_kind`, as [`message`] normalizes an event. A `channel` or
/// `channel_type` in `message` is ignored.
///
/// # Errors
///
/// A [`Skip`] saying why the message is not for the router.
pub fn read_back(
    context: &Context<'_>,
    channel: &ConversationId,
    conv_kind: ConvKind,
    message: &Value,
) -> Result<InboundEvent, Skip> {
    let message = MessageEvent::deserialize(message).map_err(|_| Skip::Malformed)?;
    normalized(context, message, channel.clone(), conv_kind)
}

fn normalized(
    context: &Context<'_>,
    mut event: MessageEvent,
    channel: ConversationId,
    conv_kind: ConvKind,
) -> Result<InboundEvent, Skip> {
    if let Some(subtype) = &event.subtype
        && !KEPT_SUBTYPES.contains(&subtype.as_str())
    {
        return Err(Skip::Subtype(
            truncated(subtype, MAX_SUBTYPE_BYTES).to_owned(),
        ));
    }
    let Some(ts) = event.ts.take().filter(|ts| is_ts(ts)) else {
        return Err(Skip::Malformed);
    };
    let shaped = event.user.as_deref().is_none_or(is_user_id)
        && event.bot_id.as_deref().is_none_or(is_bot_id)
        && event.thread_ts.as_deref().is_none_or(is_ts);
    if !shaped {
        return Err(Skip::Malformed);
    }
    let outside = event.outside(context)?;
    let is_bot = event.bot_id.is_some() || event.bot_profile.is_some();
    if is_bot && event.edited.is_some() {
        return Err(Skip::EditedByBot);
    }
    let (sender, sender_bot_user) = match (event.user, event.bot_id) {
        (Some(user), _) => {
            let user = UserId::from(user);
            let bot_user = is_bot.then(|| user.clone());
            (user, bot_user)
        }
        (None, Some(bot_id)) => (UserId::from(bot_id), None),
        (None, None) => return Err(Skip::NoSender),
    };
    let mut text = event.text.unwrap_or_default();
    text.truncate(truncated(&text, MAX_TEXT_BYTES).len());
    let mentions = mentions(&text, event.blocks.as_ref().filter(|_| !is_bot));
    let thread_root = event.thread_ts.filter(|root| *root != ts);
    let mentioned = context.bot_user.is_some_and(|bot| mentions.contains(bot));
    let own = context.bot_user.is_some_and(|bot| *bot == sender);
    let unaddressed_bot = is_bot && context.bot_user.is_some() && !mentioned;
    let root_by_someone_else = event
        .parent_user
        .zip(context.bot_user)
        .is_some_and(|(parent, bot)| parent != *bot);
    let unaddressed_here =
        conv_kind != ConvKind::Dm && (thread_root.is_none() || root_by_someone_else) && !mentioned;
    if own || unaddressed_bot || unaddressed_here {
        return Err(Skip::NotAddressed);
    }
    let conv = ConvRef {
        surface: SurfaceKind::Slack,
        team: context.team.clone(),
        conversation: channel,
    };
    let reply_to = thread_root.as_ref().map(|root| MsgRef {
        conv: conv.clone(),
        id: root.as_str().into(),
    });
    Ok(InboundEvent {
        event_id: context.event_id.to_owned(),
        binding: context.binding,
        sender: MemberKey {
            surface: SurfaceKind::Slack,
            team: context.team.clone(),
            user: sender,
        },
        outside,
        sender_is_bot: is_bot,
        sender_bot_user,
        conv: conv.clone(),
        conv_kind,
        thread_root: thread_root.map(Into::into),
        message: MsgRef {
            conv,
            id: ts.into(),
        },
        text,
        mentions,
        reply_to,
        files: in_files(event.files.unwrap_or_default()),
        received_at: context.received_at,
    })
}

/// The first [`MAX_FILES`] of `files` the bot can download, bounded as
/// [Bounds](self#bounds) says.
pub(crate) fn in_files(files: Vec<SlackFile>) -> Vec<InFile> {
    files
        .into_iter()
        .filter_map(in_file)
        .take(MAX_FILES)
        .collect()
}

/// A file the bot can download: one with an id shaped like Slack's and a
/// private URL of at most [`MAX_FILE_URL_BYTES`]. Files Slack withholds
/// (`hidden_by_limit`, or Slack Connect files that need `files.info`
/// first) carry neither and are left out.
fn in_file(file: SlackFile) -> Option<InFile> {
    let id = file.id.filter(|id| is_file_id(id))?;
    let url = file
        .url_private_download
        .or(file.url_private)
        .filter(|url| url.len() <= MAX_FILE_URL_BYTES)?;
    let mut name = file.name.or(file.title).unwrap_or_else(|| id.clone());
    name.truncate(char_boundary(&name, MAX_FILE_NAME_CHARS));
    Some(InFile {
        id,
        name,
        mime_type: file
            .mimetype
            .filter(|mime| mime.len() <= MAX_MIME_TYPE_BYTES),
        size: file.size,
        url,
    })
}

/// The byte offset of `text`'s `chars`th character, or its length when it
/// has no more.
fn char_boundary(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(at, _)| at)
}

/// `text`, cut to at most `max` bytes at a character boundary.
fn truncated(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Decodes the three entities Slack writes in message and slash command
/// text, `&amp;`, `&lt;` and `&gt;`, so text reads as the member typed it.
/// Nothing else is decoded, and a decoded `&amp;` is not decoded again:
/// `&amp;lt;` becomes `&lt;`.
///
/// A literal `<@U…>` the member typed then looks like a mention token, so
/// decode only text that is parsed for its words, such as a command, never
/// text that mentions are read from.
///
/// ```
/// use surface_slack::normalize::unescape;
///
/// assert_eq!(unescape("a &lt;b&gt; &amp;amp; c"), "a <b> &amp; c");
/// assert_eq!(unescape("&quot;"), "&quot;");
/// ```
pub fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let (decoded, len) = [("&amp;", '&'), ("&lt;", '<'), ("&gt;", '>')]
            .into_iter()
            .find(|(entity, _)| rest.starts_with(entity))
            .map_or(('&', 1), |(entity, ch)| (ch, entity.len()));
        out.push(decoded);
        rest = &rest[len..];
    }
    out.push_str(rest);
    out
}

/// The first [`MAX_MENTIONS`] users mentioned in `text` and `blocks`, once
/// each, in order of first appearance: `<@U…>` tokens in `text`, then
/// `rich_text` `user` elements and tokens in `mrkdwn` text objects. A
/// token Slack might show as code is no mention
/// ([`without_code`](render::slack::without_code)): agentd's own post and
/// the platform's copy of it are read alike, so neither hands off what the
/// thread sees as code.
pub fn mentions(text: &str, blocks: Option<&Value>) -> Vec<UserId> {
    let mut found = Vec::new();
    scan_tokens(text, &mut found);
    if let Some(blocks) = blocks {
        walk_blocks(blocks, &mut found);
    }
    let mut seen = HashSet::with_capacity(found.len());
    found.retain(|user| seen.insert(user.clone()));
    found.truncate(MAX_MENTIONS);
    found
}

fn walk_blocks(value: &Value, found: &mut Vec<UserId>) {
    match value {
        Value::Array(items) => {
            for item in items {
                walk_blocks(item, found);
            }
        }
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str);
            match (kind, object.get("user_id"), object.get("text")) {
                (Some("user"), Some(Value::String(user)), _) if is_user_id(user) => {
                    found.push(UserId::from(user.as_str()));
                }
                (Some("mrkdwn"), _, Some(Value::String(text))) => scan_tokens(text, found),
                _ => {}
            }
            for (key, child) in object {
                if key != "text" || !child.is_string() {
                    walk_blocks(child, found);
                }
            }
        }
        _ => {}
    }
}

/// Collects the ids of `<@U…>` and `<@U…|label>` tokens outside what
/// Slack might show as code.
fn scan_tokens(text: &str, found: &mut Vec<UserId>) {
    let text = render::slack::without_code(text);
    let mut rest = text.as_str();
    while let Some(at) = rest.find("<@") {
        rest = &rest[at + 2..];
        let end = rest
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(rest.len());
        let id = &rest[..end];
        let closed = match rest[end..].chars().next() {
            Some('>') => true,
            Some('|') => rest[end..].contains('>'),
            _ => false,
        };
        if closed && is_user_id(id) {
            found.push(UserId::from(id));
        }
        rest = &rest[end..];
    }
}

/// The most characters a Slack id has after its prefix, as this crate
/// checks them. Slack's are about ten now, and Slack says they may grow.
pub const MAX_ID_TAIL: usize = 64;

/// Whether `id` is one of `prefixes` and then 1 to `max` uppercase ASCII
/// letters or digits, as Slack's ids are.
fn is_slack_id(id: &str, prefixes: &[&str], max: usize) -> bool {
    prefixes.iter().any(|prefix| {
        id.strip_prefix(prefix).is_some_and(|rest| {
            (1..=max).contains(&rest.len())
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        })
    })
}

/// Whether `id` is shaped like a Slack user id: `U` or `W`, then 1 to
/// [`MAX_ID_TAIL`] uppercase letters or digits.
pub fn is_user_id(id: &str) -> bool {
    is_slack_id(id, &["U", "W"], MAX_ID_TAIL)
}

/// Whether `id` is shaped like a Slack bot id: `B`, then 1 to
/// [`MAX_ID_TAIL`] uppercase letters or digits.
pub fn is_bot_id(id: &str) -> bool {
    is_slack_id(id, &["B"], MAX_ID_TAIL)
}

/// Whether `id` is shaped like a Slack file id: `F`, then 1 to
/// [`MAX_ID_TAIL`] uppercase letters or digits.
pub fn is_file_id(id: &str) -> bool {
    is_slack_id(id, &["F"], MAX_ID_TAIL)
}

/// Whether `id` is shaped like a Slack team id: `T`, or `E` for an
/// Enterprise Grid organization, then 1 to [`MAX_ID_TAIL`] uppercase
/// letters or digits.
pub fn is_team_id(id: &str) -> bool {
    is_slack_id(id, &["T", "E"], MAX_ID_TAIL)
}

/// Whether `id` is shaped like a Slack workspace's id: `T`, then 1 to
/// [`MAX_ID_TAIL`] uppercase letters or digits.
pub fn is_workspace_id(id: &str) -> bool {
    is_slack_id(id, &["T"], MAX_ID_TAIL)
}

/// Whether `id` is shaped like a Slack Enterprise Grid organization's id:
/// `E`, then 1 to [`MAX_ID_TAIL`] uppercase letters or digits.
pub fn is_enterprise_id(id: &str) -> bool {
    is_slack_id(id, &["E"], MAX_ID_TAIL)
}

/// Whether `id` is shaped like a Slack conversation id: `C`, `D` or `G`,
/// then 1 to [`MAX_ID_TAIL`] uppercase letters or digits.
pub fn is_channel_id(id: &str) -> bool {
    is_slack_id(id, &["C", "D", "G"], MAX_ID_TAIL)
}

/// Whether `id` is shaped like a Slack `event_id`: `Ev`, then 1 to
/// [`MAX_ID_TAIL`] uppercase letters or digits.
pub fn is_event_id(id: &str) -> bool {
    is_slack_id(id, &["Ev"], MAX_ID_TAIL)
}

/// Whether `ts` is a message timestamp as Slack writes one: seconds, a dot
/// and 6 digits of microseconds. The seconds are 10 digits now; up to 20
/// are accepted, the first not a zero, so each message still has one
/// spelling to be deduplicated by.
pub fn is_ts(ts: &str) -> bool {
    ts.split_once('.').is_some_and(|(seconds, micros)| {
        (10..=20).contains(&seconds.len())
            && !seconds.starts_with('0')
            && micros.len() == 6
            && seconds
                .bytes()
                .chain(micros.bytes())
                .all(|b| b.is_ascii_digit())
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use time::macros::datetime;

    use super::*;

    const BOT: &str = "U0BOT";

    fn normalize(event: Value) -> Result<InboundEvent, Skip> {
        let bot = UserId::from(BOT);
        let team = TeamId::from("T0TEAM");
        let context = Context {
            binding: BindingId::from_uuid(uuid::Uuid::nil()),
            bot_user: Some(&bot),
            team: &team,
            home_org: None,
            event_id: "Ev1",
            received_at: datetime!(2026-09-30 12:00 UTC),
        };
        message(&context, &event)
    }

    fn channel_message(extra: Value) -> Value {
        let mut event = json!({
            "type": "message",
            "channel": "C1",
            "channel_type": "channel",
            "user": "U1",
            "text": "hello <@U0BOT>",
            "ts": "1727697600.000100",
        });
        for (key, value) in extra.as_object().unwrap() {
            event[key] = value.clone();
        }
        event
    }

    #[test]
    fn token_scanning_accepts_labels_and_rejects_non_users() {
        let mut found = Vec::new();
        scan_tokens(
            "<@U1> <@W2|ada> <@U3 <@u4> <@B5> <@U6|no close <!here> <#C1> <@> <@U7>",
            &mut found,
        );
        assert_eq!(
            found,
            [
                UserId::from("U1"),
                UserId::from("W2"),
                "U6".into(),
                "U7".into()
            ]
        );
        let mut found = Vec::new();
        scan_tokens("<@U8", &mut found);
        scan_tokens("<@U9é>", &mut found);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn mentions_are_unique_and_ordered() {
        let blocks = json!([
            {"type": "rich_text", "elements": [
                {"type": "rich_text_section", "elements": [
                    {"type": "user", "user_id": "U2"},
                    {"type": "text", "text": " typed <@U9> literally "},
                    {"type": "user", "user_id": "U1"},
                ]},
                {"type": "rich_text_list", "elements": [
                    {"type": "rich_text_section", "elements": [
                        {"type": "user", "user_id": "U3"},
                    ]},
                ]},
            ]},
            {"type": "section", "text": {"type": "mrkdwn", "text": "cc <@U4>"}},
            {"type": "section", "text": {"type": "plain_text", "text": "<@U8>"}},
            {"type": "context", "elements": [{"type": "mrkdwn", "text": "<@U5|five>"}]},
        ]);
        let found = mentions("<@U1> and <@U1>", Some(&blocks));
        let ids: Vec<&str> = found.iter().map(UserId::as_str).collect();
        assert_eq!(ids, ["U1", "U2", "U3", "U4", "U5"]);
    }

    #[test]
    fn a_token_slack_might_show_as_code_mentions_no_one() {
        let blocks = json!([
            {"type": "section", "text": {"type": "mrkdwn", "text": "`<@U4>` and `a` <@U5> `b`"}},
            {"type": "section", "text": {"type": "mrkdwn", "text": "```\nx\n``` <@U6>"}},
        ]);
        let found = mentions("`<@U1>` over to <@U2>, see `x`y` <@U3> `z`", Some(&blocks));
        let ids: Vec<&str> = found.iter().map(UserId::as_str).collect();
        assert_eq!(ids, ["U6"]);
        let found = mentions("<@U1>, see `code`", None);
        assert_eq!(found, [UserId::from("U1")]);
    }

    #[test]
    fn a_persons_mention_between_backticks_is_read_from_the_block_their_client_sends() {
        let text = "`foo` <@U0BOT> `bar`";
        let typed = channel_message(json!({
            "text": text,
            "blocks": [{"type": "rich_text", "elements": [
                {"type": "rich_text_section", "elements": [
                    {"type": "text", "text": "foo", "style": {"code": true}},
                    {"type": "text", "text": " "},
                    {"type": "user", "user_id": "U0BOT"},
                    {"type": "text", "text": " "},
                    {"type": "text", "text": "bar", "style": {"code": true}},
                ]},
            ]}],
        }));
        let event = normalize(typed).unwrap();
        assert_eq!(event.mentions, [UserId::from("U0BOT")]);
        let bare = channel_message(json!({"text": text}));
        assert_eq!(
            normalize(bare),
            Err(Skip::NotAddressed),
            "without the block, a token between backticks addresses no one"
        );
    }

    #[test]
    fn many_mentions_are_deduplicated_in_linear_time_and_capped() {
        let text: String = (0..40_000).map(|n| format!("<@U{n}><@U{n}>")).collect();
        let found = mentions(&text, None);
        assert_eq!(found.len(), MAX_MENTIONS);
        assert_eq!(found[0].as_str(), "U0");
        assert_eq!(found[1].as_str(), "U1");
        assert_eq!(found[MAX_MENTIONS - 1].as_str(), "U99");
    }

    #[test]
    fn a_channel_mention_is_kept() {
        let event = normalize(channel_message(json!({}))).unwrap();
        assert_eq!(event.conv_kind, ConvKind::Channel);
        assert_eq!(event.sender.user.as_str(), "U1");
        assert_eq!(event.sender.team.as_str(), "T0TEAM");
        assert_eq!(event.conv.team.as_str(), "T0TEAM");
        assert_eq!(event.conv.conversation.as_str(), "C1");
        assert_eq!(event.message.id.as_str(), "1727697600.000100");
        assert_eq!(event.mentions, [UserId::from(BOT)]);
        assert_eq!(event.thread_root, None);
        assert_eq!(event.reply_to, None);
        assert!(!event.sender_is_bot);
        assert_eq!(event.sender_bot_user, None);
        assert_eq!(event.event_id, "Ev1");
        assert_eq!(event.text, "hello <@U0BOT>");
    }

    #[test]
    fn a_channel_message_without_mention_or_thread_is_dropped() {
        let plain = channel_message(json!({"text": "hello <@U2>"}));
        assert_eq!(normalize(plain), Err(Skip::NotAddressed));
        let private = channel_message(json!({"text": "hi", "channel_type": "group"}));
        assert_eq!(normalize(private), Err(Skip::NotAddressed));
        let untyped = channel_message(json!({"text": "hi", "channel_type": null}));
        assert_eq!(normalize(untyped), Err(Skip::NotAddressed));
        let group_dm = channel_message(json!({"text": "hi", "channel_type": "mpim"}));
        assert_eq!(normalize(group_dm), Err(Skip::NotAddressed));
    }

    #[test]
    fn without_a_known_bot_user_only_thread_replies_pass_in_channels() {
        let team = TeamId::from("T0TEAM");
        let context = Context {
            binding: BindingId::new_v4(),
            bot_user: None,
            team: &team,
            home_org: None,
            event_id: "Ev1",
            received_at: datetime!(2026-09-30 12:00 UTC),
        };
        let mention = channel_message(json!({}));
        assert_eq!(message(&context, &mention), Err(Skip::NotAddressed));
        let reply = channel_message(json!({
            "thread_ts": "1727697500.000050",
            "parent_user_id": "U0SOMEONE",
        }));
        assert!(message(&context, &reply).is_ok());
        let dm = channel_message(json!({"channel_type": "im", "text": "hi"}));
        assert!(message(&context, &dm).is_ok());
    }

    #[test]
    fn a_thread_reply_is_kept_only_under_a_root_the_bot_may_have_posted() {
        let reply = |extra: Value| {
            let mut event = channel_message(json!({
                "text": "no mention",
                "thread_ts": "1727697500.000050",
            }));
            for (key, value) in extra.as_object().unwrap() {
                event[key] = value.clone();
            }
            event
        };
        let foreign = json!({"parent_user_id": "U0HUMAN"});
        for kind in ["channel", "group", "mpim"] {
            let under_foreign = reply(json!({"parent_user_id": "U0HUMAN", "channel_type": kind}));
            assert_eq!(normalize(under_foreign), Err(Skip::NotAddressed), "{kind}");
            let under_other_bot = reply(json!({"parent_user_id": "U0BOT2", "channel_type": kind}));
            assert_eq!(
                normalize(under_other_bot),
                Err(Skip::NotAddressed),
                "{kind}"
            );
            let under_own = reply(json!({"parent_user_id": BOT, "channel_type": kind}));
            assert!(normalize(under_own).is_ok(), "{kind}");
        }
        let dm = reply(json!({"parent_user_id": "U0HUMAN", "channel_type": "im"}));
        assert!(normalize(dm).is_ok());
        let mentioned = reply(json!({"parent_user_id": "U0HUMAN", "text": "<@U0BOT> too"}));
        assert_eq!(normalize(mentioned).unwrap().mentions, [UserId::from(BOT)]);
        for unknown in [
            json!({}),
            json!({"parent_user_id": null}),
            json!({"parent_user_id": ""}),
            json!({"parent_user_id": "u0human"}),
            json!({"parent_user_id": "B0HUMAN"}),
            json!({"parent_user_id": format!("U{}", "A".repeat(MAX_ID_TAIL + 1))}),
            json!({"parent_user_id": 7}),
            json!({"parent_user_id": true}),
            json!({"parent_user_id": ["U0HUMAN"]}),
            json!({"parent_user_id": {"id": "U0HUMAN"}}),
        ] {
            let kept = normalize(reply(unknown.clone()));
            assert!(kept.is_ok(), "{unknown}: {kept:?}");
        }
        assert_eq!(
            read(ConvKind::Channel, reply(foreign.clone())),
            Err(Skip::NotAddressed)
        );
        assert_eq!(
            read(ConvKind::GroupDm, reply(foreign.clone())),
            Err(Skip::NotAddressed)
        );
        assert!(read(ConvKind::Dm, reply(foreign)).is_ok());
        assert!(read(ConvKind::Channel, reply(json!({"parent_user_id": BOT}))).is_ok());
        assert!(read(ConvKind::Channel, reply(json!({}))).is_ok());
    }

    #[test]
    fn a_thread_reply_is_kept_with_its_root() {
        let event = normalize(channel_message(json!({
            "text": "no mention",
            "thread_ts": "1727697500.000050",
            "parent_user_id": "U0BOT",
        })))
        .unwrap();
        assert_eq!(
            event.thread_root.as_ref().map(|id| id.as_str()),
            Some("1727697500.000050")
        );
        let reply_to = event.reply_to.unwrap();
        assert_eq!(reply_to.id.as_str(), "1727697500.000050");
        assert_eq!(reply_to.conv, event.conv);
    }

    #[test]
    fn a_thread_ts_equal_to_ts_is_the_root_itself() {
        let root = channel_message(json!({"thread_ts": "1727697600.000100"}));
        let event = normalize(root).unwrap();
        assert_eq!(event.thread_root, None);
        assert_eq!(event.reply_to, None);
        let unaddressed = channel_message(json!({
            "text": "x",
            "thread_ts": "1727697600.000100",
        }));
        assert_eq!(normalize(unaddressed), Err(Skip::NotAddressed));
    }

    #[test]
    fn channel_type_sets_conv_kind() {
        for (channel_type, kind) in [
            ("im", ConvKind::Dm),
            ("mpim", ConvKind::GroupDm),
            ("channel", ConvKind::Channel),
            ("group", ConvKind::Channel),
        ] {
            let event = normalize(channel_message(json!({"channel_type": channel_type})));
            assert_eq!(event.unwrap().conv_kind, kind, "{channel_type}");
        }
        let unaddressed = channel_message(json!({"channel_type": "im", "text": "x"}));
        assert!(normalize(unaddressed).is_ok());
    }

    #[test]
    fn only_plain_file_share_and_thread_broadcast_messages_are_kept() {
        for subtype in KEPT_SUBTYPES {
            let event = channel_message(json!({"subtype": subtype}));
            assert!(normalize(event).is_ok(), "{subtype}");
        }
        for subtype in [
            "message_changed",
            "message_deleted",
            "bot_message",
            "channel_join",
            "me_message",
            "thread_broadcast ",
        ] {
            let event = channel_message(json!({"subtype": subtype}));
            assert_eq!(
                normalize(event),
                Err(Skip::Subtype(subtype.to_owned())),
                "{subtype}"
            );
        }
    }

    #[test]
    fn a_bot_with_a_user_is_named_by_its_user() {
        let event = normalize(channel_message(json!({
            "user": "U0OTHERBOT",
            "bot_id": "B0OTHER",
            "bot_profile": {"id": "B0OTHER", "app_id": "A0OTHER"},
        })))
        .unwrap();
        assert!(event.sender_is_bot);
        assert_eq!(event.sender.user.as_str(), "U0OTHERBOT");
        assert_eq!(event.sender_bot_user, Some(UserId::from("U0OTHERBOT")));

        let profile_only =
            normalize(channel_message(json!({"bot_profile": {"id": "B1"}}))).unwrap();
        assert!(profile_only.sender_is_bot);
        assert_eq!(profile_only.sender_bot_user, Some(UserId::from("U1")));
    }

    #[test]
    fn own_posts_and_bots_not_mentioning_the_bot_are_dropped_in_every_kind() {
        for (channel_type, kind) in [
            ("im", ConvKind::Dm),
            ("mpim", ConvKind::GroupDm),
            ("channel", ConvKind::Channel),
        ] {
            let with = |extra: Value| {
                let mut event = channel_message(json!({
                    "channel_type": channel_type,
                    "thread_ts": "1727697500.000050",
                    "parent_user_id": BOT,
                }));
                for (key, value) in extra.as_object().unwrap() {
                    event[key] = value.clone();
                }
                event
            };
            let own = with(json!({"user": BOT, "bot_id": "B0SELF", "text": "hi <@U0BOT>"}));
            assert_eq!(
                normalize(own.clone()),
                Err(Skip::NotAddressed),
                "{channel_type}"
            );
            assert_eq!(read(kind, own), Err(Skip::NotAddressed), "{channel_type}");
            let own_unflagged = with(json!({"user": BOT, "text": "hi"}));
            assert_eq!(
                normalize(own_unflagged),
                Err(Skip::NotAddressed),
                "{channel_type}"
            );
            let mut quiet_bots = vec![
                with(json!({"user": "U0OTHERBOT", "bot_id": "B0OTHER", "text": "hi"})),
                with(json!({"user": "U0OTHERBOT", "bot_profile": {"id": "B0OTHER"}, "text": "hi"})),
                with(json!({"bot_id": "B0OTHER", "text": "hi <@U0HUMAN>"})),
                with(
                    json!({"user": "U0OTHERBOT", "bot_id": "B0OTHER", "text": format!("`<@{BOT}>` over to you")}),
                ),
                with(json!({
                    "user": "U0OTHERBOT",
                    "bot_id": "B0OTHER",
                    "text": format!("`a` <@{BOT}> `b`"),
                    "blocks": [{"type": "rich_text", "elements": [
                        {"type": "rich_text_section", "elements": [
                            {"type": "text", "text": "a", "style": {"code": true}},
                            {"type": "user", "user_id": BOT},
                            {"type": "text", "text": "b", "style": {"code": true}},
                        ]},
                    ]}],
                })),
                with(json!({
                    "user": "U0OTHERBOT",
                    "bot_id": "B0OTHER",
                    "text": "have a look",
                    "blocks": [{"type": "rich_text", "elements": [
                        {"type": "rich_text_section", "elements": [{"type": "user", "user_id": BOT}]},
                    ]}],
                })),
            ];
            quiet_bots[2].as_object_mut().unwrap().remove("user");
            for quiet in quiet_bots {
                assert_eq!(normalize(quiet.clone()), Err(Skip::NotAddressed), "{quiet}");
                assert_eq!(read(kind, quiet), Err(Skip::NotAddressed), "{channel_type}");
            }
            let calling = with(json!({"user": "U0OTHERBOT", "bot_id": "B0OTHER"}));
            assert!(normalize(calling.clone()).is_ok(), "{channel_type}");
            assert!(read(kind, calling).is_ok(), "{channel_type}");
            let person = with(json!({"text": "hi"}));
            assert!(normalize(person).is_ok(), "{channel_type}");
        }
        let team = TeamId::from("T0TEAM");
        let unknown_bot_user = Context {
            binding: BindingId::new_v4(),
            bot_user: None,
            team: &team,
            home_org: None,
            event_id: "Ev1",
            received_at: datetime!(2026-09-30 12:00 UTC),
        };
        let quiet = channel_message(json!({
            "channel_type": "im",
            "user": "U0OTHERBOT",
            "bot_id": "B0OTHER",
            "text": "hi",
        }));
        assert!(message(&unknown_bot_user, &quiet).is_ok());
    }

    #[test]
    fn a_bot_without_a_user_is_named_by_its_bot_id() {
        let mut event = channel_message(json!({"bot_id": "B0OTHER"}));
        event.as_object_mut().unwrap().remove("user");
        let event = normalize(event).unwrap();
        assert!(event.sender_is_bot);
        assert_eq!(event.sender.user.as_str(), "B0OTHER");
        assert_eq!(event.sender_bot_user, None);
    }

    #[test]
    fn a_message_without_sender_channel_or_ts_is_dropped() {
        let mut event = channel_message(json!({}));
        event.as_object_mut().unwrap().remove("user");
        assert_eq!(normalize(event), Err(Skip::NoSender));
        for field in ["channel", "ts"] {
            let mut event = channel_message(json!({}));
            event.as_object_mut().unwrap().remove(field);
            assert_eq!(normalize(event), Err(Skip::Malformed), "{field}");
        }
        assert_eq!(normalize(json!("text")), Err(Skip::Malformed));
        assert_eq!(
            normalize(channel_message(json!({"user": 7}))),
            Err(Skip::Malformed)
        );
    }

    #[test]
    fn ids_not_shaped_like_slacks_make_a_message_malformed() {
        let long = "A".repeat(MAX_ID_TAIL + 1);
        for extra in [
            json!({"user": format!("U{long}")}),
            json!({"user": "u0lower"}),
            json!({"user": "B0BOT"}),
            json!({"bot_id": format!("B{long}")}),
            json!({"bot_id": "U0HUMAN"}),
            json!({"channel": format!("C{long}")}),
            json!({"channel": "X0CHAN"}),
            json!({"ts": "1.1"}),
            json!({"thread_ts": "1727697500.00005"}),
        ] {
            assert_eq!(
                normalize(channel_message(extra.clone())),
                Err(Skip::Malformed),
                "{extra}"
            );
        }
        let mut userless = channel_message(json!({"bot_id": format!("B{long}")}));
        userless.as_object_mut().unwrap().remove("user");
        assert_eq!(normalize(userless), Err(Skip::Malformed));
        assert!(
            normalize(channel_message(
                json!({"user": format!("W{}", "A".repeat(MAX_ID_TAIL))})
            ))
            .is_ok()
        );
    }

    #[test]
    fn text_files_and_mentions_are_cut_to_slacks_limits() {
        let long_user = format!("U{}", "A".repeat(MAX_ID_TAIL + 1));
        let files: Vec<Value> = (0..MAX_FILES + 5)
            .map(|n| json!({"id": format!("F{n}"), "url_private": format!("https://files.slack.com/F{n}")}))
            .collect();
        let head = format!("<@{long_user}> <@U0BOT> ");
        let event = normalize(channel_message(json!({
            "text": format!("{head}{} <@U0PASTCUT>", "é".repeat(MAX_TEXT_BYTES)),
            "files": files,
        })))
        .unwrap();
        assert_eq!(event.text.len(), MAX_TEXT_BYTES - head.len() % 2);
        assert!(event.text.starts_with(&format!("{head}é")));
        assert!(event.text.ends_with('é'));
        assert_eq!(event.mentions, [UserId::from(BOT)]);
        assert_eq!(event.files.len(), MAX_FILES);
        assert_eq!(event.files[MAX_FILES - 1].id, format!("F{}", MAX_FILES - 1));
    }

    #[test]
    fn escaped_text_is_cut_in_bytes_and_a_mention_past_the_cut_comes_from_blocks() {
        let typed = "&".repeat(40_000);
        let escaped = "&amp;".repeat(40_000);
        let blocks = json!([{"type": "rich_text", "elements": [
            {"type": "rich_text_section", "elements": [
                {"type": "text", "text": typed},
                {"type": "user", "user_id": BOT},
            ]},
        ]}]);
        let event = normalize(channel_message(json!({
            "text": format!("{escaped} <@{BOT}>"),
            "blocks": blocks,
        })))
        .unwrap();
        assert_eq!(event.text, escaped[..MAX_TEXT_BYTES]);
        assert_eq!(event.mentions, [UserId::from(BOT)]);
        let whole = "&amp;".repeat(20_000);
        let event = normalize(channel_message(
            json!({"text": format!("{whole} <@{BOT}>")}),
        ))
        .unwrap();
        assert_eq!(event.text, format!("{whole} <@{BOT}>"));
    }

    #[test]
    fn a_files_fields_are_bounded() {
        let url = "https://files.slack.com/F1";
        let event = normalize(channel_message(json!({
            "files": [
                {"id": format!("F{}", "A".repeat(MAX_ID_TAIL + 1)), "url_private": url},
                {"id": "f0lower", "url_private": url},
                {"id": "F2", "url_private": format!("{url}/{}", "x".repeat(MAX_FILE_URL_BYTES))},
                {
                    "id": "F3",
                    "name": "ñ".repeat(MAX_FILE_NAME_CHARS + 1),
                    "mimetype": "x".repeat(MAX_MIME_TYPE_BYTES + 1),
                    "url_private": url,
                },
                {"id": "F4", "mimetype": "text/plain", "url_private": url},
            ],
        })))
        .unwrap();
        let ids: Vec<&str> = event.files.iter().map(|file| file.id.as_str()).collect();
        assert_eq!(ids, ["F3", "F4"]);
        assert_eq!(event.files[0].name, "ñ".repeat(MAX_FILE_NAME_CHARS));
        assert_eq!(event.files[0].mime_type, None);
        assert_eq!(event.files[1].mime_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn an_ignored_subtype_is_carried_cut_short() {
        let subtype = "x".repeat(10_000);
        assert_eq!(
            normalize(channel_message(json!({"subtype": subtype}))),
            Err(Skip::Subtype("x".repeat(MAX_SUBTYPE_BYTES)))
        );
    }

    #[test]
    fn files_become_in_files() {
        let event = normalize(channel_message(json!({
            "subtype": "file_share",
            "files": [
                {
                    "id": "F1",
                    "name": "notes.txt",
                    "title": "Notes",
                    "mimetype": "text/plain",
                    "size": 12,
                    "url_private": "https://files.slack.com/files-pri/T0TEAM-F1/notes.txt",
                    "url_private_download": "https://files.slack.com/files-pri/T0TEAM-F1/download/notes.txt",
                },
                {"id": "F2", "title": "Untitled", "url_private": "https://files.slack.com/F2"},
                {"id": "F3", "url_private": "https://files.slack.com/F3"},
                {"id": "F4", "mode": "hidden_by_limit"},
                {"id": "F5", "file_access": "check_file_info"},
            ],
        })))
        .unwrap();
        assert_eq!(
            event.files,
            [
                InFile {
                    id: "F1".into(),
                    name: "notes.txt".into(),
                    mime_type: Some("text/plain".into()),
                    size: Some(12),
                    url: "https://files.slack.com/files-pri/T0TEAM-F1/download/notes.txt".into(),
                },
                InFile {
                    id: "F2".into(),
                    name: "Untitled".into(),
                    mime_type: None,
                    size: None,
                    url: "https://files.slack.com/F2".into(),
                },
                InFile {
                    id: "F3".into(),
                    name: "F3".into(),
                    mime_type: None,
                    size: None,
                    url: "https://files.slack.com/F3".into(),
                },
            ]
        );
    }

    fn read(kind: ConvKind, message: Value) -> Result<InboundEvent, Skip> {
        let bot = UserId::from(BOT);
        let team = TeamId::from("T0TEAM");
        let context = Context {
            binding: BindingId::from_uuid(uuid::Uuid::nil()),
            bot_user: Some(&bot),
            team: &team,
            home_org: None,
            event_id: "Ev1",
            received_at: datetime!(2026-09-30 12:00 UTC),
        };
        read_back(&context, &ConversationId::from("C9"), kind, &message)
    }

    #[test]
    fn a_read_back_message_takes_its_channel_and_kind_from_the_caller() {
        let claims_dm =
            channel_message(json!({"channel": "D1", "channel_type": "im", "text": "hi"}));
        assert_eq!(
            read(ConvKind::Channel, claims_dm.clone()),
            Err(Skip::NotAddressed)
        );
        let dm = read(ConvKind::Dm, claims_dm).unwrap();
        assert_eq!(dm.conv_kind, ConvKind::Dm);
        assert_eq!(dm.conv.conversation.as_str(), "C9");
        assert_eq!(dm.message.conv.conversation.as_str(), "C9");

        let mut unnamed = channel_message(json!({}));
        unnamed.as_object_mut().unwrap().remove("channel");
        let mention = read(ConvKind::Channel, unnamed).unwrap();
        assert_eq!(
            mention,
            normalize(channel_message(json!({"channel": "C9"}))).unwrap()
        );
    }

    #[test]
    fn a_read_back_message_is_addressed_by_its_own_blocks_thread_and_files() {
        let blocks = json!([{"type": "rich_text", "elements": [
            {"type": "rich_text_section", "elements": [{"type": "user", "user_id": BOT}]},
        ]}]);
        let mentioned = read(
            ConvKind::Channel,
            channel_message(json!({"text": "", "blocks": blocks})),
        )
        .unwrap();
        assert_eq!(mentioned.mentions, [UserId::from(BOT)]);
        let reply = read(
            ConvKind::Channel,
            channel_message(json!({"text": "x", "thread_ts": "1727697500.000050"})),
        )
        .unwrap();
        assert_eq!(
            reply.reply_to.map(|msg| msg.id),
            Some("1727697500.000050".into())
        );
        let files = read(
            ConvKind::Channel,
            channel_message(json!({
                "subtype": "file_share",
                "files": [{"id": "F1", "url_private": "https://files.slack.com/F1"}],
            })),
        )
        .unwrap();
        assert_eq!(files.files.len(), 1);
        for subtype in ["tombstone", "message_changed"] {
            let gone = channel_message(json!({"subtype": subtype}));
            assert_eq!(
                read(ConvKind::Channel, gone),
                Err(Skip::Subtype(subtype.into()))
            );
        }
    }

    #[test]
    fn an_edited_message_counts_as_edited_only_for_bots() {
        let edited =
            json!({"edited": {"user": "U1", "ts": "1727697700.000000"}, "text": "now <@U0BOT>"});
        let human = read(ConvKind::Channel, channel_message(edited.clone())).unwrap();
        assert_eq!(human.text, "now <@U0BOT>");
        let mut by_bot = channel_message(edited);
        by_bot["bot_id"] = json!("B0OTHER");
        assert_eq!(
            read(ConvKind::Channel, by_bot.clone()),
            Err(Skip::EditedByBot)
        );
        assert_eq!(normalize(by_bot), Err(Skip::EditedByBot));
    }

    #[test]
    fn skips_never_carry_message_text() {
        let text = Skip::Subtype("message_changed".into()).to_string();
        assert_eq!(text, "message subtype message_changed is ignored");
        for skip in [
            Skip::Malformed,
            Skip::NoSender,
            Skip::NotAddressed,
            Skip::EditedByBot,
        ] {
            assert!(!skip.to_string().is_empty());
        }
    }

    fn in_workspace(event: &Value, home_org: Option<&str>) -> Result<InboundEvent, Skip> {
        let bot = UserId::from(testkit::slack::BOT_USER);
        let team = TeamId::from(testkit::slack::TEAM);
        let home_org = home_org.map(TeamId::from);
        let context = Context {
            binding: BindingId::from_uuid(uuid::Uuid::nil()),
            bot_user: Some(&bot),
            team: &team,
            home_org: home_org.as_ref(),
            event_id: "Ev1",
            received_at: datetime!(2026-09-30 12:00 UTC),
        };
        let as_event = message(&context, event);
        let as_read_back = read_back(
            &context,
            &ConversationId::from(testkit::slack::SHARED_CHANNEL),
            ConvKind::Channel,
            event,
        );
        assert_eq!(
            as_event.as_ref().map(|event| &event.outside),
            as_read_back.as_ref().map(|event| &event.outside),
            "a read-back message is read alike"
        );
        as_event
    }

    fn connect_message(fields: Value) -> Value {
        let mut event = channel_message(fields);
        event["text"] = json!(format!("<@{}> hello", testkit::slack::BOT_USER));
        event
    }

    fn fixture_event(fixture: &str) -> Value {
        serde_json::from_str::<Value>(fixture).unwrap()["event"].clone()
    }

    fn outside(team: &str) -> Option<Outside> {
        Some(Outside { team: team.into() })
    }

    #[test]
    fn an_outside_member_is_keyed_by_the_workspace_and_marked_outside() {
        let event = in_workspace(
            &fixture_event(testkit::slack::MESSAGE_CONNECT_THEIR_TEAM),
            None,
        )
        .unwrap();
        assert_eq!(
            event.sender,
            MemberKey {
                surface: SurfaceKind::Slack,
                team: testkit::slack::TEAM.into(),
                user: testkit::slack::OUTSIDE_USER.into(),
            }
        );
        assert_eq!(event.conv.team.as_str(), testkit::slack::TEAM);
        assert_eq!(event.outside, outside(testkit::slack::OUTSIDE_TEAM));
    }

    #[test]
    fn a_dm_from_an_outside_member_is_marked_outside_by_its_fields() {
        let bot = UserId::from(testkit::slack::BOT_USER);
        let team = TeamId::from(testkit::slack::TEAM);
        let context = Context {
            binding: BindingId::from_uuid(uuid::Uuid::nil()),
            bot_user: Some(&bot),
            team: &team,
            home_org: None,
            event_id: "Ev1",
            received_at: datetime!(2026-09-30 12:00 UTC),
        };
        let mut dm = fixture_event(testkit::slack::MESSAGE_IM);
        dm["user"] = json!(testkit::slack::OUTSIDE_USER);
        dm["team"] = json!(testkit::slack::TEAM);
        dm["user_team"] = json!(testkit::slack::OUTSIDE_TEAM);
        dm["source_team"] = json!(testkit::slack::OUTSIDE_TEAM);
        let event = message(&context, &dm).unwrap();
        assert!(event.is_dm());
        assert_eq!(event.sender.team.as_str(), testkit::slack::TEAM);
        assert_eq!(event.sender.user.as_str(), testkit::slack::OUTSIDE_USER);
        assert_eq!(event.outside, outside(testkit::slack::OUTSIDE_TEAM));

        let home = message(&context, &fixture_event(testkit::slack::MESSAGE_IM)).unwrap();
        assert!(home.is_dm());
        assert_eq!(home.outside, None, "a home member's DM stays home");
    }

    #[test]
    fn an_outside_actor_with_the_installing_team_in_team_is_outside() {
        let event = in_workspace(
            &fixture_event(testkit::slack::MESSAGE_CONNECT_NO_ACTOR_TEAM),
            None,
        )
        .unwrap();
        assert_eq!(event.sender.team.as_str(), testkit::slack::TEAM);
        assert_eq!(event.outside, outside(testkit::slack::OUTSIDE_TEAM));
    }

    #[test]
    fn source_team_and_user_profile_team_count() {
        let home = testkit::slack::TEAM;
        for fields in [
            json!({"source_team": "T0THEIRS1"}),
            json!({"user_profile": {"team": "T0THEIRS1"}}),
            json!({"team": home, "user_team": home, "source_team": "T0THEIRS1"}),
            json!({"team": home, "user_team": home, "user_profile": {"team": "T0THEIRS1", "display_name": "zoe"}}),
        ] {
            let event = in_workspace(&connect_message(fields.clone()), None).unwrap();
            assert_eq!(event.outside, outside("T0THEIRS1"), "{fields}");
        }
    }

    #[test]
    fn the_first_foreign_field_names_the_organization() {
        let event = in_workspace(
            &connect_message(json!({
                "team": "T0FOURTH1",
                "user_profile": {"team": "T0THIRD01"},
                "source_team": "T0SECOND1",
                "user_team": "T0FIRST01",
            })),
            None,
        )
        .unwrap();
        assert_eq!(event.outside, outside("T0FIRST01"));
        let event = in_workspace(
            &connect_message(json!({
                "team": "T0FOURTH1",
                "user_profile": {"team": "T0THIRD01"},
                "source_team": testkit::slack::TEAM,
                "user_team": testkit::slack::TEAM,
            })),
            None,
        )
        .unwrap();
        assert_eq!(event.outside, outside("T0THIRD01"));
    }

    #[test]
    fn a_home_member_in_a_shared_channel_is_not_outside_by_its_fields() {
        let event =
            in_workspace(&fixture_event(testkit::slack::MESSAGE_CONNECT_HOME), None).unwrap();
        assert_eq!(event.outside, None);
        assert_eq!(event.sender.user.as_str(), testkit::slack::USER);
        let teamless = in_workspace(&connect_message(json!({})), None).unwrap();
        assert_eq!(teamless.outside, None, "the home check decides");
    }

    #[test]
    fn a_home_organization_field_counts_as_home_only_with_the_organization_known() {
        let event = fixture_event(testkit::slack::MESSAGE_HOME_ORG);
        assert_eq!(
            in_workspace(&event, Some(testkit::slack::HOME_ORG))
                .unwrap()
                .outside,
            None
        );
        assert_eq!(
            in_workspace(&event, None).unwrap().outside,
            outside(testkit::slack::HOME_ORG),
            "without auth.test's enterprise_id an E… field is another organization"
        );
        assert_eq!(
            in_workspace(&event, Some("E0OTHERORG")).unwrap().outside,
            outside(testkit::slack::HOME_ORG)
        );
    }

    #[test]
    fn another_workspace_of_the_home_organization_is_outside() {
        let event = connect_message(json!({
            "team": testkit::slack::TEAM,
            "user_team": testkit::slack::HOME_ORG,
            "source_team": "T0SIBLING",
        }));
        assert_eq!(
            in_workspace(&event, Some(testkit::slack::HOME_ORG))
                .unwrap()
                .outside,
            outside("T0SIBLING")
        );
    }

    #[test]
    fn a_sender_team_not_shaped_like_slacks_is_malformed() {
        for fields in [
            json!({"team": ""}),
            json!({"user_team": "t0lower01"}),
            json!({"source_team": "U0HUMAN01"}),
            json!({"user_profile": {"team": "T0TEAM001 "}}),
            json!({"team": format!("T{}", "0".repeat(MAX_ID_TAIL + 1))}),
            json!({"user_team": 7}),
            json!({"user_profile": {"team": ["T0TEAM001"]}}),
        ] {
            assert_eq!(
                in_workspace(&connect_message(fields.clone()), None),
                Err(Skip::Malformed),
                "{fields}"
            );
        }
        assert!(in_workspace(&connect_message(json!({"user_profile": {}})), None).is_ok());
        assert!(
            in_workspace(
                &connect_message(json!({"team": format!("T{}", "0".repeat(MAX_ID_TAIL))})),
                None
            )
            .is_ok()
        );
    }
}

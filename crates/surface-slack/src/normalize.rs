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
//! - In a channel (`channel_type` other than `im` and `mpim`), a message is
//!   kept only if it mentions the binding's bot user or replies in a thread.
//!   Whether the thread's root is the agent's own message is the router's
//!   question; it needs the thread root, which is in `reply_to`.
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
//!   block is not scanned: a literal `<@U…>` there is not a mention.
//! - The team is the envelope's `team_id`, for the sender and the
//!   conversation alike.

use core_types::{
    BindingId, ConvKind, ConvRef, InFile, InboundEvent, MemberKey, MsgRef, SurfaceKind, TeamId,
    UserId,
};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;
use time::OffsetDateTime;

/// The `message` subtypes that are kept. A message with no subtype is kept
/// too.
pub const KEPT_SUBTYPES: [&str; 2] = ["file_share", "thread_broadcast"];

/// What [`message`] needs besides the event.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The binding whose app received the event.
    pub binding: BindingId,
    /// The binding's bot user, if known. Without it, channel messages are
    /// kept only when they reply in a thread.
    pub bot_user: Option<&'a UserId>,
    /// The envelope's `team_id`.
    pub team: &'a TeamId,
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
    /// A channel message that neither mentions the bot nor replies in a
    /// thread.
    #[error("a channel message that neither mentions the bot nor replies in a thread")]
    NotAddressed,
}

#[derive(Debug, Default, Deserialize)]
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
    blocks: Option<Value>,
    files: Option<Vec<SlackFile>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SlackFile {
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
    if let Some(subtype) = &event.subtype
        && !KEPT_SUBTYPES.contains(&subtype.as_str())
    {
        return Err(Skip::Subtype(subtype.clone()));
    }
    let (Some(channel), Some(ts)) = (event.channel, event.ts) else {
        return Err(Skip::Malformed);
    };
    let is_bot = event.bot_id.is_some() || event.bot_profile.is_some();
    let (sender, sender_bot_user) = match (event.user, event.bot_id) {
        (Some(user), _) => {
            let user = UserId::from(user);
            let bot_user = is_bot.then(|| user.clone());
            (user, bot_user)
        }
        (None, Some(bot_id)) => (UserId::from(bot_id), None),
        (None, None) => return Err(Skip::NoSender),
    };
    let conv_kind = match event.channel_type.as_deref() {
        Some("im") => ConvKind::Dm,
        Some("mpim") => ConvKind::GroupDm,
        _ => ConvKind::Channel,
    };
    let text = event.text.unwrap_or_default();
    let mentions = mentions(&text, event.blocks.as_ref());
    let thread_root = event.thread_ts.filter(|root| *root != ts);
    if conv_kind == ConvKind::Channel
        && thread_root.is_none()
        && !context.bot_user.is_some_and(|bot| mentions.contains(bot))
    {
        return Err(Skip::NotAddressed);
    }
    let conv = ConvRef {
        surface: SurfaceKind::Slack,
        team: context.team.clone(),
        conversation: channel.into(),
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
        files: event
            .files
            .unwrap_or_default()
            .into_iter()
            .filter_map(in_file)
            .collect(),
        received_at: context.received_at,
    })
}

/// A file the bot can download: one with an id and a private URL. Files
/// Slack withholds (`hidden_by_limit`, or Slack Connect files that need
/// `files.info` first) carry neither and are left out.
fn in_file(file: SlackFile) -> Option<InFile> {
    let id = file.id?;
    let url = file.url_private_download.or(file.url_private)?;
    let name = file.name.or(file.title).unwrap_or_else(|| id.clone());
    Some(InFile {
        id,
        name,
        mime_type: file.mimetype,
        size: file.size,
        url,
    })
}

/// Every user mentioned in `text` and `blocks`, once each, in order of first
/// appearance: `<@U…>` tokens in `text`, then `rich_text` `user` elements
/// and tokens in `mrkdwn` text objects.
pub fn mentions(text: &str, blocks: Option<&Value>) -> Vec<UserId> {
    let mut found = Vec::new();
    scan_tokens(text, &mut found);
    if let Some(blocks) = blocks {
        walk_blocks(blocks, &mut found);
    }
    let mut unique: Vec<UserId> = Vec::with_capacity(found.len());
    for user in found {
        if !unique.contains(&user) {
            unique.push(user);
        }
    }
    unique
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

/// Collects the ids of `<@U…>` and `<@U…|label>` tokens.
fn scan_tokens(text: &str, found: &mut Vec<UserId>) {
    let mut rest = text;
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

/// Whether `id` looks like a Slack user id: `U` or `W`, then uppercase
/// letters and digits.
fn is_user_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some('U' | 'W'))
        && id.len() >= 2
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
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
    }

    #[test]
    fn without_a_known_bot_user_only_thread_replies_pass_in_channels() {
        let team = TeamId::from("T0TEAM");
        let context = Context {
            binding: BindingId::new_v4(),
            bot_user: None,
            team: &team,
            event_id: "Ev1",
            received_at: datetime!(2026-09-30 12:00 UTC),
        };
        let mention = channel_message(json!({}));
        assert_eq!(message(&context, &mention), Err(Skip::NotAddressed));
        let reply = channel_message(json!({"thread_ts": "1.1"}));
        assert!(message(&context, &reply).is_ok());
        let dm = channel_message(json!({"channel_type": "im", "text": "hi"}));
        assert!(message(&context, &dm).is_ok());
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
        for channel_type in ["im", "mpim"] {
            let unaddressed = channel_message(json!({"channel_type": channel_type, "text": "x"}));
            assert!(normalize(unaddressed).is_ok(), "{channel_type}");
        }
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

    #[test]
    fn skips_never_carry_message_text() {
        let text = Skip::Subtype("message_changed".into()).to_string();
        assert_eq!(text, "message subtype message_changed is ignored");
        for skip in [Skip::Malformed, Skip::NoSender, Skip::NotAddressed] {
            assert!(!skip.to_string().is_empty());
        }
    }
}

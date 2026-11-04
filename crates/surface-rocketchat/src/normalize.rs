//! Turning a Rocket.Chat message into an [`InboundEvent`].
//!
//! The rules, from T12 and `InboundEvent`'s rustdoc:
//!
//! - System messages (a `t` field, such as `uj` for a user joining) and
//!   edits (`editedAt`) are skipped, and so is any other change to an old
//!   message (see [`Skip::Update`]).
//! - `event_id` is the message `_id`, and so is the message id.
//! - `mentions` are `mentions[]._id`, without the `all` and `here`
//!   broadcasts, which name no user, and only the first
//!   [`MAX_MENTIONS`] of them.
//! - `tmid` becomes both `thread_root` and `reply_to`: the router decides
//!   whether the thread root is the agent's own message.
//! - A room of type `d` is a `Dm`, or a `GroupDm` with more than two
//!   members. Channels and private groups are `Channel`s.
//! - A bot sender (the `bot` field, or the `bot` role) has
//!   `sender_bot_user` equal to `sender.user`, its `u._id`.
//! - A bot's own messages are kept: every connection in a room receives
//!   every message and only the first to record it delivers it.

use core_types::{
    BindingId, ConvKind, ConvRef, InFile, InboundEvent, MAX_MENTIONS, MemberKey, MsgRef,
    SurfaceKind, TeamId, UserId,
};
use time::{Duration, OffsetDateTime};

use crate::rest::{FileRef, Message, RoomInfo, RoomType};

/// How long after `ts` a message's `_updatedAt` may be and the message
/// still count as new. Rocket.Chat rebroadcasts a message on
/// `stream-room-messages` whenever it changes: a reaction, a reply in its
/// thread, a link preview. A message seen for the first time with a much
/// later `_updatedAt` is an old one that changed, not a new one.
///
/// A new message can have a `ts` up to 60 s before the server received it:
/// the `sendMessage` method accepts a client's `ts` within that skew. Two
/// minutes leaves room for that and for a slow insert.
pub(crate) const UPDATE_GRACE: Duration = Duration::seconds(120);

/// The broadcast names Rocket.Chat puts in `mentions[]._id`.
const BROADCASTS: &[&str] = &["all", "here"];

/// Why a message produces no event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Skip {
    /// A system message (`t`).
    System,
    /// An edited message (`editedAt`).
    Edited,
    /// A message whose `_updatedAt` is more than [`UPDATE_GRACE`] after its
    /// `ts`: an old message rebroadcast because it changed.
    Update,
}

impl Skip {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Edited => "edited",
            Self::Update => "update",
        }
    }
}

/// Whether `message` is a system message, such as a user joining.
pub(crate) fn is_system(message: &Message) -> bool {
    message.kind.as_deref().is_some_and(|t| !t.is_empty())
}

/// Why `message` should be skipped, or `None` to keep it.
pub(crate) fn skip_reason(message: &Message) -> Option<Skip> {
    if is_system(message) {
        Some(Skip::System)
    } else if message.edited {
        Some(Skip::Edited)
    } else if message
        .updated_at
        .is_some_and(|updated| updated - message.sent_at > UPDATE_GRACE)
    {
        Some(Skip::Update)
    } else {
        None
    }
}

/// The conversation kind of a room, or `None` for a room type agents don't
/// take part in, such as omnichannel.
///
/// A direct message is a one-to-one [`ConvKind::Dm`] only when its member
/// count or member list says it has at most two members. One that reports
/// neither is a [`ConvKind::GroupDm`], since a DM counts as private and
/// others may be reading.
pub(crate) fn conv_kind(room: &RoomInfo) -> Option<ConvKind> {
    match room.room_type {
        RoomType::Channel | RoomType::Group => Some(ConvKind::Channel),
        RoomType::Direct => {
            let listed = u64::try_from(room.uids.len()).unwrap_or(u64::MAX);
            if room.users_count.is_none() && listed == 0 {
                return Some(ConvKind::GroupDm);
            }
            let members = room.users_count.unwrap_or(0).max(listed);
            Some(if members > 2 {
                ConvKind::GroupDm
            } else {
                ConvKind::Dm
            })
        }
        RoomType::Livechat | RoomType::Other(_) => None,
    }
}

/// What [`to_event`] needs besides the message.
pub(crate) struct Context<'a> {
    pub(crate) binding: BindingId,
    pub(crate) team: &'a TeamId,
    pub(crate) conv_kind: ConvKind,
    pub(crate) sender_is_bot: bool,
    pub(crate) received_at: OffsetDateTime,
    pub(crate) file_url: &'a dyn Fn(&FileRef) -> String,
}

/// The event for `message`. Call it only when [`skip_reason`] is `None`.
pub(crate) fn to_event(message: &Message, ctx: &Context<'_>) -> InboundEvent {
    let conv = ConvRef {
        surface: SurfaceKind::RocketChat,
        team: ctx.team.clone(),
        conversation: message.room.clone(),
    };
    let sender = MemberKey {
        surface: SurfaceKind::RocketChat,
        team: ctx.team.clone(),
        user: message.sender.id.clone(),
    };
    let mut mentions: Vec<UserId> = Vec::with_capacity(message.mentions.len());
    for mention in &message.mentions {
        if mentions.len() == MAX_MENTIONS {
            break;
        }
        if !BROADCASTS.contains(&mention.as_str()) && !mentions.contains(mention) {
            mentions.push(mention.clone());
        }
    }
    InboundEvent {
        event_id: message.id.as_str().to_owned(),
        binding: ctx.binding,
        sender_is_bot: ctx.sender_is_bot,
        sender_bot_user: ctx.sender_is_bot.then(|| sender.user.clone()),
        sender,
        conv_kind: ctx.conv_kind,
        thread_root: message.thread_root.clone(),
        message: MsgRef {
            conv: conv.clone(),
            id: message.id.clone(),
        },
        reply_to: message.thread_root.clone().map(|id| MsgRef {
            conv: conv.clone(),
            id,
        }),
        conv,
        text: message.text.clone(),
        mentions,
        files: message
            .files
            .iter()
            .map(|file| in_file(file, ctx.file_url))
            .collect(),
        received_at: ctx.received_at,
    }
}

pub(crate) fn in_file(file: &FileRef, file_url: &dyn Fn(&FileRef) -> String) -> InFile {
    InFile {
        id: file.id.clone(),
        name: file.name.clone(),
        mime_type: file.mime_type.clone(),
        size: file.size,
        url: file_url(file),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use time::macros::datetime;

    use super::*;

    fn message(extra: Value) -> Message {
        let mut raw = json!({
            "_id": "m1",
            "rid": "R1",
            "msg": "hi @helper",
            "ts": { "$date": 1_790_000_000_000_i64 },
            "u": { "_id": "u1", "username": "alice" },
            "_updatedAt": { "$date": 1_790_000_000_005_i64 },
        });
        if let (Some(raw), Some(extra)) = (raw.as_object_mut(), extra.as_object()) {
            raw.extend(extra.clone());
        }
        serde_json::from_value(raw).unwrap()
    }

    fn room(t: &str, users_count: Option<u64>, uids: &[&str]) -> RoomInfo {
        let mut raw = json!({ "_id": "R1", "t": t, "uids": uids });
        if let Some(count) = users_count {
            raw["usersCount"] = json!(count);
        }
        serde_json::from_value(raw).unwrap()
    }

    fn url(file: &FileRef) -> String {
        format!("https://chat.example/file-upload/{}/{}", file.id, file.name)
    }

    fn event(message: &Message, conv_kind: ConvKind, sender_is_bot: bool) -> InboundEvent {
        let team = TeamId::from("chat.example");
        to_event(
            message,
            &Context {
                binding: BindingId::new_v4(),
                team: &team,
                conv_kind,
                sender_is_bot,
                received_at: datetime!(2026-09-30 12:00 UTC),
                file_url: &url,
            },
        )
    }

    #[test]
    fn system_messages_edits_and_old_updates_are_skipped() {
        assert_eq!(skip_reason(&message(json!({}))), None);
        assert_eq!(
            skip_reason(&message(json!({ "t": "uj" }))),
            Some(Skip::System)
        );
        assert_eq!(skip_reason(&message(json!({ "t": "" }))), None);
        assert_eq!(
            skip_reason(&message(
                json!({ "editedAt": { "$date": 1_790_000_001_000_i64 } })
            )),
            Some(Skip::Edited)
        );
        let reacted_later = json!({ "_updatedAt": { "$date": 1_790_000_121_000_i64 } });
        assert_eq!(skip_reason(&message(reacted_later)), Some(Skip::Update));
        let reacted_soon = json!({ "_updatedAt": { "$date": 1_790_000_119_000_i64 } });
        assert_eq!(skip_reason(&message(reacted_soon)), None);
        let no_updated_at = json!({ "_updatedAt": null });
        assert_eq!(skip_reason(&message(no_updated_at)), None);
        for skip in [Skip::System, Skip::Edited, Skip::Update] {
            assert!(!skip.as_str().is_empty());
        }
    }

    #[test]
    fn a_file_message_without_text_takes_its_attachment_description() {
        let file = json!({ "_id": "f1", "name": "persona.md" });
        let described = message(json!({
            "msg": "",
            "files": [file],
            "attachments": [{ "title": "persona.md", "description": "persona helper" }],
        }));
        assert_eq!(described.text, "persona helper");
        let typed = message(json!({
            "msg": "persona helper",
            "files": [file],
            "attachments": [{ "description": "something else" }],
        }));
        assert_eq!(typed.text, "persona helper");
        let no_file = message(json!({ "msg": "", "attachments": [{ "description": "quoted" }] }));
        assert_eq!(no_file.text, "");
        let no_text_no_file =
            message(json!({ "msg": null, "attachments": [{ "description": "q" }] }));
        assert_eq!(no_text_no_file.text, "");
        let no_text = message(json!({
            "msg": null,
            "files": [file],
            "attachments": [{ "description": "persona helper" }],
        }));
        assert_eq!(no_text.text, "persona helper");
        let no_description = message(json!({ "msg": null, "files": [file] }));
        assert_eq!(no_description.text, "");
    }

    #[test]
    fn room_type_d_is_a_dm_or_a_group_dm_by_member_count() {
        assert_eq!(conv_kind(&room("d", Some(2), &[])), Some(ConvKind::Dm));
        assert_eq!(conv_kind(&room("d", None, &["a", "b"])), Some(ConvKind::Dm));
        assert_eq!(conv_kind(&room("d", None, &["a"])), Some(ConvKind::Dm));
        assert_eq!(conv_kind(&room("d", None, &[])), Some(ConvKind::GroupDm));
        assert_eq!(conv_kind(&room("d", Some(3), &[])), Some(ConvKind::GroupDm));
        assert_eq!(
            conv_kind(&room("d", None, &["a", "b", "c"])),
            Some(ConvKind::GroupDm)
        );
        assert_eq!(
            conv_kind(&room("c", Some(40), &[])),
            Some(ConvKind::Channel)
        );
        assert_eq!(conv_kind(&room("p", Some(2), &[])), Some(ConvKind::Channel));
        assert_eq!(conv_kind(&room("l", Some(2), &[])), None);
        assert_eq!(conv_kind(&room("v", None, &[])), None);
    }

    #[test]
    fn a_mention_becomes_an_event_with_ids_from_the_message() {
        let msg = message(json!({
            "mentions": [
                { "_id": "b1", "username": "helper" },
                { "_id": "all", "username": "all" },
                { "_id": "b2", "username": "other" },
                { "_id": "here", "username": "here" },
                { "_id": "b1", "username": "helper" },
            ],
        }));
        let ev = event(&msg, ConvKind::Channel, false);
        assert_eq!(ev.event_id, "m1");
        assert_eq!(ev.message.id.as_str(), "m1");
        assert_eq!(ev.conv.surface, SurfaceKind::RocketChat);
        assert_eq!(ev.conv.team.as_str(), "chat.example");
        assert_eq!(ev.conv.conversation.as_str(), "R1");
        assert_eq!(ev.message.conv, ev.conv);
        assert_eq!(ev.sender.user.as_str(), "u1");
        assert_eq!(ev.sender.team, ev.conv.team);
        assert!(!ev.sender_is_bot);
        assert_eq!(ev.sender_bot_user, None);
        assert_eq!(ev.mentions, [UserId::from("b1"), UserId::from("b2")]);
        assert_eq!(ev.text, "hi @helper");
        assert_eq!(ev.thread_root, None);
        assert_eq!(ev.reply_to, None);
        assert_eq!(ev.conv_kind, ConvKind::Channel);
        assert_eq!(ev.received_at, datetime!(2026-09-30 12:00 UTC));
    }

    #[test]
    fn tmid_is_the_thread_root_and_the_reply_target() {
        let ev = event(
            &message(json!({ "tmid": "root" })),
            ConvKind::Channel,
            false,
        );
        assert_eq!(ev.thread_root.as_ref().map(|t| t.as_str()), Some("root"));
        let reply_to = ev.reply_to.unwrap();
        assert_eq!(reply_to.id.as_str(), "root");
        assert_eq!(reply_to.conv, ev.conv);
    }

    #[test]
    fn a_bot_sender_names_its_user_id_twice() {
        let ev = event(&message(json!({})), ConvKind::Dm, true);
        assert!(ev.sender_is_bot);
        assert_eq!(ev.sender_bot_user.as_ref(), Some(&ev.sender.user));
        assert!(ev.is_dm());
    }

    #[test]
    fn files_carry_a_download_url() {
        let msg = message(json!({
            "files": [{ "_id": "f1", "name": "a.png", "type": "image/png", "size": 3 }],
        }));
        let ev = event(&msg, ConvKind::Channel, false);
        assert_eq!(
            ev.files,
            [InFile {
                id: "f1".into(),
                name: "a.png".into(),
                mime_type: Some("image/png".into()),
                size: Some(3),
                url: "https://chat.example/file-upload/f1/a.png".into(),
            }]
        );
    }
}

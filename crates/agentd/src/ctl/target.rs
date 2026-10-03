//! Where a turn may post and react, from its [`Side`].
//!
//! Targets are strings as the model wrote them:
//!
//! | Target | Means |
//! | --- | --- |
//! | `here` | the turn's own thread (a DM's conversation) |
//! | `<conversation id>` | that conversation's top level |
//! | `<conversation id>/<message id>` | the thread under that message |
//!
//! A conversation id may be written `#C123` or as Slack's `<#C123|name>`.
//! Ids name conversations on the turn's own surface and team.
//!
//! The rules:
//!
//! - [`Side::Public`] (every channel turn, the owner's included, since
//!   channel text is untrusted): `post` may target only the current
//!   conversation.
//! - [`Side::Owner`] (the owner's DMs and owner-requested private tasks):
//!   `post` may target any conversation. The surface refuses those the
//!   agent's bot isn't a member of when the post is delivered.
//! - On either side, `react` may name only messages in the current
//!   conversation.
//!
//! Anything else is refused with a reason the model can read.

use core_types::{
    ConvRef, ConversationId, CtlError, CtlErrorCode, MessageId, MsgRef, ReplyTarget, Side,
};
use store::CtlTurn;

/// The longest conversation or message id accepted.
const MAX_ID_LEN: usize = 128;

/// The target of `agentctl post --to <to>` in `turn`, if its rules allow it.
pub(crate) fn post_target(turn: &CtlTurn, to: &str) -> Result<ReplyTarget, CtlError> {
    let to = to.trim();
    if to == "here" {
        return Ok(ReplyTarget::from(turn.thread.clone()));
    }
    let (conv, root) = parse(turn, to)?;
    match turn.side {
        Side::Owner => {}
        Side::Public if conv == turn.thread.conv => {}
        Side::Public => {
            return Err(refused(
                "on the public side, agentctl post may only target this conversation; use --to here",
            ));
        }
    }
    Ok(ReplyTarget {
        conv,
        thread_root: root,
    })
}

/// The message `agentctl react <emoji> [message]` reacts to in `turn`: the
/// turn's own message without one.
pub(crate) fn react_target(turn: &CtlTurn, message: Option<&str>) -> Result<MsgRef, CtlError> {
    let Some(message) = message.map(str::trim) else {
        return turn
            .trigger
            .clone()
            .map(|id| MsgRef {
                conv: turn.thread.conv.clone(),
                id,
            })
            .ok_or_else(|| {
                CtlError::new(
                    CtlErrorCode::BadRequest,
                    "this turn has no message of its own to react to; name a message id",
                )
            });
    };
    let (conv, id) = match message.split_once('/') {
        Some(_) => match parse(turn, message)? {
            (conv, Some(id)) => (conv, id),
            (_, None) => return Err(bad("expected a message id")),
        },
        None => (turn.thread.conv.clone(), message_id(message)?),
    };
    if conv != turn.thread.conv {
        return Err(refused(
            "agentctl react may only react to messages in this conversation",
        ));
    }
    Ok(MsgRef { conv, id })
}

/// A message id as the model wrote it, checked for shape.
pub(crate) fn message_id(text: &str) -> Result<MessageId, CtlError> {
    if is_id(text) {
        Ok(MessageId::new(text))
    } else {
        Err(bad(
            "a message id starts with a letter or digit, then letters, digits, '.', '_' and '-', at most 128 characters",
        ))
    }
}

/// `<conversation>[/<message>]`, on the turn's surface and team.
fn parse(turn: &CtlTurn, text: &str) -> Result<(ConvRef, Option<MessageId>), CtlError> {
    let (conv, root) = match text.split_once('/') {
        Some((conv, root)) => (conv, Some(message_id(root)?)),
        None => (text, None),
    };
    let conv = unwrap_conversation(conv);
    if !is_id(conv) {
        return Err(bad(
            "--to takes `here`, a conversation id, or `<conversation id>/<message id>`",
        ));
    }
    Ok((
        ConvRef {
            surface: turn.thread.conv.surface,
            team: turn.thread.conv.team.clone(),
            conversation: ConversationId::new(conv),
        },
        root,
    ))
}

/// `C123` from `C123`, `#C123` or `<#C123|name>`.
fn unwrap_conversation(text: &str) -> &str {
    if let Some(inner) = text.strip_prefix("<#").and_then(|t| t.strip_suffix('>')) {
        return inner.split_once('|').map_or(inner, |(id, _)| id);
    }
    text.strip_prefix('#').unwrap_or(text)
}

fn is_id(text: &str) -> bool {
    text.as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && text.len() <= MAX_ID_LEN
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn bad(message: &str) -> CtlError {
    CtlError::new(CtlErrorCode::BadRequest, message)
}

fn refused(message: &str) -> CtlError {
    CtlError::new(CtlErrorCode::Refused, message)
}

/// An emoji name as `agentctl react` takes it: colons around it are
/// dropped, and it is lowercased. Short names are letters, digits, `_`,
/// `+`, `'` and `-`, with an optional `::skin-tone-2` to `-6`.
pub(crate) fn emoji_name(text: &str) -> Result<String, CtlError> {
    let name = text.trim().trim_matches(':').to_ascii_lowercase();
    let (base, tone) = match name.split_once("::") {
        Some((base, tone)) => (base, Some(tone)),
        None => (name.as_str(), None),
    };
    let base_ok = !base.is_empty()
        && base.len() <= 64
        && base
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'+' | b'\'' | b'-'));
    let tone_ok = tone.is_none_or(|tone| {
        tone.strip_prefix("skin-tone-")
            .is_some_and(|n| matches!(n, "2" | "3" | "4" | "5" | "6"))
    });
    if base_ok && tone_ok {
        Ok(name)
    } else {
        Err(bad(
            "an emoji is a short name such as eyes or thumbsup, without spaces",
        ))
    }
}

#[cfg(test)]
mod tests {
    use core_types::{Hop, MemberKey, Requester, SurfaceKind, ThreadKey, TurnId, TurnKind};

    use super::*;

    fn conv(id: &str) -> ConvRef {
        ConvRef {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            conversation: id.into(),
        }
    }

    fn turn(side: Side) -> CtlTurn {
        CtlTurn {
            id: TurnId::new_v4(),
            requester: Requester {
                member: None,
                key: MemberKey {
                    surface: SurfaceKind::Slack,
                    team: "T1".into(),
                    user: "U1".into(),
                },
                outside: None,
            },
            hop: Hop::ZERO,
            kind: TurnKind::Normal,
            side,
            thread: ThreadKey {
                conv: conv("C1"),
                root: Some(MessageId::new("100.1")),
            },
            trigger: Some(MessageId::new("100.2")),
        }
    }

    fn code(result: Result<impl std::fmt::Debug, CtlError>) -> CtlErrorCode {
        result.unwrap_err().code
    }

    #[test]
    fn here_is_the_turns_thread_on_both_sides() {
        for side in [Side::Public, Side::Owner] {
            assert_eq!(
                post_target(&turn(side), " here ").unwrap(),
                ReplyTarget {
                    conv: conv("C1"),
                    thread_root: Some(MessageId::new("100.1")),
                }
            );
        }
    }

    #[test]
    fn public_post_may_target_only_the_current_conversation() {
        let public = turn(Side::Public);
        assert_eq!(
            post_target(&public, "C1").unwrap(),
            ReplyTarget {
                conv: conv("C1"),
                thread_root: None,
            }
        );
        assert_eq!(
            post_target(&public, "#C1/99.5").unwrap(),
            ReplyTarget {
                conv: conv("C1"),
                thread_root: Some(MessageId::new("99.5")),
            }
        );
        for other in ["C2", "#C2", "<#C2|general>", "C2/100.1", "general"] {
            let err = post_target(&public, other).unwrap_err();
            assert_eq!(err.code, CtlErrorCode::Refused, "{other}");
            assert!(err.message.contains("public side"), "{err}");
        }
    }

    #[test]
    fn owner_post_may_target_any_conversation_on_its_surface() {
        let owner = turn(Side::Owner);
        for (to, expected, root) in [
            ("C2", conv("C2"), None),
            ("<#C3|general>", conv("C3"), None),
            ("#D4/5.5", conv("D4"), Some(MessageId::new("5.5"))),
        ] {
            assert_eq!(
                post_target(&owner, to).unwrap(),
                ReplyTarget {
                    conv: expected,
                    thread_root: root,
                },
                "{to}"
            );
        }
    }

    #[test]
    fn malformed_targets_are_refused_on_both_sides() {
        for side in [Side::Public, Side::Owner] {
            for to in [
                "",
                "#",
                "a b",
                "C1/",
                "C1/x y",
                "../C1",
                "C1/2/3",
                &"C".repeat(129),
            ] {
                assert_eq!(
                    code(post_target(&turn(side), to)),
                    CtlErrorCode::BadRequest,
                    "{to:?}"
                );
            }
        }
    }

    #[test]
    fn react_defaults_to_the_turns_message() {
        assert_eq!(
            react_target(&turn(Side::Public), None).unwrap(),
            MsgRef {
                conv: conv("C1"),
                id: MessageId::new("100.2"),
            }
        );
        let mut no_trigger = turn(Side::Owner);
        no_trigger.trigger = None;
        assert_eq!(
            code(react_target(&no_trigger, None)),
            CtlErrorCode::BadRequest
        );
    }

    #[test]
    fn react_may_name_only_messages_in_the_current_conversation() {
        for side in [Side::Public, Side::Owner] {
            let turn = turn(side);
            assert_eq!(
                react_target(&turn, Some("99.9")).unwrap(),
                MsgRef {
                    conv: conv("C1"),
                    id: MessageId::new("99.9"),
                }
            );
            assert_eq!(
                react_target(&turn, Some("C1/99.9")).unwrap(),
                MsgRef {
                    conv: conv("C1"),
                    id: MessageId::new("99.9"),
                }
            );
            let err = react_target(&turn, Some("C2/99.9")).unwrap_err();
            assert_eq!(err.code, CtlErrorCode::Refused);
            assert!(err.message.contains("this conversation"), "{err}");
            for bad in ["", "a b", "C1/", "C2"] {
                let result = react_target(&turn, Some(bad));
                if bad == "C2" {
                    assert!(result.is_ok(), "a bare id is a message id");
                } else {
                    assert_eq!(code(result), CtlErrorCode::BadRequest, "{bad:?}");
                }
            }
        }
    }

    #[test]
    fn emoji_names_are_normalized_and_checked() {
        assert_eq!(emoji_name(":Eyes:").unwrap(), "eyes");
        assert_eq!(emoji_name("+1").unwrap(), "+1");
        assert_eq!(
            emoji_name("wave::skin-tone-3").unwrap(),
            "wave::skin-tone-3"
        );
        for bad in [
            "",
            "::",
            "a b",
            "wave::skin-tone-7",
            "wave::x",
            "<!here>",
            &"a".repeat(65),
        ] {
            assert_eq!(code(emoji_name(bad)), CtlErrorCode::BadRequest, "{bad:?}");
        }
    }
}

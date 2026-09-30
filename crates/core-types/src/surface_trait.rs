//! The [`Surface`] trait, which every chat platform adapter implements, and
//! the types it uses.
//!
//! Everything after [`InboundEvent`] is shared. Behavior differences go
//! through [`Surface::caps`], never through surface-name checks in shared
//! code.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{
    AgentId, BindingId, ConvRef, Cursor, InboundEvent, MemberKey, MessageId, MsgRef, ReplyTarget,
    ThreadKey,
};

/// The result type of [`Surface`] methods.
pub type Result<T, E = SurfaceError> = std::result::Result<T, E>;

/// A chat platform adapter.
///
/// Implementations are used as `dyn Surface`. Methods that post act as the
/// bot identity of the binding the implementation was built for.
#[async_trait::async_trait]
pub trait Surface: Send + Sync {
    /// Receives messages for `binding` and sends each, normalized, to `tx`.
    ///
    /// Runs until the connection ends for good, or until `tx` is closed,
    /// which returns [`SurfaceError::Closed`]. Reconnecting after a dropped
    /// connection happens inside.
    async fn events(&self, binding: &Binding, tx: Sender<InboundEvent>) -> Result<()>;

    /// Posts one message: one chunk from [`Surface::render`].
    async fn post(&self, to: &ReplyTarget, text: &str) -> Result<MsgRef>;

    /// Replaces the text of a message the bot posted.
    async fn edit(&self, msg: &MsgRef, text: &str) -> Result<()>;

    /// Adds a reaction, named without colons (`eyes`).
    async fn react(&self, msg: &MsgRef, emoji: &str) -> Result<()>;

    /// Removes a reaction the bot added, named without colons. Removing
    /// one that isn't there succeeds.
    async fn unreact(&self, msg: &MsgRef, emoji: &str) -> Result<()>;

    /// Whether the bot may post in `conv` as it is: whether it is a member
    /// already. A platform where posting to a conversation joins the poster
    /// to it (Rocket.Chat's public channels) checks membership, and
    /// [`post`](Self::post) and [`upload`](Self::upload) refuse such a
    /// conversation with [`SurfaceError::Forbidden`], so a bot never joins
    /// a conversation it wasn't added to. Where posting never joins, the
    /// platform refuses a post itself, and this answers true.
    async fn can_post(&self, conv: &ConvRef) -> Result<bool>;

    /// Uploads files to a conversation or thread.
    async fn upload(&self, to: &ReplyTarget, files: &[OutFile]) -> Result<()>;

    /// Reads the messages of a thread, or of a conversation's top level when
    /// the thread has no root.
    ///
    /// Returns at most `limit` messages, the newest ones older than `before`
    /// (or the newest ones overall without it), oldest first.
    async fn history(
        &self,
        thread: &ThreadKey,
        before: Option<Cursor>,
        limit: usize,
    ) -> Result<Vec<Msg>>;

    /// The platform's own copy of `event`'s message, normalized as the
    /// surface normalizes events, or `None` when the platform doesn't have
    /// it, or has it in a form the surface wouldn't deliver. Only the
    /// binding, the event id and the arrival time come from `event`;
    /// everything routing reads comes from the platform.
    ///
    /// A surface whose events arrive over a connection only the platform
    /// can speak on returns `event` itself without asking. One whose events
    /// someone else could forge, such as Slack's, where an agent's owner
    /// holds the app's signing secret, reads the message back from the
    /// platform. The pipeline asks before acting on a message for anyone
    /// but the agent's owner, and routes the copy instead of the event.
    async fn confirm(&self, event: &InboundEvent) -> Result<Option<InboundEvent>>;

    /// Converts Markdown to the surface's format and splits it into
    /// messages that each fit [`Caps::message_limit`].
    fn render(&self, markdown: &str) -> Vec<String>;

    /// What the surface supports.
    fn caps(&self) -> Caps;
}

/// One chat identity a surface listens and posts as: an agent's bot user, or
/// the manager bot.
///
/// Credentials such as bot tokens are not part of it. The surface looks them
/// up by [`Binding::id`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Binding {
    /// agentd's id for the binding.
    pub id: BindingId,
    /// The agent the binding belongs to, or `None` for the manager bot.
    pub agent: Option<AgentId>,
    /// The bot user's identity: surface, team and user id.
    pub bot: MemberKey,
}

/// A file to upload, staged on local disk.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutFile {
    /// The file name to show in the chat.
    pub name: String,
    /// Where the file's contents are on disk.
    pub path: PathBuf,
}

/// A file attached to an inbound message.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InFile {
    /// The platform's id for the file.
    pub id: String,
    /// The file name.
    pub name: String,
    /// The MIME type, when the platform gave one.
    pub mime_type: Option<String>,
    /// The size in bytes, when the platform gave one.
    pub size: Option<u64>,
    /// Where to download it. Downloading needs the bot's credentials.
    pub url: String,
}

/// One message read back with [`Surface::history`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Msg {
    /// The message's id in its conversation.
    pub id: MessageId,
    /// Who sent it. For a bot, its user id when known, or else its bot id,
    /// as for [`InboundEvent::sender`](crate::InboundEvent::sender).
    pub sender: MemberKey,
    /// Whether the sender is a bot.
    pub sender_is_bot: bool,
    /// The message text.
    pub text: String,
    /// Files attached to it.
    pub files: Vec<InFile>,
    /// When it was sent.
    #[serde(with = "time::serde::rfc3339")]
    pub sent_at: OffsetDateTime,
}

/// What a surface supports. Shared code branches on these, never on the
/// surface's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Caps {
    /// The most one message may hold, in the unit the platform counts.
    pub message_limit: Limit,
    /// Whether posted messages can be edited.
    pub supports_edit: bool,
    /// Whether messages can carry interactive buttons.
    pub supports_buttons: bool,
    /// Whether messages can be posted in threads.
    pub supports_threads: bool,
    /// Whether every agent's binding receives its own copy of an event
    /// (Slack), rather than agentd keeping one copy per message
    /// (Rocket.Chat). When true, only the receiving binding's agent is a
    /// candidate for the event.
    pub per_binding_delivery: bool,
}

/// A text length limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Limit {
    /// The largest allowed length.
    pub max: usize,
    /// The unit `max` counts.
    pub unit: LengthUnit,
}

/// The unit a [`Limit`] counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LengthUnit {
    /// Unicode scalar values (Rust `char`s).
    Chars,
    /// UTF-16 code units, as JavaScript counts string length. A character
    /// outside the Basic Multilingual Plane, such as most emoji, counts as
    /// two.
    Utf16,
}

/// An error from a [`Surface`] method.
///
/// Messages carry platform error codes and descriptions, never tokens or
/// message content.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SurfaceError {
    /// The platform asked to slow down. Retry after the given time.
    #[error("rate limited, retry after {retry_after:?}")]
    RateLimited {
        /// How long to wait before retrying.
        retry_after: Duration,
    },
    /// The platform rejected the bot's credentials.
    #[error("the platform rejected the bot's credentials")]
    Unauthorized,
    /// The bot may not do this, for example post in a conversation it isn't
    /// a member of.
    #[error("not permitted: {0}")]
    Forbidden(String),
    /// The conversation, message or file doesn't exist.
    #[error("not found: {0}")]
    NotFound(String),
    /// The surface doesn't support the operation named. [`Caps`] says what
    /// it supports.
    #[error("{0} is not supported on this surface")]
    Unsupported(&'static str),
    /// The receiver of [`Surface::events`] was dropped.
    #[error("the event receiver is closed")]
    Closed,
    /// A file is larger than the limit the caller set, which the message
    /// names.
    #[error("too large: {0}")]
    TooLarge(String),
    /// The platform answered with another error.
    #[error("platform error: {0}")]
    Api(String),
    /// The platform couldn't be reached, or its answer couldn't be read.
    #[error("transport error: {0}")]
    Transport(String),
}

impl From<SendError> for SurfaceError {
    fn from(_: SendError) -> Self {
        Self::Closed
    }
}

/// Where [`Surface::events`] delivers events: a cloneable handle to a
/// [`Sink`].
///
/// core-types does no I/O and has no async runtime, so it defines this
/// instead of using a runtime's channel. The receiving side wraps its
/// channel's sender in a [`Sink`].
pub struct Sender<T: Send + 'static> {
    sink: Arc<dyn Sink<T>>,
}

impl<T: Send + 'static> Sender<T> {
    /// Wraps a sink.
    pub fn new(sink: impl Sink<T> + 'static) -> Self {
        Self {
            sink: Arc::new(sink),
        }
    }

    /// Delivers one item, waiting while the receiver is full. Fails once the
    /// receiver is gone.
    pub async fn send(&self, item: T) -> Result<(), SendError> {
        self.sink.send(item).await
    }
}

impl<T: Send + 'static> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            sink: Arc::clone(&self.sink),
        }
    }
}

impl<T: Send + 'static> fmt::Debug for Sender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sender").finish_non_exhaustive()
    }
}

/// The receiving end behind a [`Sender`], usually a runtime channel's
/// sender.
///
/// ```
/// use core_types::{SendError, Sender, Sink};
/// use std::sync::mpsc;
///
/// struct Channel(mpsc::Sender<u32>);
///
/// #[async_trait::async_trait]
/// impl Sink<u32> for Channel {
///     async fn send(&self, item: u32) -> Result<(), SendError> {
///         self.0.send(item).map_err(|_| SendError)
///     }
/// }
///
/// let (tx, rx) = mpsc::channel();
/// let sender = Sender::new(Channel(tx));
/// # let _ = (sender, rx);
/// ```
#[async_trait::async_trait]
pub trait Sink<T: Send + 'static>: Send + Sync {
    /// Delivers one item. Fails once the receiver is gone.
    async fn send(&self, item: T) -> Result<(), SendError>;
}

/// The receiver behind a [`Sender`] is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the receiver is closed")]
pub struct SendError;

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::Mutex;
    use std::task::{Context, Poll, Waker};

    use time::macros::datetime;

    use super::*;
    use crate::test_util::json_round_trip;
    use crate::{ConvKind, ConvRef, SurfaceKind};

    fn ready<F: Future>(future: F) -> F::Output {
        match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(output) => output,
            Poll::Pending => panic!("future was not ready"),
        }
    }

    #[derive(Default)]
    struct Collect {
        items: Mutex<Vec<InboundEvent>>,
        closed: bool,
    }

    #[async_trait::async_trait]
    impl Sink<InboundEvent> for Arc<Collect> {
        async fn send(&self, item: InboundEvent) -> Result<(), SendError> {
            if self.closed {
                return Err(SendError);
            }
            self.items.lock().unwrap().push(item);
            Ok(())
        }
    }

    fn conv() -> ConvRef {
        ConvRef {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            conversation: "C1".into(),
        }
    }

    fn member(user: &str) -> MemberKey {
        MemberKey {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            user: user.into(),
        }
    }

    fn event(binding: BindingId) -> InboundEvent {
        InboundEvent {
            event_id: "Ev1".into(),
            binding,
            sender: member("U1"),
            sender_is_bot: false,
            sender_bot_user: None,
            conv: conv(),
            conv_kind: ConvKind::Channel,
            thread_root: None,
            message: MsgRef {
                conv: conv(),
                id: "1.1".into(),
            },
            text: "hello".into(),
            mentions: vec![],
            reply_to: None,
            files: vec![],
            received_at: datetime!(2026-09-30 00:00 UTC),
        }
    }

    struct Echo {
        caps: Caps,
    }

    #[async_trait::async_trait]
    impl Surface for Echo {
        async fn events(&self, binding: &Binding, tx: Sender<InboundEvent>) -> Result<()> {
            tx.send(event(binding.id)).await?;
            tx.send(event(binding.id)).await?;
            Ok(())
        }

        async fn post(&self, to: &ReplyTarget, text: &str) -> Result<MsgRef> {
            Ok(MsgRef {
                conv: to.conv.clone(),
                id: MessageId::new(text),
            })
        }

        async fn edit(&self, _msg: &MsgRef, _text: &str) -> Result<()> {
            Err(SurfaceError::Unsupported("edit"))
        }

        async fn react(&self, msg: &MsgRef, _emoji: &str) -> Result<()> {
            Err(SurfaceError::NotFound(msg.id.to_string()))
        }

        async fn unreact(&self, _msg: &MsgRef, _emoji: &str) -> Result<()> {
            Ok(())
        }

        async fn can_post(&self, conv: &ConvRef) -> Result<bool> {
            Ok(conv.conversation.as_str() != "elsewhere")
        }

        async fn upload(&self, _to: &ReplyTarget, files: &[OutFile]) -> Result<()> {
            match files {
                [] => Err(SurfaceError::Api("no_file_data".into())),
                _ => Ok(()),
            }
        }

        async fn history(
            &self,
            thread: &ThreadKey,
            before: Option<Cursor>,
            limit: usize,
        ) -> Result<Vec<Msg>> {
            let text = before.map_or_else(String::new, |c| c.as_str().to_owned());
            Ok(vec![
                Msg {
                    id: thread.root.clone().unwrap_or_else(|| "0".into()),
                    sender: member("U1"),
                    sender_is_bot: false,
                    text,
                    files: vec![],
                    sent_at: datetime!(2026-09-30 00:00 UTC),
                };
                limit
            ])
        }

        async fn confirm(&self, event: &InboundEvent) -> Result<Option<InboundEvent>> {
            Ok((event.text != "forged").then(|| event.clone()))
        }

        fn render(&self, markdown: &str) -> Vec<String> {
            vec![markdown.to_owned()]
        }

        fn caps(&self) -> Caps {
            self.caps
        }
    }

    fn echo() -> Box<dyn Surface> {
        Box::new(Echo {
            caps: Caps {
                message_limit: Limit {
                    max: 5000,
                    unit: LengthUnit::Utf16,
                },
                supports_edit: false,
                supports_buttons: false,
                supports_threads: true,
                per_binding_delivery: false,
            },
        })
    }

    fn binding() -> Binding {
        Binding {
            id: BindingId::new_v4(),
            agent: Some(AgentId::new_v4()),
            bot: member("UBOT"),
        }
    }

    #[test]
    fn surface_is_usable_as_dyn() {
        let surface = echo();
        let target = ReplyTarget {
            conv: conv(),
            thread_root: None,
        };
        let posted = ready(surface.post(&target, "hi")).unwrap();
        assert_eq!(posted.id.as_str(), "hi");
        assert_eq!(
            ready(surface.edit(&posted, "x")),
            Err(SurfaceError::Unsupported("edit"))
        );
        assert!(matches!(
            ready(surface.react(&posted, "eyes")),
            Err(SurfaceError::NotFound(_))
        ));
        assert_eq!(ready(surface.unreact(&posted, "eyes")), Ok(()));
        assert_eq!(ready(surface.can_post(&conv())), Ok(true));
        let mut other = conv();
        other.conversation = "elsewhere".into();
        assert_eq!(ready(surface.can_post(&other)), Ok(false));
        let file = OutFile {
            name: "a.txt".into(),
            path: PathBuf::from("/tmp/a.txt"),
        };
        assert_eq!(ready(surface.upload(&target, &[file])), Ok(()));
        assert!(ready(surface.upload(&target, &[])).is_err());
        let thread = ThreadKey {
            conv: conv(),
            root: Some("1.0".into()),
        };
        let history = ready(surface.history(&thread, Some(Cursor::new("2.0")), 2)).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id.as_str(), "1.0");
        assert_eq!(history[0].text, "2.0");
        let mut forged = event(BindingId::new_v4());
        assert_eq!(ready(surface.confirm(&forged)), Ok(Some(forged.clone())));
        forged.text = "forged".into();
        assert_eq!(ready(surface.confirm(&forged)), Ok(None));
        assert_eq!(surface.render("**x**"), ["**x**"]);
        assert_eq!(surface.caps().message_limit.unit, LengthUnit::Utf16);
        assert_eq!(surface.caps().message_limit.max, 5000);
    }

    #[test]
    fn events_reach_the_sink_through_a_cloned_sender() {
        let surface = echo();
        let sink = Arc::new(Collect::default());
        let sender = Sender::new(Arc::clone(&sink));
        let binding = binding();
        ready(surface.events(&binding, sender.clone())).unwrap();
        let items = sink.items.lock().unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|e| e.binding == binding.id));
        assert_eq!(format!("{sender:?}"), "Sender { .. }");
    }

    #[test]
    fn a_closed_sink_ends_events_with_closed() {
        let surface = echo();
        let sink = Arc::new(Collect {
            closed: true,
            ..Collect::default()
        });
        let result = ready(surface.events(&binding(), Sender::new(sink)));
        assert_eq!(result, Err(SurfaceError::Closed));
    }

    #[test]
    fn surface_errors_describe_themselves() {
        let cases = [
            (
                SurfaceError::RateLimited {
                    retry_after: Duration::from_secs(3),
                },
                "rate limited, retry after 3s",
            ),
            (
                SurfaceError::Unauthorized,
                "the platform rejected the bot's credentials",
            ),
            (
                SurfaceError::Forbidden("not_in_channel".into()),
                "not permitted: not_in_channel",
            ),
            (
                SurfaceError::NotFound("message".into()),
                "not found: message",
            ),
            (
                SurfaceError::Unsupported("edit"),
                "edit is not supported on this surface",
            ),
            (SurfaceError::Closed, "the event receiver is closed"),
            (
                SurfaceError::Api("invalid_blocks".into()),
                "platform error: invalid_blocks",
            ),
            (
                SurfaceError::Transport("connection reset".into()),
                "transport error: connection reset",
            ),
        ];
        for (err, text) in cases {
            assert_eq!(err.to_string(), text);
        }
        assert_eq!(SendError.to_string(), "the receiver is closed");
        assert_eq!(SurfaceError::from(SendError), SurfaceError::Closed);
    }

    #[test]
    fn history_types_serde_round_trip() {
        let file = InFile {
            id: "F1".into(),
            name: "a.png".into(),
            mime_type: None,
            size: None,
            url: "https://files.example/F1".into(),
        };
        json_round_trip(&file);
        let json = json_round_trip(&Msg {
            id: "1.1".into(),
            sender: member("U1"),
            sender_is_bot: true,
            text: "hi".into(),
            files: vec![file],
            sent_at: datetime!(2026-09-30 01:02:03 UTC),
        });
        assert_eq!(json["sent_at"], "2026-09-30T01:02:03Z");
    }
}

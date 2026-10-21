//! DDP frames: what the realtime client sends and how it reads what the
//! server sends.
//!
//! Shapes follow the Rocket.Chat server source: `ee/apps/ddp-streamer`
//! (`codec.ts`, `Session.ts`) and Meteor's `ddp-server`, which Rocket.Chat
//! runs when the streamer isn't split out. Both accept the same client
//! frames and send the same server frames, except that the first frame is
//! `{"server_id": "0"}` from Meteor and `{"msg": "server_id", …}` from the
//! streamer; neither matters to the client.

use serde_json::{Value, json};

/// The DDP protocol version the client asks for. Rocket.Chat speaks `1`.
const VERSION: &str = "1";

/// `connect`: the first frame a client sends.
pub(crate) fn connect() -> String {
    json!({ "msg": "connect", "version": VERSION, "support": [VERSION] }).to_string()
}

/// `method login` with a resume token: a login token, or a personal access
/// token, which Rocket.Chat stores the same way.
pub(crate) fn login(id: &str, token: &str) -> String {
    json!({
        "msg": "method",
        "id": id,
        "method": "login",
        "params": [{ "resume": token }],
    })
    .to_string()
}

/// `sub` to one event of a Rocket.Chat stream. The second parameter,
/// `false`, turns collection compatibility off, so events arrive only as
/// `changed` frames.
pub(crate) fn sub(id: &str, stream: &str, event: &str) -> String {
    json!({
        "msg": "sub",
        "id": id,
        "name": stream,
        "params": [event, false],
    })
    .to_string()
}

/// `unsub`.
pub(crate) fn unsub(id: &str) -> String {
    json!({ "msg": "unsub", "id": id }).to_string()
}

/// `ping`, which the server answers with `pong`.
pub(crate) fn ping(id: &str) -> String {
    json!({ "msg": "ping", "id": id }).to_string()
}

/// `pong`, answering a server `ping` with its id, if it had one.
pub(crate) fn pong(id: Option<&str>) -> String {
    match id {
        Some(id) => json!({ "msg": "pong", "id": id }),
        None => json!({ "msg": "pong" }),
    }
    .to_string()
}

/// A method or subscription error, as Meteor reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DdpError {
    /// `error`: a number such as `403`, or a code such as `not-allowed`,
    /// as text.
    pub(crate) code: String,
    /// `reason`, when given. Never message content.
    pub(crate) reason: Option<String>,
}

impl DdpError {
    fn read(value: Option<&Value>) -> Option<Self> {
        let value = value?;
        let code = match value.get("error") {
            Some(Value::String(code)) => code.clone(),
            Some(Value::Number(code)) => code.to_string(),
            _ => String::new(),
        };
        let reason = value
            .get("reason")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        Some(Self { code, reason })
    }
}

impl std::fmt::Display for DdpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.reason {
            Some(reason) => write!(f, "{reason} [{}]", self.code),
            None => f.write_str(&self.code),
        }
    }
}

/// One frame from the server, as far as the client cares.
///
/// `Debug` prints whether a result is present and how many arguments an
/// event has, never their values: a login result holds the auth token, and
/// event arguments hold messages.
#[derive(Clone, PartialEq)]
pub(crate) enum Incoming {
    /// `connected`: the server accepted `connect`.
    Connected,
    /// `failed`: the server doesn't speak the version asked for.
    Failed,
    /// `ping`, with its id if any.
    Ping(Option<String>),
    /// `result` of a method call.
    Result {
        /// The call's id.
        id: String,
        /// The result, on success.
        result: Option<Value>,
        /// The error, on failure.
        error: Option<DdpError>,
    },
    /// `ready`: these subscriptions are live.
    Ready(Vec<String>),
    /// `nosub`: the subscription was refused or ended.
    Nosub {
        /// The subscription's id.
        id: String,
        /// Why, if the server said.
        error: Option<DdpError>,
    },
    /// `changed` in a stream: one event.
    Event {
        /// The stream, such as `stream-room-messages`.
        stream: String,
        /// The event name, such as a room id.
        event: String,
        /// The event's arguments.
        args: Vec<Value>,
    },
    /// Anything else: `pong`, `updated`, `added`, `server_id`, errors about
    /// frames the client never sends.
    Other,
}

impl std::fmt::Debug for Incoming {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connected => f.write_str("Connected"),
            Self::Failed => f.write_str("Failed"),
            Self::Ping(id) => f.debug_tuple("Ping").field(id).finish(),
            Self::Result { id, result, error } => f
                .debug_struct("Result")
                .field("id", id)
                .field("has_result", &result.is_some())
                .field("error", error)
                .finish(),
            Self::Ready(subs) => f.debug_tuple("Ready").field(subs).finish(),
            Self::Nosub { id, error } => f
                .debug_struct("Nosub")
                .field("id", id)
                .field("error", error)
                .finish(),
            Self::Event {
                stream,
                event,
                args,
            } => f
                .debug_struct("Event")
                .field("stream", stream)
                .field("event", event)
                .field("args_len", &args.len())
                .finish(),
            Self::Other => f.write_str("Other"),
        }
    }
}

impl Incoming {
    /// Reads one text frame. `None` when it isn't a JSON object.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let object = value.as_object()?;
        let text_field = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_owned);
        Some(match object.get("msg").and_then(Value::as_str) {
            Some("connected") => Self::Connected,
            Some("failed") => Self::Failed,
            Some("ping") => Self::Ping(text_field("id")),
            Some("result") => Self::Result {
                id: text_field("id").unwrap_or_default(),
                result: object.get("result").cloned(),
                error: DdpError::read(object.get("error")),
            },
            Some("ready") => Self::Ready(
                object
                    .get("subs")
                    .and_then(Value::as_array)
                    .map(|subs| {
                        subs.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default(),
            ),
            Some("nosub") => Self::Nosub {
                id: text_field("id").unwrap_or_default(),
                error: DdpError::read(object.get("error")),
            },
            Some("changed") => {
                let fields = object.get("fields");
                let event = fields
                    .and_then(|f| f.get("eventName"))
                    .and_then(Value::as_str);
                match (text_field("collection"), event) {
                    (Some(stream), Some(event)) => Self::Event {
                        stream,
                        event: event.to_owned(),
                        args: fields
                            .and_then(|f| f.get("args"))
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default(),
                    },
                    _ => Self::Other,
                }
            }
            _ => Self::Other,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value(frame: &str) -> Value {
        serde_json::from_str(frame).unwrap()
    }

    #[test]
    fn client_frames_have_the_ddp_shapes() {
        assert_eq!(
            value(&connect()),
            json!({ "msg": "connect", "version": "1", "support": ["1"] })
        );
        assert_eq!(
            value(&login("m1", "tok")),
            json!({ "msg": "method", "id": "m1", "method": "login", "params": [{ "resume": "tok" }] })
        );
        assert_eq!(
            value(&sub("s1", "stream-room-messages", "R1")),
            json!({
                "msg": "sub",
                "id": "s1",
                "name": "stream-room-messages",
                "params": ["R1", false],
            })
        );
        assert_eq!(value(&unsub("s1")), json!({ "msg": "unsub", "id": "s1" }));
        assert_eq!(value(&ping("p1")), json!({ "msg": "ping", "id": "p1" }));
        assert_eq!(value(&pong(Some("x"))), json!({ "msg": "pong", "id": "x" }));
        assert_eq!(value(&pong(None)), json!({ "msg": "pong" }));
    }

    #[test]
    fn debug_hides_results_and_event_arguments() {
        let login = Incoming::parse(
            r#"{"msg":"result","id":"m1","result":{"id":"u1","token":"TOKEN-SECRET"}}"#,
        )
        .unwrap();
        let event = Incoming::parse(
            r#"{"msg":"changed","collection":"stream-room-messages","fields":{"eventName":"r1","args":[{"msg":"MESSAGE-TEXT"},{}]}}"#,
        )
        .unwrap();
        let debug = format!("{login:?} {login:#?} {event:?} {event:#?}");
        assert!(!debug.contains("TOKEN-SECRET"), "{debug}");
        assert!(!debug.contains("MESSAGE-TEXT"), "{debug}");
        for shown in [
            "Result",
            "m1",
            "has_result: true",
            "stream-room-messages",
            "r1",
            "args_len: 2",
        ] {
            assert!(debug.contains(shown), "{shown}: {debug}");
        }
    }

    #[test]
    fn server_frames_parse() {
        assert_eq!(
            Incoming::parse(r#"{"msg":"connected","session":"s"}"#),
            Some(Incoming::Connected)
        );
        assert_eq!(
            Incoming::parse(r#"{"msg":"failed","version":"1"}"#),
            Some(Incoming::Failed)
        );
        assert_eq!(
            Incoming::parse(r#"{"msg":"ping"}"#),
            Some(Incoming::Ping(None))
        );
        assert_eq!(
            Incoming::parse(r#"{"msg":"ping","id":"7"}"#),
            Some(Incoming::Ping(Some("7".into())))
        );
        assert_eq!(
            Incoming::parse(r#"{"msg":"ready","subs":["a","b"]}"#),
            Some(Incoming::Ready(vec!["a".into(), "b".into()]))
        );
        assert_eq!(
            Incoming::parse(r#"{"server_id":"0"}"#),
            Some(Incoming::Other)
        );
        assert_eq!(
            Incoming::parse(r#"{"msg":"server_id","server_id":"0"}"#),
            Some(Incoming::Other)
        );
        assert_eq!(Incoming::parse("not json"), None);
        assert_eq!(Incoming::parse("[1]"), None);
    }

    #[test]
    fn results_and_errors_parse() {
        let ok = Incoming::parse(r#"{"msg":"result","id":"m1","result":{"id":"u1"}}"#);
        assert_eq!(
            ok,
            Some(Incoming::Result {
                id: "m1".into(),
                result: Some(json!({ "id": "u1" })),
                error: None,
            })
        );
        let Some(Incoming::Result { error, .. }) = Incoming::parse(
            r#"{"msg":"result","id":"m1","error":{"isClientSafe":true,"error":403,"reason":"You've been logged out by the server. Please log in again.","message":"x [403]","errorType":"Meteor.Error"}}"#,
        ) else {
            panic!("not a result");
        };
        let error = error.unwrap();
        assert_eq!(error.code, "403");
        assert_eq!(
            error.to_string(),
            "You've been logged out by the server. Please log in again. [403]"
        );
        let Some(Incoming::Nosub { id, error }) = Incoming::parse(
            r#"{"msg":"nosub","id":"s1","error":{"error":"not-allowed","message":"not-allowed"}}"#,
        ) else {
            panic!("not a nosub");
        };
        assert_eq!(id, "s1");
        assert_eq!(error.unwrap().to_string(), "not-allowed [not-allowed]");
        let bare = DdpError::read(Some(&json!({ "error": true }))).unwrap();
        assert_eq!(bare.to_string(), "");
    }

    #[test]
    fn stream_events_parse() {
        let frame = r#"{"msg":"changed","collection":"stream-room-messages","id":"id","fields":{"eventName":"R1","args":[{"_id":"m1"}]}}"#;
        assert_eq!(
            Incoming::parse(frame),
            Some(Incoming::Event {
                stream: "stream-room-messages".into(),
                event: "R1".into(),
                args: vec![json!({ "_id": "m1" })],
            })
        );
        let no_event =
            r#"{"msg":"changed","collection":"users","id":"u1","fields":{"status":"online"}}"#;
        assert_eq!(Incoming::parse(no_event), Some(Incoming::Other));
        let no_args = r#"{"msg":"changed","collection":"stream-notify-user","id":"id","fields":{"eventName":"u/x"}}"#;
        assert_eq!(
            Incoming::parse(no_args),
            Some(Incoming::Event {
                stream: "stream-notify-user".into(),
                event: "u/x".into(),
                args: vec![],
            })
        );
    }
}

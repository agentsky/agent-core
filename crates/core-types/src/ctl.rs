//! Request and response types of the agentctl API, shared by the `agentctl`
//! binary and agentd's ctl server.
//!
//! Every request is a `POST` to its [`CtlRequest::PATH`] with a bearer
//! token. The body is the request as JSON, except for [`AttachRequest`],
//! which goes in the query string while the body streams the file. A
//! success answers with [`CtlRequest::Response`] as JSON, and a failure with
//! a [`CtlError`].
//!
//! Message ids and targets are strings as the model wrote them. The server
//! resolves them against the current turn.

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{ConsentId, LeaseId, Msg};

/// A request to the agentctl API.
pub trait CtlRequest: Serialize + DeserializeOwned {
    /// The route the request is sent to.
    const PATH: &'static str;
    /// What a successful request answers with.
    type Response: Serialize + DeserializeOwned;
}

/// An empty success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Ack {}

/// `agentctl attach <path>`: stage a file to upload with this turn's reply.
///
/// Sent as the query string. The request body is the file's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachRequest {
    /// The file name to show in the chat.
    pub name: String,
}

impl CtlRequest for AttachRequest {
    const PATH: &'static str = "/v1/attach";
    type Response = AttachResponse;
}

/// The answer to [`AttachRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachResponse {
    /// The file name it was staged under.
    pub name: String,
    /// How many bytes were staged.
    pub size: u64,
}

/// `agentctl post --to <target> <text>`: post somewhere else the agent may
/// post. The message is queued and sent after the turn.
///
/// Its `Debug` output shows the text's length, never the text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostRequest {
    /// Where to post, as the model named it.
    pub to: String,
    /// The Markdown text to post.
    pub text: String,
}

impl fmt::Debug for PostRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PostRequest")
            .field("to", &self.to)
            .field("text_len", &self.text.len())
            .finish()
    }
}

impl CtlRequest for PostRequest {
    const PATH: &'static str = "/v1/post";
    type Response = Ack;
}

/// `agentctl react <emoji> [message id]`: add a reaction. It is queued and
/// applied after the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactRequest {
    /// The emoji name, without colons.
    pub emoji: String,
    /// The message to react to, or `None` for the message that started the
    /// turn.
    pub message: Option<String>,
}

impl CtlRequest for ReactRequest {
    const PATH: &'static str = "/v1/react";
    type Response = Ack;
}

/// `agentctl history [--before id]`: read more of the thread than the turn
/// included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryRequest {
    /// Return messages older than this one, or the newest without it.
    pub before: Option<String>,
    /// The most messages to return, or the server's default without it.
    pub limit: Option<u32>,
}

impl CtlRequest for HistoryRequest {
    const PATH: &'static str = "/v1/history";
    type Response = HistoryResponse;
}

/// The answer to [`HistoryRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryResponse {
    /// The messages, oldest first.
    pub messages: Vec<Msg>,
}

/// `agentctl lock -- <command>`: one step of holding the scope's `shared/`
/// lock while a command runs.
///
/// The lock is a lease. agentctl picks a new [`LeaseId`] for each `lock`
/// and sends it with every [`LockRequest::Acquire`], which grants that
/// lease when the lock is free or already held under it by the caller's
/// session. So an attempt that agentd granted after agentctl stopped
/// waiting for it is picked up by the next attempt instead of blocking it.
/// agentctl renews the lease while the command runs and releases it when
/// the command exits. Renew and release name the lease, and only the
/// current lease matches, so a second `agentctl lock` in the same session
/// (Claude Code runs tool calls in parallel) waits like any other holder,
/// and a stale release never frees the lock under someone else. A holder
/// that dies stops renewing, and the lease expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum LockRequest {
    /// Take the lock under `lease` if it is free, or hold it again if the
    /// caller's session already holds it under `lease`.
    Acquire {
        /// The lease agentctl picked for this `lock`.
        lease: LeaseId,
    },
    /// Extend a lease.
    Renew {
        /// The lease from [`LockResponse::Held`].
        lease: LeaseId,
    },
    /// Give a lease up.
    Release {
        /// The lease from [`LockResponse::Held`].
        lease: LeaseId,
    },
}

impl CtlRequest for LockRequest {
    const PATH: &'static str = "/v1/lock";
    type Response = LockResponse;
}

/// The state of a lease after a [`LockRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LockResponse {
    /// The caller holds the lock under `lease` until `expires_at`. Answers
    /// an acquire that got the lock and a renew of the current lease.
    Held {
        /// The lease to renew and release.
        lease: LeaseId,
        /// When the lease runs out unless renewed, on agentd's clock.
        #[serde(with = "time::serde::rfc3339")]
        expires_at: OffsetDateTime,
        /// How many seconds are left until `expires_at`, as agentd measured
        /// it when it answered, in the whole seconds leases are kept in: the
        /// lease really runs out between `seconds_left - 1` and
        /// `seconds_left` seconds after that. agentctl times the lease from
        /// this on its own clock, so skew between the two hosts' clocks
        /// doesn't matter.
        seconds_left: u64,
    },
    /// Another lease holds the lock, possibly one of the same session's.
    /// Answers an acquire. Try again later.
    Busy,
    /// The named lease doesn't hold the lock: it was released, it expired,
    /// or it never existed. Answers every release, and a renew of any lease
    /// but the current one.
    Released,
}

/// `agentctl ask-agent <agent> <task>`: hand a task to another agent. The
/// hop is billed to this turn's requester.
///
/// Its `Debug` output shows the task's length, never the task.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AskAgentRequest {
    /// The other agent's name.
    pub agent: String,
    /// The task for it.
    pub task: String,
}

impl fmt::Debug for AskAgentRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AskAgentRequest")
            .field("agent", &self.agent)
            .field("task_len", &self.task.len())
            .finish()
    }
}

impl CtlRequest for AskAgentRequest {
    const PATH: &'static str = "/v1/ask-agent";
    type Response = Ack;
}

/// `agentctl private [--file <path>]… <task>`: ask for a task on the owner's
/// private resources. It answers at once with a consent id.
///
/// Its `Debug` output shows the task's length, never the task.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateRequest {
    /// The task text, shown to the owner exactly as given.
    pub task: String,
    /// Files in the calling session's directory, as the CLI sees their
    /// paths, to copy into the private task.
    pub files: Vec<String>,
}

impl fmt::Debug for PrivateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateRequest")
            .field("task_len", &self.task.len())
            .field("files", &self.files)
            .finish()
    }
}

impl CtlRequest for PrivateRequest {
    const PATH: &'static str = "/v1/private";
    type Response = PrivateResponse;
}

/// The answer to [`PrivateRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateResponse {
    /// The consent request created for the task.
    pub consent: ConsentId,
}

/// A refused or failed agentctl request.
///
/// `message` is one line the model can read. agentctl prints it and exits
/// non-zero.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("{message}")]
pub struct CtlError {
    /// What kind of failure it is.
    pub code: CtlErrorCode,
    /// Why, in one line.
    pub message: String,
}

impl CtlError {
    /// Builds an error.
    pub fn new(code: CtlErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// The kind of a [`CtlError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CtlErrorCode {
    /// The token is unknown, or the connection comes from the wrong address.
    Unauthorized,
    /// The token's process has no turn running.
    NoTurn,
    /// The turn may not do this: a target rule, or a command refused inside
    /// a private task.
    Refused,
    /// The command isn't available yet.
    NotAvailable,
    /// The request is malformed.
    BadRequest,
    /// A message, target or agent it names doesn't exist.
    NotFound,
    /// An attachment is over the size cap.
    TooLarge,
    /// agentd failed.
    Internal,
}

#[cfg(test)]
mod tests {
    use serde::de::DeserializeOwned;
    use serde_json::json;
    use time::macros::datetime;

    use super::*;
    use crate::test_util::json_round_trip;
    use crate::{MemberKey, SurfaceKind};

    fn exchange<R>(request: &R, response: &R::Response) -> (serde_json::Value, serde_json::Value)
    where
        R: CtlRequest + PartialEq + std::fmt::Debug,
        R::Response: PartialEq + std::fmt::Debug,
    {
        (json_round_trip(request), json_round_trip(response))
    }

    fn assert_rejects<T: DeserializeOwned + std::fmt::Debug>(value: serde_json::Value) {
        assert!(serde_json::from_value::<T>(value).is_err());
    }

    #[test]
    fn paths_are_distinct_and_versioned() {
        let paths = [
            AttachRequest::PATH,
            PostRequest::PATH,
            ReactRequest::PATH,
            HistoryRequest::PATH,
            LockRequest::PATH,
            AskAgentRequest::PATH,
            PrivateRequest::PATH,
        ];
        for (i, path) in paths.iter().enumerate() {
            assert!(path.starts_with("/v1/"), "{path}");
            assert!(!paths[..i].contains(path), "{path}");
        }
    }

    #[test]
    fn attach_round_trips() {
        let (request, response) = exchange(
            &AttachRequest {
                name: "report.pdf".into(),
            },
            &AttachResponse {
                name: "report.pdf".into(),
                size: 1024,
            },
        );
        assert_eq!(request, json!({"name": "report.pdf"}));
        assert_eq!(response, json!({"name": "report.pdf", "size": 1024}));
    }

    #[test]
    fn post_and_react_round_trip() {
        let (request, response) = exchange(
            &PostRequest {
                to: "#general".into(),
                text: "done".into(),
            },
            &Ack {},
        );
        assert_eq!(request, json!({"to": "#general", "text": "done"}));
        assert_eq!(response, json!({}));
        exchange(
            &ReactRequest {
                emoji: "eyes".into(),
                message: Some("3".into()),
            },
            &Ack::default(),
        );
        json_round_trip(&ReactRequest {
            emoji: "eyes".into(),
            message: None,
        });
    }

    #[test]
    fn history_round_trips() {
        exchange(
            &HistoryRequest {
                before: Some("1700000000.000100".into()),
                limit: Some(50),
            },
            &HistoryResponse {
                messages: vec![Msg {
                    id: "1699999999.000100".into(),
                    sender: MemberKey {
                        surface: SurfaceKind::Slack,
                        team: "T1".into(),
                        user: "U1".into(),
                    },
                    sender_is_bot: false,
                    text: "earlier".into(),
                    files: vec![],
                    sent_at: datetime!(2026-09-29 23:00 UTC),
                }],
            },
        );
        json_round_trip(&HistoryRequest {
            before: None,
            limit: None,
        });
    }

    #[test]
    fn lock_round_trips() {
        let lease: LeaseId = "67e55044-10b1-426f-9247-bb680e5fe0c8".parse().unwrap();
        assert_eq!(
            json_round_trip(&LockRequest::Acquire { lease }),
            json!({"op": "acquire", "lease": lease.to_string()})
        );
        assert_eq!(
            json_round_trip(&LockRequest::Renew { lease }),
            json!({"op": "renew", "lease": lease.to_string()})
        );
        assert_eq!(
            json_round_trip(&LockRequest::Release { lease }),
            json!({"op": "release", "lease": lease.to_string()})
        );
        assert_eq!(
            json_round_trip(&LockResponse::Held {
                lease,
                expires_at: datetime!(2026-09-30 12:00 UTC),
                seconds_left: 30,
            }),
            json!({
                "state": "held",
                "lease": lease.to_string(),
                "expires_at": "2026-09-30T12:00:00Z",
                "seconds_left": 30,
            })
        );
        assert_eq!(
            json_round_trip(&LockResponse::Busy),
            json!({"state": "busy"})
        );
        assert_eq!(
            json_round_trip(&LockResponse::Released),
            json!({"state": "released"})
        );
    }

    #[test]
    fn lock_renew_and_release_require_a_lease() {
        assert_rejects::<LockRequest>(json!({"op": "steal"}));
        assert_rejects::<LockRequest>(json!({"op": "renew"}));
        assert_rejects::<LockRequest>(json!({"op": "release"}));
        assert_rejects::<LockRequest>(json!({"op": "release", "lease": "nope"}));
        assert_rejects::<LockResponse>(
            json!({"state": "held", "expires_at": "2026-09-30T12:00:00Z", "seconds_left": 30}),
        );
        assert_rejects::<LockResponse>(json!({
            "state": "held",
            "lease": "67e55044-10b1-426f-9247-bb680e5fe0c8",
            "expires_at": "2026-09-30T12:00:00Z",
        }));
    }

    #[test]
    fn ask_agent_and_private_round_trip() {
        exchange(
            &AskAgentRequest {
                agent: "reviewer".into(),
                task: "review the diff".into(),
            },
            &Ack {},
        );
        let consent = ConsentId::new_v4();
        let (request, response) = exchange(
            &PrivateRequest {
                task: "check the private repo".into(),
                files: vec!["work/notes.md".into()],
            },
            &PrivateResponse { consent },
        );
        assert_eq!(
            request,
            json!({"task": "check the private repo", "files": ["work/notes.md"]})
        );
        assert_eq!(response, json!({"consent": consent.to_string()}));
        assert_rejects::<PrivateResponse>(json!({"consent": "nope"}));
    }

    #[test]
    fn debug_shows_text_lengths_not_chat_or_model_text() {
        let post = format!(
            "{:?}",
            PostRequest {
                to: "#general".into(),
                text: "the secret plan".into(),
            }
        );
        assert!(!post.contains("secret plan"), "{post}");
        assert!(
            post.contains("text_len: 15") && post.contains("#general"),
            "{post}"
        );
        let ask = format!(
            "{:?}",
            AskAgentRequest {
                agent: "reviewer".into(),
                task: "the secret plan".into(),
            }
        );
        assert!(!ask.contains("secret plan"), "{ask}");
        assert!(
            ask.contains("task_len: 15") && ask.contains("reviewer"),
            "{ask}"
        );
        let private = format!(
            "{:?}",
            PrivateRequest {
                task: "the secret plan".into(),
                files: vec!["work/notes.md".into()],
            }
        );
        assert!(!private.contains("secret plan"), "{private}");
        assert!(private.contains("task_len: 15"), "{private}");
        let history = format!(
            "{:?}",
            HistoryResponse {
                messages: vec![Msg {
                    id: "1.1".into(),
                    sender: MemberKey {
                        surface: SurfaceKind::Slack,
                        team: "T1".into(),
                        user: "U1".into(),
                    },
                    sender_is_bot: false,
                    text: "the secret plan".into(),
                    files: vec![],
                    sent_at: datetime!(2026-09-29 23:00 UTC),
                }],
            }
        );
        assert!(!history.contains("secret plan"), "{history}");
        assert!(history.contains("text_len: 15"), "{history}");
    }

    #[test]
    fn ctl_error_round_trips_and_displays_its_message() {
        let err = CtlError::new(
            CtlErrorCode::Refused,
            "post may only target this conversation",
        );
        assert_eq!(err.to_string(), "post may only target this conversation");
        assert_eq!(
            json_round_trip(&err),
            json!({"code": "refused", "message": "post may only target this conversation"})
        );
        for code in [
            CtlErrorCode::Unauthorized,
            CtlErrorCode::NoTurn,
            CtlErrorCode::Refused,
            CtlErrorCode::NotAvailable,
            CtlErrorCode::BadRequest,
            CtlErrorCode::NotFound,
            CtlErrorCode::TooLarge,
            CtlErrorCode::Internal,
        ] {
            json_round_trip(&CtlError::new(code, "x"));
        }
        assert_eq!(json_round_trip(&CtlErrorCode::NoTurn), json!("no_turn"));
        assert_rejects::<CtlError>(json!({"code": "teapot", "message": "x"}));
    }
}

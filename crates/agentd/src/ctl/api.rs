//! The agentctl API's routes: authentication, the private-task rule, and
//! one handler per command.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::rejection::{BytesRejection, QueryRejection};
use axum::extract::{ConnectInfo, DefaultBodyLimit, FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use core_types::{
    Ack, AskAgentRequest, AttachRequest, AttachResponse, CtlError, CtlErrorCode, CtlRequest,
    Cursor, HistoryRequest, HistoryResponse, LockRequest, LockResponse, MsgRef, OutFile,
    PostRequest, PrivateRequest, PrivateResponse, ReactRequest, SurfaceError, TurnKind,
};
use http_body_util::BodyExt as _;
use serde::de::DeserializeOwned;
use store::{CtlToken, CtlTurn, TokenHash};
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt as _;

use super::outbox::{QueuedPost, QueuedReaction};
use super::target;
use super::token::{MAX_PRESENTED_LEN, hash_token};
use super::{Ctl, MAX_POST_BYTES};
use crate::consents::{RequestError, StageError};

/// The largest JSON request body.
pub const JSON_BODY_LIMIT: usize = 64 * 1024;
/// How many messages `agentctl history` returns without `--limit`.
pub const DEFAULT_HISTORY_LIMIT: u32 = 50;
/// The most messages one `agentctl history` returns.
pub const MAX_HISTORY_LIMIT: u32 = 200;
/// How long a history read may take on the surface.
const HISTORY_TIMEOUT: Duration = Duration::from_secs(20);
/// How long an upload may take to arrive.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(300);
/// The longest attachment file name.
const MAX_NAME_LEN: usize = 255;

/// The routes, over `ctl`.
pub(super) fn router(ctl: Ctl) -> Router {
    Router::new()
        .route(AttachRequest::PATH, post(attach))
        .route(PostRequest::PATH, post(post_message))
        .route(ReactRequest::PATH, post(react))
        .route(HistoryRequest::PATH, post(history))
        .route(LockRequest::PATH, post(lock))
        .route(AskAgentRequest::PATH, post(ask_agent))
        .route(PrivateRequest::PATH, post(private))
        .fallback(unknown_route)
        .layer(DefaultBodyLimit::max(JSON_BODY_LIMIT))
        .with_state(ctl)
}

/// A refusal or failure, answered as a [`CtlError`] with a matching status.
#[derive(Debug)]
pub(super) struct ApiError(pub(super) CtlError);

impl From<CtlError> for ApiError {
    fn from(err: CtlError) -> Self {
        Self(err)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            CtlErrorCode::Unauthorized => StatusCode::UNAUTHORIZED,
            CtlErrorCode::NoTurn => StatusCode::CONFLICT,
            CtlErrorCode::Refused => StatusCode::FORBIDDEN,
            CtlErrorCode::NotAvailable => StatusCode::NOT_IMPLEMENTED,
            CtlErrorCode::BadRequest => StatusCode::BAD_REQUEST,
            CtlErrorCode::NotFound => StatusCode::NOT_FOUND,
            CtlErrorCode::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            CtlErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(self.0)).into_response()
    }
}

fn error(code: CtlErrorCode, message: impl Into<String>) -> ApiError {
    ApiError(CtlError::new(code, message))
}

pub(super) fn no_turn() -> ApiError {
    error(
        CtlErrorCode::NoTurn,
        "no turn is running for this process; agentctl works only during a turn",
    )
}

fn internal(what: &str, err: &dyn std::fmt::Display) -> ApiError {
    tracing::error!(error = %err, "agentctl: {what} failed");
    error(CtlErrorCode::Internal, "agentd failed; try again")
}

fn unauthorized() -> ApiError {
    error(
        CtlErrorCode::Unauthorized,
        "the agentctl token is missing, revoked, or not valid from this container",
    )
}

/// An authenticated request from a process with a turn running.
pub(super) struct Authorized {
    pub(super) hash: TokenHash,
    pub(super) token: CtlToken,
    pub(super) turn: CtlTurn,
}

/// Checks the bearer token, the connection's source address, and that a
/// turn is running.
async fn authorize(parts: &Parts, ctl: &Ctl) -> Result<Authorized, ApiError> {
    let presented = bearer(&parts.headers).ok_or_else(unauthorized)?;
    let hash = hash_token(presented);
    let token = ctl
        .store()
        .ctl_token(&hash)
        .await
        .map_err(|err| internal("reading the token", &err))?
        .ok_or_else(|| {
            tracing::debug!("agentctl request with an unknown token");
            unauthorized()
        })?;
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| peer.ip().to_canonical());
    if peer != Some(token.container_ip) {
        tracing::warn!(
            session = %token.session,
            expected = %token.container_ip,
            peer = ?peer,
            "agentctl request from an address its token is not bound to"
        );
        return Err(unauthorized());
    }
    let turn = token.turn.clone().ok_or_else(no_turn)?;
    Ok(Authorized { hash, token, turn })
}

/// The token in `Authorization: Bearer <token>`.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && token.len() <= MAX_PRESENTED_LEN)
        .then_some(token)
}

/// A caller for any command but `attach`: refused inside a private task.
///
/// Every handler but `attach` takes this, so a new command is refused inside
/// private tasks unless it opts out.
pub(super) struct Caller(pub(super) Authorized);

impl FromRequestParts<Ctl> for Caller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, ctl: &Ctl) -> Result<Self, ApiError> {
        let authorized = authorize(parts, ctl).await?;
        if let TurnKind::PrivateTask(_) = authorized.turn.kind {
            tracing::info!(
                session = %authorized.token.session,
                turn = %authorized.turn.id,
                path = parts.uri.path(),
                "agentctl command refused inside a private task"
            );
            return Err(error(
                CtlErrorCode::Refused,
                "only `agentctl attach` is available inside a private task",
            ));
        }
        Ok(Self(authorized))
    }
}

/// A caller for `attach`, the one command a private task may use.
pub(super) struct AttachCaller(pub(super) Authorized);

impl FromRequestParts<Ctl> for AttachCaller {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, ctl: &Ctl) -> Result<Self, ApiError> {
        authorize(parts, ctl).await.map(Self)
    }
}

fn json_body<T: DeserializeOwned>(
    body: Result<Bytes, BytesRejection>,
    what: &str,
) -> Result<T, ApiError> {
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            error(
                CtlErrorCode::TooLarge,
                format!("the request is over {JSON_BODY_LIMIT} bytes"),
            )
        } else {
            error(
                CtlErrorCode::BadRequest,
                "the request body couldn't be read",
            )
        }
    })?;
    serde_json::from_slice(&body).map_err(|_| {
        error(
            CtlErrorCode::BadRequest,
            format!("the request isn't a valid {what} request"),
        )
    })
}

async fn unknown_route() -> ApiError {
    error(CtlErrorCode::NotFound, "unknown agentctl command")
}

/// Removes a partly written file unless kept.
struct PartialFile(Option<PathBuf>);

impl Drop for PartialFile {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// `POST /v1/attach?name=…`: streams the body into the turn's staging
/// directory.
///
/// A refused upload's body is still read, and thrown away, before the
/// refusal is sent, within the same time limit as an upload: a client
/// still sending when the connection closes would lose the answer, and the
/// model its reason.
async fn attach(
    State(ctl): State<Ctl>,
    AttachCaller(caller): AttachCaller,
    query: Result<Query<AttachRequest>, QueryRejection>,
    headers: HeaderMap,
    mut body: Body,
) -> Result<Json<AttachResponse>, ApiError> {
    let deadline = tokio::time::Instant::now() + ATTACH_TIMEOUT;
    match stage(&ctl, &caller, query, &headers, &mut body, deadline).await {
        Ok(response) => Ok(Json(response)),
        Err(err) => {
            let drain = async { while let Some(Ok(_)) = body.frame().await {} };
            let _ = tokio::time::timeout_at(deadline, drain).await;
            Err(err)
        }
    }
}

async fn stage(
    ctl: &Ctl,
    caller: &Authorized,
    query: Result<Query<AttachRequest>, QueryRejection>,
    headers: &HeaderMap,
    body: &mut Body,
    deadline: tokio::time::Instant,
) -> Result<AttachResponse, ApiError> {
    let Query(request) =
        query.map_err(|_| error(CtlErrorCode::BadRequest, "attach needs a file name"))?;
    let name = attachment_name(&request.name)?;
    let cap = ctl.settings().attach_max_bytes;
    let too_large = || {
        error(
            CtlErrorCode::TooLarge,
            format!("the file is over the {cap}-byte attachment limit"),
        )
    };
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|len| len > cap) {
        return Err(too_large());
    }

    let reservation = ctl.reserve_attachment(caller)?;
    let path = reservation.dir().join(uuid::Uuid::new_v4().to_string());
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => no_turn(),
            _ => internal("creating a staged file", &err),
        })?;
    let mut partial = PartialFile(Some(path.clone()));
    let mut size: u64 = 0;
    let receive = async {
        while let Some(frame) = body.frame().await {
            let frame =
                frame.map_err(|_| error(CtlErrorCode::BadRequest, "the upload was interrupted"))?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            size = size.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
            if size > cap {
                return Err(too_large());
            }
            file.write_all(&data)
                .await
                .map_err(|err| internal("writing a staged file", &err))?;
        }
        file.flush()
            .await
            .map_err(|err| internal("writing a staged file", &err))
    };
    tokio::time::timeout_at(deadline, receive)
        .await
        .map_err(|_| error(CtlErrorCode::BadRequest, "the upload took too long"))??;
    drop(file);

    ctl.commit_attachment(
        reservation,
        OutFile {
            name: name.clone(),
            path,
        },
    )?;
    partial.0 = None;
    tracing::debug!(
        session = %caller.token.session,
        turn = %caller.turn.id,
        size,
        "agentctl staged an attachment"
    );
    Ok(AttachResponse { name, size })
}

/// Whether `name` is a plain file name, never a path: at most 255 bytes,
/// not `.` or `..`, with no slash, backslash, control or invisible
/// formatting character.
pub(crate) fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control() || is_invisible(c))
}

/// Checks an attachment's display name: a plain file name, never a path.
fn attachment_name(name: &str) -> Result<String, ApiError> {
    if is_plain_file_name(name) {
        Ok(name.to_owned())
    } else {
        Err(error(
            CtlErrorCode::BadRequest,
            "the attachment name must be a plain file name of at most 255 bytes, with no \
             control or invisible formatting characters",
        ))
    }
}

/// Whether `c` changes how text displays without showing itself:
/// Unicode's default-ignorable code points
/// ([`render::is_default_ignorable`]), among them bidirectional controls,
/// which can make `exe.txt` read as `txt.exe`, zero-width characters,
/// variation selectors and tag characters, and also the line and paragraph
/// separators and the interlinear annotation characters.
pub(crate) fn is_invisible(c: char) -> bool {
    render::is_default_ignorable(c)
        || matches!(c, '\u{2028}' | '\u{2029}' | '\u{FFF9}'..='\u{FFFB}')
}

/// `text` without the characters that only choose how what is around them
/// is drawn: the text and emoji presentation selectors (U+FE0E, U+FE0F),
/// which emoji such as ⚠️ carry, and the zero-width non-joiner and joiner
/// (U+200C, U+200D), which Persian and other joining scripts, and emoji
/// sequences such as 👨‍💻, use. Without them the text reads the same, its
/// emoji in their default form or as their parts, so a card can show
/// exactly what the model reads while [`is_invisible`] refuses every other
/// invisible character.
pub(crate) fn without_joiners(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '\u{FE0E}' | '\u{FE0F}' | '\u{200C}' | '\u{200D}'))
        .collect()
}

/// `POST /v1/post`: queues a message after checking its target.
async fn post_message(
    State(ctl): State<Ctl>,
    Caller(caller): Caller,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<Ack>, ApiError> {
    let request: PostRequest = json_body(body, "post")?;
    let to = target::post_target(&caller.turn, &request.to)?;
    if request.text.trim().is_empty() {
        return Err(error(CtlErrorCode::BadRequest, "the message is empty"));
    }
    if request.text.len() > MAX_POST_BYTES {
        return Err(error(
            CtlErrorCode::TooLarge,
            format!("the message is over {MAX_POST_BYTES} bytes"),
        ));
    }
    let text_len = request.text.len();
    ctl.queue(&caller, |outbox| {
        outbox
            .push_post(QueuedPost {
                to,
                text: request.text,
            })
            .then_some(())
            .ok_or_else(|| {
                error(
                    CtlErrorCode::Refused,
                    format!(
                        "this turn has already queued {} messages",
                        super::outbox::MAX_POSTS
                    ),
                )
            })
    })?;
    tracing::debug!(
        session = %caller.token.session,
        turn = %caller.turn.id,
        text_len,
        "agentctl queued a post"
    );
    Ok(Json(Ack {}))
}

/// `POST /v1/react`: queues a reaction to a message in this conversation.
async fn react(
    State(ctl): State<Ctl>,
    Caller(caller): Caller,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<Ack>, ApiError> {
    let request: ReactRequest = json_body(body, "react")?;
    let emoji = target::emoji_name(&request.emoji)?;
    let msg = match short_message(&ctl, &caller, request.message.as_deref()).await? {
        Some(msg) if msg.conv == caller.turn.thread.conv => msg,
        Some(_) => {
            return Err(error(
                CtlErrorCode::Refused,
                "agentctl react may only react to messages in this conversation",
            ));
        }
        None => target::react_target(&caller.turn, request.message.as_deref())?,
    };
    ctl.queue(&caller, |outbox| {
        outbox
            .push_reaction(QueuedReaction { msg, emoji })
            .then_some(())
            .ok_or_else(|| {
                error(
                    CtlErrorCode::Refused,
                    format!(
                        "this turn has already queued {} reactions",
                        super::outbox::MAX_REACTIONS
                    ),
                )
            })
    })?;
    Ok(Json(Ack {}))
}

/// `POST /v1/history`: reads the turn's thread through the surface.
async fn history(
    State(ctl): State<Ctl>,
    Caller(caller): Caller,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<HistoryResponse>, ApiError> {
    let request: HistoryRequest = json_body(body, "history")?;
    let limit = request.limit.unwrap_or(DEFAULT_HISTORY_LIMIT);
    if !(1..=MAX_HISTORY_LIMIT).contains(&limit) {
        return Err(error(
            CtlErrorCode::BadRequest,
            format!("--limit must be between 1 and {MAX_HISTORY_LIMIT}"),
        ));
    }
    let thread = &caller.turn.thread;
    let before = match short_message(&ctl, &caller, request.before.as_deref()).await? {
        Some(msg) if msg.conv == thread.conv => Some(Cursor::from(msg.id)),
        Some(_) => {
            return Err(error(
                CtlErrorCode::BadRequest,
                "--before must name a message in this conversation",
            ));
        }
        None => request
            .before
            .as_deref()
            .map(|id| target::message_id(id.trim()).map(Cursor::from))
            .transpose()?,
    };
    let surface = ctl
        .surfaces()
        .surface(caller.token.agent, &thread.conv)
        .await
        .map_err(|err| internal("looking up the agent's surface", &err))?
        .ok_or_else(|| {
            error(
                CtlErrorCode::NotAvailable,
                "history isn't available for this conversation yet",
            )
        })?;
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let messages = tokio::time::timeout(HISTORY_TIMEOUT, surface.history(thread, before, limit))
        .await
        .map_err(|_| {
            error(
                CtlErrorCode::Internal,
                "the chat platform didn't answer in time",
            )
        })?
        .map_err(|err| match err {
            SurfaceError::NotFound(_) => {
                error(CtlErrorCode::NotFound, "no such message in this thread")
            }
            other => {
                tracing::warn!(error = %other, "agentctl history: the surface failed");
                error(
                    CtlErrorCode::Internal,
                    "the chat platform failed; try again",
                )
            }
        })?;
    Ok(Json(HistoryResponse { messages }))
}

/// The message a short id names, when `text` is one: `#` and up to nine
/// digits, as the turn message shows them, resolved in the caller's
/// session. `None` when `text` isn't a short id, so it is read as a
/// platform message id, which never starts with `#`.
async fn short_message(
    ctl: &Ctl,
    caller: &Authorized,
    text: Option<&str>,
) -> Result<Option<MsgRef>, ApiError> {
    let Some(short_id) = text.and_then(|text| short_id(text.trim())) else {
        return Ok(None);
    };
    let found = ctl
        .store()
        .message_ref_by_short_id(caller.token.session, short_id)
        .await
        .map_err(|err| internal("reading a message id", &err))?;
    match found {
        Some(row) => Ok(Some(row.msg)),
        None => Err(error(
            CtlErrorCode::NotFound,
            format!("no message #{short_id} in this session"),
        )),
    }
}

/// `3` from `#3`: a short id, 1 to 9 digits after `#`.
fn short_id(text: &str) -> Option<u32> {
    let digits = text.strip_prefix('#')?;
    (!digits.is_empty() && digits.len() <= 9 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

/// `POST /v1/lock`: one step of a lease on the volume's `shared/` lock.
async fn lock(
    State(ctl): State<Ctl>,
    Caller(caller): Caller,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<LockResponse>, ApiError> {
    let request: LockRequest = json_body(body, "lock")?;
    let store = ctl.store();
    let (token, turn) = (&caller.hash, caller.turn.id);
    let ttl = ctl.settings().lease_ttl;
    let now = OffsetDateTime::now_utc();
    let held = |lease, expires_at: OffsetDateTime| LockResponse::Held {
        lease,
        expires_at,
        seconds_left: u64::try_from(expires_at.unix_timestamp() - now.unix_timestamp())
            .unwrap_or(0),
    };
    let failed = |err: store::StoreError| internal("a scope lock query", &err);
    let response = match request {
        LockRequest::Acquire => match store
            .acquire_scope_lock(token, turn, now, ttl)
            .await
            .map_err(failed)?
        {
            Some(lease) => held(lease.lease, lease.expires_at),
            None => LockResponse::Busy,
        },
        LockRequest::Renew { lease } => match store
            .renew_scope_lock(token, turn, lease, now, ttl)
            .await
            .map_err(failed)?
        {
            Some(expires_at) => held(lease, expires_at),
            None => LockResponse::Released,
        },
        LockRequest::Release { lease } => {
            store
                .release_scope_lock(token, lease)
                .await
                .map_err(failed)?;
            LockResponse::Released
        }
    };
    tracing::debug!(
        session = %caller.token.session,
        op = ?request,
        state = ?response,
        "agentctl lock"
    );
    Ok(Json(response))
}

/// `POST /v1/ask-agent`: not available until agent-to-agent hand-off (T34).
async fn ask_agent(Caller(_): Caller) -> ApiError {
    error(
        CtlErrorCode::NotAvailable,
        "agentctl ask-agent is not available yet",
    )
}

/// `POST /v1/private`: records a consent for the task, with the files it
/// names copied out of the caller's session directory, and returns its id
/// at once. The task runs once the owner approves it, at once when the
/// owner asked for it in their own DM with the agent. A turn may ask for
/// [`MAX_PRIVATE_TASKS`].
///
/// [`MAX_PRIVATE_TASKS`]: super::MAX_PRIVATE_TASKS
async fn private(
    State(ctl): State<Ctl>,
    Caller(caller): Caller,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<PrivateResponse>, ApiError> {
    let request: PrivateRequest = json_body(body, "private")?;
    ctl.reserve_private_task(&caller)?;
    let requested = ctl
        .consents()
        .request(&caller.token, &caller.turn, request)
        .await;
    if requested.is_err() {
        ctl.release_private_task(&caller);
    }
    let consent = requested.map_err(|err| match err {
        RequestError::BadRequest(message) => error(CtlErrorCode::BadRequest, message),
        RequestError::Stage(StageError::TooLarge(..)) => {
            error(CtlErrorCode::TooLarge, err.to_string())
        }
        RequestError::Stage(StageError::NotFound(_)) => {
            error(CtlErrorCode::NotFound, err.to_string())
        }
        RequestError::Stage(StageError::Io(ref io)) => internal("staging a file", io),
        RequestError::Stage(_) => error(CtlErrorCode::BadRequest, err.to_string()),
        RequestError::Inactive | RequestError::TooMany => {
            error(CtlErrorCode::Refused, err.to_string())
        }
        RequestError::Store(ref store) => internal("recording a consent", store),
        RequestError::Io(ref io) => internal("staging a consent's files", io),
    })?;
    Ok(Json(PrivateResponse { consent }))
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_str(value).unwrap());
        headers
    }

    #[test]
    fn bearer_accepts_only_a_bearer_token_of_sane_length() {
        assert_eq!(bearer(&headers("Bearer abc")), Some("abc"));
        assert_eq!(bearer(&headers("bearer  abc ")), Some("abc"));
        assert_eq!(bearer(&headers("Basic abc")), None);
        assert_eq!(bearer(&headers("Bearer ")), None);
        assert_eq!(bearer(&headers("Bearer")), None);
        assert_eq!(
            bearer(&headers(&format!("Bearer {}", "a".repeat(257)))),
            None
        );
        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn attachment_names_are_plain_file_names() {
        assert_eq!(attachment_name("report.pdf").unwrap(), "report.pdf");
        assert_eq!(attachment_name("résumé 1.txt").unwrap(), "résumé 1.txt");
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "../x",
            "a\\b",
            "a\nb",
            "invoice\u{202E}txt.exe",
            "a\u{200E}b",
            "a\u{200F}b",
            "\u{2066}x\u{2069}",
            "x\u{061C}",
            "a\u{200B}b",
            "a\u{FEFF}b",
            "a\u{2028}b",
            "a\u{E0041}b",
            &"x".repeat(256),
        ] {
            assert_eq!(
                attachment_name(bad).unwrap_err().0.code,
                CtlErrorCode::BadRequest,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn short_ids_are_a_hash_and_up_to_nine_digits() {
        assert_eq!(short_id("#3"), Some(3));
        assert_eq!(short_id("#123456789"), Some(123_456_789));
        for not in ["3", "#", "#1234567890", "#3a", "#-3", "1.2", "#C123"] {
            assert_eq!(short_id(not), None, "{not}");
        }
    }

    #[test]
    fn every_error_code_has_a_status() {
        for (code, status) in [
            (CtlErrorCode::Unauthorized, 401),
            (CtlErrorCode::NoTurn, 409),
            (CtlErrorCode::Refused, 403),
            (CtlErrorCode::NotAvailable, 501),
            (CtlErrorCode::BadRequest, 400),
            (CtlErrorCode::NotFound, 404),
            (CtlErrorCode::TooLarge, 413),
            (CtlErrorCode::Internal, 500),
        ] {
            assert_eq!(error(code, "x").into_response().status().as_u16(), status);
        }
    }
}

//! [`FireClient`] and [`classify`]: one request to a routine's fire
//! endpoint, and what its answer means.

use std::fmt;
use std::time::Duration;

use reqwest::header::{HeaderValue, RETRY_AFTER};
use reqwest::{Client, Response, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde_json::Value;

use crate::config::{CloudConfig, ConfigError};

/// The `anthropic-version` header every fire request sends.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";
/// The most of a response body read: past it a body counts as unreadable.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// The longest task sent, in bytes of UTF-8. The endpoint takes a `text` of
/// at most 65,536 characters, which this keeps under however they are
/// counted.
pub const MAX_TASK_BYTES: usize = 65_536;
/// What a session's link must start with, followed by exactly its id, to be
/// kept.
pub const SESSION_URL_PREFIX: &str = "https://claude.ai/code/";
/// The most characters after a routine id's `trig_`.
const MAX_ROUTINE_ID_TAIL: usize = 64;
/// The most characters after a session id's `session_`.
const MAX_SESSION_ID_TAIL: usize = 128;
/// The longest `error.type` kept from an error body.
const MAX_ERROR_TYPE: usize = 64;

/// Fires Claude Code routines: one `POST` per hand-off, never retried.
///
/// The client follows no redirects, so a request and its token never go
/// anywhere but the URL built from `[cloud] base_url`; decompresses
/// nothing; retries nothing, not even what reqwest would retry on its own;
/// keeps no idle connection, so a fire never meets a connection the server
/// closed meanwhile and is never left unsure for that; and honors the
/// system proxy settings as `auth`'s client does. The token, the task and
/// response bodies appear in no log line or error.
#[derive(Clone)]
pub struct FireClient {
    http: Client,
    base: Url,
    beta: HeaderValue,
}

impl fmt::Debug for FireClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FireClient")
            .field("base", &self.base.as_str())
            .field("beta", &self.beta)
            .finish_non_exhaustive()
    }
}

/// Why a [`FireClient`] couldn't be built, or why [`FireClient::fire`] sent
/// nothing. None of them repeats a token or a task.
#[derive(Debug, thiserror::Error)]
pub enum FireError {
    /// `[cloud]` has a bad value.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The HTTP client couldn't be built.
    #[error("couldn't build the cloud hand-off's HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    /// The routine id isn't `trig_` and 1 to 64 ASCII letters and digits.
    #[error("the routine id isn't trig_ and 1 to 64 ASCII letters and digits")]
    RoutineId,
    /// The routine token is empty or holds a character a header can't
    /// carry, such as a space or a line break.
    #[error("the routine token is empty or holds characters a header can't carry")]
    Token,
    /// The task is empty or longer than [`MAX_TASK_BYTES`].
    #[error("the task is empty or longer than 65536 bytes")]
    Task,
}

/// What a fire request led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireOutcome {
    /// A 200 naming the session it started.
    Fired {
        /// `claude_code_session_id`: `session_` and 1 to 128 ASCII letters
        /// and digits.
        session_id: String,
        /// `claude_code_session_url`, kept only when it is
        /// [`SESSION_URL_PREFIX`] followed by `session_id`.
        session_url: Option<String>,
    },
    /// The endpoint refused the request with a documented status (400,
    /// 401, 403, 404 or 429), or the connection failed before the request
    /// was sent. Assumed to have started no session.
    Rejected {
        /// The status, or `None` when nothing was sent.
        status: Option<u16>,
        /// `error.type` from the error body, when it is 1 to 64 lowercase
        /// ASCII letters, digits and `_`.
        error_type: Option<String>,
        /// `Retry-After` in whole seconds. An HTTP date is ignored.
        retry_after: Option<u32>,
    },
    /// Anything else: the session may or may not have started.
    Unknown {
        /// The status, if one came back.
        status: Option<u16>,
        /// Why nobody can tell.
        reason: UnknownReason,
    },
}

impl FireOutcome {
    /// `fired`, `rejected` or `unknown`: the hand-off's state, for logs and
    /// the store.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Fired { .. } => "fired",
            Self::Rejected { .. } => "rejected",
            Self::Unknown { .. } => "unknown",
        }
    }

    /// The HTTP status that came back, if any.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Fired { .. } => Some(200),
            Self::Rejected { status, .. } | Self::Unknown { status, .. } => *status,
        }
    }
}

/// Why a [`FireOutcome::Unknown`] can't say whether a session started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownReason {
    /// A 5xx.
    ServerError,
    /// A 3xx, which the client doesn't follow.
    Redirect,
    /// A status that is neither 200, a documented 4xx, a 3xx nor a 5xx.
    OtherStatus,
    /// The request, or reading the answer, timed out after the request may
    /// have been sent.
    Timeout,
    /// The connection failed after the request may have been sent.
    ConnectionLost,
    /// A 200 without a session id of the expected shape, or whose body
    /// isn't JSON or is longer than [`MAX_BODY_BYTES`].
    Unreadable,
}

impl UnknownReason {
    /// A short name for logs.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServerError => "server_error",
            Self::Redirect => "redirect",
            Self::OtherStatus => "other_status",
            Self::Timeout => "timeout",
            Self::ConnectionLost => "connection_lost",
            Self::Unreadable => "unreadable",
        }
    }
}

/// What came back from one fire request, for [`classify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exchange {
    /// The server answered with a status.
    Answered(Answer),
    /// Connecting failed before the request was sent: DNS, a refused
    /// connection, TLS, or the connect timeout.
    NotSent,
    /// The request timed out after it may have been sent.
    TimedOut,
    /// The connection failed after the request may have been sent.
    Lost,
}

/// An answer's status, `Retry-After` and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The HTTP status.
    pub status: u16,
    /// The `Retry-After` header, if it is visible ASCII.
    pub retry_after: Option<String>,
    /// The body, up to [`MAX_BODY_BYTES`].
    pub body: Result<Vec<u8>, BodyError>,
}

/// Why an answer's body couldn't be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    /// It is longer than [`MAX_BODY_BYTES`].
    TooLarge,
    /// Reading it timed out.
    TimedOut,
    /// The connection failed while it was read.
    Lost,
}

#[derive(Serialize)]
struct FireBody<'a> {
    text: &'a str,
}

impl FireClient {
    /// A client for `config`.
    ///
    /// # Errors
    ///
    /// [`FireError::Config`] for a value `[cloud]` refuses, and
    /// [`FireError::Client`] if the HTTP client can't be built.
    pub fn new(config: &CloudConfig) -> Result<Self, FireError> {
        config.validate()?;
        Self::build(config, config.timeout(), config.connect_timeout())
    }

    /// [`new`](Self::new) with the timeouts given, which tests shorten
    /// below what `[cloud]` accepts.
    fn build(
        config: &CloudConfig,
        timeout: Duration,
        connect_timeout: Duration,
    ) -> Result<Self, FireError> {
        let base = config.base_url()?;
        let beta = HeaderValue::from_str(&config.beta).map_err(|_| ConfigError::Invalid {
            key: "cloud.beta".to_owned(),
            message: "must be a header value".to_owned(),
        })?;
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(timeout)
            .connect_timeout(connect_timeout)
            .pool_max_idle_per_host(0)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .user_agent(concat!("agent-core/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| FireError::Client(err.without_url()))?;
        Ok(Self { http, base, beta })
    }

    /// Fires routine `routine_id` with `task` as its text, authenticated by
    /// the routine's `token`: one `POST
    /// {base_url}/v1/claude_code/routines/{routine_id}/fire`, never
    /// retried, whatever comes back. Logs the routine id, the status and
    /// the outcome's kind, never the token, the task or the body.
    ///
    /// # Errors
    ///
    /// [`FireError::RoutineId`], [`FireError::Token`] or
    /// [`FireError::Task`] for an argument that can't be sent, in which
    /// case nothing is.
    pub async fn fire(
        &self,
        routine_id: &str,
        token: &SecretString,
        task: &str,
    ) -> Result<FireOutcome, FireError> {
        if !is_routine_id(routine_id) {
            return Err(FireError::RoutineId);
        }
        let token = token.expose_secret();
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(FireError::Token);
        }
        if task.is_empty() || task.len() > MAX_TASK_BYTES {
            return Err(FireError::Task);
        }
        let mut url = self.base.clone();
        url.set_path(&format!("/v1/claude_code/routines/{routine_id}/fire"));
        let outcome = classify(self.exchange(url, token, task).await);
        log_outcome(routine_id, &outcome);
        Ok(outcome)
    }

    async fn exchange(&self, url: Url, token: &str, task: &str) -> Exchange {
        let sent = self
            .http
            .post(url)
            .bearer_auth(token)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("anthropic-beta", self.beta.clone())
            .json(&FireBody { text: task })
            .send()
            .await;
        let mut response = match sent {
            Ok(response) => response,
            Err(err) if err.is_builder() || err.is_connect() => return Exchange::NotSent,
            Err(err) if err.is_timeout() => return Exchange::TimedOut,
            Err(_) => return Exchange::Lost,
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let body = read_body(&mut response).await;
        Exchange::Answered(Answer {
            status,
            retry_after,
            body,
        })
    }
}

async fn read_body(response: &mut Response) -> Result<Vec<u8>, BodyError> {
    if response
        .content_length()
        .is_some_and(|len| len > MAX_BODY_BYTES as u64)
    {
        return Err(BodyError::TooLarge);
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > MAX_BODY_BYTES {
                    return Err(BodyError::TooLarge);
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => return Ok(body),
            Err(err) if err.is_timeout() => return Err(BodyError::TimedOut),
            Err(_) => return Err(BodyError::Lost),
        }
    }
}

fn log_outcome(routine: &str, outcome: &FireOutcome) {
    let kind = outcome.kind();
    match outcome {
        FireOutcome::Fired { session_id, .. } => tracing::info!(
            routine,
            status = 200,
            outcome = kind,
            session_id = session_id.as_str(),
            "fired a cloud routine"
        ),
        FireOutcome::Rejected {
            status,
            error_type,
            retry_after,
        } => tracing::warn!(
            routine,
            status = *status,
            outcome = kind,
            error_type = error_type.as_deref(),
            retry_after_secs = *retry_after,
            "fired a cloud routine"
        ),
        FireOutcome::Unknown { status, reason } => tracing::warn!(
            routine,
            status = *status,
            outcome = kind,
            reason = reason.as_str(),
            "fired a cloud routine"
        ),
    }
}

/// What `exchange` means for the hand-off:
///
/// - a 200 whose body names a session id of the expected shape is
///   [`FireOutcome::Fired`];
/// - 400, 401, 403, 404 and 429, whatever their body, and a connection that
///   failed before the request was sent, are [`FireOutcome::Rejected`];
/// - everything else is [`FireOutcome::Unknown`]: a 5xx, a redirect,
///   another status, a timeout or a connection lost after sending, and a
///   200 that can't be read.
pub fn classify(exchange: Exchange) -> FireOutcome {
    let answer = match exchange {
        Exchange::Answered(answer) => answer,
        Exchange::NotSent => {
            return FireOutcome::Rejected {
                status: None,
                error_type: None,
                retry_after: None,
            };
        }
        Exchange::TimedOut => return unknown(None, UnknownReason::Timeout),
        Exchange::Lost => return unknown(None, UnknownReason::ConnectionLost),
    };
    let status = Some(answer.status);
    match answer.status {
        200 => match answer.body {
            Ok(body) => session(&body).map_or_else(
                || unknown(status, UnknownReason::Unreadable),
                |(session_id, session_url)| FireOutcome::Fired {
                    session_id,
                    session_url,
                },
            ),
            Err(BodyError::TooLarge) => unknown(status, UnknownReason::Unreadable),
            Err(BodyError::TimedOut) => unknown(status, UnknownReason::Timeout),
            Err(BodyError::Lost) => unknown(status, UnknownReason::ConnectionLost),
        },
        400 | 401 | 403 | 404 | 429 => FireOutcome::Rejected {
            status,
            error_type: answer.body.ok().and_then(|body| error_type(&body)),
            retry_after: answer.retry_after.as_deref().and_then(retry_after_secs),
        },
        300..=399 => unknown(status, UnknownReason::Redirect),
        500..=599 => unknown(status, UnknownReason::ServerError),
        _ => unknown(status, UnknownReason::OtherStatus),
    }
}

fn unknown(status: Option<u16>, reason: UnknownReason) -> FireOutcome {
    FireOutcome::Unknown { status, reason }
}

/// The session id and, when it is the session's own claude.ai link, the
/// URL a 200's body names.
fn session(body: &[u8]) -> Option<(String, Option<String>)> {
    let value = serde_json::from_slice::<Value>(body).ok()?;
    let id = value.get("claude_code_session_id")?.as_str()?;
    if !is_session_id(id) {
        return None;
    }
    let url = value
        .get("claude_code_session_url")
        .and_then(Value::as_str)
        .filter(|url| url.strip_prefix(SESSION_URL_PREFIX) == Some(id))
        .map(str::to_owned);
    Some((id.to_owned(), url))
}

/// `error.type` from an error body, read leniently: anything that isn't
/// such an envelope, or a type that isn't a short code, gives `None`.
fn error_type(body: &[u8]) -> Option<String> {
    let value = serde_json::from_slice::<Value>(body).ok()?;
    let kind = value.get("error")?.get("type")?.as_str()?;
    let code = !kind.is_empty()
        && kind.len() <= MAX_ERROR_TYPE
        && kind
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    code.then(|| kind.to_owned())
}

/// `Retry-After` as whole seconds. An HTTP date, or anything but digits,
/// gives `None`.
fn retry_after_secs(value: &str) -> Option<u32> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn is_routine_id(id: &str) -> bool {
    id.strip_prefix("trig_")
        .is_some_and(|tail| is_alphanumeric_tail(tail, MAX_ROUTINE_ID_TAIL))
}

fn is_session_id(id: &str) -> bool {
    id.strip_prefix("session_")
        .is_some_and(|tail| is_alphanumeric_tail(tail, MAX_SESSION_ID_TAIL))
}

fn is_alphanumeric_tail(tail: &str, max: usize) -> bool {
    (1..=max).contains(&tail.len()) && tail.bytes().all(|b| b.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests;

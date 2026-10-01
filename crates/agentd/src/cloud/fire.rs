//! [`FireClient`]: one request to a routine's fire endpoint, and what its
//! answer means.

use std::fmt;
use std::time::Duration;

use core_types::RoutineId;
use reqwest::header::{HeaderValue, RETRY_AFTER};
use reqwest::{Client, Response, Url};
use secrecy::ExposeSecret;
use serde::Serialize;
use serde_json::Value;
use store::{CloudOutcome, CloudRoutineToken, CloudUnknownReason};

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
/// The most characters after a session id's `session_`.
const MAX_SESSION_ID_TAIL: usize = 128;
/// The longest `error.type` kept from an error body.
const MAX_ERROR_TYPE: usize = 64;
/// The longest `Retry-After` kept, a day: the endpoint's documented limits
/// are hourly, and a longer one is read as this.
pub const MAX_RETRY_AFTER_SECS: u32 = 24 * 60 * 60;

/// Fires Claude Code routines: one `POST` per hand-off, never retried.
///
/// The client follows no redirects, so a request and its token never go
/// anywhere but the URL built from `[cloud] base_url`; decompresses
/// nothing; retries nothing, not even what reqwest would retry on its own;
/// keeps no idle connection, so a fire never meets a connection the server
/// closed meanwhile and is never left unsure for that; and honors the
/// system proxy settings as `auth`'s client does, except where
/// [`core_types::skips_proxy`] says a `base_url` goes direct: plain `http`,
/// which a proxy would read token and all, and a loopback IP address,
/// which a proxy would reach on its own host. The token, the task and
/// response bodies appear in no log line or error.
#[derive(Clone)]
pub struct FireClient {
    http: Client,
    base: Url,
    origin: String,
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

/// Why a [`FireClient`] couldn't be built.
#[derive(Debug, thiserror::Error)]
pub enum FireClientError {
    /// `[cloud]` has a bad value.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The HTTP client couldn't be built.
    #[error("couldn't build the cloud hand-off's HTTP client: {0}")]
    Client(#[source] reqwest::Error),
}

/// Why a task can't be fired: it is empty or longer than
/// [`MAX_TASK_BYTES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the task is empty or longer than {} bytes", MAX_TASK_BYTES)]
pub struct TaskError;

/// Checks that `task` can be fired, before a hand-off is written: one that
/// can't would reach [`FireClient::fire`] only to be refused unsent.
///
/// # Errors
///
/// [`TaskError`] if it is empty or longer than [`MAX_TASK_BYTES`].
pub fn check_task(task: &str) -> Result<(), TaskError> {
    if task.is_empty() || task.len() > MAX_TASK_BYTES {
        return Err(TaskError);
    }
    Ok(())
}

/// What a fire request led to, which the hand-off records as it is:
///
/// - [`Fired`](CloudOutcome::Fired) for a 200 naming the session it
///   started. `session_id` is `session_` and 1 to 128 ASCII letters and
///   digits; `session_url` is kept only when it is [`SESSION_URL_PREFIX`]
///   followed by `session_id`.
/// - [`Rejected`](CloudOutcome::Rejected) for a documented refusal (400,
///   401, 403, 404 or 429), or a request that wasn't sent, with no status:
///   the connection failed first, the routine was registered for another
///   origin, or the task can't be sent. Assumed to have started no session.
///   `error_type` is `error.type` from the error body, when it is 1 to 64
///   lowercase ASCII letters, digits and `_`; `retry_after_secs` is
///   `Retry-After` in whole seconds, at most [`MAX_RETRY_AFTER_SECS`], and
///   an HTTP date is ignored.
/// - [`Unknown`](CloudOutcome::Unknown) for anything else: the session may
///   or may not have started. Its reason is never
///   [`NoAnswer`](store::CloudUnknownReason::NoAnswer), which only the
///   store's pass sets.
pub type FireOutcome = CloudOutcome;

/// What came back from one fire request, for [`classify`].
#[derive(Debug)]
enum Exchange {
    /// The server answered with a status.
    Answered(Answer),
    /// Nothing was sent: the routine or task couldn't be, or connecting
    /// failed (DNS, a refused connection, TLS, or the connect timeout).
    NotSent,
    /// The request timed out after it may have been sent.
    TimedOut,
    /// The connection failed after the request may have been sent.
    Lost,
}

/// An answer's status, `Retry-After` and body. `Debug` shows the body's
/// length only, since a body may echo what was sent.
struct Answer {
    /// The HTTP status.
    status: u16,
    /// The `Retry-After` header, if it is visible ASCII.
    retry_after: Option<String>,
    /// The body, up to [`MAX_BODY_BYTES`].
    body: Result<Vec<u8>, BodyError>,
}

impl fmt::Debug for Answer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Answer")
            .field("status", &self.status)
            .field("retry_after", &self.retry_after)
            .field("body_len", &self.body.as_ref().map(Vec::len))
            .finish()
    }
}

/// Why an answer's body couldn't be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyError {
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
    /// [`FireClientError::Config`] for a value `[cloud]` refuses, and
    /// [`FireClientError::Client`] if the HTTP client can't be built.
    pub fn new(config: &CloudConfig) -> Result<Self, FireClientError> {
        config.validate()?;
        Self::build(config, config.timeout(), config.connect_timeout(), None)
    }

    /// [`new`](Self::new) with the timeouts given, which tests shorten
    /// below what `[cloud]` accepts, and `proxy`, a proxy tests add as if
    /// the system had it.
    fn build(
        config: &CloudConfig,
        timeout: Duration,
        connect_timeout: Duration,
        proxy: Option<reqwest::Proxy>,
    ) -> Result<Self, FireClientError> {
        let base = config.base_url()?;
        let beta = HeaderValue::from_str(&config.beta).map_err(|_| ConfigError::Invalid {
            key: "cloud.beta".to_owned(),
            message: "must be a header value".to_owned(),
        })?;
        let mut builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(timeout)
            .connect_timeout(connect_timeout)
            .pool_max_idle_per_host(0)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .user_agent(concat!("agent-core/", env!("CARGO_PKG_VERSION")));
        if let Some(proxy) = proxy {
            builder = builder.proxy(proxy);
        }
        if core_types::skips_proxy(base.scheme(), base.host_str().unwrap_or_default()) {
            builder = builder.no_proxy();
        }
        let http = builder
            .build()
            .map_err(|err| FireClientError::Client(err.without_url()))?;
        let origin = base.origin().ascii_serialization();
        Ok(Self {
            http,
            base,
            origin,
            beta,
        })
    }

    /// The origin `[cloud] base_url` names, as
    /// `url::Origin::ascii_serialization` writes it: the form the store
    /// keeps a routine's with ([`CloudRoutineToken::url_origin`]).
    pub fn origin(&self) -> &str {
        &self.origin
    }

    /// Whether a routine registered with the fire URL origin `url_origin`,
    /// as the store keeps it, is for this client's endpoint, so that
    /// [`fire`](Self::fire) may send its token: the stored origin parses to
    /// [`origin`](Self::origin)'s. Parsing rather than comparing strings
    /// keeps a change in how an origin is written from locking members out.
    pub fn fires_for(&self, url_origin: &str) -> bool {
        Url::parse(url_origin).is_ok_and(|stored| stored.origin() == self.base.origin())
    }

    /// Fires `routine` with `task` as its text, authenticated by the
    /// routine's token: one `POST
    /// {base_url}/v1/claude_code/routines/{routine_id}/fire`, never
    /// retried, whatever comes back. A routine id is only letters and
    /// digits after `trig_`, so it can't change the path. Logs the routine
    /// id, the status and the outcome's state, and why nothing was sent
    /// when it wasn't, never the token, the task or the body.
    ///
    /// A routine registered for another origin than [`origin`](Self::origin)
    /// ([`fires_for`](Self::fires_for)), whose token is then not for this
    /// endpoint, and a task [`check_task`]
    /// refuses are not sent, and the outcome is [`CloudOutcome::Rejected`]
    /// with no status, as for a connection that failed first. The caller
    /// checks both before it writes the hand-off, to say what is wrong, and
    /// records whatever comes back.
    pub async fn fire(&self, routine: &CloudRoutineToken, task: &str) -> FireOutcome {
        let id = &routine.routine_id;
        let exchange = if !self.fires_for(&routine.url_origin) {
            not_sent(
                id,
                "the routine was registered for another origin than [cloud] base_url's",
            )
        } else if let Err(err) = check_task(task) {
            not_sent(id, &err.to_string())
        } else {
            self.exchange(id, routine.token.expose_secret(), task).await
        };
        let outcome = classify(exchange);
        log_outcome(id.as_str(), &outcome);
        outcome
    }

    async fn exchange(&self, routine: &RoutineId, token: &str, task: &str) -> Exchange {
        let mut url = self.base.clone();
        url.set_path(&format!("/v1/claude_code/routines/{routine}/fire"));
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
            Err(err) if err.is_builder() || err.is_connect() => {
                return not_sent(routine, &core_types::error_chain(&err.without_url()));
            }
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

/// Logs why a request wasn't sent, and says so.
fn not_sent(routine: &RoutineId, cause: &str) -> Exchange {
    tracing::warn!(
        routine = routine.as_str(),
        cause,
        "a cloud routine's fire request wasn't sent"
    );
    Exchange::NotSent
}

fn log_outcome(routine: &str, outcome: &FireOutcome) {
    let kind = outcome.state().as_str();
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
            retry_after_secs,
        } => tracing::warn!(
            routine,
            status = *status,
            outcome = kind,
            error_type = error_type.as_deref(),
            retry_after_secs = *retry_after_secs,
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
///   [`CloudOutcome::Fired`];
/// - 400, 401, 403, 404 and 429, whatever their body, and a request that
///   wasn't sent, are [`CloudOutcome::Rejected`];
/// - everything else is [`CloudOutcome::Unknown`]: a 5xx, a redirect,
///   another status, a timeout or a connection lost after sending, and a
///   200 that can't be read.
fn classify(exchange: Exchange) -> FireOutcome {
    let answer = match exchange {
        Exchange::Answered(answer) => answer,
        Exchange::NotSent => {
            return FireOutcome::Rejected {
                status: None,
                error_type: None,
                retry_after_secs: None,
            };
        }
        Exchange::TimedOut => return unknown(None, CloudUnknownReason::Timeout),
        Exchange::Lost => return unknown(None, CloudUnknownReason::ConnectionLost),
    };
    let status = Some(answer.status);
    match answer.status {
        200 => match answer.body {
            Ok(body) => session(&body).map_or_else(
                || unknown(status, CloudUnknownReason::UnreadableAnswer),
                |(session_id, session_url)| FireOutcome::Fired {
                    session_id,
                    session_url,
                },
            ),
            Err(BodyError::TooLarge) => unknown(status, CloudUnknownReason::UnreadableAnswer),
            Err(BodyError::TimedOut) => unknown(status, CloudUnknownReason::Timeout),
            Err(BodyError::Lost) => unknown(status, CloudUnknownReason::ConnectionLost),
        },
        400 | 401 | 403 | 404 | 429 => FireOutcome::Rejected {
            status,
            error_type: answer.body.ok().and_then(|body| error_type(&body)),
            retry_after_secs: answer.retry_after.as_deref().and_then(retry_after_secs),
        },
        300..=399 => unknown(status, CloudUnknownReason::Redirect),
        500..=599 => unknown(status, CloudUnknownReason::ServerError),
        _ => unknown(status, CloudUnknownReason::OtherStatus),
    }
}

fn unknown(status: Option<u16>, reason: CloudUnknownReason) -> FireOutcome {
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

/// `Retry-After` as whole seconds, at most [`MAX_RETRY_AFTER_SECS`]. An
/// HTTP date, or anything but digits, gives `None`.
fn retry_after_secs(value: &str) -> Option<u32> {
    let value = value.trim();
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(
        value
            .parse::<u32>()
            .map_or(MAX_RETRY_AFTER_SECS, |secs| secs.min(MAX_RETRY_AFTER_SECS)),
    )
}

fn is_session_id(id: &str) -> bool {
    id.strip_prefix("session_").is_some_and(|tail| {
        (1..=MAX_SESSION_ID_TAIL).contains(&tail.len())
            && tail.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

#[cfg(test)]
mod tests;

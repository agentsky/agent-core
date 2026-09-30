//! The HTTPS ingress: Slack's request URLs, verification, the ack, and the
//! queue behind it.
//!
//! Every Slack app agentd runs has its own set of request URLs, keyed by
//! agentd's binding id rather than Slack's app id, because the manifest that
//! creates an app must already carry its URLs:
//!
//! - `POST /slack/b/{binding}/events` for the Events API,
//! - `POST /slack/b/{binding}/interactivity` for interactivity,
//! - `POST /slack/b/{binding}/commands` for slash commands.
//!
//! `{binding}` is `manager` for the manager app, or an agent binding's id.
//! It selects the signing secret through [`SigningSecrets`]. A request is
//! answered, in this order:
//!
//! 1. 404 if the binding is unknown (or not a binding id at all), 503 if the
//!    lookup failed.
//! 2. 413 if the body is larger than [`MAX_BODY_BYTES`].
//! 3. On `/events` only, a `url_verification` body is answered with its
//!    challenge, without checking the signature. Slack sends it while
//!    `apps.manifest.create` runs, before agentd has the new app's signing
//!    secret. The echo reads nothing and changes nothing, and it is given
//!    only for bindings agentd already knows, so it tells a forger nothing
//!    a 401 wouldn't (see the design's Slack transport notes).
//! 4. 401 unless the request carries a valid `v0` signature (see
//!    [`verify`](mod@crate::verify)) made with the binding's secret. A binding
//!    whose secret isn't known yet can't be verified, so everything but the
//!    challenge gets 401.
//! 5. 400 if a verified body can't be parsed.
//! 6. An empty 200 as soon as the request is on the queue, or 503 if the
//!    queue is full, so Slack retries later. The handler never waits for
//!    the queue. Slash commands and interactivity reply later through their
//!    `response_url`.
//!
//! [`Queue::run`] then deduplicates each request through [`Dedup`],
//! normalizes it, and hands it on as a [`SlackInbound`]:
//!
//! - Events by `event_id`, under the source `slack:<binding>`, which drops
//!   Slack's retries.
//! - Normalized messages also by `<channel>:<ts>`, under
//!   `slack:<binding>:message`, which drops a message that reached the same
//!   app twice with different event ids.
//! - Slash commands and interactivity by their signature, under
//!   `slack:<binding>:request`. Slack doesn't retry them, so a second copy is
//!   a replay inside the five-minute window.

use std::fmt;
use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use core_types::{BindingId, ConvRef, MemberKey, Sender, SurfaceKind, TeamId, UserId};
use http_body_util::{BodyExt as _, LengthLimitError, Limited};
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use uuid::Uuid;

use crate::inbound::{Interaction, SlackEvent, SlackInbound, SlashCommand};
use crate::normalize;
use crate::verify::{self, SIGNATURE_HEADER};

/// The largest request body accepted, in bytes. Slack's payloads are a few
/// kilobytes; a message is at most 40,000 characters of text plus its
/// blocks.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
/// The longest `url_verification` challenge echoed, in bytes. Slack's are
/// about 50 characters.
pub const MAX_CHALLENGE_BYTES: usize = 256;
/// The header in which Slack numbers a retried delivery.
pub const RETRY_NUM_HEADER: &str = "x-slack-retry-num";
/// The header in which Slack says why it retried.
pub const RETRY_REASON_HEADER: &str = "x-slack-retry-reason";

/// The error type of [`SigningSecrets`] and [`Dedup`]. It must not carry a
/// secret.
pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A binding named in a request URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingRef {
    /// The manager app, at `/slack/b/manager/…`.
    Manager,
    /// An agent's app, at `/slack/b/<binding id>/…`.
    Agent(BindingId),
}

impl BindingRef {
    /// The path segment of the manager app's URLs.
    pub const MANAGER_SEGMENT: &'static str = "manager";
    /// The binding id the manager app's events carry: the nil UUID, which
    /// no agent binding is ever minted with.
    pub const MANAGER_ID: BindingId = BindingId::from_uuid(Uuid::nil());

    /// Reads a request URL's `{binding}` segment: `manager`, or a binding id
    /// in its canonical lowercase form. Anything else is `None`.
    pub fn parse(segment: &str) -> Option<Self> {
        if segment == Self::MANAGER_SEGMENT {
            return Some(Self::Manager);
        }
        segment
            .parse::<BindingId>()
            .ok()
            .filter(|id| *id != Self::MANAGER_ID)
            .map(Self::Agent)
    }

    /// The binding id events from this binding carry.
    pub fn id(self) -> BindingId {
        match self {
            Self::Manager => Self::MANAGER_ID,
            Self::Agent(id) => id,
        }
    }
}

impl fmt::Display for BindingRef {
    /// The path segment: `manager` or the binding id.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manager => f.write_str(Self::MANAGER_SEGMENT),
            Self::Agent(id) => id.fmt(f),
        }
    }
}

/// What agentd knows about a binding's Slack app.
#[derive(Debug, Clone)]
pub struct SlackApp {
    /// The app's signing secret. `None` while the app is being created and
    /// `apps.manifest.create` hasn't returned it yet: the binding then
    /// answers only `url_verification`.
    pub signing_secret: Option<SecretString>,
    /// The app's bot user, once known. Channel messages are kept when they
    /// mention it.
    pub bot_user: Option<UserId>,
}

/// Looks up a binding's signing secret.
///
/// agentd implements it: the manager's secret comes from configuration, and
/// agent bindings' from the store.
#[async_trait::async_trait]
pub trait SigningSecrets: Send + Sync {
    /// The app behind `binding`, or `None` if agentd has no such binding.
    ///
    /// # Errors
    ///
    /// If the lookup itself fails. The request then gets 503, so Slack
    /// retries it.
    async fn lookup(&self, binding: BindingRef) -> Result<Option<SlackApp>, BoxError>;
}

/// Remembers which deliveries were already handled. agentd implements it
/// with the store's `processed_events`.
#[async_trait::async_trait]
pub trait Dedup: Send + Sync {
    /// Records `key` under `source`. Returns true the first time, and false
    /// when it was already recorded.
    ///
    /// # Errors
    ///
    /// If the record can't be written. The request is then dropped rather
    /// than risk handling it twice.
    async fn first_time(&self, source: &str, key: &str) -> Result<bool, BoxError>;
}

/// Builds the ingress: the router serving the request URLs, and the
/// [`Queue`] behind it, which holds at most `capacity` requests.
///
/// The queue closes when the router and every clone of it are dropped, and
/// [`Queue::run`] returns once it has handled what was queued.
pub fn ingress(secrets: Arc<dyn SigningSecrets>, capacity: usize) -> (Router, Queue) {
    let (sender, receiver) = mpsc::channel(capacity.max(1));
    let state = Ingress {
        secrets,
        queue: sender,
    };
    let router = Router::new()
        .route("/slack/b/{binding}/events", post(events))
        .route("/slack/b/{binding}/interactivity", post(interactivity))
        .route("/slack/b/{binding}/commands", post(commands))
        .with_state(state);
    (router, Queue { receiver })
}

#[derive(Clone)]
struct Ingress {
    secrets: Arc<dyn SigningSecrets>,
    queue: mpsc::Sender<Queued>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Events,
    Interactivity,
    Commands,
}

impl Kind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Events => "events",
            Self::Interactivity => "interactivity",
            Self::Commands => "commands",
        }
    }
}

async fn events(
    State(ingress): State<Ingress>,
    Path(binding): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    handle(&ingress, Kind::Events, &binding, &headers, body).await
}

async fn interactivity(
    State(ingress): State<Ingress>,
    Path(binding): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    handle(&ingress, Kind::Interactivity, &binding, &headers, body).await
}

async fn commands(
    State(ingress): State<Ingress>,
    Path(binding): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    handle(&ingress, Kind::Commands, &binding, &headers, body).await
}

async fn handle(
    ingress: &Ingress,
    kind: Kind,
    segment: &str,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let Some(binding) = BindingRef::parse(segment) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let app = match ingress.secrets.lookup(binding).await {
        Ok(Some(app)) => app,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            tracing::warn!(%binding, error = %err, "looking up a Slack binding failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let body = match read_body(body).await {
        Ok(body) => body,
        Err(status) => {
            tracing::warn!(%binding, kind = kind.as_str(), status = status.as_u16(), "refused a Slack request body");
            return status.into_response();
        }
    };
    let received_at = OffsetDateTime::now_utc();

    if kind == Kind::Events
        && let Some(challenge) = url_verification(&body)
    {
        return match challenge {
            Some(challenge) => {
                tracing::info!(%binding, "answered Slack's url_verification challenge");
                (
                    [
                        (CONTENT_TYPE, "text/plain; charset=utf-8"),
                        (X_CONTENT_TYPE_OPTIONS, "nosniff"),
                    ],
                    challenge,
                )
                    .into_response()
            }
            None => StatusCode::BAD_REQUEST.into_response(),
        };
    }

    let Some(secret) = app.signing_secret.as_ref() else {
        tracing::warn!(%binding, kind = kind.as_str(), "refused a Slack request: the binding has no signing secret yet");
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if let Err(rejection) = verify::verify(secret, headers, &body, received_at.unix_timestamp()) {
        tracing::warn!(%binding, kind = kind.as_str(), reason = %rejection, "refused a Slack request");
        return StatusCode::UNAUTHORIZED.into_response();
    }
    log_retry(binding, kind, headers);

    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let request = match parse(kind, &body, signature) {
        Ok(Parsed::Queue(request)) => request,
        Ok(Parsed::RateLimited {
            minute_rate_limited,
        }) => {
            tracing::warn!(%binding, minute_rate_limited, "Slack is rate limiting this app's events");
            return StatusCode::OK.into_response();
        }
        Ok(Parsed::Ignore(envelope_type)) => {
            tracing::debug!(%binding, envelope_type, "ignored a Slack envelope");
            return StatusCode::OK.into_response();
        }
        Err(reason) => {
            tracing::warn!(%binding, kind = kind.as_str(), reason, "refused a verified Slack request");
            return StatusCode::BAD_REQUEST.into_response();
        }
    };
    let queued = Queued {
        binding,
        bot_user: app.bot_user,
        received_at,
        request,
    };
    match ingress.queue.try_send(queued) {
        Ok(()) => StatusCode::OK.into_response(),
        Err(TrySendError::Full(_)) => {
            tracing::warn!(%binding, kind = kind.as_str(), "the Slack queue is full; asking Slack to retry");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
        Err(TrySendError::Closed(_)) => {
            tracing::warn!(%binding, kind = kind.as_str(), "the Slack queue is closed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

/// Reads the whole body, refusing more than [`MAX_BODY_BYTES`].
async fn read_body(body: Body) -> Result<Bytes, StatusCode> {
    match Limited::new(body, MAX_BODY_BYTES).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(err) if err.downcast_ref::<LengthLimitError>().is_some() => {
            Err(StatusCode::PAYLOAD_TOO_LARGE)
        }
        Err(_) => Err(StatusCode::BAD_REQUEST),
    }
}

#[derive(Deserialize)]
struct Probe {
    #[serde(rename = "type")]
    kind: Option<String>,
    challenge: Option<Value>,
}

/// `None` unless the body is a JSON object whose `type` is
/// `url_verification`. Then `Some` of its challenge, or `Some(None)` if the
/// challenge is missing, empty, longer than [`MAX_CHALLENGE_BYTES`], or not
/// printable ASCII.
fn url_verification(body: &[u8]) -> Option<Option<String>> {
    let probe: Probe = serde_json::from_slice(body).ok()?;
    if probe.kind.as_deref() != Some("url_verification") {
        return None;
    }
    Some(match probe.challenge {
        Some(Value::String(challenge))
            if !challenge.is_empty()
                && challenge.len() <= MAX_CHALLENGE_BYTES
                && challenge.bytes().all(|b| b.is_ascii_graphic()) =>
        {
            Some(challenge)
        }
        _ => None,
    })
}

/// Logs Slack's retry headers, which say that an earlier delivery wasn't
/// acknowledged in time.
fn log_retry(binding: BindingRef, kind: Kind, headers: &HeaderMap) {
    let Some(retry_num) = headers.get(RETRY_NUM_HEADER) else {
        return;
    };
    let retry_num = retry_num
        .to_str()
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let retry_reason = headers
        .get(RETRY_REASON_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|reason| {
            reason.len() <= 64 && reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        });
    tracing::info!(%binding, kind = kind.as_str(), retry_num, retry_reason, "Slack retried a delivery");
}

/// An Events API envelope. Other types are acknowledged and ignored.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Envelope {
    EventCallback(EventCallback),
    AppRateLimited {
        minute_rate_limited: Option<i64>,
    },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct EventCallback {
    team_id: Option<String>,
    event_id: String,
    event: Map<String, Value>,
}

#[derive(Deserialize)]
struct CommandForm {
    team_id: String,
    channel_id: String,
    user_id: String,
    command: String,
    #[serde(default)]
    text: String,
    response_url: String,
    trigger_id: Option<String>,
}

#[derive(Deserialize)]
struct InteractivityForm {
    payload: String,
}

enum Parsed {
    Queue(Request),
    RateLimited { minute_rate_limited: Option<i64> },
    Ignore(&'static str),
}

enum Request {
    Event(EventCallback),
    Command {
        form: CommandForm,
        signature: String,
    },
    Interaction {
        payload: Map<String, Value>,
        signature: String,
    },
}

struct Queued {
    binding: BindingRef,
    bot_user: Option<UserId>,
    received_at: OffsetDateTime,
    request: Request,
}

/// Parses a verified body. The error names what was wrong, never what was
/// sent.
fn parse(kind: Kind, body: &[u8], signature: String) -> Result<Parsed, &'static str> {
    match kind {
        Kind::Events => {
            let envelope: Envelope =
                serde_json::from_slice(body).map_err(|_| "not an Events API envelope")?;
            Ok(match envelope {
                Envelope::EventCallback(callback) => Parsed::Queue(Request::Event(callback)),
                Envelope::AppRateLimited {
                    minute_rate_limited,
                } => Parsed::RateLimited {
                    minute_rate_limited,
                },
                Envelope::Other => Parsed::Ignore("other"),
            })
        }
        Kind::Commands => {
            let form: CommandForm =
                serde_urlencoded::from_bytes(body).map_err(|_| "not a slash command form")?;
            let required = [
                &form.team_id,
                &form.channel_id,
                &form.user_id,
                &form.command,
                &form.response_url,
            ];
            if required.iter().any(|field| field.is_empty()) {
                return Err("a slash command field is empty");
            }
            Ok(Parsed::Queue(Request::Command { form, signature }))
        }
        Kind::Interactivity => {
            let form: InteractivityForm =
                serde_urlencoded::from_bytes(body).map_err(|_| "not an interactivity form")?;
            let payload: Map<String, Value> = serde_json::from_str(&form.payload)
                .map_err(|_| "the interactivity payload is not a JSON object")?;
            Ok(Parsed::Queue(Request::Interaction { payload, signature }))
        }
    }
}

/// The receiving end of the ingress: requests that were acknowledged and
/// still have to be deduplicated, normalized and handed on.
pub struct Queue {
    receiver: mpsc::Receiver<Queued>,
}

impl fmt::Debug for Queue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Queue")
            .field("queued", &self.receiver.len())
            .finish_non_exhaustive()
    }
}

impl Queue {
    /// Handles queued requests one at a time, in the order they arrived,
    /// sending each that survives deduplication and normalization to `out`.
    ///
    /// Returns once the queue is closed and empty, or as soon as `out` is
    /// closed, dropping what is left.
    pub async fn run(mut self, dedup: Arc<dyn Dedup>, out: Sender<SlackInbound>) {
        while let Some(queued) = self.receiver.recv().await {
            let Some(inbound) = process(queued, dedup.as_ref()).await else {
                continue;
            };
            let (binding, kind) = (inbound.binding(), inbound.kind());
            if out.send(inbound).await.is_err() {
                tracing::warn!(%binding, kind, "the receiver of Slack requests is gone; the Slack queue stops");
                return;
            }
        }
    }
}

async fn process(queued: Queued, dedup: &dyn Dedup) -> Option<SlackInbound> {
    let Queued {
        binding,
        bot_user,
        received_at,
        request,
    } = queued;
    match request {
        Request::Event(callback) => {
            process_event(binding, bot_user.as_ref(), received_at, callback, dedup).await
        }
        Request::Command { form, signature } => {
            if !first_time(
                dedup,
                &format!("slack:{binding}:request"),
                &signature,
                binding,
            )
            .await
            {
                return None;
            }
            let team = TeamId::from(form.team_id);
            Some(SlackInbound::Command(SlashCommand {
                binding: binding.id(),
                sender: MemberKey {
                    surface: SurfaceKind::Slack,
                    team: team.clone(),
                    user: form.user_id.into(),
                },
                conv: ConvRef {
                    surface: SurfaceKind::Slack,
                    team,
                    conversation: form.channel_id.into(),
                },
                command: form.command,
                text: form.text,
                response_url: SecretString::from(form.response_url),
                trigger_id: form.trigger_id,
                received_at,
            }))
        }
        Request::Interaction {
            mut payload,
            signature,
        } => {
            if !first_time(
                dedup,
                &format!("slack:{binding}:request"),
                &signature,
                binding,
            )
            .await
            {
                return None;
            }
            payload.remove("token");
            let response_url = match payload.remove("response_url") {
                Some(Value::String(url)) if !url.is_empty() => Some(SecretString::from(url)),
                _ => None,
            };
            let id_of = |key: &str| {
                payload
                    .get(key)
                    .and_then(|object| object.get("id"))
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
            };
            let sender = id_of("team")
                .zip(id_of("user"))
                .map(|(team, user)| MemberKey {
                    surface: SurfaceKind::Slack,
                    team: team.into(),
                    user: user.into(),
                });
            let kind = payload
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            Some(SlackInbound::Interaction(Interaction {
                binding: binding.id(),
                kind,
                sender,
                response_url,
                payload,
                received_at,
            }))
        }
    }
}

async fn process_event(
    binding: BindingRef,
    bot_user: Option<&UserId>,
    received_at: OffsetDateTime,
    callback: EventCallback,
    dedup: &dyn Dedup,
) -> Option<SlackInbound> {
    let EventCallback {
        team_id,
        event_id,
        event,
    } = callback;
    if !first_time(dedup, &format!("slack:{binding}"), &event_id, binding).await {
        tracing::debug!(%binding, event_id, "dropped a Slack event already handled");
        return None;
    }
    let team = team_id.filter(|team| !team.is_empty()).map(TeamId::from);
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let event = Value::Object(event);
    if event_type != "message" {
        return Some(SlackInbound::Event(SlackEvent {
            binding: binding.id(),
            team,
            event_id,
            event_type,
            event,
            received_at,
        }));
    }
    let Some(team) = team else {
        tracing::debug!(%binding, event_id, "dropped a Slack message without a team_id");
        return None;
    };
    let context = normalize::Context {
        binding: binding.id(),
        bot_user,
        team: &team,
        event_id: &event_id,
        received_at,
    };
    let message = match normalize::message(&context, &event) {
        Ok(message) => message,
        Err(skip) => {
            tracing::debug!(%binding, event_id, reason = %skip, "dropped a Slack message");
            return None;
        }
    };
    let key = format!("{}:{}", message.conv.conversation, message.message.id);
    if !first_time(dedup, &format!("slack:{binding}:message"), &key, binding).await {
        tracing::debug!(%binding, event_id, "dropped a Slack message this app already received");
        return None;
    }
    Some(SlackInbound::Message(Box::new(message)))
}

async fn first_time(dedup: &dyn Dedup, source: &str, key: &str, binding: BindingRef) -> bool {
    match dedup.first_time(source, key).await {
        Ok(first) => first,
        Err(err) => {
            tracing::warn!(%binding, source, error = %err, "deduplicating a Slack request failed; dropping it");
            false
        }
    }
}

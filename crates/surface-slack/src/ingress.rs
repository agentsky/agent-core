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
//! 2. 413 if the body is larger than [`MAX_BODY_BYTES`]. The lookup and the
//!    body together get [`PRE_ACK_TIMEOUT`]: 503 if the lookup is still
//!    running then, 408 if the body hasn't all arrived.
//! 3. Two probes are answered without checking the signature. Each reads
//!    nothing and changes nothing, and each is answered only for bindings
//!    agentd already knows, so it tells a forger nothing a 401 wouldn't (see
//!    the design's Slack transport notes):
//!    - On `/events`, a `url_verification` body gets its challenge back
//!      while the binding has no signing secret yet: Slack sends it while
//!      `apps.manifest.create` runs, before agentd has the new app's signing
//!      secret. A binding that has one answers a challenge only once it
//!      verified, after step 4.
//!    - On `/commands`, a form whose `ssl_check` is `1` gets an empty 200.
//!      Slack sends it, unsigned, to check the certificate of a slash
//!      command's URL.
//! 4. 401 unless the request carries a valid `v0` signature (see
//!    [`verify`](mod@crate::verify)) made with the binding's secret. A binding
//!    whose secret isn't known yet can't be verified, so everything but the
//!    probes gets 401.
//! 5. 400 if a verified body can't be parsed, or names an id that isn't
//!    shaped like Slack's (see [Shapes](#shapes)).
//! 6. 503 if the binding has too many requests in flight: acknowledged, and
//!    not yet handed on (see [`InFlight`]). Each agent's app may have
//!    [`MAX_IN_FLIGHT_PER_AGENT`] at once, the apps of one owner's agents
//!    together [`MAX_IN_FLIGHT_PER_OWNER`], and agents' apps together the
//!    `capacity` given to [`ingress`]; the manager app has a `capacity` of
//!    its own, so no agent's traffic can take its places, and one agent's
//!    can't take another's. 503 too if an agent's app sends faster than
//!    Slack delivers to one app: [`AGENT_BURST`] at once, then
//!    [`AGENT_REQUESTS_PER_SECOND`]. Slack retries events, and the forger
//!    of a flood is its app's owner, whose own app is the one refused.
//! 7. An empty 200 as soon as the request is on the queue. Slack retries
//!    an event that got 503, but not a slash command or an interaction:
//!    its user sees Slack's error and can try again. The handler never
//!    waits for the queue. Slash commands and interactivity reply later
//!    through their `response_url`.
//!
//! Anyone can send requests that are refused before verification (steps 2
//! to 4) or challenges, so at most one refusal per binding per
//! [`WARNING_INTERVAL`] is logged as a warning and one challenge as info;
//! the rest are logged at debug level. A request refused at step 5 or 6,
//! and Slack's `app_rate_limited` notice, are logged the same way, since
//! an agent's owner can sign as many as they like; each binding's are
//! counted apart, so one app's flood hides no other's.
//!
//! The queue holds each request's body as it arrived, at most
//! [`MAX_BODY_BYTES`], so what waits is bounded in bytes, not only in
//! count. [`Queue::run`] then parses each body again, deduplicates the
//! request through [`Dedup`], normalizes it, and hands it on as a
//! [`SlackInbound`]:
//!
//! - `message` events are normalized first, which needs no I/O, so the
//!   unaddressed channel messages every agent app receives cost no store
//!   write. A kept message is deduplicated by `<channel>:<ts>`, under
//!   `slack:<binding>:message`, which drops Slack's retries and a message
//!   that reached the same app twice with different event ids.
//! - Other events by `event_id`, under the source `slack:<binding>`, which
//!   drops Slack's retries.
//! - Slash commands and interactivity by their signature, under
//!   `slack:<binding>:request`. Slack doesn't retry them, so a second copy is
//!   a replay inside the five-minute window.
//!
//! # Shapes
//!
//! An agent's owner can sign any body, and each event the queue keeps is a
//! deduplication row for seven days, so the ids that make up its key must
//! be shaped like Slack's. A body whose id isn't gets 400 and writes
//! nothing:
//!
//! - `event_id`: `Ev` and 1 to 32 uppercase letters or digits.
//! - `team_id`, when there is one: `T` (or `E`, an Enterprise Grid
//!   organization) and 1 to 20 uppercase letters or digits.
//! - A `message` event's `channel`: `C`, `D` or `G` and 1 to 20 uppercase
//!   letters or digits; its `ts` and `thread_ts`: 10 digits, a dot and 6
//!   digits.
//!
//! Slash commands and interactions are keyed by their signature, whose
//! shape verification fixes.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::header::{CONTENT_TYPE, X_CONTENT_TYPE_OPTIONS};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use core_types::{
    BindingId, ConvRef, MemberId, MemberKey, Sender, SurfaceKind, TeamId, Throttle, UserId,
};
use http_body_util::{BodyExt as _, LengthLimitError, Limited};
use secrecy::SecretString;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use tokio::sync::mpsc;
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
/// How long the secret lookup and reading the body may take together.
/// Slack wants its ack within three seconds, and a connection that holds
/// either up would otherwise also hold up shutdown for the whole drain
/// timeout.
pub const PRE_ACK_TIMEOUT: Duration = Duration::from_secs(2);
/// How often a refusal before verification is logged as a warning, and an
/// answered challenge as info, at most. The others are logged at debug
/// level, and the next warning or info says how many there were.
pub const WARNING_INTERVAL: Duration = Duration::from_secs(60);
/// How many requests to one agent's app may be in flight at once. Beyond
/// that, the app's requests get 503 until some are handed on.
pub const MAX_IN_FLIGHT_PER_AGENT: usize = 32;
/// How many requests to the apps of one owner's agents may be in flight at
/// once, however many agents they have.
pub const MAX_IN_FLIGHT_PER_OWNER: usize = 2 * MAX_IN_FLIGHT_PER_AGENT;
/// How many requests an agent's app may send at once before
/// [`AGENT_REQUESTS_PER_SECOND`] applies.
pub const AGENT_BURST: u32 = 100;
/// How many requests a second an agent's app may send once its
/// [`AGENT_BURST`] is used up: about Slack's own ceiling of 30,000 events
/// an hour for one app. Past it, the app's requests get 503.
pub const AGENT_REQUESTS_PER_SECOND: u32 = 8;
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
    /// The owner of the agent whose app this is, whose agents' apps
    /// together have [`MAX_IN_FLIGHT_PER_OWNER`] places. `None` for the
    /// manager app.
    pub owner: Option<MemberId>,
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
    /// If the lookup itself fails. The request then gets 503, as it does
    /// when the lookup takes longer than [`PRE_ACK_TIMEOUT`]; Slack retries
    /// events, but not slash commands or interactions.
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
/// [`Queue`] behind it. At most `capacity` requests to the manager app, and
/// `capacity` to agents' apps, [`MAX_IN_FLIGHT_PER_AGENT`] of them to each,
/// are in flight at once.
///
/// The queue closes when the router and every clone of it are dropped, and
/// [`Queue::run`] returns once it has handled what was queued.
pub fn ingress(secrets: Arc<dyn SigningSecrets>, capacity: usize) -> (Router, Queue) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let state = Ingress {
        secrets,
        queue: sender,
        places: Arc::new(Places {
            capacity,
            taken: Mutex::default(),
        }),
        refusals: Arc::new(Throttle::new(WARNING_INTERVAL)),
        rate_limits: Arc::new(Throttle::new(WARNING_INTERVAL)),
        challenges: Arc::new(Throttle::new(WARNING_INTERVAL)),
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
    queue: mpsc::UnboundedSender<Queued>,
    places: Arc<Places>,
    refusals: Arc<Throttle<BindingRef>>,
    rate_limits: Arc<Throttle<BindingRef>>,
    challenges: Arc<Throttle>,
}

impl Ingress {
    /// Logs a request refused before verification, for a body that doesn't
    /// parse, or for having too many in flight or coming too fast.
    fn refused(
        &self,
        binding: BindingRef,
        kind: Kind,
        status: StatusCode,
        reason: &dyn fmt::Display,
    ) {
        let (kind, status) = (kind.as_str(), status.as_u16());
        match self.refusals.record(binding, Instant::now()) {
            Some(quiet) => tracing::warn!(
                %binding,
                kind,
                status,
                %reason,
                refused_since_last_warning = quiet,
                "refused a Slack request"
            ),
            None => tracing::debug!(%binding, kind, status, %reason, "refused a Slack request"),
        }
    }

    /// Logs Slack's notice that it is rate limiting `binding`'s events.
    fn rate_limited(&self, binding: BindingRef, minute_rate_limited: Option<i64>) {
        match self.rate_limits.record(binding, Instant::now()) {
            Some(quiet) => tracing::warn!(
                %binding,
                minute_rate_limited,
                notices_since_last_warning = quiet,
                "Slack is rate limiting this app's events"
            ),
            None => {
                tracing::debug!(%binding, minute_rate_limited, "Slack is rate limiting this app's events")
            }
        }
    }

    /// Logs an answered `url_verification` challenge.
    fn challenged(&self, binding: BindingRef) {
        match self.challenges.record((), Instant::now()) {
            Some(quiet) => tracing::info!(
                %binding,
                answered_since_last_info = quiet,
                "answered Slack's url_verification challenge"
            ),
            None => tracing::debug!(%binding, "answered Slack's url_verification challenge"),
        }
    }
}

/// A request's place among those in flight for its binding: taken when the
/// ingress acknowledges the request, and given back when this is dropped.
///
/// [`Queue::run`] drops it once it has handed the request on, except for a
/// message, whose place goes on in [`SlackInbound::Message`], so a message
/// to an agent's app holds it until whoever takes the message drops it.
pub struct InFlight(Option<(Seat, Arc<Places>)>);

impl InFlight {
    /// A place counted nowhere, for a message that didn't come through the
    /// ingress, as in tests.
    #[doc(hidden)]
    pub fn untracked() -> Self {
        Self(None)
    }
}

impl fmt::Debug for InFlight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some((seat, _)) => f.debug_tuple("InFlight").field(&seat.binding).finish(),
            None => f.write_str("InFlight(untracked)"),
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some((seat, places)) = self.0.take() {
            places.give_back(seat);
        }
    }
}

/// Whose place an [`InFlight`] holds.
#[derive(Debug, Clone, Copy)]
struct Seat {
    binding: BindingRef,
    owner: Option<MemberId>,
}

/// Why a request got no place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum Busy {
    #[error("too many of the binding's requests in flight")]
    InFlight,
    #[error("too many of the owner's agents' requests in flight")]
    Owner,
    #[error("the agent's app is sending faster than its rate")]
    Rate,
}

/// The places in flight: `capacity` for the manager app, and `capacity`
/// for agents' apps, [`MAX_IN_FLIGHT_PER_AGENT`] for each and
/// [`MAX_IN_FLIGHT_PER_OWNER`] for each owner's; and each agent app's
/// [`Bucket`].
#[derive(Debug)]
struct Places {
    capacity: usize,
    taken: Mutex<Taken>,
}

#[derive(Debug, Default)]
struct Taken {
    manager: usize,
    agents: usize,
    by_agent: HashMap<BindingId, usize>,
    by_owner: HashMap<MemberId, usize>,
    buckets: HashMap<BindingId, Bucket>,
}

/// A token bucket: [`AGENT_BURST`] tokens, refilled at
/// [`AGENT_REQUESTS_PER_SECOND`], one taken by each request let through.
/// It lives only in memory: after a restart every app starts with a full
/// one, which costs at most one more burst.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn full(now: Instant) -> Self {
        Self {
            tokens: f64::from(AGENT_BURST),
            at: now,
        }
    }

    /// Takes a token at `now`, if there is one.
    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * f64::from(AGENT_REQUESTS_PER_SECOND))
            .min(f64::from(AGENT_BURST));
        self.at = self.at.max(now);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

impl Places {
    /// A place at `now` for a request to `binding`, whose agent's owner is
    /// `owner`, or why there is none.
    fn take(
        self: &Arc<Self>,
        binding: BindingRef,
        owner: Option<MemberId>,
        now: Instant,
    ) -> Result<InFlight, Busy> {
        let mut taken = self.taken.lock().unwrap_or_else(PoisonError::into_inner);
        let taken = &mut *taken;
        match binding {
            BindingRef::Manager if taken.manager >= self.capacity => return Err(Busy::InFlight),
            BindingRef::Manager => taken.manager += 1,
            BindingRef::Agent(id) => {
                let held = taken.by_agent.get(&id).copied().unwrap_or_default();
                if taken.agents >= self.capacity || held >= MAX_IN_FLIGHT_PER_AGENT {
                    return Err(Busy::InFlight);
                }
                if let Some(owner) = owner
                    && taken.by_owner.get(&owner).copied().unwrap_or_default()
                        >= MAX_IN_FLIGHT_PER_OWNER
                {
                    return Err(Busy::Owner);
                }
                let bucket = taken.buckets.entry(id).or_insert_with(|| Bucket::full(now));
                if !bucket.take(now) {
                    return Err(Busy::Rate);
                }
                taken.agents += 1;
                *taken.by_agent.entry(id).or_default() += 1;
                if let Some(owner) = owner {
                    *taken.by_owner.entry(owner).or_default() += 1;
                }
            }
        }
        Ok(InFlight(Some((Seat { binding, owner }, Arc::clone(self)))))
    }

    fn give_back(&self, seat: Seat) {
        let mut taken = self.taken.lock().unwrap_or_else(PoisonError::into_inner);
        let BindingRef::Agent(id) = seat.binding else {
            taken.manager -= 1;
            return;
        };
        taken.agents -= 1;
        release(&mut taken.by_agent, id);
        if let Some(owner) = seat.owner {
            release(&mut taken.by_owner, owner);
        }
    }
}

/// Gives back one of `key`'s places, forgetting it once it holds none.
fn release<K: Eq + std::hash::Hash>(held: &mut HashMap<K, usize>, key: K) {
    if let Some(count) = held.get_mut(&key) {
        *count -= 1;
        if *count == 0 {
            held.remove(&key);
        }
    }
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
    let deadline = tokio::time::Instant::now() + PRE_ACK_TIMEOUT;
    let app = match tokio::time::timeout_at(deadline, ingress.secrets.lookup(binding)).await {
        Ok(Ok(Some(app))) => app,
        Ok(Ok(None)) => return StatusCode::NOT_FOUND.into_response(),
        Ok(Err(err)) => {
            tracing::warn!(%binding, error = %err, "looking up a Slack binding failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Err(_) => {
            tracing::warn!(
                %binding,
                timeout_ms = PRE_ACK_TIMEOUT.as_millis(),
                "looking up a Slack binding took too long"
            );
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let body = match tokio::time::timeout_at(deadline, read_body(body)).await {
        Ok(Ok(body)) => body,
        Ok(Err(refusal)) => {
            ingress.refused(binding, kind, refusal.status(), &refusal);
            return refusal.status().into_response();
        }
        Err(_) => {
            let refusal = BodyRefusal::Slow;
            ingress.refused(binding, kind, refusal.status(), &refusal);
            return refusal.status().into_response();
        }
    };
    let received_at = OffsetDateTime::now_utc();

    let challenge = match kind {
        Kind::Events => url_verification(&body),
        _ => None,
    };
    if let Some(challenge) = &challenge
        && app.signing_secret.is_none()
    {
        return answer_challenge(ingress, binding, challenge.as_deref());
    }
    if kind == Kind::Commands && ssl_check(&body) {
        tracing::debug!(%binding, "answered Slack's ssl_check");
        return StatusCode::OK.into_response();
    }

    let Some(secret) = app.signing_secret.as_ref() else {
        ingress.refused(
            binding,
            kind,
            StatusCode::UNAUTHORIZED,
            &"the binding has no signing secret yet",
        );
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if let Err(rejection) = verify::verify(secret, headers, &body, received_at.unix_timestamp()) {
        ingress.refused(binding, kind, StatusCode::UNAUTHORIZED, &rejection);
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if let Some(challenge) = challenge {
        return answer_challenge(ingress, binding, challenge.as_deref());
    }
    log_retry(binding, kind, headers);

    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match check(kind, &body) {
        Ok(Checked::Queue) => {}
        Ok(Checked::RateLimited {
            minute_rate_limited,
        }) => {
            ingress.rate_limited(binding, minute_rate_limited);
            return StatusCode::OK.into_response();
        }
        Ok(Checked::Ignore(envelope_type)) => {
            tracing::debug!(%binding, envelope_type, "ignored a Slack envelope");
            return StatusCode::OK.into_response();
        }
        Err(reason) => {
            ingress.refused(binding, kind, StatusCode::BAD_REQUEST, &reason);
            return StatusCode::BAD_REQUEST.into_response();
        }
    }
    let place = match ingress.places.take(binding, app.owner, Instant::now()) {
        Ok(place) => place,
        Err(busy) => {
            ingress.refused(binding, kind, StatusCode::SERVICE_UNAVAILABLE, &busy);
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let queued = Queued {
        binding,
        bot_user: app.bot_user,
        received_at,
        kind,
        body,
        signature,
        place,
    };
    if ingress.queue.send(queued).is_err() {
        tracing::warn!(%binding, kind = kind.as_str(), "the Slack queue is closed");
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    StatusCode::OK.into_response()
}

/// Echoes a `url_verification` challenge, or refuses a malformed one.
fn answer_challenge(ingress: &Ingress, binding: BindingRef, challenge: Option<&str>) -> Response {
    match challenge {
        Some(challenge) => {
            ingress.challenged(binding);
            (
                [
                    (CONTENT_TYPE, "text/plain; charset=utf-8"),
                    (X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                challenge.to_owned(),
            )
                .into_response()
        }
        None => StatusCode::BAD_REQUEST.into_response(),
    }
}

/// Why a request body was refused before verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum BodyRefusal {
    #[error("the body is larger than {MAX_BODY_BYTES} bytes")]
    TooLarge,
    #[error("the body could not be read")]
    Unreadable,
    #[error("the body did not arrive within the pre-ack timeout")]
    Slow,
}

impl BodyRefusal {
    fn status(self) -> StatusCode {
        match self {
            Self::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::Unreadable => StatusCode::BAD_REQUEST,
            Self::Slow => StatusCode::REQUEST_TIMEOUT,
        }
    }
}

/// Reads the whole body, refusing more than [`MAX_BODY_BYTES`].
async fn read_body(body: Body) -> Result<Bytes, BodyRefusal> {
    match Limited::new(body, MAX_BODY_BYTES).collect().await {
        Ok(collected) => Ok(collected.to_bytes()),
        Err(err) if err.downcast_ref::<LengthLimitError>().is_some() => Err(BodyRefusal::TooLarge),
        Err(_) => Err(BodyRefusal::Unreadable),
    }
}

#[derive(Deserialize)]
struct SslCheck {
    ssl_check: Option<String>,
}

/// Whether the body is a form whose `ssl_check` is `1`: Slack's check of a
/// slash command URL's certificate.
fn ssl_check(body: &[u8]) -> bool {
    serde_urlencoded::from_bytes::<SslCheck>(body)
        .is_ok_and(|form| form.ssl_check.as_deref() == Some("1"))
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

/// What of an Events API envelope is checked before it is queued. Other
/// fields are skipped, not kept.
#[derive(Deserialize)]
struct EnvelopeHead {
    #[serde(rename = "type")]
    kind: String,
    team_id: Option<String>,
    event_id: Option<String>,
    event: Option<EventHead>,
    minute_rate_limited: Option<i64>,
}

#[derive(Deserialize)]
struct EventHead {
    #[serde(rename = "type")]
    kind: Option<String>,
}

/// The ids of a `message` event that make up its deduplication key.
#[derive(Deserialize)]
struct MessageHead {
    event: MessageIds,
}

#[derive(Deserialize)]
struct MessageIds {
    channel: Option<String>,
    ts: Option<String>,
    thread_ts: Option<String>,
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

enum Checked {
    Queue,
    RateLimited { minute_rate_limited: Option<i64> },
    Ignore(&'static str),
}

struct Queued {
    binding: BindingRef,
    bot_user: Option<UserId>,
    received_at: OffsetDateTime,
    kind: Kind,
    body: Bytes,
    signature: String,
    place: InFlight,
}

/// Checks that a verified body parses, and that the ids an event is
/// deduplicated by are shaped like Slack's (see [Shapes](self#shapes)),
/// without keeping what it parsed. The error names what was wrong, never
/// what was sent.
fn check(kind: Kind, body: &[u8]) -> Result<Checked, &'static str> {
    match kind {
        Kind::Events => {
            let head: EnvelopeHead =
                serde_json::from_slice(body).map_err(|_| "not an Events API envelope")?;
            match head.kind.as_str() {
                "event_callback" => {
                    check_callback(&head, body)?;
                    Ok(Checked::Queue)
                }
                "app_rate_limited" => Ok(Checked::RateLimited {
                    minute_rate_limited: head.minute_rate_limited,
                }),
                _ => Ok(Checked::Ignore("other")),
            }
        }
        Kind::Commands => {
            command_form(body)?;
            Ok(Checked::Queue)
        }
        Kind::Interactivity => {
            let form: InteractivityForm =
                serde_urlencoded::from_bytes(body).map_err(|_| "not an interactivity form")?;
            serde_json::from_str::<std::collections::BTreeMap<String, IgnoredAny>>(&form.payload)
                .map_err(|_| "the interactivity payload is not a JSON object")?;
            Ok(Checked::Queue)
        }
    }
}

fn check_callback(head: &EnvelopeHead, body: &[u8]) -> Result<(), &'static str> {
    let Some(event) = &head.event else {
        return Err("an event_callback without an event");
    };
    if !head.event_id.as_deref().is_some_and(is_event_id) {
        return Err("the event_id isn't shaped like Slack's");
    }
    if !head.team_id.as_deref().is_none_or(is_team_id) {
        return Err("the team_id isn't shaped like Slack's");
    }
    if event.kind.as_deref() != Some("message") {
        return Ok(());
    }
    let MessageHead { event: ids } =
        serde_json::from_slice(body).map_err(|_| "a message event whose ids aren't strings")?;
    if !ids.channel.as_deref().is_none_or(is_channel_id) {
        return Err("the message's channel isn't shaped like Slack's");
    }
    if ![ids.ts, ids.thread_ts].iter().flatten().all(|ts| is_ts(ts)) {
        return Err("the message's ts or thread_ts isn't shaped like Slack's");
    }
    Ok(())
}

fn command_form(body: &[u8]) -> Result<CommandForm, &'static str> {
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
    Ok(form)
}

/// Whether `id` is one of `prefixes` and then 1 to `max` uppercase ASCII
/// letters or digits.
fn is_slack_id(id: &str, prefixes: &[&str], max: usize) -> bool {
    prefixes.iter().any(|prefix| {
        id.strip_prefix(prefix).is_some_and(|rest| {
            (1..=max).contains(&rest.len())
                && rest
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        })
    })
}

fn is_event_id(id: &str) -> bool {
    is_slack_id(id, &["Ev"], 32)
}

fn is_team_id(id: &str) -> bool {
    is_slack_id(id, &["T", "E"], 20)
}

fn is_channel_id(id: &str) -> bool {
    is_slack_id(id, &["C", "D", "G"], 20)
}

/// Whether `ts` is a message timestamp as Slack writes one: 10 digits of
/// seconds, a dot and 6 of microseconds.
fn is_ts(ts: &str) -> bool {
    ts.split_once('.').is_some_and(|(seconds, micros)| {
        seconds.len() == 10
            && micros.len() == 6
            && seconds
                .bytes()
                .chain(micros.bytes())
                .all(|b| b.is_ascii_digit())
    })
}

/// The receiving end of the ingress: requests that were acknowledged and
/// still have to be deduplicated, normalized and handed on.
pub struct Queue {
    receiver: mpsc::UnboundedReceiver<Queued>,
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
        kind,
        body,
        signature,
        place,
    } = queued;
    let reparsed = |what: &'static str| {
        tracing::warn!(%binding, kind = kind.as_str(), what, "a queued Slack request no longer parses; dropped it");
    };
    match kind {
        Kind::Events => {
            let callback: EventCallback = serde_json::from_slice(&body)
                .inspect_err(|_| reparsed("the event"))
                .ok()?;
            drop(body);
            process_event(
                binding,
                bot_user.as_ref(),
                received_at,
                callback,
                dedup,
                place,
            )
            .await
        }
        Kind::Commands => {
            let form = command_form(&body)
                .inspect_err(|_| reparsed("the slash command"))
                .ok()?;
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
        Kind::Interactivity => {
            let mut payload = serde_urlencoded::from_bytes::<InteractivityForm>(&body)
                .ok()
                .and_then(|form| serde_json::from_str::<Map<String, Value>>(&form.payload).ok())
                .or_else(|| {
                    reparsed("the interaction");
                    None
                })?;
            drop(body);
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
    place: InFlight,
) -> Option<SlackInbound> {
    let EventCallback {
        team_id,
        event_id,
        event,
    } = callback;
    let team = team_id.filter(|team| !team.is_empty()).map(TeamId::from);
    let event_type = event
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let event = Value::Object(event);
    if event_type != "message" {
        if !first_time(dedup, &format!("slack:{binding}"), &event_id, binding).await {
            tracing::debug!(%binding, event_id, "dropped a Slack event already handled");
            return None;
        }
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
    Some(SlackInbound::Message(Box::new(message), place))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slacks_own_ids_have_slacks_shapes() {
        for id in ["Ev0PV52K21", "Ev08MFMKH6", "Ev1"] {
            assert!(is_event_id(id), "{id}");
        }
        for id in ["", "Ev", "ev0PV52K21", "Ev0pv52k21", "Ev0PV-2K21", "EvÄ"] {
            assert!(!is_event_id(id), "{id}");
        }
        assert!(is_event_id(&format!("Ev{}", "A".repeat(32))));
        assert!(!is_event_id(&format!("Ev{}", "A".repeat(33))));
        for id in ["T024BE7LD", "E0ORG0001"] {
            assert!(is_team_id(id), "{id}");
        }
        for id in ["C024BE91L", "D024BE91L", "G024BE91L"] {
            assert!(is_channel_id(id), "{id}");
        }
        for id in ["U024BE7LH", "C", "c024be91l", "C024 BE91"] {
            assert!(!is_channel_id(id), "{id}");
        }
        assert!(is_ts("1727697600.000100"));
        for ts in [
            "",
            "1.2",
            "1727697600",
            "1727697600.",
            "1727697600.0001",
            "17276976000.000100",
            "1727697600.0001000",
            "1727697600,000100",
            "+727697600.000100",
        ] {
            assert!(!is_ts(ts), "{ts}");
        }
    }

    #[test]
    fn an_agents_bucket_lets_a_burst_through_then_its_rate() {
        let start = Instant::now();
        let mut bucket = Bucket::full(start);
        for n in 0..AGENT_BURST {
            assert!(bucket.take(start), "request {n} of the burst");
        }
        assert!(!bucket.take(start));
        let tick = Duration::from_secs(1) / AGENT_REQUESTS_PER_SECOND;
        assert!(bucket.take(start + tick));
        assert!(!bucket.take(start + tick));
        let later = start + Duration::from_secs(3600);
        for _ in 0..AGENT_BURST {
            assert!(bucket.take(later));
        }
        assert!(!bucket.take(later), "an idle hour refills only the burst");
        assert!(
            !bucket.take(start),
            "a time before the last take refills nothing"
        );
    }

    #[test]
    fn a_busy_binding_holds_no_other_bindings_places() {
        let places = Arc::new(Places {
            capacity: 4,
            taken: Mutex::default(),
        });
        let now = Instant::now();
        let (a, b) = (BindingId::new_v4(), BindingId::new_v4());
        let owner = MemberId::new_v4();
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(places.take(BindingRef::Agent(a), Some(owner), now).unwrap());
        }
        assert_eq!(
            places.take(BindingRef::Agent(b), None, now).unwrap_err(),
            Busy::InFlight
        );
        assert!(places.take(BindingRef::Manager, None, now).is_ok());
        held.pop();
        assert!(places.take(BindingRef::Agent(b), None, now).is_ok());
        drop(held);
        let taken = places.taken.lock().unwrap();
        assert_eq!((taken.manager, taken.agents), (0, 0));
        assert!(taken.by_agent.is_empty() && taken.by_owner.is_empty());
    }

    #[test]
    fn ssl_check_needs_exactly_one_ssl_check_of_1() {
        assert!(ssl_check(b"ssl_check=1&token=x"));
        assert!(ssl_check(b"token=x&ssl_check=1"));
        for body in [
            &b""[..],
            b"ssl_check=0",
            b"ssl_check=",
            b"ssl_check=1&ssl_check=1",
            b"{\"ssl_check\":\"1\"}",
            b"team_id=T1&command=%2Fagent",
        ] {
            assert!(!ssl_check(body), "{}", String::from_utf8_lossy(body));
        }
    }
}

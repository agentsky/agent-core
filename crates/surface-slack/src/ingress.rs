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
//! It selects the app, and its signing secret, through [`SigningSecrets`].
//! A request is answered, in this order:
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
//! 6. An empty 200, and nothing more, for what an agent's app doesn't need
//!    (see [Agents' apps](#agents-apps)).
//! 7. 503 if the binding has too many requests in flight: acknowledged, and
//!    not yet handed on (see [`InFlight`]). Each agent's app may have
//!    [`MAX_IN_FLIGHT_PER_AGENT`] at once, the apps of one owner's agents
//!    together [`MAX_IN_FLIGHT_PER_OWNER`], and agents' apps together the
//!    `capacity` given to [`ingress`]; the manager app has a `capacity` of
//!    its own, so no agent's traffic can take its places, and one agent's
//!    can't take another's. 503 too if an agent's app sends faster than
//!    Slack delivers to one app, [`AGENT_BURST`] at once, then
//!    [`AGENT_REQUESTS_PER_SECOND`]. Slack retries events, and the forger
//!    of a flood is its app's owner, whose own apps are the ones refused.
//! 8. An empty 200 as soon as the request is on the queue. Slack retries
//!    an event that got 503, but not a slash command or an interaction:
//!    its user sees Slack's error and can try again. The handler never
//!    waits for the queue. Slash commands and interactivity reply later
//!    through their `response_url`.
//!
//! Anyone can send requests that are refused before verification (steps 2
//! to 4) or challenges, and an agent's owner can sign as many requests as
//! they like, so the ingress logs each of these at most once per binding
//! per [`WARNING_INTERVAL`] as a warning (as info for an answered
//! challenge and a delivery Slack says it retried), and the rest at debug
//! level, the next warning saying how many there were: a refusal at any
//! step, Slack's `app_rate_limited` notice, a retried delivery, an
//! answered challenge, an agent's message older than the confirmation
//! window, a message dropped for its owner's rate, and a queued body that
//! no longer parses. Each binding's are counted apart, so one app's flood
//! hides no other's. A request refused with 400 takes none of the bucket's
//! tokens, since it writes nothing and its log is throttled like the rest.
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
//!   that reached the same app twice with different event ids. Before that
//!   write, a message to an agent's app takes a token from its owner's
//!   bucket: the apps of one owner's agents together keep [`OWNER_BURST`]
//!   messages at once, then [`OWNER_REQUESTS_PER_SECOND`], and a message
//!   past that is dropped, after its 200, with no row. Only messages that
//!   are kept count, so busy channels that one owner's agents share cost
//!   that owner nothing, however many agents are in them. What it is
//!   handed on with is bounded too, at most about 220 KB (see
//!   [`normalize`]'s Bounds), so what an agent's messages hold after the
//!   queue is bounded in bytes as well. An agent's app's other requests
//!   never reach the queue (see [Agents' apps](#agents-apps)); the manager
//!   app's, which only its operators can sign, are handed on as they came.
//! - Other events by `event_id`, under the source `slack:<binding>`, which
//!   drops Slack's retries.
//! - Slash commands and interactivity by their signature, under
//!   `slack:<binding>:request`. Slack doesn't retry them, so a second copy is
//!   a replay inside the five-minute window.
//!
//! Each key is kept for [`DEDUP_RETENTION`].
//!
//! # Shapes
//!
//! An agent's owner can sign any body, and each event the queue keeps is a
//! deduplication row, so the ids that make up its key must be shaped like
//! Slack's. A body whose id isn't gets 400 and writes nothing. The checks
//! are [`normalize`]'s, which checks the same ids again when it keeps a
//! message, and leave room for Slack's ids to grow, as Slack says they
//! may, up to [`MAX_ID_TAIL`](normalize::MAX_ID_TAIL) characters after
//! their prefix:
//!
//! - `event_id`: `Ev` and 1 to 64 uppercase letters or digits.
//! - `team_id`, when there is one: `T` (or `E`, an Enterprise Grid
//!   organization) and 1 to 64 uppercase letters or digits.
//! - A `message` event's `channel`: `C`, `D` or `G` and 1 to 64 uppercase
//!   letters or digits; its `ts` and `thread_ts`: 10 to 20 digits, the
//!   first not a zero, a dot and 6 digits.
//!
//! A message's sender is no key, so it is not checked here: [`normalize`]
//! drops a message whose `user` or `bot_id` isn't shaped like Slack's,
//! after its 200 and before any row. Slash commands and interactions are
//! keyed by their signature, whose shape verification fixes.
//!
//! # Agents' apps
//!
//! An agent's app is only a way to reach the agent, so of what it is sent
//! only `message` events are queued. Its other events, its slash commands
//! and its interactions get an empty 200 and are dropped, and so is a
//! message whose `ts` is more than
//! [`CONFIRM_WINDOW`](crate::surface::CONFIRM_WINDOW) before it arrived,
//! which [`Surface::confirm`](core_types::Surface::confirm) would refuse.
//! None of them writes a deduplication row or takes a place. A message
//! dropped for its age is logged as a warning at most once per binding
//! per [`WARNING_INTERVAL`], since a clock running fast or a backlog at
//! Slack drops every one.

use std::borrow::Cow;
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
use crate::normalize::{self, is_channel_id, is_event_id, is_team_id, is_ts};
use crate::surface::within_window;
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
/// How many messages the apps of one owner's agents may keep together at
/// once before [`OWNER_REQUESTS_PER_SECOND`] applies, however many agents
/// they have. Only a message that is kept, and so would write a
/// deduplication row, counts.
pub const OWNER_BURST: u32 = 2 * AGENT_BURST;
/// How many messages a second the apps of one owner's agents may keep
/// together once their [`OWNER_BURST`] is used up. Past it, a message is
/// dropped after its 200.
pub const OWNER_REQUESTS_PER_SECOND: u32 = 2 * AGENT_REQUESTS_PER_SECOND;
/// How long a deduplication key must be remembered: longer than Slack
/// retries a delivery (the last retry comes about five minutes after the
/// first), than a signature is accepted (five minutes), and than the
/// [`CONFIRM_WINDOW`](crate::surface::CONFIRM_WINDOW) within which an
/// agent's message is kept.
pub const DEDUP_RETENTION: time::Duration = time::Duration::hours(1);
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

/// What agentd knows about an agent's Slack app.
#[derive(Debug, Clone)]
pub struct AgentApp {
    /// The app itself.
    pub app: SlackApp,
    /// The owner of the agent whose app this is, whose agents' apps
    /// together have [`MAX_IN_FLIGHT_PER_OWNER`] places and one bucket.
    pub owner: MemberId,
}

/// Looks up a binding's app and its signing secret.
///
/// agentd implements it: the manager's secret comes from configuration, and
/// agent bindings' from the store.
///
/// # Errors
///
/// Each method fails if the lookup itself fails. The request then gets
/// 503, as it does when the lookup takes longer than [`PRE_ACK_TIMEOUT`];
/// Slack retries events, but not slash commands or interactions.
#[async_trait::async_trait]
pub trait SigningSecrets: Send + Sync {
    /// The manager app, or `None` if agentd has none.
    async fn manager(&self) -> Result<Option<SlackApp>, BoxError>;

    /// The app of the agent binding `binding`, or `None` if agentd has no
    /// such binding.
    async fn agent(&self, binding: BindingId) -> Result<Option<AgentApp>, BoxError>;
}

/// The app behind `binding`, and the seat its requests take.
async fn lookup(
    secrets: &dyn SigningSecrets,
    binding: BindingRef,
) -> Result<Option<(SlackApp, Seat)>, BoxError> {
    Ok(match binding {
        BindingRef::Manager => secrets.manager().await?.map(|app| (app, Seat::Manager)),
        BindingRef::Agent(binding) => secrets
            .agent(binding)
            .await?
            .map(|AgentApp { app, owner }| (app, Seat::Agent { binding, owner })),
    })
}

/// Remembers which deliveries were already handled. agentd implements it
/// with the store's `processed_events`.
#[async_trait::async_trait]
pub trait Dedup: Send + Sync {
    /// Records `key` under `source` for at least [`DEDUP_RETENTION`].
    /// Returns true the first time, and false when it was already recorded.
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
    let notes = Arc::new(Throttle::new(WARNING_INTERVAL));
    let state = Ingress {
        secrets,
        queue: sender,
        places: Arc::new(Places {
            capacity,
            taken: Mutex::default(),
        }),
        notes: Arc::clone(&notes),
    };
    let router = Router::new()
        .route("/slack/b/{binding}/events", post(events))
        .route("/slack/b/{binding}/interactivity", post(interactivity))
        .route("/slack/b/{binding}/commands", post(commands))
        .with_state(state);
    (router, Queue { receiver, notes })
}

/// What the ingress logs at most once per binding per
/// [`WARNING_INTERVAL`], each kind counted apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Note {
    Refused,
    RateLimited,
    Challenge,
    Retry,
    Stale,
    OwnerRate,
    Reparsed,
}

/// The throttle of every [`Note`].
type Notes = Throttle<(BindingRef, Note)>;

/// Whether `binding`'s `note` is due as a warning (or info): `Some` of how
/// many went quiet since the last.
fn note(notes: &Notes, binding: BindingRef, note: Note) -> Option<u64> {
    notes.record((binding, note), Instant::now())
}

#[derive(Clone)]
struct Ingress {
    secrets: Arc<dyn SigningSecrets>,
    queue: mpsc::UnboundedSender<Queued>,
    places: Arc<Places>,
    notes: Arc<Notes>,
}

impl Ingress {
    /// Logs a refused request.
    fn refused(
        &self,
        binding: BindingRef,
        kind: Kind,
        status: StatusCode,
        reason: &dyn fmt::Display,
    ) {
        let (kind, status) = (kind.as_str(), status.as_u16());
        match note(&self.notes, binding, Note::Refused) {
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
        match note(&self.notes, binding, Note::RateLimited) {
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

    /// Logs an agent's app's message dropped for being older than the
    /// confirmation window: all of them are, while agentd's clock runs
    /// fast or Slack delivers late.
    fn stale(&self, binding: BindingRef) {
        match note(&self.notes, binding, Note::Stale) {
            Some(quiet) => tracing::warn!(
                %binding,
                dropped_since_last_warning = quiet,
                "dropped an agent's Slack message older than the confirmation window; is agentd's clock right, or is Slack delivering late?"
            ),
            None => tracing::debug!(
                %binding,
                "dropped an agent's Slack message older than the confirmation window"
            ),
        }
    }

    /// Logs an answered `url_verification` challenge.
    fn challenged(&self, binding: BindingRef) {
        match note(&self.notes, binding, Note::Challenge) {
            Some(quiet) => tracing::info!(
                %binding,
                answered_since_last_info = quiet,
                "answered Slack's url_verification challenge"
            ),
            None => tracing::debug!(%binding, "answered Slack's url_verification challenge"),
        }
    }

    /// Logs Slack's retry headers, which say that an earlier delivery wasn't
    /// acknowledged in time. They aren't signed, so they are only logged.
    fn retried(&self, binding: BindingRef, kind: Kind, headers: &HeaderMap) {
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
        let kind = kind.as_str();
        match note(&self.notes, binding, Note::Retry) {
            Some(quiet) => tracing::info!(
                %binding,
                kind,
                retry_num,
                retry_reason,
                retried_since_last_info = quiet,
                "Slack retried a delivery"
            ),
            None => {
                tracing::debug!(%binding, kind, retry_num, retry_reason, "Slack retried a delivery")
            }
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

    /// Whether a message on this place may be kept at `now`: true, taking a
    /// token, while the bucket of the owner whose agent's app it came to
    /// has one. Always true for the manager app's, and for a place counted
    /// nowhere.
    fn keep(&self, now: Instant) -> bool {
        match &self.0 {
            Some((Seat::Agent { owner, .. }, places)) => places.keep(*owner, now),
            Some((Seat::Manager, _)) | None => true,
        }
    }
}

impl fmt::Debug for InFlight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some((seat, _)) => f.debug_tuple("InFlight").field(&seat.binding()).finish(),
            None => f.write_str("InFlight(untracked)"),
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Some((seat, places)) = self.0.take() {
            places.give_back(seat, Instant::now());
        }
    }
}

/// Whose place an [`InFlight`] holds: the manager app's, or an agent's app
/// and its owner's.
#[derive(Debug, Clone, Copy)]
enum Seat {
    Manager,
    Agent { binding: BindingId, owner: MemberId },
}

impl Seat {
    fn binding(self) -> BindingRef {
        match self {
            Self::Manager => BindingRef::Manager,
            Self::Agent { binding, .. } => BindingRef::Agent(binding),
        }
    }
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
/// [`MAX_IN_FLIGHT_PER_OWNER`] for each owner's; the [`Bucket`] of each
/// agent's app, which its requests take from; and the bucket of each owner,
/// which only the messages their agents' apps keep take from.
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
    agent_buckets: HashMap<BindingId, Bucket>,
    owner_buckets: HashMap<MemberId, Bucket>,
}

/// How fast a [`Bucket`] lets requests through.
#[derive(Debug, Clone, Copy)]
struct Rate {
    burst: u32,
    per_second: u32,
}

const AGENT_RATE: Rate = Rate {
    burst: AGENT_BURST,
    per_second: AGENT_REQUESTS_PER_SECOND,
};

const OWNER_RATE: Rate = Rate {
    burst: OWNER_BURST,
    per_second: OWNER_REQUESTS_PER_SECOND,
};

/// A token bucket: a [`Rate`]'s burst of tokens, refilled at its rate, one
/// taken by each request or message let through. It lives only in memory:
/// after a restart every app and owner starts with a full one, which costs
/// at most one more burst. A full bucket is the same as none, so one is
/// forgotten once it is full and its binding or owner has nothing in
/// flight.
#[derive(Debug)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn full(rate: Rate, now: Instant) -> Self {
        Self {
            tokens: f64::from(rate.burst),
            at: now,
        }
    }

    /// Refills the bucket up to `now`, and says whether it has a token.
    fn refill(&mut self, rate: Rate, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens =
            (self.tokens + elapsed * f64::from(rate.per_second)).min(f64::from(rate.burst));
        self.at = self.at.max(now);
        self.tokens >= 1.0
    }

    /// Takes a token at `now`, if the bucket has one, and says whether it
    /// had.
    fn take(&mut self, rate: Rate, now: Instant) -> bool {
        let has = self.refill(rate, now);
        if has {
            self.tokens -= 1.0;
        }
        has
    }

    fn is_full(&self, rate: Rate) -> bool {
        self.tokens >= f64::from(rate.burst)
    }
}

impl Places {
    /// A place at `now` for a request on `seat`, or why there is none.
    fn take(self: &Arc<Self>, seat: Seat, now: Instant) -> Result<InFlight, Busy> {
        let mut taken = self.taken.lock().unwrap_or_else(PoisonError::into_inner);
        let taken = &mut *taken;
        match seat {
            Seat::Manager if taken.manager >= self.capacity => return Err(Busy::InFlight),
            Seat::Manager => taken.manager += 1,
            Seat::Agent { binding, owner } => {
                let held = taken.by_agent.get(&binding).copied().unwrap_or_default();
                if taken.agents >= self.capacity || held >= MAX_IN_FLIGHT_PER_AGENT {
                    return Err(Busy::InFlight);
                }
                if taken.by_owner.get(&owner).copied().unwrap_or_default()
                    >= MAX_IN_FLIGHT_PER_OWNER
                {
                    return Err(Busy::Owner);
                }
                let agent = taken
                    .agent_buckets
                    .entry(binding)
                    .or_insert_with(|| Bucket::full(AGENT_RATE, now));
                if !agent.take(AGENT_RATE, now) {
                    return Err(Busy::Rate);
                }
                taken.agents += 1;
                *taken.by_agent.entry(binding).or_default() += 1;
                *taken.by_owner.entry(owner).or_default() += 1;
            }
        }
        Ok(InFlight(Some((seat, Arc::clone(self)))))
    }

    /// Whether one of `owner`'s agents' apps may keep a message at `now`,
    /// taking a token from `owner`'s bucket if it may.
    fn keep(&self, owner: MemberId, now: Instant) -> bool {
        let mut taken = self.taken.lock().unwrap_or_else(PoisonError::into_inner);
        taken
            .owner_buckets
            .entry(owner)
            .or_insert_with(|| Bucket::full(OWNER_RATE, now))
            .take(OWNER_RATE, now)
    }

    fn give_back(&self, seat: Seat, now: Instant) {
        let mut taken = self.taken.lock().unwrap_or_else(PoisonError::into_inner);
        let taken = &mut *taken;
        let Seat::Agent { binding, owner } = seat else {
            taken.manager -= 1;
            return;
        };
        taken.agents -= 1;
        if release(&mut taken.by_agent, binding) {
            forget_if_full(&mut taken.agent_buckets, binding, AGENT_RATE, now);
        }
        if release(&mut taken.by_owner, owner) {
            forget_if_full(&mut taken.owner_buckets, owner, OWNER_RATE, now);
        }
    }
}

/// Gives back one of `key`'s places, forgetting it once it holds none, and
/// says whether it holds none.
fn release<K: Eq + std::hash::Hash>(held: &mut HashMap<K, usize>, key: K) -> bool {
    let Some(count) = held.get_mut(&key) else {
        return true;
    };
    *count -= 1;
    if *count > 0 {
        return false;
    }
    held.remove(&key);
    true
}

/// Forgets `key`'s bucket if it is full at `now`.
fn forget_if_full<K: Eq + std::hash::Hash>(
    buckets: &mut HashMap<K, Bucket>,
    key: K,
    rate: Rate,
    now: Instant,
) {
    let full = buckets.get_mut(&key).is_some_and(|bucket| {
        bucket.refill(rate, now);
        bucket.is_full(rate)
    });
    if full {
        buckets.remove(&key);
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
    let (app, seat) =
        match tokio::time::timeout_at(deadline, lookup(ingress.secrets.as_ref(), binding)).await {
            Ok(Ok(Some(found))) => found,
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
    ingress.retried(binding, kind, headers);

    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match check(binding, kind, &body, received_at) {
        Ok(Checked::Queue) => {}
        Ok(Checked::RateLimited {
            minute_rate_limited,
        }) => {
            ingress.rate_limited(binding, minute_rate_limited);
            return StatusCode::OK.into_response();
        }
        Ok(Checked::Ignore(reason)) => {
            tracing::debug!(%binding, kind = kind.as_str(), reason, "ignored a Slack request");
            return StatusCode::OK.into_response();
        }
        Ok(Checked::Stale) => {
            ingress.stale(binding);
            return StatusCode::OK.into_response();
        }
        Err(reason) => {
            ingress.refused(binding, kind, StatusCode::BAD_REQUEST, &reason);
            return StatusCode::BAD_REQUEST.into_response();
        }
    }
    let place = match ingress.places.take(seat, Instant::now()) {
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

/// An events body's `type`, which must be a string. Everything else is
/// skipped, so a body that isn't a probe allocates no more than its
/// `type`.
#[derive(Deserialize)]
struct ProbeType<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<Cow<'a, str>>,
}

/// A `url_verification` body's `challenge`, which must be a string, so
/// no other JSON is ever built from an unsigned body.
#[derive(Deserialize)]
struct ProbeChallenge<'a> {
    #[serde(borrow)]
    challenge: Option<Cow<'a, str>>,
}

/// `None` unless the body is a JSON object whose `type` is
/// `url_verification`. Then `Some` of its challenge, or `Some(None)` if the
/// challenge is missing, not a string, empty, longer than
/// [`MAX_CHALLENGE_BYTES`], or not printable ASCII.
fn url_verification(body: &[u8]) -> Option<Option<String>> {
    let probe: ProbeType<'_> = serde_json::from_slice(body).ok()?;
    if probe.kind.as_deref() != Some("url_verification") {
        return None;
    }
    let challenge = serde_json::from_slice::<ProbeChallenge<'_>>(body)
        .ok()
        .and_then(|probe| probe.challenge);
    Some(
        challenge
            .filter(|challenge| {
                !challenge.is_empty()
                    && challenge.len() <= MAX_CHALLENGE_BYTES
                    && challenge.bytes().all(|b| b.is_ascii_graphic())
            })
            .map(Cow::into_owned),
    )
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

/// The ids a `message` event is deduplicated and kept by.
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

/// Whether `payload` is a JSON object, checked without keeping any of it.
fn is_json_object(payload: &str) -> bool {
    payload.trim_start().starts_with('{') && serde_json::from_str::<IgnoredAny>(payload).is_ok()
}

enum Checked {
    Queue,
    RateLimited { minute_rate_limited: Option<i64> },
    Ignore(&'static str),
    Stale,
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

/// Checks that a verified body to `binding` parses, and that the ids an
/// event is deduplicated by are shaped like Slack's (see
/// [Shapes](self#shapes)), without keeping what it parsed; and whether it
/// is one to queue, received at `received_at` (see
/// [Agents' apps](self#agents-apps)). The error names what was wrong, never
/// what was sent.
fn check(
    binding: BindingRef,
    kind: Kind,
    body: &[u8],
    received_at: OffsetDateTime,
) -> Result<Checked, &'static str> {
    let agent = binding != BindingRef::Manager;
    match kind {
        Kind::Events => {
            let head: EnvelopeHead =
                serde_json::from_slice(body).map_err(|_| "not an Events API envelope")?;
            match head.kind.as_str() {
                "event_callback" => Ok(match check_callback(&head, body)? {
                    None if agent => Checked::Ignore("an agent's app's event other than a message"),
                    Some(ts)
                        if agent
                            && ts
                                .as_deref()
                                .is_some_and(|ts| !within_window(ts, received_at)) =>
                    {
                        Checked::Stale
                    }
                    _ => Checked::Queue,
                }),
                "app_rate_limited" => Ok(Checked::RateLimited {
                    minute_rate_limited: head.minute_rate_limited,
                }),
                _ => Ok(Checked::Ignore("an envelope of another type")),
            }
        }
        Kind::Commands | Kind::Interactivity if agent => {
            Ok(Checked::Ignore("an agent's app's command or interaction"))
        }
        Kind::Commands => {
            command_form(body)?;
            Ok(Checked::Queue)
        }
        Kind::Interactivity => {
            let form: InteractivityForm =
                serde_urlencoded::from_bytes(body).map_err(|_| "not an interactivity form")?;
            if !is_json_object(&form.payload) {
                return Err("the interactivity payload is not a JSON object");
            }
            Ok(Checked::Queue)
        }
    }
}

/// Checks an `event_callback`'s ids. `Some` of its `ts`, if it has one,
/// for a `message` event, and `None` for any other.
fn check_callback(
    head: &EnvelopeHead,
    body: &[u8],
) -> Result<Option<Option<String>>, &'static str> {
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
        return Ok(None);
    }
    let MessageHead { event: ids } = serde_json::from_slice(body)
        .map_err(|_| "a message event whose channel, ts or thread_ts isn't a string")?;
    if !ids.channel.as_deref().is_none_or(is_channel_id) {
        return Err("the message's channel isn't shaped like Slack's");
    }
    if ![&ids.ts, &ids.thread_ts]
        .into_iter()
        .flatten()
        .all(|ts| is_ts(ts))
    {
        return Err("the message's ts or thread_ts isn't shaped like Slack's");
    }
    Ok(Some(ids.ts))
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

/// The receiving end of the ingress: requests that were acknowledged and
/// still have to be deduplicated, normalized and handed on.
pub struct Queue {
    receiver: mpsc::UnboundedReceiver<Queued>,
    notes: Arc<Notes>,
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
            let Some(inbound) = process(queued, dedup.as_ref(), &self.notes).await else {
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

async fn process(queued: Queued, dedup: &dyn Dedup, notes: &Notes) -> Option<SlackInbound> {
    let Queued {
        binding,
        bot_user,
        received_at,
        kind,
        body,
        signature,
        place,
    } = queued;
    let reparsed = |what: &'static str| match note(notes, binding, Note::Reparsed) {
        Some(quiet) => tracing::warn!(
            %binding,
            kind = kind.as_str(),
            what,
            dropped_since_last_warning = quiet,
            "a queued Slack request no longer parses; dropped it"
        ),
        None => {
            tracing::debug!(%binding, kind = kind.as_str(), what, "a queued Slack request no longer parses; dropped it")
        }
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
                notes,
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
    notes: &Notes,
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
    if !place.keep(Instant::now()) {
        match note(notes, binding, Note::OwnerRate) {
            Some(quiet) => tracing::warn!(
                %binding,
                event_id,
                dropped_since_last_warning = quiet,
                "dropped a Slack message: its owner's agents' apps are keeping messages faster than their rate"
            ),
            None => tracing::debug!(
                %binding,
                event_id,
                "dropped a Slack message: its owner's agents' apps are keeping messages faster than their rate"
            ),
        }
        return None;
    }
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
        let longer = "A".repeat(normalize::MAX_ID_TAIL);
        let too_long = "A".repeat(normalize::MAX_ID_TAIL + 1);
        assert!(is_event_id(&format!("Ev{longer}")));
        assert!(!is_event_id(&format!("Ev{too_long}")));
        for id in ["T024BE7LD", "E0ORG0001", &format!("T{longer}")] {
            assert!(is_team_id(id), "{id}");
        }
        assert!(!is_team_id(&format!("T{too_long}")));
        for id in ["C024BE91L", "D024BE91L", "G024BE91L", &format!("C{longer}")] {
            assert!(is_channel_id(id), "{id}");
        }
        for id in [
            "U024BE7LH",
            "C",
            "c024be91l",
            "C024 BE91",
            &format!("C{too_long}"),
        ] {
            assert!(!is_channel_id(id), "{id}");
        }
        for ts in [
            "1727697600.000100",
            "17276976000.000100",
            "99999999999999999999.999999",
        ] {
            assert!(is_ts(ts), "{ts}");
        }
        for ts in [
            "",
            "1.2",
            "1727697600",
            "1727697600.",
            "1727697600.0001",
            "172769760.000100",
            "01727697600.000100",
            "100000000000000000000.000100",
            "1727697600.0001000",
            "1727697600,000100",
            "+727697600.000100",
        ] {
            assert!(!is_ts(ts), "{ts}");
        }
    }

    fn take(bucket: &mut Bucket, now: Instant) -> bool {
        bucket.take(AGENT_RATE, now)
    }

    #[test]
    fn an_agents_bucket_lets_a_burst_through_then_its_rate() {
        let start = Instant::now();
        let mut bucket = Bucket::full(AGENT_RATE, start);
        for n in 0..AGENT_BURST {
            assert!(take(&mut bucket, start), "request {n} of the burst");
        }
        assert!(!take(&mut bucket, start));
        let tick = Duration::from_secs(1) / AGENT_REQUESTS_PER_SECOND;
        assert!(take(&mut bucket, start + tick));
        assert!(!take(&mut bucket, start + tick));
        let later = start + Duration::from_secs(3600);
        for _ in 0..AGENT_BURST {
            assert!(take(&mut bucket, later));
        }
        assert!(
            !take(&mut bucket, later),
            "an idle hour refills only the burst"
        );
        assert!(
            !take(&mut bucket, start),
            "a time before the last take refills nothing"
        );
    }

    fn places(capacity: usize) -> Arc<Places> {
        Arc::new(Places {
            capacity,
            taken: Mutex::default(),
        })
    }

    fn seat(binding: BindingId, owner: MemberId) -> Seat {
        Seat::Agent { binding, owner }
    }

    #[test]
    fn a_busy_binding_holds_no_other_bindings_places() {
        let places = places(4);
        let now = Instant::now();
        let (a, b) = (BindingId::new_v4(), BindingId::new_v4());
        let (owner, other) = (MemberId::new_v4(), MemberId::new_v4());
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(places.take(seat(a, owner), now).unwrap());
        }
        assert_eq!(
            places.take(seat(b, other), now).unwrap_err(),
            Busy::InFlight
        );
        assert!(places.take(Seat::Manager, now).is_ok());
        held.pop();
        assert!(places.take(seat(b, other), now).is_ok());
        drop(held);
        let taken = places.taken.lock().unwrap();
        assert_eq!((taken.manager, taken.agents), (0, 0));
        assert!(taken.by_agent.is_empty() && taken.by_owner.is_empty());
    }

    #[test]
    fn one_owners_agents_share_a_bucket_for_what_they_keep_at_a_fixed_time() {
        let places = places(4096);
        let now = Instant::now() + Duration::from_secs(3600);
        let ada = MemberId::new_v4();
        for _ in 0..OWNER_BURST.div_ceil(AGENT_BURST) + 1 {
            let agent = BindingId::new_v4();
            for n in 0..AGENT_BURST {
                let admitted = places.take(seat(agent, ada), now);
                assert!(admitted.is_ok(), "request {n} to {agent}");
            }
        }
        let place = places.take(seat(BindingId::new_v4(), ada), now).unwrap();
        let kept = (0..2 * OWNER_BURST).filter(|_| place.keep(now)).count();
        assert_eq!(
            kept,
            usize::try_from(OWNER_BURST).unwrap(),
            "only what is kept takes from the owner's bucket"
        );
        let other = places
            .take(seat(BindingId::new_v4(), MemberId::new_v4()), now)
            .unwrap();
        assert!(other.keep(now), "another owner has a bucket of their own");
        let later = now + Duration::from_secs(1);
        let refilled = (0..OWNER_BURST).filter(|_| place.keep(later)).count();
        assert_eq!(
            refilled,
            usize::try_from(OWNER_REQUESTS_PER_SECOND).unwrap()
        );
        let manager = places.take(Seat::Manager, now).unwrap();
        assert!(manager.keep(later), "the manager app has no owner's bucket");
        assert!(InFlight::untracked().keep(later));
    }

    /// Gives `place` back at `now` rather than when it is dropped.
    fn give_back(places: &Places, mut place: InFlight, now: Instant) {
        let (seat, _) = place.0.take().unwrap();
        places.give_back(seat, now);
    }

    #[test]
    fn a_full_bucket_with_nothing_in_flight_is_forgotten() {
        let places = places(64);
        let now = Instant::now();
        let earlier = now.checked_sub(Duration::from_secs(60)).unwrap();
        let (idle, busy) = (BindingId::new_v4(), BindingId::new_v4());
        let (ada, bob) = (MemberId::new_v4(), MemberId::new_v4());
        let idle_place = places.take(seat(idle, ada), earlier).unwrap();
        assert!(idle_place.keep(earlier));
        give_back(&places, idle_place, now);
        let held = places.take(seat(busy, bob), now).unwrap();
        assert!(held.keep(now));
        let second = places.take(seat(busy, bob), now).unwrap();
        give_back(&places, second, now);
        {
            let taken = places.taken.lock().unwrap();
            assert!(!taken.agent_buckets.contains_key(&idle));
            assert!(!taken.owner_buckets.contains_key(&ada));
            assert!(taken.agent_buckets.contains_key(&busy), "one is in flight");
            assert!(taken.owner_buckets.contains_key(&bob));
        }
        give_back(&places, held, now + Duration::from_millis(10));
        {
            let taken = places.taken.lock().unwrap();
            assert!(
                taken.agent_buckets.contains_key(&busy),
                "not full again yet"
            );
            assert!(taken.owner_buckets.contains_key(&bob), "not full again yet");
        }
        let again = places.take(seat(busy, bob), now).unwrap();
        give_back(&places, again, now + Duration::from_secs(60));
        let taken = places.taken.lock().unwrap();
        assert!(taken.agent_buckets.is_empty() && taken.owner_buckets.is_empty());
    }

    #[test]
    fn a_challenge_is_a_short_printable_string_and_nothing_else_is_built() {
        let array = format!(
            r#"{{"type":"url_verification","challenge":[{}0]}}"#,
            "0,".repeat(100_000)
        );
        assert_eq!(url_verification(array.as_bytes()), Some(None));
        assert_eq!(
            url_verification(br#"{"challenge":"abc","type":"url_verification"}"#),
            Some(Some("abc".to_owned()))
        );
        assert_eq!(
            url_verification(br#"{"type":"url_verification","challenge":7}"#),
            Some(None)
        );
        assert_eq!(
            url_verification(br#"{"type":"event_callback","challenge":"abc"}"#),
            None
        );
        assert_eq!(url_verification(br#"{"type":7,"challenge":"abc"}"#), None);
    }

    #[test]
    fn an_interaction_payload_must_be_an_object() {
        for payload in ["{}", " {\"type\":\"block_actions\"}"] {
            assert!(is_json_object(payload), "{payload}");
        }
        for payload in ["[]", "[{}]", "{\"a\":", "\"{}\"", "1", ""] {
            assert!(!is_json_object(payload), "{payload}");
        }
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

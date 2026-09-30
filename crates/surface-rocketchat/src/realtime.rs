//! The realtime client: one DDP connection over a WebSocket per bot user.
//!
//! [`RealtimeClient::run`] connects, logs in with the user's token as a
//! `resume` token, and subscribes to:
//!
//! - `stream-notify-user` `<uid>/subscriptions-changed`, so a room the user
//!   is added to is subscribed at once, and a room it leaves is dropped;
//! - `stream-room-messages` for every room the user belongs to, listed with
//!   REST `subscriptions.get`. The server's `__my_messages__` event isn't
//!   used; the design leaves open whether it covers every room.
//!
//! It answers the server's `ping`s, pings the server itself when the
//! connection goes quiet, and reconnects with jittered exponential backoff
//! when the connection drops. The backoff starts over only after a
//! connection stayed up for a while (see [`RealtimeOptions`]), so a server
//! that accepts and then drops at once is retried less and less often.
//! Each connection lists the rooms again, so it
//! subscribes to the rooms the user is in by then, including those it was
//! added to while disconnected. Messages posted while it was disconnected
//! are not fetched.
//!
//! It hands every message event to the caller unfiltered; normalizing and
//! deduplicating is [`RocketChatSurface`](crate::RocketChatSurface)'s job.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use core_types::{ConversationId, SurfaceError, UserId};
use futures::{SinkExt, StreamExt};
use rand::RngExt;
use reqwest::Url;
use secrecy::ExposeSecret;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message as Frame;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::ddp::{self, Incoming};
use crate::rest::{Credentials, Message, RestClient, Result, RoomType};

/// The stream carrying a room's messages; its event name is the room id.
const ROOM_MESSAGES: &str = "stream-room-messages";

/// The stream carrying per-user notices; event names are `<uid>/<event>`.
const NOTIFY_USER: &str = "stream-notify-user";

/// The shortest heartbeat tick, so a tiny heartbeat in tests can't spin.
const MIN_TICK: Duration = Duration::from_millis(10);

/// Timing of the realtime connection.
///
/// The backoff starts over once a connection has stayed up for the longer
/// of `backoff_max` and twice `heartbeat`: long enough that reconnecting
/// at once costs no more than waiting the longest backoff would have, and
/// longer than it takes to notice a silent server. A connection that drops
/// sooner, even after logging in and subscribing, counts as a failed
/// attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RealtimeOptions {
    /// The first wait before reconnecting. Each failed attempt doubles it,
    /// up to `backoff_max`, and each wait is jittered down by up to half.
    pub backoff_initial: Duration,
    /// The longest wait before reconnecting.
    pub backoff_max: Duration,
    /// How long the server may stay silent before the client pings it. After
    /// twice this without any frame, the connection counts as dead and the
    /// client reconnects.
    pub heartbeat: Duration,
    /// How long connecting, logging in and subscribing may take.
    pub setup_timeout: Duration,
}

impl RealtimeOptions {
    /// How long a connection must stay up before the backoff starts over.
    fn healthy_uptime(&self) -> Duration {
        self.backoff_max.max(self.heartbeat.saturating_mul(2))
    }
}

impl Default for RealtimeOptions {
    /// Reconnect after 1 s, doubling to at most 60 s; ping after 20 s of
    /// silence; 30 s to set up. Rocket.Chat itself pings a client that has
    /// been silent for 30 s and drops it 30 s later.
    fn default() -> Self {
        Self {
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            heartbeat: Duration::from_secs(20),
            setup_timeout: Duration::from_secs(30),
        }
    }
}

/// The realtime endpoint for a server: `ws://` or `wss://` for `http://` or
/// `https://`, at `<base>/websocket`.
///
/// ```
/// use surface_rocketchat::realtime::websocket_url;
///
/// assert_eq!(
///     websocket_url("https://chat.example.com/").unwrap(),
///     "wss://chat.example.com/websocket"
/// );
/// assert_eq!(
///     websocket_url("http://localhost:3000/chat").unwrap(),
///     "ws://localhost:3000/chat/websocket"
/// );
/// ```
pub fn websocket_url(base_url: &str) -> Result<String> {
    let mut url = Url::parse(base_url)
        .map_err(|err| SurfaceError::Api(format!("invalid Rocket.Chat URL: {err}")))?;
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        _ => {
            return Err(SurfaceError::Api(
                "invalid Rocket.Chat URL: the scheme must be http or https".into(),
            ));
        }
    };
    if url.set_scheme(scheme).is_err() {
        return Err(SurfaceError::Api("invalid Rocket.Chat URL".into()));
    }
    if let Ok(mut segments) = url.path_segments_mut() {
        segments.pop_if_empty().push("websocket");
    }
    url.set_query(None);
    url.set_fragment(None);
    Ok(url.into())
}

/// One DDP connection, acting as one user.
#[derive(Debug, Clone)]
pub struct RealtimeClient {
    url: String,
    rest: RestClient,
    options: RealtimeOptions,
}

/// Why a connection ended.
#[derive(Debug)]
enum End {
    /// The receiver of messages is gone.
    Closed,
    /// Reconnecting can't help, such as a rejected token.
    Fatal(SurfaceError),
    /// The connection failed; reconnect after a backoff.
    Retry(String),
}

type Step<T = ()> = std::result::Result<T, End>;

impl RealtimeClient {
    /// A client for the realtime endpoint `url` (`ws://` or `wss://`, see
    /// [`websocket_url`]), logging in as the user `rest` acts as. `rest`
    /// also lists the user's rooms.
    pub fn new(url: &str, rest: RestClient, options: RealtimeOptions) -> Result<Self> {
        let parsed = Url::parse(url)
            .map_err(|err| SurfaceError::Api(format!("invalid realtime URL: {err}")))?;
        if !matches!(parsed.scheme(), "ws" | "wss") {
            return Err(SurfaceError::Api(
                "invalid realtime URL: the scheme must be ws or wss".into(),
            ));
        }
        Ok(Self {
            url: url.to_owned(),
            rest,
            options,
        })
    }

    /// Runs the connection, sending every message event to `out`, until
    /// `out` is closed (`Ok`) or the server rejects the token
    /// ([`SurfaceError::Unauthorized`]). Any other failure reconnects.
    pub async fn run(&self, out: mpsc::Sender<Message>) -> Result<()> {
        let mut backoff = Backoff::new(self.options.backoff_initial, self.options.backoff_max);
        loop {
            match self.session(&out, &mut backoff).await {
                End::Closed => return Ok(()),
                End::Fatal(err) => return Err(err),
                End::Retry(reason) => {
                    let wait = backoff.next_wait();
                    tracing::warn!(
                        user = %self.rest.user_id(),
                        %reason,
                        wait_ms = wait.as_millis(),
                        "realtime connection lost, reconnecting"
                    );
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {}
                        () = out.closed() => return Ok(()),
                    }
                }
            }
        }
    }

    async fn session(&self, out: &mpsc::Sender<Message>, backoff: &mut Backoff) -> End {
        let timeout = self.options.setup_timeout;
        let ws = match tokio::time::timeout(timeout, tokio_tungstenite::connect_async(&self.url))
            .await
        {
            Ok(Ok((ws, _))) => ws,
            Ok(Err(err)) => return End::Retry(format!("could not connect: {err}")),
            Err(_) => return End::Retry("connecting timed out".into()),
        };
        let mut conn = Connection::new(ws, self.rest.user_id(), out);
        match tokio::time::timeout(timeout, conn.setup(&self.rest)).await {
            Ok(Ok(())) => {}
            Ok(Err(end)) => return end,
            Err(_) => return End::Retry("setting up timed out".into()),
        }
        tracing::debug!(
            user = %self.rest.user_id(),
            rooms = conn.room_subs.len(),
            "realtime connection ready"
        );
        let ready = Instant::now();
        let end = conn.serve(self.options.heartbeat).await;
        if ready.elapsed() >= self.options.healthy_uptime() {
            backoff.reset();
        }
        end
    }
}

/// Jittered exponential backoff.
#[derive(Debug)]
struct Backoff {
    initial: Duration,
    max: Duration,
    attempt: u32,
}

impl Backoff {
    fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max: max.max(initial),
            attempt: 0,
        }
    }

    /// The ceiling for this attempt, `initial × 2^attempt` capped at `max`,
    /// before jitter.
    fn ceiling(&self) -> Duration {
        let factor = 1_u32 << self.attempt.min(20);
        self.initial.saturating_mul(factor).min(self.max)
    }

    /// The next wait: between half the ceiling and the ceiling.
    fn next_wait(&mut self) -> Duration {
        let ceiling = self.ceiling();
        self.attempt = self.attempt.saturating_add(1);
        let low = ceiling / 2;
        let spread = u64::try_from((ceiling - low).as_millis()).unwrap_or(u64::MAX);
        low + Duration::from_millis(rand::rng().random_range(0..=spread))
    }

    fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// What a subscription id stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sub {
    /// `stream-room-messages` for a room.
    Room(ConversationId),
    /// `stream-notify-user` `<uid>/subscriptions-changed`.
    Notify,
}

/// What a `subscriptions-changed` notice reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Inserted,
    Updated,
    Removed,
}

/// One `subscriptions-changed` notice: `[action, subscription]`.
#[derive(Debug)]
struct Notice {
    action: Action,
    /// The subscription document's `_id`.
    doc: Option<String>,
    /// The room (`rid`). A removal may lack it.
    room: Option<ConversationId>,
    /// Whether the room's type is listened to; true when the notice has
    /// no type.
    listened: bool,
}

impl Notice {
    fn parse(args: &[Value]) -> Option<Self> {
        let action = match args.first().and_then(Value::as_str)? {
            "inserted" => Action::Inserted,
            "updated" => Action::Updated,
            "removed" => Action::Removed,
            _ => return None,
        };
        let doc = args.get(1);
        let field = |key: &str| doc.and_then(|d| d.get(key)).and_then(Value::as_str);
        Some(Self {
            action,
            doc: field("_id").map(str::to_owned),
            room: field("rid").map(ConversationId::from),
            listened: field("t")
                .map(RoomType::from_code)
                .as_ref()
                .is_none_or(listens_to),
        })
    }
}

/// One live WebSocket and what is subscribed on it.
struct Connection<'a> {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    user: &'a UserId,
    out: &'a mpsc::Sender<Message>,
    next_id: u64,
    subs: HashMap<String, Sub>,
    room_subs: HashMap<ConversationId, String>,
    /// The room of each subscription document seen, so that a removal
    /// notice without `rid` can be resolved.
    docs: HashMap<String, ConversationId>,
    /// Rooms whose subscription the server refused. They aren't asked for
    /// again until an `inserted` notice or the next connection.
    refused: HashSet<ConversationId>,
    /// Notices that arrived while the room list was being read, applied
    /// on top of it.
    pending: Option<Vec<Notice>>,
    last_frame: Instant,
    notify_event: String,
}

impl<'a> Connection<'a> {
    fn new(
        ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
        user: &'a UserId,
        out: &'a mpsc::Sender<Message>,
    ) -> Self {
        Self {
            ws,
            user,
            out,
            next_id: 0,
            subs: HashMap::new(),
            room_subs: HashMap::new(),
            docs: HashMap::new(),
            refused: HashSet::new(),
            pending: None,
            last_frame: Instant::now(),
            notify_event: format!("{user}/subscriptions-changed"),
        }
    }

    fn id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}-{}", self.next_id)
    }

    async fn send(&mut self, frame: String) -> Step {
        self.ws
            .send(Frame::text(frame))
            .await
            .map_err(|err| End::Retry(format!("could not send: {err}")))
    }

    /// The next frame the client understands.
    async fn next(&mut self) -> Step<Incoming> {
        loop {
            let frame = match self.ws.next().await {
                None => return Err(End::Retry("the connection closed".into())),
                Some(Err(err)) => return Err(End::Retry(format!("could not read: {err}"))),
                Some(Ok(frame)) => frame,
            };
            self.last_frame = Instant::now();
            match frame {
                Frame::Text(text) => match Incoming::parse(&text) {
                    Some(incoming) => return Ok(incoming),
                    None => tracing::debug!(bytes = text.len(), "ignoring a frame that isn't JSON"),
                },
                Frame::Close(_) => {
                    return Err(End::Retry("the server closed the connection".into()));
                }
                _ => {}
            }
        }
    }

    /// Connects, logs in, subscribes to room changes, lists the rooms and
    /// subscribes to each. Room changes that arrive while the list is read
    /// are applied on top of it, since it may predate them.
    async fn setup(&mut self, rest: &RestClient) -> Step {
        self.send(ddp::connect()).await?;
        loop {
            match self.next().await? {
                Incoming::Connected => break,
                Incoming::Failed => {
                    return Err(End::Fatal(SurfaceError::Api(
                        "the server doesn't speak DDP version 1".into(),
                    )));
                }
                other => self.handle(other).await?,
            }
        }
        self.login(rest.credentials()).await?;
        let notify = self.id("sub");
        self.subs.insert(notify.clone(), Sub::Notify);
        let event = self.notify_event.clone();
        self.send(ddp::sub(&notify, NOTIFY_USER, &event)).await?;
        self.await_ready(&notify).await?;
        self.pending = Some(Vec::new());
        let listing = rest.subscriptions();
        tokio::pin!(listing);
        let listed = loop {
            tokio::select! {
                listed = &mut listing => break listed,
                frame = self.next() => {
                    let frame = frame?;
                    self.handle(frame).await?;
                }
            }
        };
        let notices = self.pending.take().unwrap_or_default();
        let listed = match listed {
            Ok(listed) => listed,
            Err(SurfaceError::Unauthorized) => {
                return Err(End::Fatal(SurfaceError::Unauthorized));
            }
            Err(err) => return Err(End::Retry(format!("could not list rooms: {err}"))),
        };
        let mut rooms = HashSet::new();
        for subscription in listed {
            if listens_to(&subscription.room_type) {
                self.docs.insert(subscription.id, subscription.room.clone());
                rooms.insert(subscription.room);
            }
        }
        for notice in notices {
            match self.resolve(notice) {
                Some((Action::Removed, room)) => {
                    rooms.remove(&room);
                }
                Some((_, room)) => {
                    rooms.insert(room);
                }
                None => {}
            }
        }
        for room in rooms {
            self.subscribe_room(room).await?;
        }
        Ok(())
    }

    async fn login(&mut self, creds: &Credentials) -> Step {
        let id = self.id("login");
        self.send(ddp::login(&id, creds.token.expose_secret()))
            .await?;
        let (result, error) = loop {
            match self.next().await? {
                Incoming::Result {
                    id: got,
                    result,
                    error,
                } if got == id => break (result, error),
                other => self.handle(other).await?,
            }
        };
        if let Some(error) = error {
            return Err(match error.code.as_str() {
                "403" => End::Fatal(SurfaceError::Unauthorized),
                _ => End::Retry(format!("login failed: {error}")),
            });
        }
        let logged_in = result
            .as_ref()
            .and_then(|r| r.get("id"))
            .and_then(Value::as_str);
        if logged_in != Some(self.user.as_str()) {
            return Err(End::Fatal(SurfaceError::Api(
                "the realtime login answered for another user".into(),
            )));
        }
        Ok(())
    }

    async fn await_ready(&mut self, id: &str) -> Step {
        loop {
            match self.next().await? {
                Incoming::Ready(subs) if subs.iter().any(|s| s == id) => return Ok(()),
                Incoming::Nosub { id: got, error } if got == id => {
                    let why = error.map_or_else(String::new, |e| format!(": {e}"));
                    return Err(End::Retry(format!("subscription refused{why}")));
                }
                other => self.handle(other).await?,
            }
        }
    }

    /// Serves the connection until it fails or `out` closes.
    async fn serve(mut self, heartbeat: Duration) -> End {
        let mut tick = tokio::time::interval((heartbeat / 2).max(MIN_TICK));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let out = self.out;
        loop {
            let step = tokio::select! {
                frame = self.next() => match frame {
                    Ok(frame) => self.handle(frame).await,
                    Err(end) => Err(end),
                },
                _ = tick.tick() => self.heartbeat(heartbeat).await,
                () = out.closed() => Err(End::Closed),
            };
            if let Err(end) = step {
                return end;
            }
        }
    }

    async fn heartbeat(&mut self, heartbeat: Duration) -> Step {
        let silent = self.last_frame.elapsed();
        if silent >= heartbeat.saturating_mul(2) {
            return Err(End::Retry("the server stopped answering".into()));
        }
        if silent >= heartbeat {
            let id = self.id("ping");
            self.send(ddp::ping(&id)).await?;
        }
        Ok(())
    }

    async fn handle(&mut self, frame: Incoming) -> Step {
        match frame {
            Incoming::Ping(id) => self.send(ddp::pong(id.as_deref())).await,
            Incoming::Nosub { id, error } => match self.subs.remove(&id) {
                Some(Sub::Room(room)) => {
                    self.room_subs.remove(&room);
                    tracing::warn!(
                        user = %self.user,
                        %room,
                        error = error.map(|e| e.code).unwrap_or_default(),
                        "room subscription ended"
                    );
                    self.refused.insert(room);
                    Ok(())
                }
                Some(Sub::Notify) => Err(End::Retry("room-change subscription ended".into())),
                None => Ok(()),
            },
            Incoming::Event {
                stream,
                event,
                args,
            } => {
                if stream == ROOM_MESSAGES {
                    self.room_message(&event, args).await
                } else if stream == NOTIFY_USER && event == self.notify_event {
                    self.subscription_changed(&args).await
                } else {
                    Ok(())
                }
            }
            Incoming::Connected
            | Incoming::Failed
            | Incoming::Result { .. }
            | Incoming::Ready(_)
            | Incoming::Other => Ok(()),
        }
    }

    async fn room_message(&mut self, room: &str, args: Vec<Value>) -> Step {
        if !self.room_subs.contains_key(&ConversationId::from(room)) {
            return Ok(());
        }
        let Some(raw) = args.into_iter().next() else {
            return Ok(());
        };
        match serde_json::from_value::<Message>(raw) {
            Ok(message) => self.out.send(message).await.map_err(|_| End::Closed),
            Err(err) => {
                tracing::debug!(%room, error = ?err.classify(), "ignoring an unreadable message");
                Ok(())
            }
        }
    }

    /// `["inserted" | "updated" | "removed", subscription]`. While the
    /// room list is being read, the notice is kept for `setup` instead.
    async fn subscription_changed(&mut self, args: &[Value]) -> Step {
        let Some(notice) = Notice::parse(args) else {
            return Ok(());
        };
        if let Some(pending) = &mut self.pending {
            pending.push(notice);
            return Ok(());
        }
        match self.resolve(notice) {
            Some((Action::Removed, room)) => {
                self.refused.remove(&room);
                self.unsubscribe_room(&room).await
            }
            Some((Action::Inserted, room)) => {
                self.refused.remove(&room);
                self.unsubscribe_room(&room).await?;
                self.subscribe_room(room).await
            }
            Some((Action::Updated, room)) => self.subscribe_room(room).await,
            None => Ok(()),
        }
    }

    /// The room a notice is about, keeping `docs` up to date.
    /// A removal without `rid` is resolved through its document's `_id`.
    /// `None` for a room whose type isn't listened to, or a removal of an
    /// unknown document.
    fn resolve(&mut self, notice: Notice) -> Option<(Action, ConversationId)> {
        if notice.action == Action::Removed {
            let known = notice.doc.and_then(|doc| self.docs.get(&doc).cloned());
            let Some(room) = notice.room.or(known) else {
                tracing::debug!(user = %self.user, "ignoring the removal of an unknown room");
                return None;
            };
            self.docs.retain(|_, known| *known != room);
            return Some((Action::Removed, room));
        }
        if !notice.listened {
            return None;
        }
        let room = notice.room?;
        if let Some(doc) = notice.doc
            && self.docs.get(&doc) != Some(&room)
        {
            self.docs.retain(|_, known| *known != room);
            self.docs.insert(doc, room.clone());
        }
        Some((notice.action, room))
    }

    /// Subscribes to `room`'s messages, unless it is subscribed already or
    /// was refused on this connection.
    async fn subscribe_room(&mut self, room: ConversationId) -> Step {
        if self.room_subs.contains_key(&room) || self.refused.contains(&room) {
            return Ok(());
        }
        tracing::debug!(user = %self.user, %room, "subscribing to a room");
        let id = self.id("sub");
        self.send(ddp::sub(&id, ROOM_MESSAGES, room.as_str()))
            .await?;
        self.subs.insert(id.clone(), Sub::Room(room.clone()));
        self.room_subs.insert(room, id);
        Ok(())
    }

    async fn unsubscribe_room(&mut self, room: &ConversationId) -> Step {
        let Some(id) = self.room_subs.remove(room) else {
            return Ok(());
        };
        tracing::debug!(user = %self.user, %room, "unsubscribing from a room");
        self.subs.remove(&id);
        self.send(ddp::unsub(&id)).await
    }
}

/// Whether messages in rooms of this type are listened to: channels,
/// private groups and direct messages. Omnichannel and other room types
/// aren't agent conversations.
fn listens_to(room_type: &RoomType) -> bool {
    matches!(
        room_type,
        RoomType::Channel | RoomType::Group | RoomType::Direct
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn websocket_url_maps_schemes_and_keeps_the_path() {
        assert_eq!(
            websocket_url("http://chat.example.com").unwrap(),
            "ws://chat.example.com/websocket"
        );
        assert_eq!(
            websocket_url("https://chat.example.com:8443/rc/?x=1#f").unwrap(),
            "wss://chat.example.com:8443/rc/websocket"
        );
        assert!(websocket_url("ftp://chat.example.com").is_err());
        assert!(websocket_url("not a url").is_err());
    }

    #[test]
    fn realtime_urls_must_be_websockets() {
        let rest = RestClient::new(
            "http://chat.example.com",
            Credentials {
                user_id: "u1".into(),
                token: "t".into(),
            },
        )
        .unwrap();
        let options = RealtimeOptions::default();
        assert!(RealtimeClient::new("ws://h/websocket", rest.clone(), options).is_ok());
        assert!(RealtimeClient::new("wss://h/websocket", rest.clone(), options).is_ok());
        assert!(RealtimeClient::new("http://h/websocket", rest.clone(), options).is_err());
        assert!(RealtimeClient::new("::", rest, options).is_err());
    }

    #[test]
    fn backoff_doubles_up_to_the_cap_with_jitter_and_resets() {
        let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_millis(1000));
        let ceilings = [100, 200, 400, 800, 1000, 1000];
        for ceiling in ceilings {
            let ceiling = Duration::from_millis(ceiling);
            assert_eq!(backoff.ceiling(), ceiling);
            let wait = backoff.next_wait();
            assert!(
                wait >= ceiling / 2 && wait <= ceiling,
                "{wait:?} for {ceiling:?}"
            );
        }
        backoff.reset();
        assert_eq!(backoff.ceiling(), Duration::from_millis(100));
        for _ in 0..100 {
            backoff.next_wait();
        }
        assert_eq!(backoff.ceiling(), Duration::from_millis(1000));
    }

    #[test]
    fn a_cap_below_the_initial_wait_is_raised_to_it() {
        let mut backoff = Backoff::new(Duration::from_millis(50), Duration::from_millis(10));
        assert_eq!(backoff.ceiling(), Duration::from_millis(50));
        backoff.next_wait();
        assert_eq!(backoff.ceiling(), Duration::from_millis(50));
    }

    #[test]
    fn only_conversation_rooms_are_listened_to() {
        assert!(listens_to(&RoomType::Channel));
        assert!(listens_to(&RoomType::Group));
        assert!(listens_to(&RoomType::Direct));
        assert!(!listens_to(&RoomType::Livechat));
        assert!(!listens_to(&RoomType::Other("v".into())));
    }
}

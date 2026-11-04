//! [`FakeDdp`]: a fake Rocket.Chat realtime (DDP) server.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message as Frame;

/// How long the `wait_for_*` methods wait before they panic.
const WAIT: Duration = Duration::from_secs(10);

/// The stream carrying a room's messages.
pub const ROOM_MESSAGES: &str = "stream-room-messages";

/// The stream carrying per-user notices.
pub const NOTIFY_USER: &str = "stream-notify-user";

/// One `login` the fake received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginAttempt {
    /// The connection it arrived on, numbered from 1.
    pub connection: u64,
    /// The `resume` token sent.
    pub token: String,
    /// The user the token belongs to, or `None` when it was refused.
    pub user: Option<String>,
}

#[derive(Debug)]
struct Conn {
    id: u64,
    user: Option<String>,
    subs: HashMap<String, (String, String)>,
    muted: bool,
    out: mpsc::UnboundedSender<Frame>,
}

impl Conn {
    fn send(&self, frame: Value) -> bool {
        self.out.send(Frame::text(frame.to_string())).is_ok()
    }

    fn subscribed(&self, stream: &str, event: &str) -> bool {
        self.subs.values().any(|(s, e)| s == stream && e == event)
    }
}

#[derive(Debug, Default)]
struct State {
    next_conn: u64,
    next_ping: u64,
    tokens: HashMap<String, String>,
    forbidden_rooms: HashSet<String>,
    conns: Vec<Conn>,
    logins: Vec<LoginAttempt>,
    frames: Vec<(u64, Value)>,
    pongs: Vec<String>,
}

impl State {
    fn conn(&mut self, id: u64) -> Option<&mut Conn> {
        self.conns.iter_mut().find(|c| c.id == id)
    }

    fn logged_in(&self, user: &str) -> impl Iterator<Item = &Conn> {
        self.conns
            .iter()
            .filter(move |c| c.user.as_deref() == Some(user))
    }
}

/// A fake Rocket.Chat realtime API: a WebSocket server that speaks enough
/// DDP for `surface-rocketchat`, with frame shapes taken from the Rocket.Chat
/// server source (`ee/apps/ddp-streamer`).
///
/// - `connect` is answered with `connected`, `ping` with `pong`.
/// - `login` with a `resume` token registered with
///   [`add_token`](Self::add_token) succeeds; any other token gets the
///   server's 403 error.
/// - `sub` to `stream-room-messages` for any room, except one marked with
///   [`forbid_room`](Self::forbid_room), and to `stream-notify-user` for the
///   logged-in user's own events, is answered with `ready`; anything else
///   with `nosub` and `not-allowed`. `unsub` is answered with `nosub`.
///
/// A test scripts the server's side with [`send_message`](Self::send_message),
/// [`notify_subscription`](Self::notify_subscription),
/// [`ping`](Self::ping), [`drop_connections`](Self::drop_connections) and
/// [`mute_connections`](Self::mute_connections), and waits for the client with
/// the `wait_for_*` methods, which panic after ten seconds.
pub struct FakeDdp {
    addr: SocketAddr,
    state: Arc<Mutex<State>>,
    changes: watch::Sender<u64>,
    accept: JoinHandle<()>,
}

impl Drop for FakeDdp {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

impl FakeDdp {
    /// Starts the server on a local port.
    ///
    /// # Panics
    ///
    /// If no local port can be bound.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a local port");
        let addr = listener.local_addr().expect("read the local address");
        let state = Arc::new(Mutex::new(State::default()));
        let (changes, _) = watch::channel(0);
        let accept = tokio::spawn(accept_loop(listener, Arc::clone(&state), changes.clone()));
        Self {
            addr,
            state,
            changes,
            accept,
        }
    }

    /// The realtime endpoint, `ws://127.0.0.1:<port>/websocket`.
    pub fn url(&self) -> String {
        format!("ws://{}/websocket", self.addr)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    fn changed(&self) {
        self.changes.send_modify(|n| *n += 1);
    }

    /// Accepts `token` as a `resume` token for `user_id`.
    pub fn add_token(&self, token: &str, user_id: &str) {
        self.state().tokens.insert(token.into(), user_id.into());
    }

    /// Refuses subscriptions to the messages of `room` from now on.
    pub fn forbid_room(&self, room: &str) {
        self.state().forbidden_rooms.insert(room.into());
    }

    /// Every `login` received, oldest first.
    pub fn logins(&self) -> Vec<LoginAttempt> {
        self.state().logins.clone()
    }

    /// Every frame the clients sent, oldest first, with the connection it
    /// arrived on.
    pub fn client_frames(&self) -> Vec<(u64, Value)> {
        self.state().frames.clone()
    }

    /// How many connections are open.
    pub fn connections(&self) -> usize {
        self.state().conns.len()
    }

    /// Whether a connection logged in as `user` has a live subscription to
    /// `event` of `stream`.
    pub fn subscribed(&self, user: &str, stream: &str, event: &str) -> bool {
        self.state()
            .logged_in(user)
            .any(|c| c.subscribed(stream, event))
    }

    async fn wait_for(&self, what: &str, done: impl Fn(&State) -> bool) {
        let mut changes = self.changes.subscribe();
        let wait = async {
            loop {
                if done(&self.state()) {
                    return;
                }
                if changes.changed().await.is_err() {
                    return;
                }
            }
        };
        if tokio::time::timeout(WAIT, wait).await.is_err() {
            panic!("timed out waiting for {what}");
        }
    }

    /// Waits until `user` has logged in successfully `count` times in all.
    pub async fn wait_for_logins(&self, user: &str, count: usize) {
        self.wait_for(&format!("{count} logins of {user}"), |s| {
            s.logins
                .iter()
                .filter(|l| l.user.as_deref() == Some(user))
                .count()
                >= count
        })
        .await;
    }

    /// Waits until some `login` has arrived, successful or not.
    pub async fn wait_for_login_attempt(&self) {
        self.wait_for("a login", |s| !s.logins.is_empty()).await;
    }

    /// Waits until a connection logged in as `user` subscribes to `event` of
    /// `stream`.
    pub async fn wait_for_subscription(&self, user: &str, stream: &str, event: &str) {
        self.wait_for(&format!("{user} to subscribe to {stream} {event}"), |s| {
            s.logged_in(user).any(|c| c.subscribed(stream, event))
        })
        .await;
    }

    /// Waits until `user` subscribes to the messages of `room`.
    pub async fn wait_for_room(&self, user: &str, room: &str) {
        self.wait_for_subscription(user, ROOM_MESSAGES, room).await;
    }

    /// Waits until no connection of `user` is subscribed to the messages of
    /// `room`.
    pub async fn wait_for_room_unsubscribed(&self, user: &str, room: &str) {
        self.wait_for(&format!("{user} to leave room {room}"), |s| {
            !s.logged_in(user).any(|c| c.subscribed(ROOM_MESSAGES, room))
        })
        .await;
    }

    /// Waits until a `pong` with `id` arrives.
    pub async fn wait_for_pong(&self, id: &str) {
        self.wait_for(&format!("pong {id}"), |s| s.pongs.iter().any(|p| p == id))
            .await;
    }

    /// Waits until a client sends a frame that `matches` accepts.
    pub async fn wait_for_frame(&self, what: &str, matches: impl Fn(&Value) -> bool) {
        self.wait_for_frames(what, 1, matches).await;
    }

    /// Waits until clients have sent `count` frames that `matches` accepts.
    pub async fn wait_for_frames(
        &self,
        what: &str,
        count: usize,
        matches: impl Fn(&Value) -> bool,
    ) {
        self.wait_for(what, |s| {
            s.frames.iter().filter(|(_, f)| matches(f)).count() >= count
        })
        .await;
    }

    /// Sends `message` as a `stream-room-messages` event to every
    /// connection subscribed to its room (`rid`), and returns how many
    /// there were.
    pub fn send_message(&self, message: &Value) -> usize {
        self.send_message_where(message, |_| true)
    }

    /// Like [`send_message`](Self::send_message), but only to connections
    /// logged in as `user`.
    pub fn send_message_to(&self, user: &str, message: &Value) -> usize {
        self.send_message_where(message, |c| c.user.as_deref() == Some(user))
    }

    fn send_message_where(&self, message: &Value, to: impl Fn(&Conn) -> bool) -> usize {
        let room = message
            .get("rid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let frame = changed(ROOM_MESSAGES, &room, json!([message]));
        self.state()
            .conns
            .iter()
            .filter(|c| !c.muted && to(c) && c.subscribed(ROOM_MESSAGES, &room))
            .filter(|c| c.send(frame.clone()))
            .count()
    }

    /// Sends a `<user>/subscriptions-changed` notice with `action`
    /// (`inserted`, `updated` or `removed`) and a subscription document to
    /// `user`'s subscribed connections, and returns how many there were.
    pub fn notify_subscription(&self, user: &str, action: &str, subscription: &Value) -> usize {
        let event = format!("{user}/subscriptions-changed");
        let frame = changed(NOTIFY_USER, &event, json!([action, subscription]));
        self.state()
            .logged_in(user)
            .filter(|c| !c.muted && c.subscribed(NOTIFY_USER, &event))
            .filter(|c| c.send(frame.clone()))
            .count()
    }

    /// Sends a `ping` to every connection of `user` and returns its id.
    pub fn ping(&self, user: &str) -> String {
        let mut state = self.state();
        state.next_ping += 1;
        let id = format!("server-ping-{}", state.next_ping);
        for conn in state.logged_in(user) {
            conn.send(json!({ "msg": "ping", "id": id }));
        }
        id
    }

    /// Closes every open connection from the server's side.
    pub fn drop_connections(&self) {
        let conns = std::mem::take(&mut self.state().conns);
        for conn in conns {
            let _ = conn.out.send(Frame::Close(None));
        }
        self.changed();
    }

    /// Makes every open connection go silent: it keeps the socket open but
    /// ignores what the client sends and sends nothing more. Connections
    /// opened later behave normally.
    pub fn mute_connections(&self) {
        for conn in &mut self.state().conns {
            conn.muted = true;
        }
    }
}

/// A new message as `stream-room-messages` carries it: `_id`, `rid`, `msg`,
/// `u` and EJSON `ts` and `_updatedAt` of now. Add `mentions`, `tmid`, `t`,
/// `bot` or `editedAt` to the returned object as a test needs.
pub fn realtime_message(id: &str, room: &str, sender: (&str, &str), text: &str) -> Value {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    json!({
        "_id": id,
        "rid": room,
        "msg": text,
        "ts": { "$date": now },
        "u": { "_id": sender.0, "username": sender.1, "name": sender.1 },
        "_updatedAt": { "$date": now },
        "mentions": [],
        "channels": [],
        "md": [],
    })
}

/// A subscription document as `subscriptions-changed` carries it.
pub fn subscription_doc(user: &str, room: &str, t: &str, name: &str) -> Value {
    json!({
        "_id": format!("{room}{user}"),
        "rid": room,
        "t": t,
        "name": name,
        "fname": name,
        "u": { "_id": user },
        "open": true,
        "alert": false,
        "unread": 0,
    })
}

fn changed(stream: &str, event: &str, args: Value) -> Value {
    json!({
        "msg": "changed",
        "collection": stream,
        "id": "id",
        "fields": { "eventName": event, "args": args },
    })
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

async fn accept_loop(listener: TcpListener, state: Arc<Mutex<State>>, changes: watch::Sender<u64>) {
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(serve(stream, Arc::clone(&state), changes.clone()));
    }
}

async fn serve(stream: TcpStream, state: Arc<Mutex<State>>, changes: watch::Sender<u64>) {
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut sink, mut source) = ws.split();
    let (out, mut outgoing) = mpsc::unbounded_channel();
    let id = {
        let mut state = lock(&state);
        state.next_conn += 1;
        let id = state.next_conn;
        state.conns.push(Conn {
            id,
            user: None,
            subs: HashMap::new(),
            muted: false,
            out: out.clone(),
        });
        id
    };
    let _ = out.send(Frame::text(json!({ "server_id": "0" }).to_string()));
    changes.send_modify(|n| *n += 1);
    loop {
        tokio::select! {
            frame = source.next() => match frame {
                Some(Ok(Frame::Text(text))) => {
                    handle(&state, id, &text);
                    changes.send_modify(|n| *n += 1);
                }
                Some(Ok(Frame::Close(_)) | Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            frame = outgoing.recv() => match frame {
                Some(frame) => {
                    let close = matches!(frame, Frame::Close(_));
                    if sink.send(frame).await.is_err() || close {
                        break;
                    }
                }
                None => break,
            },
        }
    }
    lock(&state).conns.retain(|c| c.id != id);
    changes.send_modify(|n| *n += 1);
}

fn handle(state: &Mutex<State>, id: u64, text: &str) {
    let Ok(frame) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let mut state = lock(state);
    state.frames.push((id, frame.clone()));
    let text_field = |key: &str| frame.get(key).and_then(Value::as_str).map(str::to_owned);
    let call_id = text_field("id").unwrap_or_default();
    match text_field("msg").as_deref() {
        Some("pong") => {
            if let Some(pong) = text_field("id") {
                state.pongs.push(pong);
            }
            return;
        }
        None => return,
        _ => {}
    }
    let tokens = state.tokens.clone();
    let forbidden = state.forbidden_rooms.clone();
    let Some(conn) = state.conn(id) else {
        return;
    };
    if conn.muted {
        return;
    }
    let reply = match text_field("msg").as_deref() {
        Some("connect") => json!({ "msg": "connected", "session": format!("session-{id}") }),
        Some("ping") => match text_field("id") {
            Some(ping) => json!({ "msg": "pong", "id": ping }),
            None => json!({ "msg": "pong" }),
        },
        Some("method") if text_field("method").as_deref() == Some("login") => {
            let token = frame
                .pointer("/params/0/resume")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let user = tokens.get(&token).cloned();
            conn.user.clone_from(&user);
            let reply = match &user {
                Some(user) => json!({
                    "msg": "result",
                    "id": call_id,
                    "result": {
                        "id": user,
                        "token": token,
                        "tokenExpires": { "$date": 4_102_444_800_000_u64 },
                        "type": "resume",
                    },
                }),
                None => json!({
                    "msg": "result",
                    "id": call_id,
                    "error": {
                        "isClientSafe": true,
                        "error": 403,
                        "reason": "You've been logged out by the server. Please log in again.",
                        "message": "You've been logged out by the server. Please log in again. [403]",
                        "errorType": "Meteor.Error",
                    },
                }),
            };
            conn.send(reply);
            if user.is_some() {
                conn.send(json!({ "msg": "updated", "methods": [call_id] }));
            }
            state.logins.push(LoginAttempt {
                connection: id,
                token,
                user,
            });
            return;
        }
        Some("method") => json!({
            "msg": "result",
            "id": call_id,
            "error": {
                "isClientSafe": true,
                "error": 404,
                "reason": "Method not found",
                "message": "Method not found [404]",
                "errorType": "Meteor.Error",
            },
        }),
        Some("sub") => {
            let name = text_field("name").unwrap_or_default();
            let event = frame
                .pointer("/params/0")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let allowed = match (name.as_str(), &conn.user) {
                (ROOM_MESSAGES, Some(_)) => !forbidden.contains(&event),
                (NOTIFY_USER, Some(user)) => event.split('/').next() == Some(user.as_str()),
                _ => false,
            };
            if allowed {
                conn.subs.insert(call_id.clone(), (name, event));
                json!({ "msg": "ready", "subs": [call_id] })
            } else {
                json!({
                    "msg": "nosub",
                    "id": call_id,
                    "error": {
                        "isClientSafe": true,
                        "error": "not-allowed",
                        "reason": "Not allowed",
                        "message": "Not allowed [not-allowed]",
                        "errorType": "Meteor.Error",
                    },
                })
            }
        }
        Some("unsub") => {
            conn.subs.remove(&call_id);
            json!({ "msg": "nosub", "id": call_id })
        }
        _ => return,
    };
    conn.send(reply);
}

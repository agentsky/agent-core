//! A fake Rocket.Chat server for tests.
//!
//! [`FakeDdp`] is the realtime side: a WebSocket server speaking DDP.
//!
//! [`FakeRest`] answers the REST endpoints `surface-rocketchat` uses, from
//! wiremock, with shapes taken from the Rocket.Chat server source. It keeps
//! users, tokens, rooms and messages in memory, so a test can create a bot,
//! log in as it, post as it and read the result back. Failures are injected
//! with [`FakeRest::fail`] and [`FakeRest::rate_limit`].

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use wiremock::matchers::path_regex;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

mod ddp;

pub use ddp::{
    FakeDdp, LoginAttempt, NOTIFY_USER, ROOM_MESSAGES, realtime_message, subscription_doc,
};

/// A user the fake knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeUser {
    /// The user's `_id`.
    pub id: String,
    /// The username.
    pub username: String,
    /// The display name.
    pub name: String,
    /// Global roles.
    pub roles: Vec<String>,
    /// Whether the account is active. Tokens of inactive users are refused.
    pub active: bool,
    /// The avatar URL last set with `users.setAvatar`.
    pub avatar_url: Option<String>,
    /// Whether `users.create` was asked for a verified email.
    pub verified: bool,
    /// The email `users.create` was given.
    pub email: Option<String>,
    password: Option<String>,
}

/// A message the fake stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeMessage {
    /// The message's `_id`.
    pub id: String,
    /// The room.
    pub rid: String,
    /// The text.
    pub text: String,
    /// The thread root, for a reply.
    pub tmid: Option<String>,
    /// The sender's `_id`.
    pub user_id: String,
    /// Whether it was edited.
    pub edited: bool,
    /// `(emoji, user id)` pairs, emoji with colons.
    pub reactions: Vec<(String, String)>,
    /// The attached file's name, for an upload.
    pub file_name: Option<String>,
    ts: u64,
}

#[derive(Debug, Clone)]
struct FakeRoom {
    t: String,
    name: Option<String>,
    members: BTreeSet<String>,
}

#[derive(Debug, Default)]
struct State {
    seq: u64,
    users: BTreeMap<String, FakeUser>,
    tokens: HashMap<String, String>,
    token_names: BTreeSet<(String, String)>,
    rooms: BTreeMap<String, FakeRoom>,
    messages: Vec<FakeMessage>,
    uploads: HashMap<String, (String, String, String)>,
    files: HashMap<String, (String, Vec<u8>)>,
}

impl State {
    fn next(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn user_by_name(&self, username: &str) -> Option<&FakeUser> {
        self.users.values().find(|u| u.username == username)
    }

    fn message_index(&self, id: &str) -> Option<usize> {
        self.messages.iter().position(|m| m.id == id)
    }

    fn user_json(&self, id: &str) -> Value {
        self.users.get(id).map_or(Value::Null, |u| {
            let mut user = json!({
                "_id": u.id,
                "username": u.username,
                "name": u.name,
                "roles": u.roles,
                "active": u.active,
                "type": "user",
            });
            if let Some(email) = &u.email {
                user["emails"] = json!([{ "address": email, "verified": u.verified }]);
            }
            user
        })
    }

    fn room_json(&self, id: &str) -> Value {
        self.rooms.get(id).map_or(Value::Null, |r| {
            let mut room = json!({
                "_id": id,
                "t": r.t,
                "usersCount": r.members.len(),
            });
            if let Some(name) = &r.name {
                room["name"] = json!(name);
                room["fname"] = json!(name);
            }
            if r.t == "d" {
                room["uids"] = json!(r.members);
            }
            room
        })
    }

    fn message_json(&self, message: &FakeMessage) -> Value {
        let sender = self.users.get(&message.user_id);
        let mut value = json!({
            "_id": message.id,
            "rid": message.rid,
            "msg": message.text,
            "ts": iso(message.ts),
            "u": {
                "_id": message.user_id,
                "username": sender.map(|u| u.username.as_str()),
                "name": sender.map(|u| u.name.as_str()),
            },
            "_updatedAt": iso(message.ts),
        });
        if let Some(tmid) = &message.tmid {
            value["tmid"] = json!(tmid);
        }
        if message.edited {
            value["editedAt"] = json!(iso(message.ts));
        }
        if let Some(name) = &message.file_name {
            let file = json!({ "_id": format!("file-of-{}", message.id), "name": name, "type": "application/octet-stream" });
            value["file"] = file.clone();
            value["files"] = json!([file]);
        }
        value
    }
}

/// A fake Rocket.Chat REST API on a local wiremock server.
///
/// It starts with one user, the manager ([`FakeRest::MANAGER_ID`],
/// authenticated by [`FakeRest::MANAGER_TOKEN`]), and no rooms.
/// `users.info` includes `roles` and `emails` only for the caller itself
/// and for the manager, which the fake treats as holding
/// `view-full-other-user-info`.
pub struct FakeRest {
    server: MockServer,
    state: Arc<Mutex<State>>,
}

impl FakeRest {
    /// The manager's user id.
    pub const MANAGER_ID: &'static str = "manager-id";
    /// The manager's username.
    pub const MANAGER_USERNAME: &'static str = "agentd";
    /// The manager's personal access token.
    pub const MANAGER_TOKEN: &'static str = "manager-token";

    /// Starts the server.
    pub async fn start() -> Self {
        let mut state = State::default();
        state.users.insert(
            Self::MANAGER_ID.into(),
            FakeUser {
                id: Self::MANAGER_ID.into(),
                username: Self::MANAGER_USERNAME.into(),
                name: "agentd".into(),
                roles: vec!["user".into(), "agentd-manager".into()],
                active: true,
                avatar_url: None,
                verified: false,
                email: None,
                password: None,
            },
        );
        state
            .tokens
            .insert(Self::MANAGER_TOKEN.into(), Self::MANAGER_ID.into());
        let state = Arc::new(Mutex::new(state));
        let server = MockServer::start().await;
        Mock::given(path_regex("^/api/v1/"))
            .respond_with(Router(Arc::clone(&state)))
            .mount(&server)
            .await;
        Mock::given(path_regex("^/file-upload/"))
            .respond_with(Files(Arc::clone(&state)))
            .mount(&server)
            .await;
        Self { server, state }
    }

    /// The server's base URL.
    pub fn uri(&self) -> String {
        self.server.uri()
    }

    /// The underlying wiremock server, for custom routes.
    pub fn server(&self) -> &MockServer {
        &self.server
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Adds an active user with the `user` role and returns its id.
    pub fn add_user(&self, username: &str) -> String {
        let mut state = self.state();
        let id = format!("user-{}", state.next());
        state.users.insert(
            id.clone(),
            FakeUser {
                id: id.clone(),
                username: username.into(),
                name: username.into(),
                roles: vec!["user".into()],
                active: true,
                avatar_url: None,
                verified: false,
                email: None,
                password: None,
            },
        );
        id
    }

    /// Adds a room of type `t` (`c`, `p` or `d`) with the manager as its
    /// only member.
    pub fn add_room(&self, id: &str, t: &str, name: &str) {
        let room = FakeRoom {
            t: t.into(),
            name: (t != "d").then(|| name.to_owned()),
            members: BTreeSet::from([Self::MANAGER_ID.to_owned()]),
        };
        self.state().rooms.insert(id.into(), room);
    }

    /// Adds a member to a room.
    pub fn add_member(&self, room: &str, user_id: &str) {
        if let Some(room) = self.state().rooms.get_mut(room) {
            room.members.insert(user_id.into());
        }
    }

    /// Removes a member from a room, the manager included.
    pub fn remove_member(&self, room: &str, user_id: &str) {
        if let Some(room) = self.state().rooms.get_mut(room) {
            room.members.remove(user_id);
        }
    }

    /// Stores a message as if `user_id` had posted it, and returns its id.
    pub fn seed_message(
        &self,
        room: &str,
        user_id: &str,
        text: &str,
        tmid: Option<&str>,
    ) -> String {
        let mut state = self.state();
        store_message(&mut state, room, user_id, text, tmid, None)
    }

    /// Stores a file named `name` holding `content`, as if it had been
    /// uploaded, and returns its id. It is served at
    /// `/file-upload/<id>/<name>` to any active user's credentials, sent as
    /// `X-User-Id` and `X-Auth-Token` headers.
    pub fn add_file(&self, name: &str, content: &[u8]) -> String {
        let mut state = self.state();
        let id = format!("file-{}", state.next());
        state
            .files
            .insert(id.clone(), (name.to_owned(), content.to_vec()));
        id
    }

    /// A user by username.
    pub fn user(&self, username: &str) -> Option<FakeUser> {
        self.state().user_by_name(username).cloned()
    }

    /// A message by id.
    pub fn message(&self, id: &str) -> Option<FakeMessage> {
        let state = self.state();
        state.message_index(id).map(|at| state.messages[at].clone())
    }

    /// A room's member ids.
    pub fn members(&self, room: &str) -> Vec<String> {
        self.state()
            .rooms
            .get(room)
            .map(|r| r.members.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// The user a token authenticates, if it is still valid.
    pub fn token_user(&self, token: &str) -> Option<String> {
        self.state().tokens.get(token).cloned()
    }

    /// Makes every call to `endpoint` (such as `chat.postMessage`, or
    /// `rooms.media` for any room) fail with `status` and the body
    /// `{"success": false, "error": error, "errorType": error_type}`.
    pub async fn fail(&self, endpoint: &str, status: u16, error: &str, error_type: Option<&str>) {
        let mut body = json!({ "success": false, "error": error });
        if let Some(error_type) = error_type {
            body["errorType"] = json!(error_type);
        }
        Mock::given(path_regex(endpoint_pattern(endpoint)))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .with_priority(1)
            .mount(&self.server)
            .await;
    }

    /// Makes the next `times` calls to `endpoint` fail with HTTP 429, as
    /// Rocket.Chat's rate limiter does, resetting `reset_in` from now.
    pub async fn rate_limit(&self, endpoint: &str, times: u64, reset_in: Duration) {
        self.rate_limit_at(endpoint, times, reset_in, SystemTime::now())
            .await;
    }

    /// Like [`FakeRest::rate_limit`], for a server whose clock reads
    /// `server_now`, to test clock skew.
    ///
    /// The response's `Date` header is `server_now` in whole seconds, as a
    /// real server sends it, and `X-RateLimit-Reset` is that second plus
    /// `reset_in`, in milliseconds since the Unix epoch, so a client that
    /// measures the reset against `Date` waits exactly `reset_in`.
    pub async fn rate_limit_at(
        &self,
        endpoint: &str,
        times: u64,
        reset_in: Duration,
        server_now: SystemTime,
    ) {
        let now_s = server_now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let reset = u128::from(now_s) * 1000 + reset_in.as_millis();
        let seconds = reset_in.as_secs().max(1);
        let body = json!({
            "success": false,
            "error": format!("Error, too many requests. Please slow down. You must wait {seconds} seconds before trying this endpoint again. [error-too-many-requests]"),
        });
        let response = ResponseTemplate::new(429)
            .insert_header("Date", http_date(now_s))
            .insert_header("X-RateLimit-Limit", "10")
            .insert_header("X-RateLimit-Remaining", "0")
            .insert_header("X-RateLimit-Reset", reset.to_string())
            .set_body_json(body);
        Mock::given(path_regex(endpoint_pattern(endpoint)))
            .respond_with(response)
            .up_to_n_times(times)
            .with_priority(1)
            .mount(&self.server)
            .await;
    }

    /// The requests received for `endpoint`, oldest first.
    pub async fn requests(&self, endpoint: &str) -> Vec<Request> {
        let pattern = format!("/api/v1/{endpoint}");
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|r| {
                let path = r.url.path();
                path == pattern || path.starts_with(&format!("{pattern}/"))
            })
            .collect()
    }
}

/// The `rooms.mediaConfirm` body keys Rocket.Chat 7.x accepts: it passes the
/// body, less `description`, to a strict `check` in `sendFileMessage`.
const CONFIRM_KEYS: &[&str] = &[
    "alias",
    "avatar",
    "content",
    "customFields",
    "description",
    "emoji",
    "groupable",
    "msg",
    "t",
    "tmid",
];

fn unknown_key<'a>(body: &'a Value, allowed: &[&str]) -> Option<&'a str> {
    body.as_object()?
        .keys()
        .map(String::as_str)
        .find(|k| !allowed.contains(k))
}

/// Formats Unix seconds as an HTTP `Date` header (IMF-fixdate), such as
/// `Sun, 06 Nov 1994 08:49:37 GMT`.
fn http_date(unix_seconds: u64) -> String {
    let at = i64::try_from(unix_seconds)
        .ok()
        .and_then(|s| time::OffsetDateTime::from_unix_timestamp(s).ok())
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    at.format(time::macros::format_description!(
        "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
    ))
    .unwrap_or_default()
}

/// A regex matching `endpoint`, alone or followed by path parameters.
fn endpoint_pattern(endpoint: &str) -> String {
    format!("^/api/v1/{}(/.*)?$", endpoint.replace('.', r"\."))
}

/// A time for message number `seq`: seconds after 2026-09-30T00:00:00Z, in
/// JavaScript's `toISOString` form.
fn iso(seq: u64) -> String {
    let (h, m, s) = ((seq / 3600) % 24, (seq / 60) % 60, seq % 60);
    format!("2026-09-30T{h:02}:{m:02}:{s:02}.000Z")
}

fn store_message(
    state: &mut State,
    room: &str,
    user_id: &str,
    text: &str,
    tmid: Option<&str>,
    file_name: Option<String>,
) -> String {
    let seq = state.next();
    let id = format!("msg-{seq}");
    state.messages.push(FakeMessage {
        id: id.clone(),
        rid: room.into(),
        text: text.into(),
        tmid: tmid.map(str::to_owned),
        user_id: user_id.into(),
        edited: false,
        reactions: Vec::new(),
        file_name,
        ts: seq,
    });
    id
}

fn ok(mut body: Value) -> ResponseTemplate {
    body["success"] = json!(true);
    ResponseTemplate::new(200).set_body_json(body)
}

fn failure(error: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({ "success": false, "error": error }))
}

/// A Meteor error as the REST layer reports it.
fn meteor_error(code: &str, reason: &str) -> ResponseTemplate {
    ResponseTemplate::new(400).set_body_json(json!({
        "success": false,
        "error": format!("{reason} [{code}]"),
        "errorType": code,
    }))
}

fn unauthorized() -> ResponseTemplate {
    ResponseTemplate::new(401).set_body_json(json!({
        "success": false,
        "status": "error",
        "message": "You must be logged in to do this.",
    }))
}

fn not_found() -> ResponseTemplate {
    ResponseTemplate::new(404)
        .set_body_json(json!({ "success": false, "error": "Resource not found" }))
}

struct Router(Arc<Mutex<State>>);

impl Respond for Router {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let path = request.url.path().trim_start_matches("/api/v1/").to_owned();
        let segments: Vec<&str> = path.split('/').collect();
        let body: Value = request.body_json().unwrap_or(Value::Null);
        let query: HashMap<String, String> = request.url.query_pairs().into_owned().collect();
        let post = request.method.as_str() == "POST";
        if segments == ["login"] && post {
            return login(&mut state, &body);
        }
        let Some(caller) = authenticate(&state, request) else {
            return unauthorized();
        };
        let text = |key: &str| {
            body.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let param = |key: &str| query.get(key).cloned().unwrap_or_default();
        match (post, segments.as_slice()) {
            (false, ["me"]) => ok(state.user_json(&caller)),
            (false, ["users.info"]) => {
                let id = match query.get("username") {
                    Some(username) => state
                        .user_by_name(username)
                        .map(|u| u.id.clone())
                        .unwrap_or_default(),
                    None => param("userId"),
                };
                if !state.users.contains_key(&id) {
                    return failure("User not found.");
                }
                let mut user = state.user_json(&id);
                if caller != id
                    && caller != FakeRest::MANAGER_ID
                    && let Some(user) = user.as_object_mut()
                {
                    user.remove("roles");
                    user.remove("emails");
                }
                ok(json!({ "user": user }))
            }
            (false, ["subscriptions.get"]) => {
                let update: Vec<Value> = state
                    .rooms
                    .iter()
                    .filter(|(_, room)| room.members.contains(&caller))
                    .map(|(id, room)| {
                        json!({
                            "_id": format!("{id}{caller}"),
                            "rid": id,
                            "t": room.t,
                            "name": room.name.clone().unwrap_or_default(),
                            "u": { "_id": caller },
                            "open": true,
                        })
                    })
                    .collect();
                ok(json!({ "update": update, "remove": [] }))
            }
            (false, ["subscriptions.getOne"]) => {
                let id = param("roomId");
                if id.is_empty() {
                    return failure("must have required property 'roomId'");
                }
                let subscription = state
                    .rooms
                    .get(&id)
                    .filter(|room| room.members.contains(&caller))
                    .map_or(Value::Null, |room| {
                        json!({
                            "_id": format!("{id}{caller}"),
                            "rid": id,
                            "t": room.t,
                            "name": room.name.clone().unwrap_or_default(),
                            "u": { "_id": caller },
                            "open": true,
                        })
                    });
                ok(json!({ "subscription": subscription }))
            }
            (true, ["logout"]) => {
                let token = header(request, "x-auth-token");
                state.tokens.remove(&token);
                ok(json!({ "status": "success", "data": { "message": "You've been logged out!" } }))
            }
            (true, ["users.create"]) => create_user(&mut state, &body),
            (true, ["users.generatePersonalAccessToken"]) => {
                let name = text("tokenName");
                if !state.token_names.insert((caller.clone(), name)) {
                    return meteor_error(
                        "error-token-already-exists",
                        "A token with this name already exists",
                    );
                }
                let token = format!("pat-{}", state.next());
                state.tokens.insert(token.clone(), caller);
                ok(json!({ "token": token }))
            }
            (true, ["users.setAvatar"]) => match state.users.get_mut(&text("userId")) {
                Some(user) => {
                    user.avatar_url = Some(text("avatarUrl"));
                    ok(json!({}))
                }
                None => meteor_error("error-invalid-user", "Invalid user"),
            },
            (true, ["users.update"]) => {
                let id = text("userId");
                let name = body
                    .pointer("/data/name")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                match (state.users.get_mut(&id), name) {
                    (Some(user), Some(name)) => {
                        user.name = name;
                        let user = state.user_json(&id);
                        ok(json!({ "user": user }))
                    }
                    (None, _) => meteor_error("error-user-not-found", "User not found"),
                    (Some(_), None) => failure("must have required property 'name'"),
                }
            }
            (true, ["users.setActiveStatus"]) => {
                let id = text("userId");
                let active = body.get("activeStatus").and_then(Value::as_bool);
                match (state.users.get_mut(&id), active) {
                    (Some(user), Some(active)) => {
                        user.active = active;
                        ok(json!({ "user": { "_id": id, "active": active } }))
                    }
                    (None, _) => meteor_error("error-invalid-user", "Invalid user"),
                    (Some(_), None) => failure("must have required property 'activeStatus'"),
                }
            }
            (true, [endpoint @ ("channels.invite" | "groups.invite")]) => {
                let (t, key) = if *endpoint == "channels.invite" {
                    ("c", "channel")
                } else {
                    ("p", "group")
                };
                invite(&mut state, &text("roomId"), &text("userId"), t, key)
            }
            (false, ["rooms.info"]) if param("roomId").is_empty() => {
                let name = param("roomName");
                let found = state
                    .rooms
                    .iter()
                    .find(|(_, room)| room.name.as_deref() == Some(name.as_str()));
                match found {
                    Some((id, room)) if room.members.contains(&caller) || room.t == "c" => {
                        ok(json!({ "room": state.room_json(id) }))
                    }
                    Some(_) => failure("not-allowed"),
                    None => meteor_error(
                        "error-room-not-found",
                        "The required \"roomId\" or \"roomName\" param provided does not match any channel",
                    ),
                }
            }
            (false, ["rooms.info"]) => match state.rooms.get(&param("roomId")) {
                Some(room) if room.members.contains(&caller) || room.t == "c" => {
                    ok(json!({ "room": state.room_json(&param("roomId")) }))
                }
                Some(_) => failure("not-allowed"),
                None => meteor_error(
                    "error-room-not-found",
                    "The required \"roomId\" or \"roomName\" param provided does not match any channel",
                ),
            },
            (true, ["im.create"]) => {
                let target = state
                    .user_by_name(&text("username"))
                    .map_or_else(|| caller.clone(), |u| u.id.clone());
                let mut ids = [caller, target];
                ids.sort();
                let id = ids.concat();
                let room = state.rooms.entry(id.clone()).or_insert_with(|| FakeRoom {
                    t: "d".into(),
                    name: None,
                    members: BTreeSet::new(),
                });
                room.members.extend(ids);
                let usernames: Vec<&str> = state.rooms[&id]
                    .members
                    .iter()
                    .filter_map(|member| state.users.get(member))
                    .map(|user| user.username.as_str())
                    .collect();
                let mut room = state.room_json(&id);
                room["rid"] = json!(id);
                room["usernames"] = json!(usernames);
                ok(json!({ "room": room }))
            }
            (true, ["chat.postMessage"]) => post_message(&mut state, &caller, &body),
            (true, ["chat.update"]) => {
                let Some(at) = state.message_index(&text("msgId")) else {
                    return failure(&format!(
                        "No message found with the id of \"{}\".",
                        text("msgId")
                    ));
                };
                if state.messages[at].rid != text("roomId") {
                    return failure(
                        "The room id provided does not match where the message is from.",
                    );
                }
                if state.messages[at].user_id != caller {
                    return meteor_error("error-action-not-allowed", "Message editing not allowed");
                }
                state.messages[at].text = text("text");
                state.messages[at].edited = true;
                let message = state.message_json(&state.messages[at]);
                ok(json!({ "message": message }))
            }
            (true, ["chat.react"]) => {
                let Some(at) = state.message_index(&text("messageId")) else {
                    return meteor_error(
                        "error-message-not-found",
                        "The provided \"messageId\" does not match any existing message.",
                    );
                };
                let emoji = format!(":{}:", text("emoji").replace(':', ""));
                let reaction = (emoji, caller);
                let reactions = &mut state.messages[at].reactions;
                let present = reactions.contains(&reaction);
                let should = body
                    .get("shouldReact")
                    .and_then(Value::as_bool)
                    .unwrap_or(!present);
                if should && !present {
                    reactions.push(reaction);
                } else if !should {
                    reactions.retain(|r| *r != reaction);
                }
                ok(json!({}))
            }
            (false, ["chat.getMessage"]) => match state.message_index(&param("msgId")) {
                Some(at) => {
                    let message = state.message_json(&state.messages[at]);
                    ok(json!({ "message": message }))
                }
                None => ResponseTemplate::new(400).set_body_json(json!({ "success": false })),
            },
            (true, ["rooms.media", rid]) => {
                if !state
                    .rooms
                    .get(*rid)
                    .is_some_and(|r| r.members.contains(&caller))
                {
                    return ResponseTemplate::new(403)
                        .set_body_json(json!({ "success": false, "error": "unauthorized" }));
                }
                let Some(name) = multipart_file_name(&request.body) else {
                    return meteor_error("error-no-file-uploaded", "No file was uploaded");
                };
                let id = format!("file-{}", state.next());
                state
                    .uploads
                    .insert(id.clone(), ((*rid).into(), caller, name.clone()));
                ok(json!({ "file": { "_id": id, "url": format!("/file-upload/{id}/{name}") } }))
            }
            (true, ["rooms.mediaConfirm", rid, file]) => {
                let Some((room, owner, name)) = state.uploads.get(*file).cloned() else {
                    return meteor_error("invalid-file", "invalid-file");
                };
                if room != *rid || owner != caller {
                    return meteor_error("invalid-file", "invalid-file");
                }
                if let Some(key) = unknown_key(&body, CONFIRM_KEYS) {
                    return failure(&format!("Match error: Unknown key in field {key}"));
                }
                let tmid = body.get("tmid").and_then(Value::as_str);
                let id = store_message(&mut state, rid, &caller, &text("msg"), tmid, Some(name));
                state.uploads.remove(*file);
                let at = state.message_index(&id).unwrap_or_default();
                let message = state.message_json(&state.messages[at]);
                ok(json!({ "message": message }))
            }
            (false, [endpoint @ ("channels.history" | "groups.history" | "im.history")]) => {
                let t = match *endpoint {
                    "channels.history" => "c",
                    "groups.history" => "p",
                    _ => "d",
                };
                history(&state, &caller, t, &query)
            }
            (false, ["chat.getThreadMessages"]) => thread_messages(&state, &query),
            _ => not_found(),
        }
    }
}

/// Serves the files stored with [`FakeRest::add_file`].
struct Files(Arc<Mutex<State>>);

impl Respond for Files {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if authenticate(&state, request).is_none() {
            return ResponseTemplate::new(403).set_body_string("Forbidden");
        }
        let path = request.url.path().trim_start_matches("/file-upload/");
        let file = path.split_once('/').and_then(|(id, name)| {
            state
                .files
                .get(id)
                .filter(|(stored, _)| stored == name)
                .map(|(_, content)| content.clone())
        });
        match file {
            Some(content) => ResponseTemplate::new(200).set_body_bytes(content),
            None => ResponseTemplate::new(404).set_body_string("Not found"),
        }
    }
}

fn header(request: &Request, name: &str) -> String {
    request
        .headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// The caller's id, if `X-User-Id` and `X-Auth-Token` match an active user.
fn authenticate(state: &State, request: &Request) -> Option<String> {
    let user_id = header(request, "x-user-id");
    let owner = state.tokens.get(&header(request, "x-auth-token"))?;
    let active = state.users.get(owner).is_some_and(|u| u.active);
    (*owner == user_id && active).then_some(user_id)
}

fn login(state: &mut State, body: &Value) -> ResponseTemplate {
    let username = body.get("user").and_then(Value::as_str).unwrap_or_default();
    let password = body.get("password").and_then(Value::as_str);
    let user = state
        .user_by_name(username)
        .filter(|u| u.active && u.password.is_some() && u.password.as_deref() == password)
        .map(|u| u.id.clone());
    let Some(id) = user else {
        return ResponseTemplate::new(401).set_body_json(json!({
            "success": false,
            "status": "error",
            "error": "Unauthorized",
            "message": "Unauthorized",
        }));
    };
    let token = format!("login-{}", state.next());
    state.tokens.insert(token.clone(), id.clone());
    let me = state.user_json(&id);
    ok(json!({ "status": "success", "data": { "userId": id, "authToken": token, "me": me } }))
}

fn create_user(state: &mut State, body: &Value) -> ResponseTemplate {
    let field = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_owned);
    let (Some(username), Some(name), Some(email), Some(password)) = (
        field("username"),
        field("name"),
        field("email"),
        field("password"),
    ) else {
        return failure(
            "must have required property 'email', 'name', 'password' and 'username' [invalid-params]",
        );
    };
    if state.user_by_name(&username).is_some() {
        return meteor_error(
            "error-field-unavailable",
            &format!("{username} is already in use :("),
        );
    }
    let roles = body
        .get("roles")
        .and_then(Value::as_array)
        .map(|r| {
            r.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| vec!["user".to_owned()]);
    let id = format!("user-{}", state.next());
    state.users.insert(
        id.clone(),
        FakeUser {
            id: id.clone(),
            username,
            name,
            roles,
            active: true,
            avatar_url: None,
            verified: body
                .get("verified")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            email: Some(email),
            password: Some(password),
        },
    );
    let user = state.user_json(&id);
    ok(json!({ "user": user }))
}

fn invite(state: &mut State, room: &str, user: &str, t: &str, key: &str) -> ResponseTemplate {
    if !state.users.contains_key(user) {
        return meteor_error("error-invalid-user", "Invalid user");
    }
    match state.rooms.get_mut(room) {
        Some(found) if found.t == t => {
            found.members.insert(user.into());
            let mut body = json!({});
            body[key] = state.room_json(room);
            ok(body)
        }
        _ => meteor_error(
            "error-room-not-found",
            "The required \"roomId\" or \"roomName\" param provided does not match any group",
        ),
    }
}

fn post_message(state: &mut State, caller: &str, body: &Value) -> ResponseTemplate {
    let rid = body
        .get("roomId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let text = body
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let tmid = body.get("tmid").and_then(Value::as_str);
    let Some(room) = state.rooms.get_mut(&rid) else {
        return meteor_error("invalid-channel", "invalid-channel");
    };
    if room.t == "c" {
        room.members.insert(caller.into());
    } else if !room.members.contains(caller) {
        return meteor_error("error-not-allowed", "Not allowed");
    }
    if let Some(tmid) = tmid
        && !state.messages.iter().any(|m| m.id == tmid && m.rid == rid)
    {
        return meteor_error("error-invalid-message", "Invalid message");
    }
    let id = store_message(state, &rid, caller, &text, tmid, None);
    let at = state.message_index(&id).unwrap_or_default();
    let message = state.message_json(&state.messages[at]);
    ok(json!({ "ts": 1_790_000_000_000_u64, "channel": rid, "message": message }))
}

fn count(query: &HashMap<String, String>) -> usize {
    query
        .get("count")
        .and_then(|c| c.parse().ok())
        .unwrap_or(20_usize)
        .min(100)
}

fn history(
    state: &State,
    caller: &str,
    t: &str,
    query: &HashMap<String, String>,
) -> ResponseTemplate {
    let rid = query.get("roomId").cloned().unwrap_or_default();
    let Some(room) = state.rooms.get(&rid).filter(|r| r.t == t) else {
        return meteor_error(
            "error-room-not-found",
            "The required \"roomId\" or \"roomName\" param provided does not match any channel",
        );
    };
    if t != "c" && !room.members.contains(caller) {
        return ResponseTemplate::new(403)
            .set_body_json(json!({ "success": false, "error": "unauthorized" }));
    }
    let latest = query.get("latest").cloned();
    let threads = query.get("showThreadMessages").is_some_and(|v| v == "true");
    let mut messages: Vec<&FakeMessage> = state
        .messages
        .iter()
        .filter(|m| m.rid == rid && (threads || m.tmid.is_none()))
        .filter(|m| latest.as_ref().is_none_or(|l| iso(m.ts) < *l))
        .collect();
    messages.sort_by_key(|m| std::cmp::Reverse(m.ts));
    let messages: Vec<Value> = messages
        .into_iter()
        .take(count(query))
        .map(|m| state.message_json(m))
        .collect();
    ok(json!({ "messages": messages }))
}

fn thread_messages(state: &State, query: &HashMap<String, String>) -> ResponseTemplate {
    let tmid = query.get("tmid").cloned().unwrap_or_default();
    if state.message_index(&tmid).is_none() {
        return meteor_error("error-invalid-message", "Invalid Message");
    }
    let offset: usize = query
        .get("offset")
        .and_then(|o| o.parse().ok())
        .unwrap_or(0);
    let newest_first = query
        .get("sort")
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .and_then(|s| s.get("ts").and_then(Value::as_i64))
        == Some(-1);
    let mut messages: Vec<&FakeMessage> = state
        .messages
        .iter()
        .filter(|m| m.tmid.as_deref() == Some(tmid.as_str()))
        .collect();
    let total = messages.len();
    messages.sort_by_key(|m| m.ts);
    if newest_first {
        messages.reverse();
    }
    let messages: Vec<Value> = messages
        .into_iter()
        .skip(offset)
        .take(count(query))
        .map(|m| state.message_json(m))
        .collect();
    ok(json!({ "messages": messages, "count": messages.len(), "offset": offset, "total": total }))
}

/// The `filename` of the multipart part named `file`.
fn multipart_file_name(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let disposition = text.lines().find(|l| {
        l.to_ascii_lowercase().starts_with("content-disposition") && l.contains("name=\"file\"")
    })?;
    let start = disposition.find("filename=\"")? + "filename=\"".len();
    let end = disposition[start..].find('"')? + start;
    Some(disposition[start..end].to_owned())
}

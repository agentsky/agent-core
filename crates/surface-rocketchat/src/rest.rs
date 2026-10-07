//! A client for the Rocket.Chat REST API (`/api/v1`).
//!
//! Every call is authenticated with `X-User-Id` and `X-Auth-Token`: the
//! manager's personal access token, or a bot's. [`RestClient::with_credentials`]
//! switches identity while sharing the connection pool.
//!
//! Errors map to [`SurfaceError`]. Rocket.Chat answers most failures with HTTP
//! 400 and a body `{"success": false, "error": "…", "errorType": "…"}`; see
//! [`map_error`] for the rules. A 429 is retried once, after the time the
//! `x-ratelimit-reset` header names, measured against the response's own
//! `Date` header, when that is within [`RestClient::with_max_retry_wait`].
//!
//! Endpoint shapes follow the Rocket.Chat server source
//! (`apps/meteor/server/api/v1/*.ts` on `develop`, and
//! `apps/meteor/app/api/server/v1/*.ts` up to 7.x).

use std::fmt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use core_types::{ConversationId, MessageId, OutFile, SurfaceError, UserId};
use rand::RngExt;
use rand::distr::Alphanumeric;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::format_description::BorrowedFormatItem;
use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{OffsetDateTime, PrimitiveDateTime};
use tokio::io::AsyncReadExt;

/// The result type of [`RestClient`] methods.
pub type Result<T, E = SurfaceError> = std::result::Result<T, E>;

/// How long a call may take before it fails with
/// [`SurfaceError::Transport`].
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an upload may take.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait before the one retry when a 429 carries no usable
/// `x-ratelimit-reset`.
const DEFAULT_RETRY_WAIT: Duration = Duration::from_secs(1);

/// The default for [`RestClient::with_max_retry_wait`]: Rocket.Chat's
/// default rate-limit window (`API_Enable_Rate_Limiter_Limit_Time_Default`,
/// 60 seconds) plus the second the whole-second `Date` header can add to
/// the measured wait, so a limit hit early in a window is still retried.
const DEFAULT_MAX_RETRY_WAIT: Duration = Duration::from_secs(61);

/// The default for [`RestClient::with_max_upload_size`]: Rocket.Chat's
/// default `FileUpload_MaxFileSize`, 100 MiB.
const DEFAULT_MAX_UPLOAD_SIZE: u64 = 100 * 1024 * 1024;

/// RFC 7231's IMF-fixdate, the only `Date` format a server may send today.
const HTTP_DATE: &[BorrowedFormatItem<'static>] = format_description!(
    "[weekday repr:short], [day] [month repr:short] [year] [hour]:[minute]:[second] GMT"
);

/// The length of a bot's random password.
const PASSWORD_LEN: usize = 48;

/// The longest platform error description kept in a [`SurfaceError`].
const MAX_DESCRIPTION: usize = 200;

/// Error codes that mean the caller may not do this.
const FORBIDDEN_CODES: &[&str] = &[
    "error-action-not-allowed",
    "error-forbidden",
    "error-not-allowed",
    "error-not-authorized",
    "error-unauthorized",
    "forbidden",
    "not-allowed",
    "not-authorized",
    "totp-invalid",
    "totp-max-attempts",
    "totp-required",
    "unauthorized",
];

/// Error codes that mean the room, message, user or file doesn't exist.
const NOT_FOUND_CODES: &[&str] = &[
    "error-invalid-message",
    "error-invalid-room",
    "error-invalid-user",
    "error-message-not-found",
    "error-room-does-not-exist",
    "error-room-not-found",
    "error-user-not-found",
    "invalid-channel",
    "invalid-file",
];

/// The error codes of Rocket.Chat's REST rate limiter and of Meteor's DDP
/// rate limiter, which guards `login` and reports through a 401.
const RATE_LIMITED_CODES: &[&str] = &["error-too-many-requests", "too-many-requests"];

/// A user id and an auth token: a personal access token, or a login token.
///
/// `Debug` never prints the token.
#[derive(Debug, Clone)]
pub struct Credentials {
    /// The user the token belongs to (`X-User-Id`).
    pub user_id: UserId,
    /// The token (`X-Auth-Token`).
    pub token: SecretString,
}

/// A new bot user's random password, from [`RestClient::create_bot_user`].
///
/// It is never stored: [`RestClient::issue_bot_token`] consumes it. It can't
/// be cloned or serialized, and `Debug` never prints it.
pub struct BotPassword(SecretString);

impl fmt::Debug for BotPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BotPassword([REDACTED])")
    }
}

impl BotPassword {
    fn generate() -> Self {
        let password: String = rand::rng()
            .sample_iter(Alphanumeric)
            .take(PASSWORD_LEN)
            .map(char::from)
            .collect();
        Self(SecretString::from(password))
    }

    /// The value of the `x-2fa-code` header for Rocket.Chat's password
    /// fallback: the SHA-256 of the password, in lowercase hex.
    fn two_factor_code(&self) -> SecretString {
        let digest = Sha256::digest(self.0.expose_secret().as_bytes());
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        SecretString::from(hex)
    }
}

/// What [`RestClient::create_bot_user`] needs to create a bot user.
#[derive(Debug, Clone, Copy)]
pub struct NewBotUser<'a> {
    /// The username, which is what members type after `@`.
    pub username: &'a str,
    /// The display name.
    pub name: &'a str,
    /// An email address. Rocket.Chat requires one; it is created unverified
    /// (see [`RestClient::create_bot_user`]).
    pub email: &'a str,
}

/// A user as Rocket.Chat reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    /// The user's `_id`.
    #[serde(rename = "_id")]
    pub id: UserId,
    /// The username.
    pub username: String,
    /// The display name.
    #[serde(default)]
    pub name: Option<String>,
    /// Global role ids, such as `bot` or `user`.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Whether the account is active, when reported.
    #[serde(default)]
    pub active: Option<bool>,
}

/// A room's type, Rocket.Chat's `t` field.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RoomType {
    /// `c`: a public channel.
    Channel,
    /// `p`: a private group.
    Group,
    /// `d`: a direct message, with one or several members.
    Direct,
    /// `l`: an omnichannel (livechat) room.
    Livechat,
    /// Any other type.
    Other(String),
}

impl RoomType {
    fn from_code(code: &str) -> Self {
        match code {
            "c" => Self::Channel,
            "p" => Self::Group,
            "d" => Self::Direct,
            "l" => Self::Livechat,
            other => Self::Other(other.to_owned()),
        }
    }
}

impl<'de> Deserialize<'de> for RoomType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let code = String::deserialize(deserializer)?;
        Ok(Self::from_code(&code))
    }
}

/// A room, from `rooms.info`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RoomInfo {
    /// The room's `_id`.
    #[serde(rename = "_id")]
    pub id: ConversationId,
    /// The room type.
    #[serde(rename = "t")]
    pub room_type: RoomType,
    /// The room name. Direct messages have none.
    #[serde(default)]
    pub name: Option<String>,
    /// The display name, when it differs from `name`.
    #[serde(default)]
    pub fname: Option<String>,
    /// The number of members, when reported.
    #[serde(default, rename = "usersCount")]
    pub users_count: Option<u64>,
    /// The members of a direct message.
    #[serde(default)]
    pub uids: Vec<UserId>,
}

/// Who sent a message.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct UserRef {
    /// The user's `_id`.
    #[serde(rename = "_id")]
    pub id: UserId,
    /// The username.
    #[serde(default)]
    pub username: String,
    /// The display name.
    #[serde(default)]
    pub name: Option<String>,
}

/// A file attached to a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRef {
    /// The file's `_id`.
    pub id: String,
    /// The file name.
    pub name: String,
    /// The MIME type, when reported.
    pub mime_type: Option<String>,
    /// The size in bytes, when reported.
    pub size: Option<u64>,
}

/// A message, from the REST API or a realtime stream.
///
/// `ts` is read both as an ISO 8601 string (REST) and as EJSON
/// `{"$date": <ms>}` (realtime). `Debug` prints the text's length, not the
/// text.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawMessage")]
pub struct Message {
    /// The message's `_id`.
    pub id: MessageId,
    /// The room it is in (`rid`).
    pub room: ConversationId,
    /// The text (`msg`).
    pub text: String,
    /// Who sent it (`u`).
    pub sender: UserRef,
    /// When it was sent (`ts`).
    pub sent_at: OffsetDateTime,
    /// The thread root (`tmid`), for a reply in a thread.
    pub thread_root: Option<MessageId>,
    /// The system message type (`t`), such as `uj` for a user joining.
    /// `None` for an ordinary message.
    pub kind: Option<String>,
    /// Whether the message carries the `bot` field.
    pub bot: bool,
    /// Whether the message was edited (`editedAt`).
    pub edited: bool,
    /// The `_id`s of the users mentioned (`mentions[]._id`). `@all` and
    /// `@here` appear as `all` and `here`.
    pub mentions: Vec<UserId>,
    /// Attached files (`files`, or `file` on older messages).
    pub files: Vec<FileRef>,
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Message")
            .field("id", &self.id)
            .field("room", &self.room)
            .field("text_len", &self.text.len())
            .field("sender", &self.sender)
            .field("sent_at", &self.sent_at)
            .field("thread_root", &self.thread_root)
            .field("kind", &self.kind)
            .field("bot", &self.bot)
            .field("edited", &self.edited)
            .field("mentions", &self.mentions)
            .field("files", &self.files)
            .finish()
    }
}

#[derive(Deserialize)]
struct RawMessage {
    #[serde(rename = "_id")]
    id: String,
    rid: String,
    #[serde(default)]
    msg: Option<String>,
    ts: Value,
    u: UserRef,
    #[serde(default)]
    tmid: Option<String>,
    #[serde(default)]
    t: Option<String>,
    #[serde(default)]
    bot: Option<Value>,
    #[serde(default, rename = "editedAt")]
    edited_at: Option<Value>,
    #[serde(default)]
    mentions: Vec<IdOnly>,
    #[serde(default)]
    files: Vec<Value>,
    #[serde(default)]
    file: Option<Value>,
}

#[derive(Deserialize)]
struct IdOnly {
    #[serde(rename = "_id")]
    id: String,
}

impl TryFrom<RawMessage> for Message {
    type Error = String;

    fn try_from(raw: RawMessage) -> Result<Self, String> {
        let sent_at = parse_timestamp(&raw.ts).ok_or("invalid message timestamp")?;
        let files = if raw.files.is_empty() {
            raw.file.iter().filter_map(file_ref).collect()
        } else {
            raw.files.iter().filter_map(file_ref).collect()
        };
        Ok(Self {
            id: raw.id.into(),
            room: raw.rid.into(),
            text: raw.msg.unwrap_or_default(),
            sender: raw.u,
            sent_at,
            thread_root: raw.tmid.map(MessageId::from),
            kind: raw.t,
            bot: raw
                .bot
                .is_some_and(|b| !b.is_null() && b != Value::Bool(false)),
            edited: raw.edited_at.is_some_and(|e| !e.is_null()),
            mentions: raw.mentions.into_iter().map(|m| m.id.into()).collect(),
            files,
        })
    }
}

/// Reads an ISO 8601 string or EJSON `{"$date": <ms>}`.
fn parse_timestamp(ts: &Value) -> Option<OffsetDateTime> {
    match ts {
        Value::String(text) => OffsetDateTime::parse(text, &Rfc3339).ok(),
        Value::Object(map) => {
            let millis = map.get("$date")?.as_i64()?;
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000).ok()
        }
        _ => None,
    }
}

fn file_ref(file: &Value) -> Option<FileRef> {
    Some(FileRef {
        id: file.get("_id")?.as_str()?.to_owned(),
        name: file
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        mime_type: file.get("type").and_then(Value::as_str).map(str::to_owned),
        size: file.get("size").and_then(Value::as_u64),
    })
}

/// Formats a time the way JavaScript's `Date.toISOString` does, which is how
/// Rocket.Chat writes and compares message timestamps.
fn iso_millis(at: OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        at.millisecond()
    )
}

/// A client for one Rocket.Chat server, acting as one user.
///
/// Cloning is cheap and shares the connection pool.
#[derive(Debug, Clone)]
pub struct RestClient {
    http: reqwest::Client,
    base: Url,
    creds: Credentials,
    max_retry_wait: Duration,
    max_upload_size: u64,
}

/// One request, kept so it can be sent again after a 429.
struct Call<'a> {
    method: Method,
    path: Vec<&'a str>,
    query: Vec<(&'static str, String)>,
    body: Body<'a>,
    auth: Auth<'a>,
    two_factor: Option<SecretString>,
}

/// Whose credentials a call carries.
enum Auth<'a> {
    /// The client's own.
    Client,
    /// Someone else's, such as a bot's login session.
    As(&'a Credentials),
    /// None, for `login`.
    Anonymous,
}

enum Body<'a> {
    Empty,
    Json(Value),
    File(Upload<'a>),
}

/// A file read into memory once, so a retry after a 429 sends the same
/// bytes without reading the file again.
struct Upload<'a> {
    name: &'a str,
    data: Bytes,
}

impl<'a> Call<'a> {
    fn get(path: &'a str) -> Self {
        Self::new(Method::GET, path, Body::Empty)
    }

    fn post(path: &'a str, body: Value) -> Self {
        Self::new(Method::POST, path, Body::Json(body))
    }

    fn new(method: Method, path: &'a str, body: Body<'a>) -> Self {
        Self {
            method,
            path: vec![path],
            query: Vec::new(),
            body,
            auth: Auth::Client,
            two_factor: None,
        }
    }

    fn query(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.query.push((key, value.into()));
        self
    }

    fn endpoint(&self) -> String {
        self.path.first().copied().unwrap_or_default().to_owned()
    }
}

/// What one response turned out to be.
enum Outcome {
    Done(Result<Value>),
    RateLimited(Duration),
}

impl RestClient {
    /// Builds a client for the server at `base_url` (such as
    /// `https://chat.example.com`, or one with a path prefix), acting as
    /// `creds`.
    ///
    /// The URL must be `http` or `https`, with no user info, query or
    /// fragment.
    pub fn new(base_url: &str, creds: Credentials) -> Result<Self> {
        let base = Url::parse(base_url)
            .map_err(|err| SurfaceError::Api(format!("invalid Rocket.Chat URL: {err}")))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(SurfaceError::Api(
                "invalid Rocket.Chat URL: the scheme must be http or https".into(),
            ));
        }
        if !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(SurfaceError::Api(
                "invalid Rocket.Chat URL: user info, query and fragment are not allowed".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(transport)?;
        Ok(Self {
            http,
            base,
            creds,
            max_retry_wait: DEFAULT_MAX_RETRY_WAIT,
            max_upload_size: DEFAULT_MAX_UPLOAD_SIZE,
        })
    }

    /// A client for the same server acting as `creds`, sharing this one's
    /// connection pool.
    pub fn with_credentials(&self, creds: Credentials) -> Self {
        Self {
            creds,
            ..self.clone()
        }
    }

    /// Sets the longest wait before retrying a 429. A 429 whose
    /// `x-ratelimit-reset` is further away fails at once with
    /// [`SurfaceError::RateLimited`]. The default is 61 seconds: the
    /// server's default 60-second window, plus a second for the `Date`
    /// header's resolution.
    pub fn with_max_retry_wait(mut self, wait: Duration) -> Self {
        self.max_retry_wait = wait;
        self
    }

    /// Sets the largest file [`RestClient::upload`] sends, in bytes. A
    /// larger file fails with [`SurfaceError::Api`] before anything is read
    /// or sent. The default is 100 MiB, Rocket.Chat's default
    /// `FileUpload_MaxFileSize`; set it to the server's value when that is
    /// lower, so an oversized file fails without being read.
    pub fn with_max_upload_size(mut self, bytes: u64) -> Self {
        self.max_upload_size = bytes;
        self
    }

    /// The user this client acts as.
    pub fn user_id(&self) -> &UserId {
        &self.creds.user_id
    }

    /// `GET me`: the user this client acts as.
    pub async fn me(&self) -> Result<User> {
        self.call(Call::get("me")).await
    }

    /// `POST users.create`: creates a user with the `bot` role and a random
    /// password, which is returned for [`RestClient::issue_bot_token`] and
    /// never stored.
    ///
    /// The user is created with `joinDefaultChannels`,
    /// `requirePasswordChange` and `sendWelcomeEmail` off. `active` is not
    /// sent, so the caller needs only `create-user`, not
    /// `edit-other-user-active-status`. The email is left unverified: with a
    /// verified email, Rocket.Chat's email two-factor auto opt-in (on by
    /// default) would make the bot's password login ask for an emailed code.
    ///
    /// A taken username fails with [`SurfaceError::Api`] carrying
    /// `error-field-unavailable`.
    pub async fn create_bot_user(&self, new: &NewBotUser<'_>) -> Result<(User, BotPassword)> {
        let password = BotPassword::generate();
        let body = json!({
            "username": new.username,
            "name": new.name,
            "email": new.email,
            "password": password.0.expose_secret(),
            "roles": ["bot"],
            "verified": false,
            "joinDefaultChannels": false,
            "requirePasswordChange": false,
            "sendWelcomeEmail": false,
        });
        let created: UserEnvelope = self.call(Call::post("users.create", body)).await?;
        Ok((created.user, password))
    }

    /// Obtains a personal access token for a bot created with
    /// [`RestClient::create_bot_user`], consuming its password.
    ///
    /// It logs in as the bot (`POST login`), calls
    /// `users.generatePersonalAccessToken` with that session, then logs the
    /// session out. The bot's role needs `create-personal-access-tokens`,
    /// which Rocket.Chat grants only to `admin` and `user` by default. The
    /// token is created with `bypassTwoFactor`, since the bot has no second
    /// factor left once its password is gone. The endpoint requires two-factor
    /// authentication, so the call sends the password fallback's
    /// `x-2fa-code` too.
    ///
    /// `token_name` must not already name a token of this user, or the call
    /// fails with `error-token-already-exists`.
    ///
    /// This doesn't use this client's credentials. The alternative,
    /// `users.createToken`, needs the server's `CREATE_TOKENS_FOR_USERS_SECRET`
    /// (8.0 and later) and returns a login token that expires, so it isn't
    /// used; see `docs/impl-notes.md`.
    pub async fn issue_bot_token(
        &self,
        username: &str,
        password: BotPassword,
        token_name: &str,
    ) -> Result<Credentials> {
        let body = json!({ "user": username, "password": password.0.expose_secret() });
        let login = Call {
            auth: Auth::Anonymous,
            ..Call::post("login", body)
        };
        let login: LoginEnvelope = self.call(login).await?;
        let session = Credentials {
            user_id: login.data.user_id,
            token: login.data.auth_token,
        };
        let generate = Call {
            auth: Auth::As(&session),
            two_factor: Some(password.two_factor_code()),
            ..Call::post(
                "users.generatePersonalAccessToken",
                json!({ "tokenName": token_name, "bypassTwoFactor": true }),
            )
        };
        let generated: std::result::Result<TokenEnvelope, _> = self.call(generate).await;
        let logout = Call {
            auth: Auth::As(&session),
            ..Call::post("logout", json!({}))
        };
        if let Err(err) = self.call::<IgnoredAny>(logout).await {
            tracing::warn!(error = %err, "could not log out the bot's login session");
        }
        Ok(Credentials {
            user_id: session.user_id,
            token: generated?.token,
        })
    }

    /// `POST users.setAvatar` from a URL. Setting another user's avatar needs
    /// `edit-other-user-avatar`; a bot can set its own when the server allows
    /// avatar changes.
    pub async fn set_avatar(&self, user: &UserId, avatar_url: &str) -> Result<()> {
        let body = json!({ "userId": user, "avatarUrl": avatar_url });
        self.call_unit(Call::post("users.setAvatar", body)).await
    }

    /// `POST users.update` with a new display name. Renaming another user
    /// needs `edit-other-user-info`, and the endpoint requires two-factor
    /// authentication, which a personal access token passes only when it was
    /// created with "Ignore Two Factor Authentication".
    pub async fn set_name(&self, user: &UserId, name: &str) -> Result<()> {
        let body = json!({ "userId": user, "data": { "name": name } });
        self.call_unit(Call::post("users.update", body)).await
    }

    /// `POST users.setActiveStatus`. Needs `edit-other-user-active-status`
    /// or `manage-moderation-actions`.
    pub async fn set_active(&self, user: &UserId, active: bool) -> Result<()> {
        let body = json!({ "userId": user, "activeStatus": active });
        self.call_unit(Call::post("users.setActiveStatus", body))
            .await
    }

    /// Adds a user to a room: `channels.invite` for a public channel,
    /// `groups.invite` for a private group. Other room types fail with
    /// [`SurfaceError::Unsupported`].
    pub async fn invite(
        &self,
        room: &ConversationId,
        room_type: &RoomType,
        user: &UserId,
    ) -> Result<()> {
        let endpoint = match room_type {
            RoomType::Channel => "channels.invite",
            RoomType::Group => "groups.invite",
            _ => return Err(SurfaceError::Unsupported("inviting into this room type")),
        };
        let body = json!({ "roomId": room, "userId": user });
        self.call_unit(Call::post(endpoint, body)).await
    }

    /// `GET rooms.info`.
    pub async fn room_info(&self, room: &ConversationId) -> Result<RoomInfo> {
        let info: RoomEnvelope = self
            .call(Call::get("rooms.info").query("roomId", room.as_str()))
            .await?;
        info.room
            .ok_or_else(|| SurfaceError::NotFound("room".into()))
    }

    /// `POST im.create`: opens (or finds) the direct message with the user
    /// named `username` and returns its room id.
    ///
    /// Rocket.Chat answers a username it doesn't know (the match is
    /// case-sensitive) with success and the caller's own self-DM, so a room
    /// whose `usernames` leave out `username` fails with
    /// [`SurfaceError::NotFound`] carrying `error-invalid-user`.
    pub async fn create_dm(&self, username: &str) -> Result<ConversationId> {
        let created: DmEnvelope = self
            .call(Call::post("im.create", json!({ "username": username })))
            .await?;
        if !created.room.usernames.iter().any(|u| u == username) {
            return Err(SurfaceError::NotFound("error-invalid-user".into()));
        }
        Ok(created.room.id)
    }

    /// `POST chat.postMessage`, in the thread under `thread_root` if given.
    ///
    /// Posting to a public channel the user isn't in makes it join first;
    /// that is Rocket.Chat's behavior for `roomId`.
    pub async fn post_message(
        &self,
        room: &ConversationId,
        text: &str,
        thread_root: Option<&MessageId>,
    ) -> Result<Message> {
        let mut body = json!({ "roomId": room, "text": text });
        if let Some(tmid) = thread_root {
            body["tmid"] = json!(tmid);
        }
        let posted: MessageEnvelope = self.call(Call::post("chat.postMessage", body)).await?;
        Ok(posted.message)
    }

    /// `POST chat.update`: replaces a message's text.
    pub async fn update_message(
        &self,
        room: &ConversationId,
        message: &MessageId,
        text: &str,
    ) -> Result<()> {
        let body = json!({ "roomId": room, "msgId": message, "text": text });
        self.call_unit(Call::post("chat.update", body)).await
    }

    /// `POST chat.react` with `shouldReact: true`, so an existing reaction is
    /// kept rather than toggled off. `emoji` is named with or without colons.
    pub async fn react(&self, message: &MessageId, emoji: &str) -> Result<()> {
        let body = json!({ "messageId": message, "emoji": emoji, "shouldReact": true });
        self.call_unit(Call::post("chat.react", body)).await
    }

    /// `GET chat.getMessage`: one message by id.
    ///
    /// Rocket.Chat answers an unknown id with a bare `{"success": false}`
    /// (HTTP 400, no code), which this maps to [`SurfaceError::NotFound`]
    /// carrying `message`. Any other 400 without a code, such as an empty
    /// body or a proxy's error page, stays [`SurfaceError::Api`]. A message
    /// in a room the caller can't see is [`SurfaceError::Forbidden`].
    pub async fn get_message(&self, message: &MessageId) -> Result<Message> {
        let found: MessageEnvelope = self
            .call(Call::get("chat.getMessage").query("msgId", message.as_str()))
            .await
            .map_err(|err| match err {
                SurfaceError::Api(description)
                    if description == bare_failure(StatusCode::BAD_REQUEST) =>
                {
                    SurfaceError::NotFound("message".into())
                }
                err => err,
            })?;
        Ok(found.message)
    }

    /// Uploads a file to a room, in the thread under `thread_root` if given,
    /// and returns the message that carries it.
    ///
    /// It uses `rooms.media/{rid}` (multipart, field `file`) followed by
    /// `rooms.mediaConfirm/{rid}/{fileId}` with `tmid`. The plan's
    /// `rooms.upload/{rid}` was removed in Rocket.Chat 8.0; the two-step
    /// endpoints exist from 7.0 on. The confirm body carries nothing else:
    /// 7.x passes it whole to a strict `check`, which rejects keys such as
    /// `fileName` that newer servers accept.
    ///
    /// The file is read into memory once, and only if it is a regular file
    /// no larger than [`RestClient::with_max_upload_size`]; otherwise the
    /// call fails with [`SurfaceError::Api`] and sends nothing. A missing
    /// file is [`SurfaceError::NotFound`].
    pub async fn upload(
        &self,
        room: &ConversationId,
        thread_root: Option<&MessageId>,
        file: &OutFile,
    ) -> Result<Message> {
        let upload = Upload {
            name: &file.name,
            data: read_upload(&file.path, self.max_upload_size).await?,
        };
        let media = Call {
            path: vec!["rooms.media", room.as_str()],
            ..Call::new(Method::POST, "rooms.media", Body::File(upload))
        };
        let uploaded: MediaEnvelope = self.call(media).await?;
        let mut body = json!({});
        if let Some(tmid) = thread_root {
            body["tmid"] = json!(tmid);
        }
        let confirm = Call {
            path: vec!["rooms.mediaConfirm", room.as_str(), &uploaded.file.id],
            ..Call::post("rooms.mediaConfirm", body)
        };
        let confirmed: MessageEnvelope = self.call(confirm).await?;
        Ok(confirmed.message)
    }

    /// Reads a room's top-level messages: `channels.history`,
    /// `groups.history` or `im.history` by room type.
    ///
    /// Returns at most `count` messages sent strictly before `latest` (or
    /// before now), newest first. Thread replies are left out. The server
    /// caps `count` at its `API_Upper_Count_Limit` (100 by default).
    pub async fn room_history(
        &self,
        room: &ConversationId,
        room_type: &RoomType,
        latest: Option<OffsetDateTime>,
        count: usize,
    ) -> Result<Vec<Message>> {
        let endpoint = match room_type {
            RoomType::Channel => "channels.history",
            RoomType::Group => "groups.history",
            RoomType::Direct => "im.history",
            _ => return Err(SurfaceError::Unsupported("history of this room type")),
        };
        let mut call = Call::get(endpoint)
            .query("roomId", room.as_str())
            .query("count", count.to_string())
            .query("showThreadMessages", "false")
            .query("inclusive", "false");
        if let Some(latest) = latest {
            call = call.query("latest", iso_millis(latest));
        }
        let history: MessagesEnvelope = self.call(call).await?;
        Ok(history.messages)
    }

    /// `GET chat.getThreadMessages`: replies in the thread under `root`,
    /// newest first, skipping the newest `offset`. The root itself is not
    /// included. The server caps `count` as for
    /// [`RestClient::room_history`].
    pub async fn thread_messages(
        &self,
        root: &MessageId,
        offset: usize,
        count: usize,
    ) -> Result<Vec<Message>> {
        let call = Call::get("chat.getThreadMessages")
            .query("tmid", root.as_str())
            .query("offset", offset.to_string())
            .query("count", count.to_string())
            .query("sort", r#"{"ts":-1}"#);
        let thread: MessagesEnvelope = self.call(call).await?;
        Ok(thread.messages)
    }

    async fn call_unit(&self, call: Call<'_>) -> Result<()> {
        self.call::<IgnoredAny>(call).await.map(|_| ())
    }

    /// Sends `call`, retrying once after a 429 whose reset is near enough,
    /// and decodes a successful body as `T`.
    async fn call<T: DeserializeOwned>(&self, call: Call<'_>) -> Result<T> {
        let endpoint = call.endpoint();
        let mut retried = false;
        let value = loop {
            let request = self.request(&call)?;
            let response = request.send().await.map_err(transport)?;
            match outcome(response, SystemTime::now()).await? {
                Outcome::Done(result) => break result?,
                Outcome::RateLimited(wait) if !retried && wait <= self.max_retry_wait => {
                    tracing::debug!(%endpoint, wait_ms = wait.as_millis(), "rate limited, retrying once");
                    tokio::time::sleep(wait).await;
                    retried = true;
                }
                Outcome::RateLimited(retry_after) => {
                    return Err(SurfaceError::RateLimited { retry_after });
                }
            }
        };
        T::deserialize(value).map_err(|err| {
            SurfaceError::Transport(format!(
                "unexpected response from {endpoint} ({:?} error at column {})",
                err.classify(),
                err.column()
            ))
        })
    }

    fn request(&self, call: &Call<'_>) -> Result<reqwest::RequestBuilder> {
        let mut url = self.base.clone();
        if let Ok(mut segments) = url.path_segments_mut() {
            segments
                .pop_if_empty()
                .extend(["api", "v1"])
                .extend(&call.path);
        }
        let mut headers = HeaderMap::new();
        let creds = match call.auth {
            Auth::Client => Some(&self.creds),
            Auth::As(creds) => Some(creds),
            Auth::Anonymous => None,
        };
        if let Some(creds) = creds {
            headers.insert("x-user-id", header(creds.user_id.as_str())?);
            headers.insert("x-auth-token", header(creds.token.expose_secret())?);
        }
        if let Some(code) = &call.two_factor {
            headers.insert("x-2fa-code", header(code.expose_secret())?);
            headers.insert("x-2fa-method", HeaderValue::from_static("password"));
        }
        let mut request = self
            .http
            .request(call.method.clone(), url)
            .headers(headers)
            .timeout(REQUEST_TIMEOUT);
        if !call.query.is_empty() {
            request = request.query(&call.query);
        }
        Ok(match &call.body {
            Body::Empty => request,
            Body::Json(body) => request.json(body),
            Body::File(upload) => request
                .timeout(UPLOAD_TIMEOUT)
                .multipart(file_form(upload)?),
        })
    }
}

/// A sensitive header value. Tokens and ids never contain control
/// characters, so a failure means corrupt credentials.
fn header(value: &str) -> Result<HeaderValue> {
    let mut value = HeaderValue::from_str(value)
        .map_err(|_| SurfaceError::Api("credentials contain invalid header characters".into()))?;
    value.set_sensitive(true);
    Ok(value)
}

/// Reads a file to upload, refusing anything but a regular file of at most
/// `max` bytes before reading it. The read stops after `max + 1` bytes, so
/// a file that grows after the check can't take more memory than that.
async fn read_upload(path: &Path, max: u64) -> Result<Bytes> {
    let unreadable = |err: std::io::Error| match err.kind() {
        std::io::ErrorKind::NotFound => SurfaceError::NotFound("file to upload".into()),
        kind => SurfaceError::Transport(format!("could not read the file to upload: {kind}")),
    };
    let too_large = |size: u64| {
        SurfaceError::Api(format!(
            "the file to upload is {size} bytes, more than the {max}-byte limit"
        ))
    };
    let metadata = tokio::fs::metadata(path).await.map_err(unreadable)?;
    if !metadata.is_file() {
        return Err(SurfaceError::Api(
            "the file to upload is not a regular file".into(),
        ));
    }
    if metadata.len() > max {
        return Err(too_large(metadata.len()));
    }
    let file = tokio::fs::File::open(path).await.map_err(unreadable)?;
    let mut data = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(max.saturating_add(1))
        .read_to_end(&mut data)
        .await
        .map_err(unreadable)?;
    let size = u64::try_from(data.len()).unwrap_or(u64::MAX);
    if size > max {
        return Err(too_large(size));
    }
    Ok(Bytes::from(data))
}

fn file_form(upload: &Upload<'_>) -> Result<reqwest::multipart::Form> {
    let length = u64::try_from(upload.data.len()).unwrap_or(u64::MAX);
    let part = reqwest::multipart::Part::stream_with_length(upload.data.clone(), length)
        .file_name(upload.name.to_owned())
        .mime_str(mime_type(upload.name))
        .map_err(transport)?;
    Ok(reqwest::multipart::Form::new().part("file", part))
}

/// A MIME type from the file name's extension, so Rocket.Chat previews
/// images and text. Anything unknown is `application/octet-stream`.
fn mime_type(name: &str) -> &'static str {
    let extension = Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("pdf") => "application/pdf",
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        Some("md") => "text/markdown",
        Some("txt" | "log") => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Reads a response into an [`Outcome`].
async fn outcome(response: reqwest::Response, now: SystemTime) -> Result<Outcome> {
    let status = response.status();
    let reset = response.headers().get("x-ratelimit-reset").cloned();
    let date = response.headers().get(reqwest::header::DATE).cloned();
    let bytes = response.bytes().await.map_err(transport)?;
    let body: Option<Value> = serde_json::from_slice(&bytes).ok();
    let rate_limited = status == StatusCode::TOO_MANY_REQUESTS
        || body
            .as_ref()
            .is_some_and(|b| error_codes(b, true).any(|code| RATE_LIMITED_CODES.contains(&code)));
    if rate_limited {
        return Ok(Outcome::RateLimited(retry_wait(
            reset.as_ref(),
            date.as_ref(),
            now,
        )));
    }
    let failed = body
        .as_ref()
        .is_some_and(|b| b.get("success") == Some(&Value::Bool(false)));
    Ok(Outcome::Done(match body {
        Some(body) if status.is_success() && !failed => Ok(body),
        None if status.is_success() => Err(SurfaceError::Transport(format!(
            "unreadable response (HTTP {})",
            status.as_u16()
        ))),
        body => Err(map_error(status, body.as_ref())),
    }))
}

/// How long to wait before retrying a 429.
///
/// Rocket.Chat sets `x-ratelimit-reset` to the absolute time the limit
/// resets, in milliseconds since the Unix epoch by the server's clock
/// (`enforceRateLimit` in `apps/meteor/server/api/ApiClass.ts`). The wait
/// is that time minus the response's `Date` header, which comes from the
/// same clock, so skew between the server and this host doesn't matter.
/// `Date` has whole seconds, which makes the wait up to a second longer,
/// never shorter. Without a readable `Date`, `now` (the local clock) is
/// used instead. A missing or unreadable reset waits
/// [`DEFAULT_RETRY_WAIT`]; a reset in the past waits nothing.
fn retry_wait(
    reset: Option<&HeaderValue>,
    date: Option<&HeaderValue>,
    now: SystemTime,
) -> Duration {
    let Some(reset_ms) = reset
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    else {
        return DEFAULT_RETRY_WAIT;
    };
    let server_now = date.and_then(http_date).unwrap_or(now);
    let now_ms = server_now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    Duration::from_millis(reset_ms.saturating_sub(now_ms))
}

/// Reads an HTTP `Date` header in IMF-fixdate form
/// (`Sun, 06 Nov 1994 08:49:37 GMT`). The obsolete RFC 850 and asctime
/// forms, which no current server sends, read as `None`.
fn http_date(value: &HeaderValue) -> Option<SystemTime> {
    let text = value.to_str().ok()?;
    let at = PrimitiveDateTime::parse(text.trim(), HTTP_DATE).ok()?;
    Some(at.assume_utc().into())
}

/// The error codes a failure body carries: `errorType`, the `[code]` suffix
/// Meteor errors put on `error`, and, with `bare`, an `error` that is itself
/// a code.
fn error_codes(body: &Value, bare: bool) -> impl Iterator<Item = &str> {
    let error_type = body.get("errorType").and_then(Value::as_str);
    let error = body.get("error").and_then(Value::as_str);
    let bare = error.filter(|e| bare && !e.is_empty() && !e.contains(char::is_whitespace));
    let suffix = error.and_then(|e| {
        let e = e.trim_end().strip_suffix(']')?;
        e.rfind('[').map(|at| &e[at + 1..])
    });
    [error_type, bare, suffix].into_iter().flatten()
}

/// Maps a failed response to a [`SurfaceError`].
///
/// - A code such as `error-not-allowed`, `error-action-not-allowed`,
///   `not-authorized` or `totp-required` is [`SurfaceError::Forbidden`] with
///   the code, whatever the status. Rocket.Chat reports missing permissions
///   with HTTP 400 or 403 depending on the endpoint, and from 9.0 with 401
///   for `error-unauthorized`. On a 401 only `errorType` and a `[code]`
///   suffix count: a bare `error: "unauthorized"` there is a rejected token.
/// - A code such as `error-room-not-found`, `error-invalid-room`,
///   `error-message-not-found` or `error-invalid-user` is
///   [`SurfaceError::NotFound`] with the code.
/// - Otherwise 401 is [`SurfaceError::Unauthorized`] (the token was
///   rejected), 403 is [`SurfaceError::Forbidden`] and 404 is
///   [`SurfaceError::NotFound`], with the first code or the status.
/// - Anything else is [`SurfaceError::Api`] with the platform's description
///   (truncated) and code.
pub fn map_error(status: StatusCode, body: Option<&Value>) -> SurfaceError {
    let bare = status != StatusCode::UNAUTHORIZED;
    let codes: Vec<&str> = body
        .map(|b| error_codes(b, bare).collect())
        .unwrap_or_default();
    let first_code = || {
        codes
            .first()
            .map_or_else(|| status.as_u16().to_string(), |c| (*c).to_owned())
    };
    if let Some(code) = codes.iter().find(|c| FORBIDDEN_CODES.contains(c)) {
        return SurfaceError::Forbidden((*code).to_owned());
    }
    if let Some(code) = codes.iter().find(|c| NOT_FOUND_CODES.contains(c)) {
        return SurfaceError::NotFound((*code).to_owned());
    }
    match status {
        StatusCode::UNAUTHORIZED => SurfaceError::Unauthorized,
        StatusCode::FORBIDDEN => SurfaceError::Forbidden(first_code()),
        StatusCode::NOT_FOUND => SurfaceError::NotFound(first_code()),
        _ => SurfaceError::Api(describe(status, body)),
    }
}

/// The platform's description of a failure, with its code if the
/// description doesn't already carry it.
fn describe(status: StatusCode, body: Option<&Value>) -> String {
    let error = body
        .and_then(|b| b.get("error").or_else(|| b.get("message")))
        .and_then(Value::as_str)
        .map(truncate);
    let error_type = body
        .and_then(|b| b.get("errorType"))
        .and_then(Value::as_str);
    match (error, error_type) {
        (Some(error), Some(code)) if !error.contains(code) => format!("{error} [{code}]"),
        (Some(error), _) => error,
        (None, Some(code)) => code.to_owned(),
        (None, None) if body.is_some_and(|b| *b == json!({ "success": false })) => {
            bare_failure(status)
        }
        (None, None) => format!("HTTP {}", status.as_u16()),
    }
}

/// The description of a failure whose body is exactly `{"success": false}`,
/// as `API.v1.failure()` answers with no error. An empty or non-JSON body,
/// such as a proxy's error page, is described by its status alone.
fn bare_failure(status: StatusCode) -> String {
    format!("HTTP {} with a bare failure", status.as_u16())
}

fn truncate(text: &str) -> String {
    match text.char_indices().nth(MAX_DESCRIPTION) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// A transport error with its causes, but without the URL.
fn transport(err: reqwest::Error) -> SurfaceError {
    let err = err.without_url();
    let mut text = err.to_string();
    let mut source = std::error::Error::source(&err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    SurfaceError::Transport(text)
}

#[derive(Deserialize)]
struct UserEnvelope {
    user: User,
}

#[derive(Deserialize)]
struct LoginEnvelope {
    data: LoginData,
}

#[derive(Deserialize)]
struct LoginData {
    #[serde(rename = "userId")]
    user_id: UserId,
    #[serde(rename = "authToken")]
    auth_token: SecretString,
}

#[derive(Deserialize)]
struct TokenEnvelope {
    token: SecretString,
}

#[derive(Deserialize)]
struct RoomEnvelope {
    room: Option<RoomInfo>,
}

#[derive(Deserialize)]
struct DmEnvelope {
    room: DmRoom,
}

#[derive(Deserialize)]
struct DmRoom {
    #[serde(rename = "_id")]
    id: ConversationId,
    usernames: Vec<String>,
}

#[derive(Deserialize)]
struct MessageEnvelope {
    message: Message,
}

#[derive(Deserialize)]
struct MessagesEnvelope {
    messages: Vec<Message>,
}

#[derive(Deserialize)]
struct MediaEnvelope {
    file: MediaFile,
}

#[derive(Deserialize)]
struct MediaFile {
    #[serde(rename = "_id")]
    id: String,
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    fn at_ms(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    #[test]
    fn retry_wait_reads_the_reset_as_epoch_milliseconds() {
        let now = at_ms(1_790_000_000_000);
        let reset = HeaderValue::from_static("1790000002500");
        assert_eq!(
            retry_wait(Some(&reset), None, now),
            Duration::from_millis(2500)
        );
    }

    #[test]
    fn retry_wait_for_a_past_reset_is_zero() {
        let reset = HeaderValue::from_static("1000");
        assert_eq!(retry_wait(Some(&reset), None, at_ms(5000)), Duration::ZERO);
    }

    #[test]
    fn retry_wait_without_a_usable_header_uses_the_default() {
        let now = at_ms(5000);
        let date = HeaderValue::from_static("Thu, 01 Jan 1970 00:00:01 GMT");
        assert_eq!(retry_wait(None, None, now), DEFAULT_RETRY_WAIT);
        assert_eq!(retry_wait(None, Some(&date), now), DEFAULT_RETRY_WAIT);
        let garbage = HeaderValue::from_static("soon");
        assert_eq!(retry_wait(Some(&garbage), None, now), DEFAULT_RETRY_WAIT);
        let float = HeaderValue::from_static("1.5");
        assert_eq!(retry_wait(Some(&float), None, now), DEFAULT_RETRY_WAIT);
    }

    #[test]
    fn retry_wait_measures_the_reset_against_the_server_date_not_the_local_clock() {
        let date = HeaderValue::from_static("Tue, 29 Sep 2026 22:40:00 GMT");
        let server_now_ms = 1_790_721_600_000;
        let reset = HeaderValue::from_str(&(server_now_ms + 2500).to_string()).unwrap();
        for local_skew_ms in [0, 3_600_000, 90_000] {
            let ahead = at_ms(server_now_ms + local_skew_ms);
            let behind = at_ms(server_now_ms - local_skew_ms);
            for now in [ahead, behind] {
                assert_eq!(
                    retry_wait(Some(&reset), Some(&date), now),
                    Duration::from_millis(2500)
                );
            }
        }
    }

    #[test]
    fn retry_wait_with_an_unreadable_date_falls_back_to_the_local_clock() {
        let now = at_ms(1_790_721_600_000);
        let reset = HeaderValue::from_static("1790721602500");
        for date in [
            "yesterday",
            "Tuesday, 29-Sep-26 22:40:00 GMT",
            "Tue Sep 29 22:40:00 2026",
            "Tue, 29 Sep 2026 22:40:00 +0000",
            "Tue, 31 Sep 2026 22:40:00 GMT",
        ] {
            let date = HeaderValue::from_static(date);
            assert_eq!(
                retry_wait(Some(&reset), Some(&date), now),
                Duration::from_millis(2500),
                "{date:?}"
            );
        }
        let not_text = HeaderValue::from_bytes(b"Tue, 29 Sep 2026 \xff GMT").unwrap();
        assert_eq!(
            retry_wait(Some(&reset), Some(&not_text), now),
            Duration::from_millis(2500)
        );
    }

    #[test]
    fn the_default_max_wait_covers_a_full_window_measured_by_a_whole_second_date() {
        let date = HeaderValue::from_static("Fri, 02 Oct 2026 08:23:37 GMT");
        let window_start_ms = 1_790_929_417_999;
        let reset = HeaderValue::from_str(&(window_start_ms + 60_000).to_string()).unwrap();
        let wait = retry_wait(Some(&reset), Some(&date), at_ms(window_start_ms));
        assert_eq!(wait, Duration::from_millis(60_999));
        assert!(wait <= DEFAULT_MAX_RETRY_WAIT);
    }

    #[test]
    fn http_date_reads_imf_fixdate() {
        let date = HeaderValue::from_static(" Sun, 06 Nov 1994 08:49:37 GMT ");
        assert_eq!(
            http_date(&date),
            Some(UNIX_EPOCH + Duration::from_secs(784_111_777))
        );
    }

    #[test]
    fn error_codes_come_from_error_type_bare_error_and_suffix() {
        let body = json!({ "error": "Adding user is not allowed [error-action-not-allowed]", "errorType": "x" });
        assert_eq!(
            error_codes(&body, true).collect::<Vec<_>>(),
            ["x", "error-action-not-allowed"]
        );
        let body = json!({ "error": "error-message-size-exceeded" });
        assert_eq!(
            error_codes(&body, true).collect::<Vec<_>>(),
            ["error-message-size-exceeded"]
        );
        let body = json!({ "error": "Name contains invalid characters" });
        assert_eq!(error_codes(&body, true).count(), 0);
        let body = json!({ "error": 7, "errorType": null });
        assert_eq!(error_codes(&body, true).count(), 0);
    }

    #[test]
    fn map_error_401_is_unauthorized_unless_it_names_a_permission() {
        let rejected = json!({ "success": false, "status": "error", "message": "You must be logged in to do this." });
        assert_eq!(
            map_error(StatusCode::UNAUTHORIZED, Some(&rejected)),
            SurfaceError::Unauthorized
        );
        let login =
            json!({ "status": "error", "error": "Unauthorized", "message": "Unauthorized" });
        assert_eq!(
            map_error(StatusCode::UNAUTHORIZED, Some(&login)),
            SurfaceError::Unauthorized
        );
        assert_eq!(
            map_error(StatusCode::UNAUTHORIZED, None),
            SurfaceError::Unauthorized
        );
        let bare = json!({ "success": false, "error": "unauthorized" });
        assert_eq!(
            map_error(StatusCode::UNAUTHORIZED, Some(&bare)),
            SurfaceError::Unauthorized
        );
        let permission = json!({ "success": false, "error": "Not allowed [error-unauthorized]" });
        assert_eq!(
            map_error(StatusCode::UNAUTHORIZED, Some(&permission)),
            SurfaceError::Forbidden("error-unauthorized".into())
        );
    }

    #[test]
    fn map_error_permission_codes_are_forbidden_on_400() {
        for code in FORBIDDEN_CODES {
            let body = json!({ "error": format!("Nope [{code}]"), "errorType": code });
            assert_eq!(
                map_error(StatusCode::BAD_REQUEST, Some(&body)),
                SurfaceError::Forbidden((*code).to_owned())
            );
        }
    }

    #[test]
    fn map_error_missing_things_are_not_found_on_400() {
        for code in NOT_FOUND_CODES {
            let body = json!({ "error": format!("Missing [{code}]"), "errorType": code });
            assert_eq!(
                map_error(StatusCode::BAD_REQUEST, Some(&body)),
                SurfaceError::NotFound((*code).to_owned())
            );
        }
    }

    #[test]
    fn map_error_swapped_error_and_error_type_still_match() {
        let body = json!({ "success": false, "error": "not-allowed", "errorType": "Not Allowed" });
        assert_eq!(
            map_error(StatusCode::BAD_REQUEST, Some(&body)),
            SurfaceError::Forbidden("not-allowed".into())
        );
    }

    #[test]
    fn map_error_403_and_404_without_known_codes() {
        let body = json!({ "error": "denied" });
        assert_eq!(
            map_error(StatusCode::FORBIDDEN, Some(&body)),
            SurfaceError::Forbidden("denied".into())
        );
        assert_eq!(
            map_error(StatusCode::FORBIDDEN, None),
            SurfaceError::Forbidden("403".into())
        );
        let body = json!({ "error": "Resource not found" });
        assert_eq!(
            map_error(StatusCode::NOT_FOUND, Some(&body)),
            SurfaceError::NotFound("404".into())
        );
    }

    #[test]
    fn map_error_other_failures_describe_themselves() {
        let body = json!({ "error": "taken is already in use :( [error-field-unavailable]", "errorType": "error-field-unavailable" });
        assert_eq!(
            map_error(StatusCode::BAD_REQUEST, Some(&body)),
            SurfaceError::Api("taken is already in use :( [error-field-unavailable]".into())
        );
        let body =
            json!({ "error": "Name contains invalid characters", "errorType": "error-bad-name" });
        assert_eq!(
            map_error(StatusCode::BAD_REQUEST, Some(&body)),
            SurfaceError::Api("Name contains invalid characters [error-bad-name]".into())
        );
        let body = json!({ "errorType": "error-bad-name" });
        assert_eq!(
            map_error(StatusCode::BAD_REQUEST, Some(&body)),
            SurfaceError::Api("error-bad-name".into())
        );
        let body = json!({ "status": "error", "message": "Something broke" });
        assert_eq!(
            map_error(StatusCode::INTERNAL_SERVER_ERROR, Some(&body)),
            SurfaceError::Api("Something broke".into())
        );
        assert_eq!(
            map_error(StatusCode::BAD_GATEWAY, None),
            SurfaceError::Api("HTTP 502".into())
        );
    }

    #[test]
    fn long_descriptions_are_truncated_on_a_char_boundary() {
        let long = "é".repeat(MAX_DESCRIPTION + 50);
        let body = json!({ "error": long });
        let SurfaceError::Api(text) = map_error(StatusCode::BAD_REQUEST, Some(&body)) else {
            panic!("expected an API error");
        };
        assert_eq!(text.chars().count(), MAX_DESCRIPTION + 1);
        assert!(text.ends_with('…'));
    }

    #[test]
    fn message_reads_rest_json() {
        let message: Message = serde_json::from_value(json!({
            "_id": "m1",
            "rid": "C1",
            "msg": "hi @helper",
            "ts": "2026-09-30T01:02:03.456Z",
            "u": { "_id": "u1", "username": "alice", "name": "Alice" },
            "tmid": "m0",
            "bot": { "i": "integration" },
            "editedAt": "2026-09-30T01:03:00.000Z",
            "mentions": [{ "_id": "b1", "username": "helper", "type": "user" }],
            "files": [{ "_id": "f1", "name": "a.png", "type": "image/png", "size": 12 }],
            "file": { "_id": "f1", "name": "a.png" },
            "_updatedAt": "2026-09-30T01:03:00.000Z",
        }))
        .unwrap();
        assert_eq!(message.id.as_str(), "m1");
        assert_eq!(message.room.as_str(), "C1");
        assert_eq!(message.sender.name.as_deref(), Some("Alice"));
        assert_eq!(message.sent_at, datetime!(2026-09-30 01:02:03.456 UTC));
        assert_eq!(
            message.thread_root.as_ref().map(MessageId::as_str),
            Some("m0")
        );
        assert!(message.bot);
        assert!(message.edited);
        assert_eq!(message.mentions, [UserId::from("b1")]);
        assert_eq!(
            message.files,
            [FileRef {
                id: "f1".into(),
                name: "a.png".into(),
                mime_type: Some("image/png".into()),
                size: Some(12),
            }]
        );
    }

    #[test]
    fn message_reads_realtime_ejson() {
        let message: Message = serde_json::from_value(json!({
            "_id": "m2",
            "rid": "D1",
            "ts": { "$date": 1_790_000_000_123_i64 },
            "u": { "_id": "u1" },
            "t": "uj",
            "bot": false,
            "editedAt": null,
            "file": { "_id": "f2", "name": "old.txt" },
        }))
        .unwrap();
        assert_eq!(message.text, "");
        assert_eq!(message.kind.as_deref(), Some("uj"));
        assert_eq!(
            message.sent_at.unix_timestamp_nanos(),
            1_790_000_000_123_000_000
        );
        assert!(!message.bot);
        assert!(!message.edited);
        assert_eq!(message.sender.username, "");
        assert_eq!(message.files[0].id, "f2");
        assert_eq!(message.files[0].mime_type, None);
    }

    #[test]
    fn message_with_a_bad_timestamp_fails() {
        for ts in [json!("yesterday"), json!(12), json!({ "$date": "x" })] {
            let raw = json!({ "_id": "m", "rid": "r", "ts": ts, "u": { "_id": "u" } });
            assert!(serde_json::from_value::<Message>(raw).is_err());
        }
    }

    #[test]
    fn room_types_read_their_codes() {
        let types: Vec<RoomType> =
            serde_json::from_value(json!(["c", "p", "d", "l", "v"])).unwrap();
        assert_eq!(
            types,
            [
                RoomType::Channel,
                RoomType::Group,
                RoomType::Direct,
                RoomType::Livechat,
                RoomType::Other("v".into()),
            ]
        );
    }

    #[test]
    fn iso_millis_matches_javascript() {
        assert_eq!(
            iso_millis(datetime!(2026-09-30 01:02:03.456789 UTC)),
            "2026-09-30T01:02:03.456Z"
        );
        assert_eq!(
            iso_millis(datetime!(2026-09-30 03:00:00 +02:00)),
            "2026-09-30T01:00:00.000Z"
        );
    }

    #[test]
    fn mime_types_follow_the_extension() {
        let cases = [
            ("a.PNG", "image/png"),
            ("a.jpeg", "image/jpeg"),
            ("a.jpg", "image/jpeg"),
            ("a.gif", "image/gif"),
            ("a.webp", "image/webp"),
            ("a.pdf", "application/pdf"),
            ("a.json", "application/json"),
            ("a.csv", "text/csv"),
            ("a.md", "text/markdown"),
            ("a.txt", "text/plain"),
            ("a.log", "text/plain"),
            ("a", "application/octet-stream"),
            ("a.exe", "application/octet-stream"),
        ];
        for (name, mime) in cases {
            assert_eq!(mime_type(name), mime, "{name}");
        }
    }

    #[test]
    fn bot_passwords_are_random_and_their_2fa_code_is_hex_sha256() {
        let a = BotPassword::generate();
        let b = BotPassword::generate();
        assert_eq!(a.0.expose_secret().len(), PASSWORD_LEN);
        assert_ne!(a.0.expose_secret(), b.0.expose_secret());
        let known = BotPassword(SecretString::from("abc"));
        assert_eq!(
            known.two_factor_code().expose_secret(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn the_default_upload_limit_is_rocket_chats_default() {
        assert_eq!(DEFAULT_MAX_UPLOAD_SIZE, 104_857_600);
    }

    #[test]
    fn headers_with_control_characters_are_refused() {
        assert!(header("ok-token").unwrap().is_sensitive());
        assert!(matches!(header("bad\ntoken"), Err(SurfaceError::Api(_))));
    }
}

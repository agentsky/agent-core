//! A client for the Slack Web API (`https://slack.com/api/<method>`).
//!
//! [`SlackClient`] holds the connection pool and the rate limiter;
//! [`SlackClient::bot`] gives a [`WebApi`] that acts with one binding's bot
//! token. The token goes only in the `Authorization` header, never in a URL
//! or a body, and never in an error or a log line.
//!
//! Requests follow Slack's SDKs (`slackapi/python-slack-sdk`): the `chat.*`
//! methods send a JSON body, every other method a form. Posts and updates
//! never set `link_names` or `parse`: [`render`] leaves unresolved `@names`
//! and code as written, and either flag would let them ping.
//!
//! # Errors
//!
//! Slack answers most failures with HTTP 200 and `{"ok": false, "error":
//! "<code>"}`. [`map_error`] turns the code into a [`SurfaceError`]. A 429
//! (or an `ok: false` with `ratelimited`) is retried after `Retry-After`,
//! at most [`MAX_RETRIES`] times and only while the wait is within
//! [`SlackClient::with_max_retry_wait`]; otherwise it fails with
//! [`SurfaceError::RateLimited`]. Each method is also tagged with its rate
//! limit tier, and calls wait client-side before they would exceed it (see
//! the `limit` module).

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use core_types::{ConversationId, InFile, MessageId, OutFile, SurfaceError, TeamId, UserId};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{StatusCode, Url, redirect};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::limit::{Bucket, Limiter, Tier, TokenKey};
use crate::normalize::{SlackFile, in_file};

/// The result type of the Web API client.
pub type Result<T, E = SurfaceError> = std::result::Result<T, E>;

/// Slack's Web API.
pub const DEFAULT_BASE_URL: &str = "https://slack.com/api/";

/// How often a rate-limited call is retried before it fails.
pub const MAX_RETRIES: u32 = 3;

/// How long a call may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a file upload may take.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// The wait when a 429 carries no usable `Retry-After`.
const DEFAULT_RETRY_WAIT: Duration = Duration::from_secs(1);

/// The longest `Retry-After` believed.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// The default for [`SlackClient::with_max_retry_wait`]: tier quotas count
/// per minute.
const DEFAULT_MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// The page size for `users.list`, `conversations.history` and
/// `conversations.replies`. Slack recommends at most 200.
const PAGE_SIZE: usize = 200;

/// The most pages one paginated read follows, as a guard against a cursor
/// that never ends.
const MAX_PAGES: usize = 1000;

/// Error codes that mean the token is not accepted any more.
const UNAUTHORIZED_CODES: &[&str] = &[
    "account_inactive",
    "invalid_auth",
    "not_authed",
    "token_expired",
    "token_revoked",
];

/// Error codes that mean the bot may not do this.
const FORBIDDEN_CODES: &[&str] = &[
    "access_denied",
    "cannot_reply_to_message",
    "cant_update_message",
    "channel_is_archived",
    "edit_window_closed",
    "ekm_access_denied",
    "is_archived",
    "messages_tab_disabled",
    "method_not_supported_for_channel_type",
    "missing_scope",
    "no_permission",
    "not_allowed_token_type",
    "not_in_channel",
    "not_reactable",
    "org_login_required",
    "restricted_action",
    "restricted_action_non_threadable_channel",
    "restricted_action_read_only_channel",
    "restricted_action_thread_only_channel",
    "team_access_not_granted",
    "user_is_restricted",
];

/// Error codes that mean the conversation, message, user, bot or file
/// doesn't exist, or can't be seen by the bot.
const NOT_FOUND_CODES: &[&str] = &[
    "bot_not_found",
    "channel_not_found",
    "file_deleted",
    "file_not_found",
    "message_not_found",
    "no_reaction",
    "thread_not_found",
    "user_not_found",
    "users_not_found",
];

/// Error codes of Slack's rate limiter.
const RATE_LIMITED_CODES: &[&str] = &["ratelimited", "rate_limited"];

/// A Web API method this client calls, with its rate-limit tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    AuthTest,
    BotsInfo,
    ChatPostEphemeral,
    ChatPostMessage,
    ChatUpdate,
    ConversationsHistory,
    ConversationsInfo,
    ConversationsJoin,
    ConversationsReplies,
    FilesCompleteUploadExternal,
    FilesGetUploadUrlExternal,
    ReactionsAdd,
    ReactionsRemove,
    UsersInfo,
    UsersList,
}

impl Method {
    const fn name(self) -> &'static str {
        match self {
            Self::AuthTest => "auth.test",
            Self::BotsInfo => "bots.info",
            Self::ChatPostEphemeral => "chat.postEphemeral",
            Self::ChatPostMessage => "chat.postMessage",
            Self::ChatUpdate => "chat.update",
            Self::ConversationsHistory => "conversations.history",
            Self::ConversationsInfo => "conversations.info",
            Self::ConversationsJoin => "conversations.join",
            Self::ConversationsReplies => "conversations.replies",
            Self::FilesCompleteUploadExternal => "files.completeUploadExternal",
            Self::FilesGetUploadUrlExternal => "files.getUploadURLExternal",
            Self::ReactionsAdd => "reactions.add",
            Self::ReactionsRemove => "reactions.remove",
            Self::UsersInfo => "users.info",
            Self::UsersList => "users.list",
        }
    }

    /// The tier Slack's SDKs give the method (`MethodsRateLimits` in
    /// `slackapi/java-slack-sdk`).
    const fn tier(self) -> Tier {
        match self {
            Self::AuthTest => Tier::AuthTest,
            Self::ChatPostMessage => Tier::PostMessage,
            Self::UsersList | Self::ReactionsRemove => Tier::Tier2,
            Self::BotsInfo
            | Self::ChatUpdate
            | Self::ConversationsHistory
            | Self::ConversationsInfo
            | Self::ConversationsJoin
            | Self::ConversationsReplies
            | Self::ReactionsAdd => Tier::Tier3,
            Self::ChatPostEphemeral
            | Self::FilesCompleteUploadExternal
            | Self::FilesGetUploadUrlExternal
            | Self::UsersInfo => Tier::Tier4,
        }
    }
}

/// A request body.
enum Body {
    Form(Vec<(&'static str, String)>),
    Json(Value),
}

/// What a sent call came back with.
enum Reply {
    /// `ok: true`, with the raw body.
    Ok(Vec<u8>),
    /// `ok: false` with a code other than a rate limit.
    Failed(Failure),
    /// A 429, or `ok: false` with `ratelimited`.
    RateLimited(Duration),
}

/// An `ok: false` answer's code, and the scope a `missing_scope` names.
struct Failure {
    code: String,
    needed: Option<String>,
}

impl Failure {
    fn into_error(self) -> SurfaceError {
        map_error(&self.code, self.needed.as_deref())
    }
}

/// The fields every Web API answer has.
#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    needed: Option<String>,
}

/// The connection pool, base URL and rate limiter shared by every bot token.
///
/// Cloning is cheap and shares all three.
#[derive(Clone)]
pub struct SlackClient {
    http: reqwest::Client,
    base: Url,
    limiter: Arc<Limiter>,
    max_retry_wait: Duration,
}

impl fmt::Debug for SlackClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlackClient")
            .field("base", &self.base.as_str())
            .field("max_retry_wait", &self.max_retry_wait)
            .finish_non_exhaustive()
    }
}

impl SlackClient {
    /// Builds a client for the Web API at `base_url`, normally
    /// [`DEFAULT_BASE_URL`]. Method names are appended to it.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::Api`] if the URL isn't `http` or `https`, or carries
    /// user info, a query or a fragment.
    pub fn new(base_url: &str) -> Result<Self> {
        let mut base = Url::parse(base_url)
            .map_err(|err| SurfaceError::Api(format!("invalid Slack API URL: {err}")))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err(SurfaceError::Api(
                "invalid Slack API URL: the scheme must be http or https".into(),
            ));
        }
        if !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(SurfaceError::Api(
                "invalid Slack API URL: user info, query and fragment are not allowed".into(),
            ));
        }
        if !base.path().ends_with('/') {
            let path = format!("{}/", base.path());
            base.set_path(&path);
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .redirect(redirect::Policy::none())
            .build()
            .map_err(transport)?;
        Ok(Self {
            http,
            base,
            limiter: Arc::default(),
            max_retry_wait: DEFAULT_MAX_RETRY_WAIT,
        })
    }

    /// Sets the longest `Retry-After` a rate-limited call waits out before
    /// retrying. A longer one fails at once with
    /// [`SurfaceError::RateLimited`]. The default is 60 seconds.
    pub fn with_max_retry_wait(mut self, wait: Duration) -> Self {
        self.max_retry_wait = wait;
        self
    }

    /// A Web API client acting with `token`, a binding's bot token
    /// (`xoxb-…`), sharing this client's pool and limiter.
    pub fn bot(&self, token: SecretString) -> WebApi {
        WebApi {
            client: self.clone(),
            key: TokenKey::of(&token),
            token,
        }
    }

    /// Replies privately to a slash command or an interaction through its
    /// `response_url`, as an ephemeral message only the member sees
    /// (`response_type: ephemeral`). `text` is sent as mrkdwn.
    ///
    /// The URL is a secret, since anyone holding it can post to the
    /// conversation for 30 minutes. It never appears in an error or a log
    /// line. No token is sent.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::NotFound`] once the URL has expired or been used up
    /// (`expired_url`, `used_url`, HTTP 404 or 410),
    /// [`SurfaceError::RateLimited`] on a 429, and [`SurfaceError::Api`]
    /// with Slack's code for anything else.
    pub async fn respond_ephemeral(&self, response_url: &SecretString, text: &str) -> Result<()> {
        let url = Url::parse(response_url.expose_secret())
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .ok_or_else(|| SurfaceError::Api("the response_url is not an http(s) URL".into()))?;
        let body = json!({"response_type": "ephemeral", "text": text});
        let response = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .body(body.to_string())
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status();
        let wait = retry_after(response.headers().get(RETRY_AFTER));
        let bytes = response.bytes().await.map_err(transport)?;
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(SurfaceError::RateLimited {
                retry_after: wait.unwrap_or(DEFAULT_RETRY_WAIT),
            });
        }
        let code = match serde_json::from_slice::<Envelope>(&bytes) {
            Ok(envelope) if envelope.ok => None,
            Ok(envelope) => Some(envelope.error.unwrap_or_default()),
            Err(_) => Some(String::from_utf8_lossy(&bytes).trim().to_owned())
                .filter(|text| !text.is_empty() && text != "ok"),
        };
        let code = code
            .as_deref()
            .map(|code| sanitize_code(code).unwrap_or("unknown_error"));
        match code {
            None if status.is_success() => Ok(()),
            Some(code @ ("expired_url" | "used_url")) => {
                Err(SurfaceError::NotFound(code.to_owned()))
            }
            _ if matches!(status, StatusCode::NOT_FOUND | StatusCode::GONE) => {
                Err(SurfaceError::NotFound(format!("HTTP {}", status.as_u16())))
            }
            Some(code) => Err(map_error(code, None)),
            None => Err(SurfaceError::Api(format!("HTTP {}", status.as_u16()))),
        }
    }
}

/// The Web API, acting with one binding's bot token.
///
/// `Debug` never prints the token.
#[derive(Clone)]
pub struct WebApi {
    client: SlackClient,
    token: SecretString,
    key: TokenKey,
}

impl fmt::Debug for WebApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebApi")
            .field("client", &self.client)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// What `auth.test` says about a token.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct AuthTest {
    /// The workspace's URL, such as `https://example.slack.com/`.
    #[serde(default)]
    pub url: Option<String>,
    /// The workspace's name.
    #[serde(default)]
    pub team: Option<String>,
    /// The workspace.
    pub team_id: TeamId,
    /// The token's user: the bot user, for a bot token.
    pub user_id: UserId,
    /// The bot, for a bot token. `bots.info` on it gives the app's id and
    /// name.
    #[serde(default)]
    pub bot_id: Option<String>,
}

/// A conversation, from `conversations.info` or `conversations.join`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Conversation {
    /// The conversation's id.
    pub id: ConversationId,
    /// Its name, for channels.
    #[serde(default)]
    pub name: Option<String>,
    /// Whether it is a public channel.
    #[serde(default)]
    pub is_channel: bool,
    /// Whether it is a private channel.
    #[serde(default)]
    pub is_group: bool,
    /// Whether it is a one-to-one DM.
    #[serde(default)]
    pub is_im: bool,
    /// Whether it is a group DM.
    #[serde(default)]
    pub is_mpim: bool,
    /// Whether it is private.
    #[serde(default)]
    pub is_private: bool,
    /// Whether it is archived.
    #[serde(default)]
    pub is_archived: bool,
    /// Whether the bot is a member.
    #[serde(default)]
    pub is_member: bool,
}

/// A user's profile fields that name them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// The display name the member chose; may be empty.
    pub display_name: Option<String>,
    /// The member's full name.
    pub real_name: Option<String>,
}

/// A user, from `users.info` or `users.list`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct User {
    /// The user's id.
    pub id: UserId,
    /// The workspace the user belongs to.
    #[serde(default)]
    pub team_id: Option<TeamId>,
    /// The username (a legacy handle).
    #[serde(default)]
    pub name: Option<String>,
    /// The member's full name, outside the profile.
    #[serde(default)]
    pub real_name: Option<String>,
    /// Whether the account is deactivated.
    #[serde(default)]
    pub deleted: bool,
    /// Whether this is a bot user.
    #[serde(default)]
    pub is_bot: bool,
    /// The profile.
    #[serde(default)]
    pub profile: Profile,
}

/// A bot, from `bots.info`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Bot {
    /// The bot's id (`B…`).
    pub id: String,
    /// The bot's name.
    #[serde(default)]
    pub name: Option<String>,
    /// The app the bot belongs to.
    #[serde(default)]
    pub app_id: Option<String>,
    /// The bot's user, which a current Slack app's bot has and a legacy
    /// integration doesn't.
    #[serde(default)]
    pub user_id: Option<UserId>,
    /// Whether the bot was removed.
    #[serde(default)]
    pub deleted: bool,
}

/// A message read with `conversations.history` or `conversations.replies`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The message's `ts`, its id in the conversation.
    pub ts: MessageId,
    /// The sender's user id, when the message has one.
    pub user: Option<UserId>,
    /// The sending bot's id, for a bot's message.
    pub bot_id: Option<String>,
    /// Whether a bot sent it: it has a `bot_id` or `bot_profile`, or the
    /// `bot_message` subtype.
    pub is_bot: bool,
    /// The subtype, if any.
    pub subtype: Option<String>,
    /// The text as Slack stores it.
    pub text: String,
    /// The thread the message is in, if any.
    pub thread_ts: Option<MessageId>,
    /// The files the bot can download.
    pub files: Vec<InFile>,
}

#[derive(Deserialize)]
struct RawMessage {
    ts: String,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    bot_profile: Option<IgnoredAny>,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    files: Vec<SlackFile>,
}

impl From<RawMessage> for Message {
    fn from(raw: RawMessage) -> Self {
        let is_bot = raw.bot_id.is_some()
            || raw.bot_profile.is_some()
            || raw.subtype.as_deref() == Some("bot_message");
        Self {
            ts: raw.ts.into(),
            user: raw.user.filter(|user| !user.is_empty()).map(Into::into),
            bot_id: raw.bot_id.filter(|bot| !bot.is_empty()),
            is_bot,
            subtype: raw.subtype,
            text: raw.text.unwrap_or_default(),
            thread_ts: raw.thread_ts.map(Into::into),
            files: raw.files.into_iter().filter_map(in_file).collect(),
        }
    }
}

/// One page of messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessagesPage {
    /// The messages: newest first from `conversations.history`, oldest
    /// first from `conversations.replies`, as Slack returns them.
    pub messages: Vec<Message>,
    /// The cursor for the next page, or `None` on the last one.
    pub next_cursor: Option<String>,
}

/// Which page of a conversation or thread to read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageRequest<'a> {
    /// Only messages older than this `ts`.
    pub latest: Option<&'a str>,
    /// The cursor from the previous page.
    pub cursor: Option<&'a str>,
    /// The page size; 0 means the default of 200. Slack caps it at 999.
    pub limit: usize,
}

/// One page of `users.list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsersPage {
    /// The members on this page.
    pub members: Vec<User>,
    /// The cursor for the next page, or `None` on the last one.
    pub next_cursor: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ResponseMetadata {
    next_cursor: Option<String>,
}

impl ResponseMetadata {
    fn cursor(self) -> Option<String> {
        self.next_cursor.filter(|cursor| !cursor.is_empty())
    }
}

#[derive(Deserialize)]
struct MessagesResponse {
    #[serde(default)]
    messages: Vec<RawMessage>,
    #[serde(default)]
    response_metadata: ResponseMetadata,
}

#[derive(Deserialize)]
struct UsersResponse {
    #[serde(default)]
    members: Vec<User>,
    #[serde(default)]
    response_metadata: ResponseMetadata,
}

#[derive(Deserialize)]
struct PostResponse {
    ts: MessageId,
}

#[derive(Deserialize)]
struct EphemeralResponse {
    message_ts: MessageId,
}

#[derive(Deserialize)]
struct ChannelResponse {
    channel: Conversation,
}

#[derive(Deserialize)]
struct UserResponse {
    user: User,
}

#[derive(Deserialize)]
struct BotResponse {
    bot: Bot,
}

#[derive(Deserialize)]
struct UploadUrlResponse {
    upload_url: SecretString,
    file_id: String,
}

impl WebApi {
    /// `auth.test`: who the token belongs to.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::Unauthorized`] if Slack refuses the token.
    pub async fn auth_test(&self) -> Result<AuthTest> {
        self.call(Method::AuthTest, Body::Form(Vec::new()), None)
            .await
    }

    /// `chat.postMessage`: posts `text` (mrkdwn) to `channel`, in the
    /// thread `thread_ts` if given, with link previews off. Returns the new
    /// message's `ts`.
    ///
    /// Neither `link_names` nor `parse` is sent, so only `<@U…>` tokens
    /// mention anyone.
    ///
    /// # Errors
    ///
    /// See [`map_error`].
    pub async fn post_message(
        &self,
        channel: &ConversationId,
        thread_ts: Option<&MessageId>,
        text: &str,
    ) -> Result<MessageId> {
        let mut body = json!({
            "channel": channel,
            "text": text,
            "mrkdwn": true,
            "unfurl_links": false,
        });
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = json!(thread_ts);
        }
        let posted: PostResponse = self
            .call(
                Method::ChatPostMessage,
                Body::Json(body),
                Some(channel.as_str()),
            )
            .await?;
        Ok(posted.ts)
    }

    /// `chat.update`: replaces the text of the bot's message `ts` in
    /// `channel`. Like [`post_message`](Self::post_message), it sends
    /// neither `link_names` nor `parse`.
    ///
    /// # Errors
    ///
    /// See [`map_error`]; `cant_update_message` is
    /// [`SurfaceError::Forbidden`].
    pub async fn update_message(
        &self,
        channel: &ConversationId,
        ts: &MessageId,
        text: &str,
    ) -> Result<()> {
        let body = json!({"channel": channel, "ts": ts, "text": text});
        self.call::<IgnoredAny>(Method::ChatUpdate, Body::Json(body), None)
            .await
            .map(drop)
    }

    /// `chat.postEphemeral`: shows `text` to `user` alone in `channel`, in
    /// the thread `thread_ts` if given. Returns the ephemeral message's
    /// `ts`, which can't be edited or reacted to.
    ///
    /// # Errors
    ///
    /// See [`map_error`]; a user who isn't in the channel gives
    /// `user_not_in_channel` as [`SurfaceError::Api`].
    pub async fn post_ephemeral(
        &self,
        channel: &ConversationId,
        user: &UserId,
        thread_ts: Option<&MessageId>,
        text: &str,
    ) -> Result<MessageId> {
        let mut body = json!({"channel": channel, "user": user, "text": text});
        if let Some(thread_ts) = thread_ts {
            body["thread_ts"] = json!(thread_ts);
        }
        let posted: EphemeralResponse = self
            .call(Method::ChatPostEphemeral, Body::Json(body), None)
            .await?;
        Ok(posted.message_ts)
    }

    /// `reactions.add`: reacts to message `ts` in `channel` with the emoji
    /// `name`, written without colons. A reaction the bot already added
    /// (`already_reacted`) counts as success.
    ///
    /// # Errors
    ///
    /// See [`map_error`].
    pub async fn add_reaction(
        &self,
        channel: &ConversationId,
        ts: &MessageId,
        name: &str,
    ) -> Result<()> {
        let body = reaction_form(channel, ts, name);
        self.call_unit(Method::ReactionsAdd, body, "already_reacted")
            .await
    }

    /// `reactions.remove`: removes the bot's reaction `name` from message
    /// `ts` in `channel`. A reaction that isn't there (`no_reaction`)
    /// counts as success.
    ///
    /// # Errors
    ///
    /// See [`map_error`].
    pub async fn remove_reaction(
        &self,
        channel: &ConversationId,
        ts: &MessageId,
        name: &str,
    ) -> Result<()> {
        let body = reaction_form(channel, ts, name);
        self.call_unit(Method::ReactionsRemove, body, "no_reaction")
            .await
    }

    /// `conversations.history`: one page of a conversation's top level,
    /// newest first.
    ///
    /// # Errors
    ///
    /// See [`map_error`].
    pub async fn history(
        &self,
        channel: &ConversationId,
        page: PageRequest<'_>,
    ) -> Result<MessagesPage> {
        let form = page_form(vec![("channel", channel.to_string())], page);
        self.messages(Method::ConversationsHistory, form).await
    }

    /// `conversations.replies`: one page of the thread rooted at `root`,
    /// oldest first, starting with the root itself.
    ///
    /// # Errors
    ///
    /// See [`map_error`]; `thread_not_found` is [`SurfaceError::NotFound`].
    pub async fn replies(
        &self,
        channel: &ConversationId,
        root: &MessageId,
        page: PageRequest<'_>,
    ) -> Result<MessagesPage> {
        let form = page_form(
            vec![("channel", channel.to_string()), ("ts", root.to_string())],
            page,
        );
        self.messages(Method::ConversationsReplies, form).await
    }

    async fn messages(
        &self,
        method: Method,
        form: Vec<(&'static str, String)>,
    ) -> Result<MessagesPage> {
        let page: MessagesResponse = self.call(method, Body::Form(form), None).await?;
        Ok(MessagesPage {
            messages: page.messages.into_iter().map(Message::from).collect(),
            next_cursor: page.response_metadata.cursor(),
        })
    }

    /// `conversations.info`.
    ///
    /// # Errors
    ///
    /// See [`map_error`].
    pub async fn conversation_info(&self, channel: &ConversationId) -> Result<Conversation> {
        let form = vec![("channel", channel.to_string())];
        let info: ChannelResponse = self
            .call(Method::ConversationsInfo, Body::Form(form), None)
            .await?;
        Ok(info.channel)
    }

    /// `conversations.join`: adds the bot to a public channel. An app hears
    /// only the channels its bot user is in. Private channels need an
    /// invitation instead (`method_not_supported_for_channel_type`).
    ///
    /// # Errors
    ///
    /// See [`map_error`].
    pub async fn join(&self, channel: &ConversationId) -> Result<Conversation> {
        let form = vec![("channel", channel.to_string())];
        let joined: ChannelResponse = self
            .call(Method::ConversationsJoin, Body::Form(form), None)
            .await?;
        Ok(joined.channel)
    }

    /// `users.info`: one user by id. Slack can't look a user up by name;
    /// that is what [`all_users`](Self::all_users) is for.
    ///
    /// # Errors
    ///
    /// See [`map_error`]; `user_not_found` is [`SurfaceError::NotFound`].
    pub async fn user_info(&self, user: &UserId) -> Result<User> {
        let form = vec![("user", user.to_string())];
        let info: UserResponse = self.call(Method::UsersInfo, Body::Form(form), None).await?;
        Ok(info.user)
    }

    /// `users.list`: one page of the workspace's members.
    ///
    /// # Errors
    ///
    /// See [`map_error`].
    pub async fn users_page(&self, cursor: Option<&str>) -> Result<UsersPage> {
        let mut form = vec![("limit", PAGE_SIZE.to_string())];
        if let Some(cursor) = cursor {
            form.push(("cursor", cursor.to_owned()));
        }
        let page: UsersResponse = self.call(Method::UsersList, Body::Form(form), None).await?;
        Ok(UsersPage {
            members: page.members,
            next_cursor: page.response_metadata.cursor(),
        })
    }

    /// Every member of the workspace, following `users.list`'s cursor to
    /// the last page.
    ///
    /// # Errors
    ///
    /// See [`map_error`]. A cursor still going after 1,000 pages is
    /// [`SurfaceError::Api`].
    pub async fn all_users(&self) -> Result<Vec<User>> {
        let mut users = Vec::new();
        let mut cursor = None;
        for _ in 0..MAX_PAGES {
            let page = self.users_page(cursor.as_deref()).await?;
            users.extend(page.members);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(users),
            }
        }
        Err(SurfaceError::Api(
            "users.list returned too many pages".into(),
        ))
    }

    /// `bots.info`: the bot `bot` (`B…`), including its user id when it has
    /// one.
    ///
    /// # Errors
    ///
    /// See [`map_error`]; `bot_not_found` is [`SurfaceError::NotFound`].
    pub async fn bot_info(&self, bot: &str) -> Result<Bot> {
        let form = vec![("bot", bot.to_owned())];
        let info: BotResponse = self.call(Method::BotsInfo, Body::Form(form), None).await?;
        Ok(info.bot)
    }

    /// Uploads `files` into `channel`, in the thread `thread_ts` if given,
    /// with Slack's external upload flow: for each file
    /// `files.getUploadURLExternal` and a `POST` of its raw bytes to the URL
    /// it returns, then one `files.completeUploadExternal` that shares them all
    /// with `channel_id` and `thread_ts`. Returns the file ids.
    ///
    /// The upload URL is presigned: it is treated as a secret, and the bot
    /// token is not sent to it. Nothing is shared if any step fails. No
    /// files means no calls.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::NotFound`] if a file can't be read, and
    /// [`SurfaceError::Api`] if an upload is refused; otherwise see
    /// [`map_error`].
    pub async fn upload_files(
        &self,
        channel: &ConversationId,
        thread_ts: Option<&MessageId>,
        files: &[OutFile],
    ) -> Result<Vec<String>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let mut ids = Vec::with_capacity(files.len());
        let mut shared = Vec::with_capacity(files.len());
        for file in files {
            let data = read_file(file).await?;
            let form = vec![
                ("filename", file.name.clone()),
                ("length", data.len().to_string()),
            ];
            let target: UploadUrlResponse = self
                .call(Method::FilesGetUploadUrlExternal, Body::Form(form), None)
                .await?;
            self.put_bytes(&target.upload_url, data).await?;
            shared.push(json!({"id": target.file_id, "title": file.name}));
            ids.push(target.file_id);
        }
        let mut form = vec![
            ("files", Value::Array(shared).to_string()),
            ("channel_id", channel.to_string()),
        ];
        if let Some(thread_ts) = thread_ts {
            form.push(("thread_ts", thread_ts.to_string()));
        }
        self.call::<IgnoredAny>(Method::FilesCompleteUploadExternal, Body::Form(form), None)
            .await?;
        Ok(ids)
    }

    /// Sends a file's raw bytes to its presigned upload URL without the bot
    /// token, as both of Slack's SDKs do.
    async fn put_bytes(&self, upload_url: &SecretString, data: Vec<u8>) -> Result<()> {
        let url = Url::parse(upload_url.expose_secret())
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https"))
            .ok_or_else(|| {
                SurfaceError::Transport(
                    "files.getUploadURLExternal returned an unusable upload URL".into(),
                )
            })?;
        let response = self
            .client
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(data)
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            Err(SurfaceError::Api(format!(
                "the file upload was refused (HTTP {})",
                status.as_u16()
            )))
        }
    }

    async fn call_unit(&self, method: Method, body: Body, tolerated: &'static str) -> Result<()> {
        match self.send(method, &body, None).await? {
            Ok(_) => Ok(()),
            Err(failure) if failure.code == tolerated => Ok(()),
            Err(failure) => Err(failure.into_error()),
        }
    }

    /// Sends a call and decodes a successful answer as `T`.
    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        body: Body,
        channel: Option<&str>,
    ) -> Result<T> {
        let bytes = self
            .send(method, &body, channel)
            .await?
            .map_err(Failure::into_error)?;
        serde_json::from_slice(&bytes).map_err(|err| {
            SurfaceError::Transport(format!(
                "unexpected response from {} ({:?} error at line {} column {})",
                method.name(),
                err.classify(),
                err.line(),
                err.column()
            ))
        })
    }

    /// Sends a call, waiting for the limiter first and retrying a rate
    /// limit that clears soon enough. Returns the body of an `ok: true`
    /// answer, or the failure of an `ok: false` one.
    async fn send(
        &self,
        method: Method,
        body: &Body,
        channel: Option<&str>,
    ) -> Result<std::result::Result<Vec<u8>, Failure>> {
        let bucket = Bucket::new(self.key, method.name(), channel);
        let mut retries = 0;
        loop {
            let max_wait = self.client.max_retry_wait;
            if let Err(retry_after) = self
                .client
                .limiter
                .acquire(&bucket, method.tier(), max_wait)
                .await
            {
                return Err(SurfaceError::RateLimited { retry_after });
            }
            let response = self
                .request(method, body)?
                .send()
                .await
                .map_err(transport)?;
            match reply(response).await? {
                Reply::RateLimited(wait) => {
                    self.client.limiter.block(&bucket, Instant::now() + wait);
                    if retries >= MAX_RETRIES || wait > self.client.max_retry_wait {
                        tracing::warn!(
                            method = method.name(),
                            retry_after_s = wait.as_secs(),
                            "Slack rate limited a call; giving up"
                        );
                        return Err(SurfaceError::RateLimited { retry_after: wait });
                    }
                    retries += 1;
                    tracing::debug!(
                        method = method.name(),
                        retry_after_s = wait.as_secs(),
                        retries,
                        "Slack rate limited a call; retrying"
                    );
                }
                Reply::Ok(bytes) => return Ok(Ok(bytes)),
                Reply::Failed(failure) => return Ok(Err(failure)),
            }
        }
    }

    fn request(&self, method: Method, body: &Body) -> Result<reqwest::RequestBuilder> {
        let url = self
            .client
            .base
            .join(method.name())
            .map_err(|err| SurfaceError::Api(format!("invalid Slack API URL: {err}")))?;
        let mut auth = HeaderValue::try_from(format!("Bearer {}", self.token.expose_secret()))
            .map_err(|_| SurfaceError::Api("the bot token has invalid header characters".into()))?;
        auth.set_sensitive(true);
        let request = self
            .client
            .http
            .post(url)
            .header(AUTHORIZATION, auth)
            .timeout(REQUEST_TIMEOUT);
        Ok(match body {
            Body::Form(form) => request.form(form),
            Body::Json(json) => request
                .header(CONTENT_TYPE, "application/json; charset=utf-8")
                .body(json.to_string()),
        })
    }
}

fn reaction_form(channel: &ConversationId, ts: &MessageId, name: &str) -> Body {
    Body::Form(vec![
        ("channel", channel.to_string()),
        ("timestamp", ts.to_string()),
        ("name", name.trim_matches(':').to_owned()),
    ])
}

fn page_form(
    mut form: Vec<(&'static str, String)>,
    page: PageRequest<'_>,
) -> Vec<(&'static str, String)> {
    let limit = if page.limit == 0 {
        PAGE_SIZE
    } else {
        page.limit.min(999)
    };
    form.push(("limit", limit.to_string()));
    if let Some(latest) = page.latest {
        form.push(("latest", latest.to_owned()));
        form.push(("inclusive", "false".to_owned()));
    }
    if let Some(cursor) = page.cursor {
        form.push(("cursor", cursor.to_owned()));
    }
    form
}

async fn read_file(file: &OutFile) -> Result<Vec<u8>> {
    tokio::fs::read(&file.path)
        .await
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => SurfaceError::NotFound("file to upload".into()),
            kind => SurfaceError::Transport(format!("could not read the file to upload: {kind}")),
        })
}

/// Reads a Web API answer.
async fn reply(response: reqwest::Response) -> Result<Reply> {
    let status = response.status();
    let wait = retry_after(response.headers().get(RETRY_AFTER));
    let bytes = response.bytes().await.map_err(transport)?;
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Ok(Reply::RateLimited(wait.unwrap_or(DEFAULT_RETRY_WAIT)));
    }
    if !status.is_success() {
        return Err(SurfaceError::Api(format!("HTTP {}", status.as_u16())));
    }
    let envelope: Envelope = serde_json::from_slice(&bytes).map_err(|_| {
        SurfaceError::Transport(format!("unreadable response (HTTP {})", status.as_u16()))
    })?;
    if envelope.ok {
        return Ok(Reply::Ok(bytes.to_vec()));
    }
    let code = envelope
        .error
        .as_deref()
        .and_then(sanitize_code)
        .unwrap_or("unknown_error");
    if RATE_LIMITED_CODES.contains(&code) {
        return Ok(Reply::RateLimited(wait.unwrap_or(DEFAULT_RETRY_WAIT)));
    }
    Ok(Reply::Failed(Failure {
        code: code.to_owned(),
        needed: envelope.needed.filter(|needed| {
            needed.len() <= 200
                && needed
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b".:_-,".contains(&b))
        }),
    }))
}

/// `Retry-After` in whole seconds, as Slack sends it, capped at
/// [`MAX_RETRY_AFTER`] so a hostile value can't overflow a deadline.
fn retry_after(header: Option<&HeaderValue>) -> Option<Duration> {
    header
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds).min(MAX_RETRY_AFTER))
}

/// A Slack error code, if `code` looks like one: at most 64 lowercase
/// letters, digits and underscores. Anything else could be arbitrary text.
fn sanitize_code(code: &str) -> Option<&str> {
    (!code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'))
    .then_some(code)
}

/// Maps a Slack error code from an `ok: false` answer to a
/// [`SurfaceError`]. `needed` is the scope a `missing_scope` answer names.
///
/// - `invalid_auth`, `not_authed`, `token_revoked`, `token_expired` and
///   `account_inactive` are [`SurfaceError::Unauthorized`]: the binding's
///   token is dead.
/// - Codes that mean the bot may not do this (`missing_scope`,
///   `not_in_channel`, `is_archived`, `cant_update_message`,
///   `restricted_action`, …) are [`SurfaceError::Forbidden`] with the code,
///   and for `missing_scope` the scope needed.
/// - Codes that mean something doesn't exist or can't be seen
///   (`channel_not_found`, `message_not_found`, `thread_not_found`,
///   `user_not_found`, `bot_not_found`, …) are [`SurfaceError::NotFound`]
///   with the code.
/// - Anything else is [`SurfaceError::Api`] with the code.
///
/// ```
/// use core_types::SurfaceError;
/// use surface_slack::web::map_error;
///
/// assert_eq!(map_error("token_revoked", None), SurfaceError::Unauthorized);
/// assert_eq!(
///     map_error("missing_scope", Some("chat:write")),
///     SurfaceError::Forbidden("missing_scope (needs chat:write)".into()),
/// );
/// assert_eq!(
///     map_error("channel_not_found", None),
///     SurfaceError::NotFound("channel_not_found".into()),
/// );
/// assert_eq!(map_error("msg_too_long", None), SurfaceError::Api("msg_too_long".into()));
/// ```
pub fn map_error(code: &str, needed: Option<&str>) -> SurfaceError {
    let code = sanitize_code(code).unwrap_or("unknown_error");
    if UNAUTHORIZED_CODES.contains(&code) {
        SurfaceError::Unauthorized
    } else if FORBIDDEN_CODES.contains(&code) {
        match needed {
            Some(needed) => SurfaceError::Forbidden(format!("{code} (needs {needed})")),
            None => SurfaceError::Forbidden(code.to_owned()),
        }
    } else if NOT_FOUND_CODES.contains(&code) {
        SurfaceError::NotFound(code.to_owned())
    } else if RATE_LIMITED_CODES.contains(&code) {
        SurfaceError::RateLimited {
            retry_after: DEFAULT_RETRY_WAIT,
        }
    } else {
        SurfaceError::Api(code.to_owned())
    }
}

/// A transport error with its causes, but without the URL, which may be a
/// presigned upload URL or a `response_url`.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_method_has_a_tier() {
        let cases = [
            (Method::AuthTest, Tier::AuthTest),
            (Method::BotsInfo, Tier::Tier3),
            (Method::ChatPostEphemeral, Tier::Tier4),
            (Method::ChatPostMessage, Tier::PostMessage),
            (Method::ChatUpdate, Tier::Tier3),
            (Method::ConversationsHistory, Tier::Tier3),
            (Method::ConversationsInfo, Tier::Tier3),
            (Method::ConversationsJoin, Tier::Tier3),
            (Method::ConversationsReplies, Tier::Tier3),
            (Method::FilesCompleteUploadExternal, Tier::Tier4),
            (Method::FilesGetUploadUrlExternal, Tier::Tier4),
            (Method::ReactionsAdd, Tier::Tier3),
            (Method::ReactionsRemove, Tier::Tier2),
            (Method::UsersInfo, Tier::Tier4),
            (Method::UsersList, Tier::Tier2),
        ];
        for (method, tier) in cases {
            assert_eq!(method.tier(), tier, "{}", method.name());
        }
    }

    #[test]
    fn error_codes_map_by_meaning() {
        for code in UNAUTHORIZED_CODES {
            assert_eq!(map_error(code, None), SurfaceError::Unauthorized, "{code}");
        }
        for code in FORBIDDEN_CODES {
            assert_eq!(
                map_error(code, None),
                SurfaceError::Forbidden((*code).to_owned())
            );
        }
        for code in NOT_FOUND_CODES {
            assert_eq!(
                map_error(code, None),
                SurfaceError::NotFound((*code).to_owned())
            );
        }
        assert!(matches!(
            map_error("ratelimited", None),
            SurfaceError::RateLimited { .. }
        ));
        assert_eq!(
            map_error("invalid_blocks", None),
            SurfaceError::Api("invalid_blocks".into())
        );
    }

    #[test]
    fn codes_that_are_not_codes_are_not_repeated() {
        assert_eq!(
            map_error("xoxb-123 leaked text", None),
            SurfaceError::Api("unknown_error".into())
        );
        assert_eq!(
            map_error("", None),
            SurfaceError::Api("unknown_error".into())
        );
        assert_eq!(sanitize_code(&"a".repeat(65)), None);
        assert_eq!(
            sanitize_code("channel_not_found"),
            Some("channel_not_found")
        );
    }

    #[test]
    fn retry_after_reads_whole_seconds() {
        let header = HeaderValue::from_static("30");
        assert_eq!(retry_after(Some(&header)), Some(Duration::from_secs(30)));
        let date = HeaderValue::from_static("Wed, 21 Oct 2015 07:28:00 GMT");
        assert_eq!(retry_after(Some(&date)), None);
        assert_eq!(retry_after(None), None);
        let huge = HeaderValue::from_static("18446744073709551615");
        assert_eq!(retry_after(Some(&huge)), Some(MAX_RETRY_AFTER));
    }

    #[test]
    fn page_forms_bound_the_limit_and_exclude_latest() {
        let form = page_form(
            vec![("channel", "C1".into())],
            PageRequest {
                latest: Some("1.5"),
                cursor: Some("abc"),
                limit: 5000,
            },
        );
        assert_eq!(
            form,
            [
                ("channel", "C1".to_owned()),
                ("limit", "999".into()),
                ("latest", "1.5".into()),
                ("inclusive", "false".into()),
                ("cursor", "abc".into()),
            ]
        );
        let default = page_form(Vec::new(), PageRequest::default());
        assert_eq!(default, [("limit", "200".to_owned())]);
    }

    #[test]
    fn base_urls_are_checked_and_get_a_trailing_slash() {
        let client = SlackClient::new("http://localhost:1234/api").unwrap();
        assert_eq!(client.base.as_str(), "http://localhost:1234/api/");
        for bad in [
            "ftp://slack.com/api/",
            "https://user:pw@slack.com/api/",
            "https://slack.com/api/?x=1",
            "https://slack.com/api/#f",
            "not a url",
        ] {
            assert!(
                matches!(SlackClient::new(bad), Err(SurfaceError::Api(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn debug_never_prints_the_token() {
        let api = SlackClient::new(DEFAULT_BASE_URL)
            .unwrap()
            .bot(SecretString::from("xoxb-very-secret"));
        let debug = format!("{api:?}");
        assert!(!debug.contains("very-secret"), "{debug}");
        assert!(debug.contains("REDACTED"));
    }
}

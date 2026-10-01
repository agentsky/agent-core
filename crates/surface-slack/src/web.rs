//! A client for the Slack Web API (`https://slack.com/api/<method>`).
//!
//! [`SlackClient`] holds the connection pool and the rate limiter;
//! [`SlackClient::bot`] gives a [`WebApi`] that acts with one binding's bot
//! token, and [`SlackClient::rotate_config_token`] renews a member's app
//! configuration token. The token goes only in the `Authorization` header, never in a URL
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
//! the `limit` module). A client made with [`WebApi::without_waiting`]
//! does neither: it fails with [`SurfaceError::RateLimited`] instead.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use core_types::{ConversationId, InFile, MessageId, OutFile, SurfaceError, TeamId, UserId};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER};
use reqwest::{StatusCode, Url, redirect};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::{Value, json};
use time::OffsetDateTime;
use tokio::time::Instant;

use crate::limit::{Bucket, Limiter, Tier, TokenKey};
use crate::normalize::{SlackFile, in_files};

/// The result type of the Web API client.
pub type Result<T, E = SurfaceError> = std::result::Result<T, E>;

/// Slack's Web API.
pub const DEFAULT_BASE_URL: &str = "https://slack.com/api/";

/// How often a rate-limited call is retried before it fails.
pub const MAX_RETRIES: u32 = 3;

/// How long a call may take.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an `apps.manifest.*` call may take. Slack sends the new app's
/// `url_verification` challenge while `apps.manifest.create` runs, so it
/// can take longer than other calls, and giving up on one Slack goes on
/// with leaves an app agentd never hears of.
pub const MANIFEST_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// How long a file upload may take.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// The wait when a 429 carries no usable `Retry-After`.
const DEFAULT_RETRY_WAIT: Duration = Duration::from_secs(1);

/// The longest `Retry-After` believed.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// The default for [`SlackClient::with_max_retry_wait`]: tier quotas count
/// per minute.
const DEFAULT_MAX_RETRY_WAIT: Duration = Duration::from_secs(60);

/// The default page size for `conversations.history` and
/// `conversations.replies`. Slack recommends at most 200.
const PAGE_SIZE: usize = 200;

/// The largest page size asked for, and the one `users.list` uses: its
/// Tier 2 quota of 20 calls a minute makes page count what a large
/// workspace's refresh costs.
const MAX_PAGE_SIZE: usize = 999;

/// The most pages one paginated read follows, as a guard against a cursor
/// that never ends.
const MAX_PAGES: usize = 1000;

/// Error codes that mean the token is not accepted any more.
const UNAUTHORIZED_CODES: &[&str] = &[
    "account_inactive",
    "invalid_auth",
    "invalid_refresh_token",
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

/// Error codes with which `apps.manifest.delete` says the app is gone.
const APP_GONE_CODES: &[&str] = &["app_not_found", "invalid_app_id"];

/// Error codes of Slack's rate limiter.
const RATE_LIMITED_CODES: &[&str] = &["ratelimited", "rate_limited"];

/// A Web API method this client calls, with its rate-limit tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    AppsManifestCreate,
    AppsManifestDelete,
    AuthTest,
    BotsInfo,
    ChatPostEphemeral,
    ChatPostMessage,
    ChatUpdate,
    ConversationsHistory,
    ConversationsInfo,
    ConversationsJoin,
    ConversationsOpen,
    ConversationsReplies,
    FilesCompleteUploadExternal,
    FilesGetUploadUrlExternal,
    OauthV2Access,
    ReactionsAdd,
    ReactionsRemove,
    ToolingTokensRotate,
    UsersInfo,
    UsersList,
}

impl Method {
    const fn name(self) -> &'static str {
        match self {
            Self::AppsManifestCreate => "apps.manifest.create",
            Self::AppsManifestDelete => "apps.manifest.delete",
            Self::AuthTest => "auth.test",
            Self::BotsInfo => "bots.info",
            Self::ChatPostEphemeral => "chat.postEphemeral",
            Self::ChatPostMessage => "chat.postMessage",
            Self::ChatUpdate => "chat.update",
            Self::ConversationsHistory => "conversations.history",
            Self::ConversationsInfo => "conversations.info",
            Self::ConversationsJoin => "conversations.join",
            Self::ConversationsOpen => "conversations.open",
            Self::ConversationsReplies => "conversations.replies",
            Self::FilesCompleteUploadExternal => "files.completeUploadExternal",
            Self::FilesGetUploadUrlExternal => "files.getUploadURLExternal",
            Self::OauthV2Access => "oauth.v2.access",
            Self::ReactionsAdd => "reactions.add",
            Self::ReactionsRemove => "reactions.remove",
            Self::ToolingTokensRotate => "tooling.tokens.rotate",
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
            Self::AppsManifestCreate | Self::AppsManifestDelete | Self::ToolingTokensRotate => {
                Tier::Tier1
            }
            Self::UsersList | Self::ReactionsRemove => Tier::Tier2,
            Self::BotsInfo
            | Self::ChatUpdate
            | Self::ConversationsHistory
            | Self::ConversationsInfo
            | Self::ConversationsJoin
            | Self::ConversationsOpen
            | Self::ConversationsReplies
            | Self::ReactionsAdd => Tier::Tier3,
            Self::ChatPostEphemeral
            | Self::FilesCompleteUploadExternal
            | Self::FilesGetUploadUrlExternal
            | Self::OauthV2Access
            | Self::UsersInfo => Tier::Tier4,
        }
    }

    /// How long one request may take.
    const fn timeout(self) -> Duration {
        match self {
            Self::AppsManifestCreate | Self::AppsManifestDelete => MANIFEST_TIMEOUT,
            _ => REQUEST_TIMEOUT,
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

/// An `ok: false` answer's code, the scope a `missing_scope` names, and
/// what an `apps.manifest.*` answer's `errors` say, sanitized.
struct Failure {
    code: String,
    needed: Option<String>,
    problems: Vec<String>,
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
    #[serde(default)]
    errors: Option<Value>,
}

/// The most entries of an `errors` list kept.
const MAX_PROBLEMS: usize = 5;

/// The most characters of one `errors` entry kept.
const MAX_PROBLEM_CHARS: usize = 200;

/// What the `errors` of an `apps.manifest.*` answer say, such as
/// `/settings/event_subscriptions/request_url: URL didn't respond with the
/// value of the challenge parameter.` for each `{"message", "pointer"}`
/// entry: at most [`MAX_PROBLEMS`] entries of at most
/// [`MAX_PROBLEM_CHARS`] printable ASCII characters each, without
/// backticks or angle brackets, so they can be logged and shown safely.
fn problems(errors: Option<&Value>) -> Vec<String> {
    let Some(Value::Array(errors)) = errors else {
        return Vec::new();
    };
    let text = |value: Option<&Value>| {
        value
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .filter(|c| (c.is_ascii_graphic() || *c == ' ') && !matches!(c, '`' | '<' | '>'))
            .collect::<String>()
    };
    errors
        .iter()
        .take(MAX_PROBLEMS)
        .filter_map(|error| {
            let message = text(error.get("message"));
            let pointer = text(error.get("pointer"));
            let problem = match (pointer.trim(), message.trim()) {
                ("", "") => return None,
                ("", message) => message.to_owned(),
                (pointer, "") => pointer.to_owned(),
                (pointer, message) => format!("{pointer}: {message}"),
            };
            Some(problem.chars().take(MAX_PROBLEM_CHARS).collect())
        })
        .collect()
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
        let http = http_client(&base, None)?;
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
            auth: Auth::Bearer(token),
            waits: true,
        }
    }

    /// `tooling.tokens.rotate`: exchanges an app configuration token's
    /// `refresh_token` for a new configuration token and refresh token.
    /// The refresh token is used up: only the returned one works from now
    /// on, so store it before anything else.
    ///
    /// No token is sent in `Authorization`; the refresh token goes in the
    /// form body, never in the URL, and never in an error or a log line.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::Unauthorized`] when Slack refuses the refresh token
    /// (`invalid_refresh_token`, among others); otherwise see
    /// [`map_error`].
    pub async fn rotate_config_token(&self, refresh_token: &SecretString) -> Result<ConfigToken> {
        let api = WebApi {
            client: self.clone(),
            key: TokenKey::of(refresh_token),
            auth: Auth::None,
            waits: true,
        };
        let form = vec![("refresh_token", refresh_token.expose_secret().to_owned())];
        let rotated: RotateResponse = api
            .call(Method::ToolingTokensRotate, Body::Form(form), None)
            .await?;
        let expires_at = OffsetDateTime::from_unix_timestamp(rotated.exp).map_err(|_| {
            SurfaceError::Transport("tooling.tokens.rotate returned an invalid exp".into())
        })?;
        Ok(ConfigToken {
            token: rotated.token,
            refresh_token: rotated.refresh_token,
            team: rotated.team_id,
            user: rotated.user_id,
            expires_at,
        })
    }

    /// `apps.manifest.create`: creates an app from `manifest`, acting as the
    /// member whose app configuration token `config_token` is. Slack
    /// verifies the manifest's events URL with a `url_verification`
    /// challenge while the call runs, so the request may take
    /// [`MANIFEST_TIMEOUT`].
    ///
    /// The token goes only in `Authorization: Bearer`, and the manifest as
    /// JSON in the form body. An `ok: true` answer that names an app but
    /// can't be read in full has the app deleted again with the same token,
    /// since nothing could use it.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::Unauthorized`] when Slack refuses the token (it
    /// expired, or was revoked); [`SurfaceError::Api`] with Slack's code,
    /// such as `invalid_manifest`, for a manifest it refuses (what its
    /// `errors` say is logged); [`SurfaceError::Transport`] for an answer
    /// that can't be read; otherwise see [`map_error`].
    pub async fn create_app(
        &self,
        config_token: &SecretString,
        manifest: &Value,
    ) -> Result<CreatedApp> {
        let method = Method::AppsManifestCreate;
        let form = vec![("manifest", manifest.to_string())];
        let bytes = self
            .config_api(config_token)
            .send(method, &Body::Form(form), None)
            .await?
            .map_err(Failure::into_error)?;
        let created = decode::<CreateAppResponse>(method, &bytes).and_then(|created| {
            if created.app_id.is_empty() || created.credentials.client_id.is_empty() {
                Err(SurfaceError::Transport(
                    "apps.manifest.create returned no app id or client id".into(),
                ))
            } else {
                Ok(created)
            }
        });
        match created {
            Ok(created) => Ok(CreatedApp {
                app_id: created.app_id,
                client_id: created.credentials.client_id,
                client_secret: created.credentials.client_secret,
                signing_secret: created.credentials.signing_secret,
            }),
            Err(err) => {
                let app_id = serde_json::from_slice::<AppIdResponse>(&bytes)
                    .ok()
                    .and_then(|named| named.app_id)
                    .filter(|app_id| {
                        !app_id.is_empty() && app_id.bytes().all(|b| b.is_ascii_alphanumeric())
                    });
                if let Some(app_id) = app_id {
                    match self.delete_app(config_token, &app_id).await {
                        Ok(()) | Err(SurfaceError::NotFound(_)) => {
                            tracing::warn!(
                                app_id,
                                "deleted an app Slack created but answered for unreadably"
                            );
                        }
                        Err(delete) => {
                            tracing::warn!(app_id, error = %delete, "couldn't delete an app Slack created but answered for unreadably");
                        }
                    }
                }
                Err(err)
            }
        }
    }

    /// `apps.manifest.delete`: deletes the app `app_id`, its bot user and
    /// its installations, acting as the member whose app configuration
    /// token `config_token` is.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::NotFound`] when the app is gone already
    /// (`app_not_found`, `invalid_app_id`); otherwise as for
    /// [`create_app`](Self::create_app).
    pub async fn delete_app(&self, config_token: &SecretString, app_id: &str) -> Result<()> {
        let form = vec![("app_id", app_id.to_owned())];
        match self
            .config_api(config_token)
            .send(Method::AppsManifestDelete, &Body::Form(form), None)
            .await?
        {
            Ok(_) => Ok(()),
            Err(failure) if APP_GONE_CODES.contains(&failure.code.as_str()) => {
                Err(SurfaceError::NotFound(failure.code))
            }
            Err(failure) => Err(failure.into_error()),
        }
    }

    /// `oauth.v2.access`: exchanges the `code` an install's OAuth redirect
    /// carried for the app's bot token. `client_id` and `client_secret` are
    /// the app's, sent in `Authorization: Basic`; `redirect_url` must be the
    /// one the install link named.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::Api`] with Slack's code (`invalid_code`,
    /// `bad_redirect_uri`, …) for a refused exchange, and
    /// [`SurfaceError::Transport`] when the answer carries no bot token.
    pub async fn install_app(
        &self,
        client_id: &str,
        client_secret: &SecretString,
        code: &SecretString,
        redirect_url: &str,
    ) -> Result<Installation> {
        let api = WebApi {
            client: self.clone(),
            key: TokenKey::of(client_secret),
            auth: Auth::Basic {
                client_id: client_id.to_owned(),
                client_secret: client_secret.clone(),
            },
            waits: true,
        };
        let form = vec![
            ("code", code.expose_secret().to_owned()),
            ("redirect_uri", redirect_url.to_owned()),
        ];
        let installed: InstallResponse = api
            .call(Method::OauthV2Access, Body::Form(form), None)
            .await?;
        if installed
            .token_type
            .as_deref()
            .is_some_and(|kind| kind != "bot")
            || installed.access_token.expose_secret().is_empty()
            || installed.bot_user_id.as_str().is_empty()
        {
            return Err(SurfaceError::Transport(
                "oauth.v2.access returned no bot token".into(),
            ));
        }
        Ok(Installation {
            app_id: installed.app_id,
            team: installed.team.id,
            bot_user: installed.bot_user_id,
            bot_token: installed.access_token,
            scopes: installed
                .scope
                .split(',')
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    /// A client acting with a member's app configuration token.
    fn config_api(&self, config_token: &SecretString) -> WebApi {
        WebApi {
            client: self.clone(),
            key: TokenKey::of(config_token),
            auth: Auth::Bearer(config_token.clone()),
            waits: true,
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
        if status.is_server_error() {
            return Err(SurfaceError::Transport(format!("HTTP {}", status.as_u16())));
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
    auth: Auth,
    key: TokenKey,
    waits: bool,
}

/// How a [`WebApi`] authenticates its calls.
#[derive(Clone)]
enum Auth {
    /// Not at all, as `tooling.tokens.rotate` does.
    None,
    /// With a token in `Authorization: Bearer`: a bot token, or a member's
    /// configuration token.
    Bearer(SecretString),
    /// With an app's client id and secret in `Authorization: Basic`, as
    /// `oauth.v2.access` does.
    Basic {
        client_id: String,
        client_secret: SecretString,
    },
}

impl fmt::Debug for WebApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebApi")
            .field("client", &self.client)
            .field("waits", &self.waits)
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

/// An app configuration token and its refresh token, from
/// [`SlackClient::rotate_config_token`]. `Debug` redacts both.
#[derive(Debug)]
pub struct ConfigToken {
    /// The configuration token (`xoxe.xoxp-…`), which calls the
    /// `apps.manifest.*` methods.
    pub token: SecretString,
    /// The refresh token (`xoxe-…`) for the next rotation.
    pub refresh_token: SecretString,
    /// The workspace the token acts in.
    pub team: TeamId,
    /// The member the token acts as.
    pub user: UserId,
    /// When `token` stops working. Configuration tokens last 12 hours.
    pub expires_at: OffsetDateTime,
}

/// An app `apps.manifest.create` created, from
/// [`SlackClient::create_app`]. `Debug` redacts both secrets.
#[derive(Debug)]
pub struct CreatedApp {
    /// The app's id (`A…`).
    pub app_id: String,
    /// The app's OAuth client id.
    pub client_id: String,
    /// The app's OAuth client secret, which `oauth.v2.access` needs.
    pub client_secret: SecretString,
    /// The secret the app's requests are signed with.
    pub signing_secret: SecretString,
}

/// An app's installation in a workspace, from `oauth.v2.access`
/// ([`SlackClient::install_app`]). `Debug` redacts the token.
#[derive(Debug)]
pub struct Installation {
    /// The app that was installed.
    pub app_id: String,
    /// The workspace it was installed in.
    pub team: TeamId,
    /// The app's bot user.
    pub bot_user: UserId,
    /// The bot token (`xoxb-…`).
    pub bot_token: SecretString,
    /// The bot scopes the install granted.
    pub scopes: Vec<String>,
}

/// A conversation, from `conversations.info`, `conversations.join` or
/// `conversations.open`.
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
            files: in_files(raw.files),
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
struct RawMessagesResponse {
    #[serde(default)]
    messages: Vec<Value>,
}

#[derive(Deserialize)]
struct UsersResponse {
    #[serde(default)]
    members: Vec<User>,
    #[serde(default)]
    response_metadata: ResponseMetadata,
}

#[derive(Deserialize)]
struct CreateAppResponse {
    app_id: String,
    credentials: AppCredentials,
}

#[derive(Deserialize)]
struct AppIdResponse {
    #[serde(default)]
    app_id: Option<String>,
}

#[derive(Deserialize)]
struct AppCredentials {
    client_id: String,
    client_secret: SecretString,
    signing_secret: SecretString,
}

#[derive(Deserialize)]
struct InstallResponse {
    app_id: String,
    #[serde(default)]
    token_type: Option<String>,
    access_token: SecretString,
    bot_user_id: UserId,
    team: IdObject<TeamId>,
    #[serde(default)]
    scope: String,
}

#[derive(Deserialize)]
struct IdObject<T> {
    id: T,
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
struct RotateResponse {
    token: SecretString,
    refresh_token: SecretString,
    team_id: TeamId,
    user_id: UserId,
    exp: i64,
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

    /// `chat.postMessage` with Block Kit `blocks`: posts them to the top
    /// level of `channel`, with `text` (mrkdwn) as the notification's and
    /// screen readers' fallback, and link previews off. Returns the new
    /// message's `ts`. Like [`post_message`](Self::post_message), it sends
    /// neither `link_names` nor `parse`.
    ///
    /// # Errors
    ///
    /// See [`map_error`]; `invalid_blocks` is [`SurfaceError::Api`].
    pub async fn post_blocks(
        &self,
        channel: &ConversationId,
        text: &str,
        blocks: &Value,
    ) -> Result<MessageId> {
        let body = json!({
            "channel": channel,
            "text": text,
            "blocks": blocks,
            "unfurl_links": false,
            "unfurl_media": false,
        });
        let posted: PostResponse = self
            .call(
                Method::ChatPostMessage,
                Body::Json(body),
                Some(channel.as_str()),
            )
            .await?;
        Ok(posted.ts)
    }

    /// `chat.update` with Block Kit `blocks`: replaces the bot's message
    /// `ts` in `channel` with them, and its fallback text with `text`.
    ///
    /// # Errors
    ///
    /// As for [`update_message`](Self::update_message).
    pub async fn update_blocks(
        &self,
        channel: &ConversationId,
        ts: &MessageId,
        text: &str,
        blocks: &Value,
    ) -> Result<()> {
        let body = json!({"channel": channel, "ts": ts, "text": text, "blocks": blocks});
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

    /// The message `ts` in `channel`, as Slack stores it, in the thread
    /// rooted at `root` when it is a thread reply, read with
    /// `conversations.replies` there or `conversations.history` at the top
    /// level, between `ts` and `ts` inclusive. `None` if Slack has no
    /// message with exactly that `ts`.
    ///
    /// The message is returned whole, `blocks`, `files`, `thread_ts`,
    /// `edited` and all, for [`normalize::read_back`](crate::normalize::read_back).
    ///
    /// # Errors
    ///
    /// See [`map_error`]; `thread_not_found` is [`SurfaceError::NotFound`].
    pub async fn message(
        &self,
        channel: &ConversationId,
        root: Option<&MessageId>,
        ts: &MessageId,
    ) -> Result<Option<Value>> {
        let mut form = vec![("channel", channel.to_string())];
        let method = match root {
            Some(root) => {
                form.push(("ts", root.to_string()));
                Method::ConversationsReplies
            }
            None => Method::ConversationsHistory,
        };
        form.extend([
            ("oldest", ts.to_string()),
            ("latest", ts.to_string()),
            ("inclusive", "true".to_owned()),
            ("limit", "2".to_owned()),
        ]);
        let page: RawMessagesResponse = self.call(method, Body::Form(form), None).await?;
        Ok(page
            .messages
            .into_iter()
            .find(|message| message.get("ts").and_then(Value::as_str) == Some(ts.as_str())))
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

    /// `conversations.open`: the bot's direct message with `user`, opened if
    /// there is none yet. Needs the `im:write` scope.
    ///
    /// # Errors
    ///
    /// See [`map_error`]; `user_not_found` is [`SurfaceError::NotFound`].
    pub async fn open_dm(&self, user: &UserId) -> Result<ConversationId> {
        let form = vec![("users", user.to_string())];
        let opened: ChannelResponse = self
            .call(Method::ConversationsOpen, Body::Form(form), None)
            .await?;
        Ok(opened.channel.id)
    }

    /// Downloads a file a message carried ([`InFile::url`], Slack's
    /// `url_private_download`) with the bot token, reading at most
    /// `max_bytes`. Needs the `files:read` scope.
    ///
    /// The token is sent only to Slack: an `https` URL on `slack.com` or one
    /// of its subdomains, or the origin of this client's API URL. Redirects
    /// are not followed, since Slack answers a request it doesn't accept
    /// with a redirect to its sign-in page.
    ///
    /// # Errors
    ///
    /// [`SurfaceError::TooLarge`] if the file is larger than `max_bytes`;
    /// [`SurfaceError::Api`] if the URL isn't Slack's, or Slack answers
    /// anything but 200; [`SurfaceError::Transport`] if the download fails.
    /// No error repeats the URL.
    pub async fn download_file(&self, file: &InFile, max_bytes: u64) -> Result<Vec<u8>> {
        let url = Url::parse(&file.url)
            .ok()
            .filter(|url| self.client.may_send_token_to(url))
            .ok_or_else(|| SurfaceError::Api("the file's URL is not a Slack URL".into()))?;
        let too_large =
            || SurfaceError::TooLarge(format!("the file is larger than {max_bytes} bytes"));
        if file.size.is_some_and(|size| size > max_bytes) {
            return Err(too_large());
        }
        let mut response = self
            .authorized(self.client.http.get(url))?
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await
            .map_err(transport)?;
        let status = response.status();
        if status != StatusCode::OK {
            return Err(SurfaceError::Api(format!(
                "the file download was refused (HTTP {})",
                status.as_u16()
            )));
        }
        if response.content_length().is_some_and(|len| len > max_bytes) {
            return Err(too_large());
        }
        let mut data = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport)? {
            data.extend_from_slice(&chunk);
            if data.len() as u64 > max_bytes {
                return Err(too_large());
            }
        }
        Ok(data)
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
        let mut form = vec![("limit", MAX_PAGE_SIZE.to_string())];
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
        } else if status.is_server_error() {
            Err(SurfaceError::Transport(format!(
                "the file upload failed (HTTP {})",
                status.as_u16()
            )))
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
        decode(method, &bytes)
    }

    /// Sends a call, waiting for the limiter first and retrying a rate
    /// limit that clears soon enough, unless this client
    /// [doesn't wait](Self::without_waiting). Returns the body of an
    /// `ok: true` answer, or the failure of an `ok: false` one.
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
            let limiter = &self.client.limiter;
            let acquired = if self.waits {
                limiter.acquire(&bucket, method.tier(), max_wait).await
            } else {
                limiter.try_now(&bucket, method.tier())
            };
            if let Err(retry_after) = acquired {
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
                    if !self.waits || retries >= MAX_RETRIES || wait > self.client.max_retry_wait {
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
                Reply::Failed(failure) => {
                    if !failure.problems.is_empty() {
                        tracing::warn!(
                            method = method.name(),
                            code = failure.code,
                            problems = ?failure.problems,
                            "Slack refused a call and said why"
                        );
                    }
                    return Ok(Err(failure));
                }
            }
        }
    }

    fn request(&self, method: Method, body: &Body) -> Result<reqwest::RequestBuilder> {
        let url = self
            .client
            .base
            .join(method.name())
            .map_err(|err| SurfaceError::Api(format!("invalid Slack API URL: {err}")))?;
        let request = self
            .authorized(self.client.http.post(url))?
            .timeout(method.timeout());
        Ok(match body {
            Body::Form(form) => request.form(form),
            Body::Json(json) => request
                .header(CONTENT_TYPE, "application/json; charset=utf-8")
                .body(json.to_string()),
        })
    }
}

impl WebApi {
    /// This client, but never waiting on the rate limit: a call over its
    /// tier's quota, or in a bucket a 429 holds, fails at once with
    /// [`SurfaceError::RateLimited`] without being sent, and a 429 isn't
    /// retried. For lookups a flood of forged events could otherwise queue
    /// behind the token's quota.
    pub fn without_waiting(&self) -> Self {
        Self {
            waits: false,
            ..self.clone()
        }
    }

    /// `request` with the bot token in `Authorization`, if this client has
    /// one.
    fn authorized(&self, request: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let value = match &self.auth {
            Auth::None => return Ok(request),
            Auth::Bearer(token) => format!("Bearer {}", token.expose_secret()),
            Auth::Basic {
                client_id,
                client_secret,
            } => format!(
                "Basic {}",
                BASE64.encode(format!("{client_id}:{}", client_secret.expose_secret()))
            ),
        };
        let mut auth = HeaderValue::try_from(value).map_err(|_| {
            SurfaceError::Api("the credential has invalid header characters".into())
        })?;
        auth.set_sensitive(true);
        Ok(request.header(AUTHORIZATION, auth))
    }
}

impl SlackClient {
    /// Whether a bot token may go to `url`: an `https` URL on `slack.com` or
    /// a subdomain, or the API URL's own origin.
    fn may_send_token_to(&self, url: &Url) -> bool {
        let slack = url.scheme() == "https"
            && url.port().is_none()
            && url
                .host_str()
                .is_some_and(|host| host == "slack.com" || host.ends_with(".slack.com"));
        slack || url.origin() == self.base.origin()
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
        page.limit.min(MAX_PAGE_SIZE)
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

/// Decodes the successful answer of `method` as `T`. The error names where
/// the answer stopped making sense, never what it held.
fn decode<T: DeserializeOwned>(method: Method, bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|err| {
        SurfaceError::Transport(format!(
            "unexpected response from {} ({:?} error at line {} column {})",
            method.name(),
            err.classify(),
            err.line(),
            err.column()
        ))
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
    if status.is_server_error() {
        return Err(SurfaceError::Transport(format!("HTTP {}", status.as_u16())));
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
        problems: problems(envelope.errors.as_ref()),
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
/// Builds the client that calls `base`. It honors the system proxy
/// settings unless `base` is on a loopback IP address, as only tests' fakes
/// are: a proxy would read a plain `http` request, the bot token included,
/// and reach the address on its own host. `proxy` is a proxy tests add as
/// if the system had it.
fn http_client(base: &Url, proxy: Option<reqwest::Proxy>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .redirect(redirect::Policy::none());
    if let Some(proxy) = proxy {
        builder = builder.proxy(proxy);
    }
    if base.host_str().is_some_and(core_types::is_loopback_ip_host) {
        builder = builder.no_proxy();
    }
    builder.build().map_err(transport)
}

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

    #[tokio::test]
    async fn a_loopback_api_is_called_without_a_proxy() {
        testkit::proxy::assert_loopback_skips_proxy(|base, proxy| {
            http_client(&Url::parse(base).unwrap(), Some(proxy)).unwrap()
        })
        .await;
    }

    #[test]
    fn every_method_has_a_tier() {
        let cases = [
            (Method::AppsManifestCreate, Tier::Tier1),
            (Method::AppsManifestDelete, Tier::Tier1),
            (Method::AuthTest, Tier::AuthTest),
            (Method::BotsInfo, Tier::Tier3),
            (Method::ChatPostEphemeral, Tier::Tier4),
            (Method::ChatPostMessage, Tier::PostMessage),
            (Method::ChatUpdate, Tier::Tier3),
            (Method::ConversationsHistory, Tier::Tier3),
            (Method::ConversationsInfo, Tier::Tier3),
            (Method::ConversationsJoin, Tier::Tier3),
            (Method::ConversationsOpen, Tier::Tier3),
            (Method::ConversationsReplies, Tier::Tier3),
            (Method::FilesCompleteUploadExternal, Tier::Tier4),
            (Method::FilesGetUploadUrlExternal, Tier::Tier4),
            (Method::OauthV2Access, Tier::Tier4),
            (Method::ReactionsAdd, Tier::Tier3),
            (Method::ReactionsRemove, Tier::Tier2),
            (Method::ToolingTokensRotate, Tier::Tier1),
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
    fn manifest_problems_are_sanitized_and_bounded() {
        let errors = json!([
            {
                "code": "failed_to_verify",
                "message": "URL didn't respond with the value of the challenge parameter.",
                "pointer": "/settings/event_subscriptions/request_url",
            },
            {"message": "`<b>` bad\n"},
            {"code": "no_text"},
            "not an object",
            {"pointer": "/display_information/name"},
        ]);
        assert_eq!(
            problems(Some(&errors)),
            [
                "/settings/event_subscriptions/request_url: URL didn't respond with the value of \
                 the challenge parameter.",
                "b bad",
                "/display_information/name",
            ]
        );
        let many = Value::Array(vec![json!({"message": "x".repeat(500)}); 9]);
        let kept = problems(Some(&many));
        assert_eq!(kept.len(), MAX_PROBLEMS);
        assert!(
            kept.iter()
                .all(|problem| problem.len() == MAX_PROBLEM_CHARS)
        );
        assert!(problems(Some(&json!("errors"))).is_empty());
        assert!(problems(None).is_empty());
    }

    #[test]
    fn manifest_calls_may_take_longer() {
        assert_eq!(Method::AppsManifestCreate.timeout(), MANIFEST_TIMEOUT);
        assert_eq!(Method::AppsManifestDelete.timeout(), MANIFEST_TIMEOUT);
        assert_eq!(Method::ChatPostMessage.timeout(), REQUEST_TIMEOUT);
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

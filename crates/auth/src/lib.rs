//! Claude PKCE linking, token exchange, refresh and plan lookup for agent-core.
//!
//! [`Auth`] links a member's Claude subscription with the OAuth PKCE flow
//! Claude Code uses for a pasted code, and keeps the tokens fresh:
//!
//! 1. [`Auth::start_login`] draws a random verifier and a separate random
//!    `state`, stores the verifier sealed as a pending login for
//!    [`PENDING_LOGIN_TTL`], and returns the authorize URL. The URL carries
//!    only the S256 challenge; the verifier never leaves agentd.
//! 2. The member approves, and Anthropic's callback page shows `code#state`.
//! 3. [`Auth::complete_login`] parses what the member pasted, takes the
//!    pending login for that `state`, checks it is theirs and unexpired,
//!    exchanges the code, reads the plan from the profile, and stores the
//!    link.
//! 4. [`TokenSource::access_token`] hands out the access token, refreshing
//!    it first when it expires within [`REFRESH_MARGIN`]. Refreshes are
//!    single-flight per member.
//! 5. [`Auth::logout`] deletes the link and revokes the refresh token.
//!
//! Endpoints, client ID and scopes come from [`OAuthConfig`]. Tokens, codes
//! and verifiers are [`SecretString`]s and appear in no error message or log
//! line.

#![warn(missing_docs)]

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use core_types::MemberId;
use reqwest::Client;
use secrecy::SecretString;
use store::{ClaudeLink, NewClaudeLink, Store, StoreError};
use time::OffsetDateTime;

mod client;
mod config;
mod keyed;
mod pkce;
mod plan;

pub use config::{ConfigError, OAuthConfig};
pub use plan::{Plan, PlanInfo};

use client::Exchange;
use config::Urls;
use keyed::KeyedLocks;

/// How long a started login stays valid.
pub const PENDING_LOGIN_TTL: Duration = Duration::from_secs(10 * 60);

/// An access token that expires within this margin is refreshed before it
/// is handed out, as Claude Code does.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);

/// Which HTTP peer an error came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// Building the HTTP client, before any request.
    Client,
    /// The token endpoint: code exchange or refresh.
    Token,
    /// The profile endpoint.
    Profile,
    /// The revocation endpoint.
    Revoke,
}

impl fmt::Display for Endpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Client => "HTTP client",
            Self::Token => "token endpoint",
            Self::Profile => "profile endpoint",
            Self::Revoke => "revocation endpoint",
        })
    }
}

/// The error returned by [`Auth`] and [`TokenSource`].
///
/// No variant carries a token, code, verifier or anything the member pasted.
/// Response bodies are never included; an OAuth `error` code is kept only
/// when it looks like one (lowercase letters and underscores).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// The configuration is invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The member has no Claude link.
    #[error("no Claude account is linked")]
    NotLinked,
    /// The token endpoint refused to refresh the member's token, so the
    /// link is marked broken and the member must log in again.
    #[error("the Claude link has stopped working; log in again")]
    RelinkRequired {
        /// True only for the call that marked the link broken, so the member
        /// is told once per failure.
        newly_broken: bool,
    },
    /// The pasted text is neither `code#state` nor a callback URL.
    #[error("that is not a login code; paste the whole code#state text")]
    MalformedCode,
    /// No pending login of this member has the pasted `state`: it was
    /// never started, already used, replaced by a newer login, or belongs
    /// to someone else.
    #[error("no pending login matches that code; start a new login")]
    UnknownLogin,
    /// The pending login expired before the code was pasted.
    #[error("the login expired; start a new login")]
    LoginExpired,
    /// The token endpoint rejected the code (HTTP 400, 401 or 403).
    #[error("the token endpoint rejected the code (HTTP {status}{})", code_suffix(.error))]
    CodeRejected {
        /// The HTTP status.
        status: u16,
        /// The OAuth `error` code, if the response had a safe one.
        error: Option<String>,
    },
    /// A request couldn't be sent or its response couldn't be read.
    #[error("{endpoint} request failed: {source}")]
    Http {
        /// The peer.
        endpoint: Endpoint,
        /// The transport error, without its URL.
        source: reqwest::Error,
    },
    /// A peer answered with an unexpected HTTP status.
    #[error("{endpoint} returned HTTP {status}{}", code_suffix(.error))]
    Status {
        /// The peer.
        endpoint: Endpoint,
        /// The HTTP status.
        status: u16,
        /// The OAuth `error` code, if the response had a safe one.
        error: Option<String>,
    },
    /// A peer answered with success but a body this crate can't use.
    #[error("{endpoint} returned an invalid response: {reason}")]
    InvalidResponse {
        /// The peer.
        endpoint: Endpoint,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// The operating system's random number generator failed.
    #[error("the system random number generator failed")]
    Random,
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

fn code_suffix(error: &Option<String>) -> String {
    error
        .as_deref()
        .map(|code| format!(", {code}"))
        .unwrap_or_default()
}

impl AuthError {
    /// Whether a failed refresh means the refresh token is no longer
    /// accepted: the token endpoint answered 400 (`invalid_grant` and the
    /// like), 401 or 403. Anything else (a network failure, a timeout, a
    /// 5xx or 429, an unreadable body) may pass, so it doesn't break the
    /// link.
    fn is_refusal(&self) -> bool {
        matches!(
            self,
            Self::Status {
                endpoint: Endpoint::Token,
                status: 400 | 401 | 403,
                ..
            }
        )
    }
}

/// A started login, as [`Auth::start_login`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginStart {
    /// The authorize URL to send the member, privately.
    pub url: String,
    /// When the login stops being valid.
    pub expires_at: OffsetDateTime,
}

/// A completed login, as [`Auth::complete_login`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Linked {
    /// The plan from the profile, or `None` if the profile couldn't be read.
    /// The link is stored either way; the plan is read again on the next
    /// refresh.
    pub plan: Option<PlanInfo>,
}

/// Hands out a member's Claude access token, for the credential proxy.
#[async_trait]
pub trait TokenSource: Send + Sync {
    /// A valid access token for `member`.
    ///
    /// # Errors
    ///
    /// [`AuthError::NotLinked`] if the member has no link, and
    /// [`AuthError::RelinkRequired`] if it is broken. Implementations may
    /// return other errors when a token can't be obtained right now.
    async fn access_token(&self, member: MemberId) -> Result<SecretString, AuthError>;
}

/// Claude account linking and token upkeep over a [`Store`].
///
/// One `Auth` should serve the whole process, since refreshes are
/// single-flight per member only within one instance. Share it in an `Arc`.
pub struct Auth {
    config: OAuthConfig,
    urls: Urls,
    store: Store,
    http: Client,
    locks: KeyedLocks<MemberId>,
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn needs_refresh(link: &ClaudeLink, now: OffsetDateTime) -> bool {
    link.expires_at <= now + REFRESH_MARGIN
}

impl Auth {
    /// Validates `config` and builds the HTTP client.
    ///
    /// # Errors
    ///
    /// [`AuthError::Config`] if `config` is invalid, [`AuthError::Http`] if
    /// the HTTP client can't be built.
    pub fn new(config: OAuthConfig, store: Store) -> Result<Self, AuthError> {
        config.validate()?;
        let urls = config.urls()?;
        Ok(Self {
            config,
            urls,
            store,
            http: client::build_client()?,
            locks: KeyedLocks::default(),
        })
    }

    /// Starts a login for `member`: stores a pending login and returns the
    /// authorize URL to send them.
    ///
    /// The verifier is 32 random bytes, base64url; the `state` is 32 other
    /// random bytes, so it says nothing about the verifier. A member has at
    /// most one pending login: starting one drops any earlier one, so only
    /// the newest link works.
    ///
    /// # Errors
    ///
    /// [`AuthError::Random`] if the random number generator fails,
    /// [`AuthError::Store`] if the store does.
    pub async fn start_login(&self, member: MemberId) -> Result<LoginStart, AuthError> {
        let verifier = pkce::new_verifier()?;
        let state = pkce::new_state()?;
        let challenge = pkce::challenge(&verifier);
        let expires_at = (now() + PENDING_LOGIN_TTL).truncate_to_second();
        self.store.invalidate_pending_logins(member).await?;
        self.store
            .put_pending_login(&state, member, &verifier, expires_at)
            .await?;
        let url = pkce::authorize_url(
            &self.urls.authorize,
            &self.config.client_id,
            &self.urls.redirect,
            &self.config.scope_param(),
            &challenge,
            &state,
        );
        Ok(LoginStart {
            url: url.into(),
            expires_at,
        })
    }

    /// Completes `member`'s login with the text they pasted: `code#state`,
    /// or the callback URL.
    ///
    /// The pending login for that `state` is used up whatever happens next,
    /// even if it belongs to another member (the code has leaked, so that
    /// member must start again). On success the link is stored, replacing
    /// any earlier one, and the member's other pending logins are dropped.
    /// If the profile can't be read, the link is stored without a plan.
    ///
    /// # Errors
    ///
    /// [`AuthError::MalformedCode`] if the text doesn't parse,
    /// [`AuthError::UnknownLogin`] if no pending login of this member has
    /// that `state`, [`AuthError::LoginExpired`] if it expired,
    /// [`AuthError::CodeRejected`] if the token endpoint refuses the code,
    /// and [`AuthError::Http`], [`AuthError::Status`] or
    /// [`AuthError::InvalidResponse`] if the exchange fails otherwise.
    pub async fn complete_login(
        &self,
        member: MemberId,
        pasted: &SecretString,
    ) -> Result<Linked, AuthError> {
        let pasted = pkce::parse_pasted(pasted)?;
        let pending = self
            .store
            .take_pending_login(&pasted.state)
            .await?
            .ok_or(AuthError::UnknownLogin)?;
        if pending.member != member {
            tracing::warn!(
                %member,
                owner = %pending.member,
                "a member pasted another member's login code; that login is invalidated"
            );
            return Err(AuthError::UnknownLogin);
        }
        if pending.is_expired(now()) {
            return Err(AuthError::LoginExpired);
        }
        let exchange = Exchange {
            code: &pasted.code,
            state: &pasted.state,
            verifier: &pending.verifier,
            redirect_uri: &self.urls.redirect,
            client_id: &self.config.client_id,
        };
        let issued_at = now();
        let tokens = client::exchange_code(&self.http, &self.urls.token, exchange)
            .await
            .map_err(|err| match err {
                AuthError::Status {
                    endpoint: Endpoint::Token,
                    status: status @ (400 | 401 | 403),
                    error,
                } => AuthError::CodeRejected { status, error },
                other => other,
            })?;
        let plan = match self.fetch_plan(&tokens.access_token).await {
            Ok(plan) => Some(plan),
            Err(err) => {
                tracing::warn!(%member, error = %err, "couldn't read the plan after login");
                None
            }
        };
        let stored = plan.clone().unwrap_or_default();
        let link = NewClaudeLink {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token.ok_or(AuthError::InvalidResponse {
                endpoint: Endpoint::Token,
                reason: "no refresh_token",
            })?,
            expires_at: issued_at + tokens.expires_in,
            plan: stored.stored_plan(),
            rate_limit_tier: stored.rate_limit_tier,
        };
        {
            let _guard = self.locks.lock(member).await;
            self.store.put_claude_link(member, &link).await?;
        }
        if let Err(err) = self.store.invalidate_pending_logins(member).await {
            tracing::warn!(%member, error = %err, "couldn't drop the member's other pending logins");
        }
        Ok(Linked { plan })
    }

    /// Reads the plan from the profile, with `access_token` as the Bearer
    /// token.
    ///
    /// # Errors
    ///
    /// [`AuthError::Http`], [`AuthError::Status`] or
    /// [`AuthError::InvalidResponse`] if the profile can't be read. An
    /// unknown organization type is not an error; it becomes
    /// [`Plan::Unknown`].
    pub async fn fetch_plan(&self, access_token: &SecretString) -> Result<PlanInfo, AuthError> {
        client::fetch_profile(&self.http, &self.urls.profile, access_token).await
    }

    /// Deletes `member`'s link, then revokes its refresh token at Anthropic,
    /// as Claude Code's logout does. Returns whether there was a link.
    ///
    /// Revocation is best effort: a failure is logged and the link stays
    /// deleted. A link whose tokens no longer decrypt is still deleted.
    /// A refresh in flight finishes first and can't bring the link back.
    ///
    /// # Errors
    ///
    /// [`AuthError::Store`] if the link can't be deleted.
    pub async fn logout(&self, member: MemberId) -> Result<bool, AuthError> {
        let (existing, deleted) = {
            let _guard = self.locks.lock(member).await;
            let existing = self.store.get_claude_link(member).await;
            (existing, self.store.delete_claude_link(member).await?)
        };
        match existing {
            Ok(Some(link)) => self.revoke(member, &link.refresh_token).await,
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(%member, error = %err, "deleted a link whose tokens couldn't be read; nothing revoked");
            }
        }
        Ok(deleted)
    }

    async fn revoke(&self, member: MemberId, refresh_token: &SecretString) {
        if let Err(err) = client::revoke(
            &self.http,
            &self.urls.revoke,
            refresh_token,
            &self.config.client_id,
        )
        .await
        {
            tracing::warn!(%member, error = %err, "couldn't revoke a refresh token");
        }
    }

    /// The member's link, if it can be used or refreshed.
    async fn live_link(&self, member: MemberId) -> Result<ClaudeLink, AuthError> {
        let link = self
            .store
            .get_claude_link(member)
            .await?
            .ok_or(AuthError::NotLinked)?;
        if link.broken_at.is_some() {
            return Err(AuthError::RelinkRequired {
                newly_broken: false,
            });
        }
        Ok(link)
    }

    /// Refreshes `link`. The caller holds the member's lock.
    async fn refresh(
        &self,
        member: MemberId,
        link: ClaudeLink,
        now: OffsetDateTime,
    ) -> Result<SecretString, AuthError> {
        let refreshed = client::refresh(
            &self.http,
            &self.urls.token,
            &link.refresh_token,
            &self.config.client_id,
            &self.config.scope_param(),
        )
        .await;
        let tokens = match refreshed {
            Ok(tokens) => tokens,
            Err(err) if err.is_refusal() => {
                let newly_broken = self.store.mark_claude_link_broken(member, now).await?;
                tracing::warn!(%member, error = %err, "the token endpoint refused a refresh; the link is broken");
                return Err(AuthError::RelinkRequired { newly_broken });
            }
            Err(err) if link.expires_at > now => {
                tracing::warn!(%member, error = %err, "token refresh failed; using the current token until it expires");
                return Ok(link.access_token);
            }
            Err(err) => {
                tracing::warn!(%member, error = %err, "token refresh failed");
                return Err(err);
            }
        };
        let plan = match self.fetch_plan(&tokens.access_token).await {
            Ok(plan) => plan,
            Err(err) => {
                tracing::warn!(%member, error = %err, "couldn't read the plan after a refresh; keeping the old one");
                PlanInfo::from_stored(link.plan.as_deref(), link.rate_limit_tier.as_deref())
            }
        };
        let updated = NewClaudeLink {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token.unwrap_or(link.refresh_token),
            expires_at: now + tokens.expires_in,
            plan: plan.stored_plan(),
            rate_limit_tier: plan.rate_limit_tier,
        };
        if !self.store.update_claude_link(member, &updated).await? {
            self.revoke(member, &updated.refresh_token).await;
            return Err(AuthError::NotLinked);
        }
        Ok(updated.access_token)
    }
}

#[async_trait]
impl TokenSource for Auth {
    /// The member's access token, refreshed first if it expires within
    /// [`REFRESH_MARGIN`].
    ///
    /// Concurrent calls for one member send at most one refresh: the first
    /// takes the member's lock and refreshes, and the others wait for it and
    /// then find the new token. After every refresh the plan is read again
    /// from the profile and stored with the tokens (the old plan is kept if
    /// the profile can't be read).
    ///
    /// If the token endpoint refuses the refresh token (HTTP 400, 401 or
    /// 403), the link is marked broken and [`AuthError::RelinkRequired`] is
    /// returned, then and on every later call until the member logs in
    /// again. Any other refresh failure leaves the link alone: the current
    /// token is returned while it is still valid, and the error otherwise.
    async fn access_token(&self, member: MemberId) -> Result<SecretString, AuthError> {
        let link = self.live_link(member).await?;
        if !needs_refresh(&link, now()) {
            return Ok(link.access_token);
        }
        let _guard = self.locks.lock(member).await;
        let link = self.live_link(member).await?;
        let now = now();
        if !needs_refresh(&link, now) {
            return Ok(link.access_token);
        }
        self.refresh(member, link, now).await
    }
}

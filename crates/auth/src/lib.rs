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
//!    it first when it expires within [`REFRESH_MARGIN`]. A refresh runs in
//!    its own task, single-flight per member, and finishes even if every
//!    caller waiting for it goes away. When the token endpoint says a refresh
//!    token is dead, the link is marked broken and the member is announced
//!    once on [`Auth::take_relink_notices`].
//! 5. [`Auth::status`] reads whether a member is linked, their plan, and
//!    whether the link is broken, without touching the tokens.
//! 6. [`Auth::logout`] deletes the link and revokes the refresh token.
//!
//! Endpoints, client ID and scopes come from [`OAuthConfig`]. Tokens, codes
//! and verifiers are [`SecretString`]s and appear in no error message or log
//! line.

#![warn(missing_docs)]

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use core_types::MemberId;
use reqwest::Client;
use secrecy::SecretString;
use store::{ClaudeLink, ClaudeTokens, NewClaudeLink, Store, StoreError};
use time::OffsetDateTime;
use tokio::sync::{mpsc, watch};

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

/// After a refresh fails without the refresh token being dead, a member's
/// token isn't refreshed again for this long while it is still valid; the
/// current token is handed out instead.
pub const REFRESH_BACKOFF: Duration = Duration::from_secs(30);

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
///
/// It is `Clone`, because every caller waiting for one refresh gets its
/// result.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum AuthError {
    /// The configuration is invalid.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// The member has no Claude link.
    #[error("no Claude account is linked")]
    NotLinked,
    /// The token endpoint said the member's refresh token is dead, so the
    /// link is marked broken and the member must log in again. The member is
    /// announced once per failure on [`Auth::take_relink_notices`], not
    /// through this error.
    #[error("the Claude link has stopped working; log in again")]
    RelinkRequired,
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
        source: Arc<reqwest::Error>,
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
    /// The task refreshing the token stopped before it had a result, for
    /// example because the runtime is shutting down.
    #[error("the token refresh stopped before it finished")]
    RefreshInterrupted,
    /// The store failed.
    #[error(transparent)]
    Store(Arc<StoreError>),
}

impl From<StoreError> for AuthError {
    fn from(error: StoreError) -> Self {
        Self::Store(Arc::new(error))
    }
}

fn code_suffix(error: &Option<String>) -> String {
    error
        .as_deref()
        .map(|code| format!(", {code}"))
        .unwrap_or_default()
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

/// A member's link as [`Auth::status`] reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkStatus {
    /// Whether the member has a link, working or broken.
    pub linked: bool,
    /// The plan last read from the profile. Empty when the member has no
    /// link or the profile hasn't been read.
    pub plan: PlanInfo,
    /// Whether the link is broken and the member has to log in again.
    pub broken: bool,
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
    inner: Arc<Inner>,
}

/// What a refresh hands every caller waiting for it.
type Outcome = Result<SecretString, AuthError>;

/// The refresh in flight for a member: where it publishes its result. Each
/// caller waiting for it holds a receiver.
type Flight = watch::Sender<Option<Outcome>>;

struct Inner {
    config: OAuthConfig,
    urls: Urls,
    store: Store,
    http: Client,
    locks: KeyedLocks<MemberId>,
    flights: Mutex<HashMap<MemberId, Flight>>,
    failures: Mutex<HashMap<MemberId, Instant>>,
    relink: mpsc::UnboundedSender<MemberId>,
    relink_notices: Mutex<Option<mpsc::UnboundedReceiver<MemberId>>>,
}

/// Work a refresh does after it released the member's lock.
enum Afterwards {
    Nothing,
    /// Read the plan with the new access token and store it on the link of
    /// that generation.
    ReadPlan {
        generation: i64,
        access_token: SecretString,
    },
    /// Revoke a refresh token no link holds any more.
    Revoke(SecretString),
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn needs_refresh(link: &ClaudeLink, now: OffsetDateTime) -> bool {
    link.expires_at <= now + REFRESH_MARGIN
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
        let (relink, relink_notices) = mpsc::unbounded_channel();
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                urls,
                store,
                http: client::build_client()?,
                locks: KeyedLocks::default(),
                flights: Mutex::default(),
                failures: Mutex::default(),
                relink,
                relink_notices: Mutex::new(Some(relink_notices)),
            }),
        })
    }

    /// The members whose link a refresh marked broken, each once per
    /// failure, in order. agentd takes this once at startup and sends each
    /// member the relink notice.
    ///
    /// The refresh task sends the member right after it sets
    /// `claude_links.broken_at`, whether or not any caller is still waiting
    /// for it. Notices queue until they are received. Returns `None` after
    /// the first call.
    pub fn take_relink_notices(&self) -> Option<mpsc::UnboundedReceiver<MemberId>> {
        locked(&self.inner.relink_notices).take()
    }

    /// Starts a login for `member`: stores a pending login and returns the
    /// authorize URL to send them.
    ///
    /// The verifier is 32 random bytes, base64url; the `state` is 32 other
    /// random bytes, so it says nothing about the verifier. A member has at
    /// most one pending login: starting one drops any earlier one in the same
    /// store transaction, so only the newest link works, even when two logins
    /// start at once.
    ///
    /// # Errors
    ///
    /// [`AuthError::Random`] if the random number generator fails,
    /// [`AuthError::Store`] if the store does.
    pub async fn start_login(&self, member: MemberId) -> Result<LoginStart, AuthError> {
        let inner = &self.inner;
        let verifier = pkce::new_verifier()?;
        let state = pkce::new_state()?;
        let challenge = pkce::challenge(&verifier);
        let expires_at = (now() + PENDING_LOGIN_TTL).truncate_to_second();
        inner
            .store
            .put_pending_login(&state, member, &verifier, expires_at)
            .await?;
        let url = pkce::authorize_url(
            &inner.urls.authorize,
            &inner.config.client_id,
            &inner.urls.redirect,
            &inner.config.scope_param(),
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
        let inner = &self.inner;
        let pasted = pkce::parse_pasted(pasted)?;
        let pending = inner
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
            redirect_uri: &inner.urls.redirect,
            client_id: &inner.config.client_id,
        };
        let issued_at = now();
        let tokens = client::exchange_code(&inner.http, &inner.urls.token, exchange)
            .await
            .map_err(|err| match err {
                AuthError::Status {
                    endpoint: Endpoint::Token,
                    status: status @ (400 | 401 | 403),
                    error,
                } => AuthError::CodeRejected { status, error },
                other => other,
            })?;
        let plan = match inner.fetch_plan(&tokens.access_token).await {
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
            let _guard = inner.locks.lock(member).await;
            inner.store.put_claude_link(member, &link, now()).await?;
        }
        if let Err(err) = inner.store.invalidate_pending_logins(member).await {
            tracing::warn!(%member, error = %err, "couldn't drop the member's other pending logins");
        }
        Ok(Linked { plan })
    }

    /// Drops the pending login that the pasted text (as for
    /// [`complete_login`](Self::complete_login)) names by its `state`,
    /// whichever member it belongs to, without using the code. For a code
    /// posted where others can read it: whoever read it can't finish that
    /// login either. Returns whether there was such a login; text that
    /// doesn't parse names none.
    ///
    /// # Errors
    ///
    /// [`AuthError::Store`] if the store fails.
    pub async fn cancel_pasted_login(&self, pasted: &SecretString) -> Result<bool, AuthError> {
        let Ok(pasted) = pkce::parse_pasted(pasted) else {
            return Ok(false);
        };
        Ok(self
            .inner
            .store
            .take_pending_login(&pasted.state)
            .await?
            .is_some())
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
        self.inner.fetch_plan(access_token).await
    }

    /// Whether `member` is linked, the plan last read from their profile,
    /// and whether the link is broken. Reads no token.
    ///
    /// # Errors
    ///
    /// [`AuthError::Store`] if the store fails.
    pub async fn status(&self, member: MemberId) -> Result<LinkStatus, AuthError> {
        let Some(status) = self.inner.store.claude_link_status(member).await? else {
            return Ok(LinkStatus::default());
        };
        Ok(LinkStatus {
            linked: true,
            plan: PlanInfo::from_stored(status.plan.as_deref(), status.rate_limit_tier.as_deref()),
            broken: status.broken_at.is_some(),
        })
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
        let inner = &self.inner;
        let (existing, deleted) = {
            let _guard = inner.locks.lock(member).await;
            let existing = inner.store.get_claude_link(member).await;
            (existing, inner.store.delete_claude_link(member).await?)
        };
        locked(&inner.failures).remove(&member);
        match existing {
            Ok(Some(link)) => inner.revoke(member, &link.refresh_token).await,
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(%member, error = %err, "deleted a link whose tokens couldn't be read; nothing revoked");
            }
        }
        Ok(deleted)
    }

    /// How many callers wait for the refresh in flight for `member`; 0 when
    /// none is in flight. For tests that need every caller to have joined a
    /// refresh before it ends.
    #[doc(hidden)]
    pub fn refresh_waiters(&self, member: MemberId) -> usize {
        locked(&self.inner.flights)
            .get(&member)
            .map_or(0, Flight::receiver_count)
    }

    /// Joins the refresh in flight for `member`, started now if there is
    /// none.
    fn flight(&self, member: MemberId) -> watch::Receiver<Option<Outcome>> {
        let mut flights = locked(&self.inner.flights);
        if let Some(flight) = flights.get(&member) {
            return flight.subscribe();
        }
        let result = Flight::new(None);
        let joined = result.subscribe();
        flights.insert(member, result.clone());
        drop(flights);
        let landing = Landing {
            inner: Arc::clone(&self.inner),
            member,
        };
        tokio::spawn(Arc::clone(&self.inner).refresh(landing, result));
        joined
    }
}

/// Removes a member's flight from [`Inner::flights`] when dropped, so a
/// refresh task that ends in any way, a panic or being dropped before it
/// first runs included, lets the next caller start a new one. The task owns
/// it from the moment it is spawned.
struct Landing {
    inner: Arc<Inner>,
    member: MemberId,
}

impl Drop for Landing {
    fn drop(&mut self) {
        locked(&self.inner.flights).remove(&self.member);
    }
}

impl Inner {
    async fn fetch_plan(&self, access_token: &SecretString) -> Result<PlanInfo, AuthError> {
        client::fetch_profile(&self.http, &self.urls.profile, access_token).await
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
            return Err(AuthError::RelinkRequired);
        }
        Ok(link)
    }

    /// Whether a refresh of `member`'s token failed within
    /// [`REFRESH_BACKOFF`].
    fn backing_off(&self, member: MemberId) -> bool {
        let mut failures = locked(&self.failures);
        match failures.get(&member) {
            Some(failed) if failed.elapsed() < REFRESH_BACKOFF => true,
            Some(_) => {
                failures.remove(&member);
                false
            }
            None => false,
        }
    }

    /// The refresh task: refreshes the token of `landing`'s member under their
    /// lock, hands the result to every waiting caller, then does what is left
    /// without the lock. It runs to the end whether or not anyone still waits.
    async fn refresh(self: Arc<Self>, landing: Landing, result: Flight) {
        let member = landing.member;
        let guard = self.locks.lock(member).await;
        let (outcome, afterwards) = self.refresh_locked(member).await;
        drop(guard);
        drop(landing);
        result.send_replace(Some(outcome));
        match afterwards {
            Afterwards::Nothing => {}
            Afterwards::ReadPlan {
                generation,
                access_token,
            } => self.store_plan(member, generation, &access_token).await,
            Afterwards::Revoke(refresh_token) => self.revoke(member, &refresh_token).await,
        }
    }

    /// Refreshes `member`'s token if it still needs it. The caller holds the
    /// member's lock.
    async fn refresh_locked(&self, member: MemberId) -> (Outcome, Afterwards) {
        let link = match self.live_link(member).await {
            Ok(link) => link,
            Err(err) => return (Err(err), Afterwards::Nothing),
        };
        let started = now();
        if !needs_refresh(&link, started) {
            return (Ok(link.access_token), Afterwards::Nothing);
        }
        if link.expires_at > started && self.backing_off(member) {
            return (Ok(link.access_token), Afterwards::Nothing);
        }
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
            Err(failure) if failure.dead => {
                tracing::warn!(%member, error = %failure.error, "the token endpoint says the refresh token is dead; the link is broken");
                return (self.mark_broken(member, &link).await, Afterwards::Nothing);
            }
            Err(failure) => {
                locked(&self.failures).insert(member, Instant::now());
                if link.expires_at > now() {
                    tracing::warn!(%member, error = %failure.error, "token refresh failed; using the current token until it expires");
                    return (Ok(link.access_token), Afterwards::Nothing);
                }
                tracing::warn!(%member, error = %failure.error, "token refresh failed");
                return (Err(failure.error), Afterwards::Nothing);
            }
        };
        locked(&self.failures).remove(&member);
        let updated = ClaudeTokens {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token.unwrap_or(link.refresh_token),
            expires_at: started + tokens.expires_in,
        };
        match self
            .store
            .update_claude_tokens(member, link.generation, &updated, now())
            .await
        {
            Ok(true) => (
                Ok(updated.access_token.clone()),
                Afterwards::ReadPlan {
                    generation: link.generation,
                    access_token: updated.access_token,
                },
            ),
            Ok(false) => (
                self.link_after_replacement(member).await,
                Afterwards::Revoke(updated.refresh_token),
            ),
            Err(err) => {
                tracing::error!(%member, error = %err, "couldn't store refreshed tokens");
                (Err(err.into()), Afterwards::Nothing)
            }
        }
    }

    /// Marks `link` broken after the token endpoint said its refresh token
    /// is dead, and announces the member if this call broke it.
    async fn mark_broken(&self, member: MemberId, link: &ClaudeLink) -> Outcome {
        let newly_broken = self
            .store
            .mark_claude_link_broken(member, link.generation, now())
            .await?;
        if !newly_broken {
            return self.link_after_replacement(member).await;
        }
        if self.relink.send(member).is_err() {
            tracing::warn!(%member, "nobody receives relink notices");
        }
        Err(AuthError::RelinkRequired)
    }

    /// What to hand out after a write found the link deleted, replaced by a
    /// newer login, or already broken: whatever the store holds now.
    async fn link_after_replacement(&self, member: MemberId) -> Outcome {
        Ok(self.live_link(member).await?.access_token)
    }

    /// Reads the plan with a freshly refreshed access token and stores it on
    /// the link of `generation`. A failure keeps the old plan.
    async fn store_plan(&self, member: MemberId, generation: i64, access_token: &SecretString) {
        let plan = match self.fetch_plan(access_token).await {
            Ok(plan) => plan,
            Err(err) => {
                tracing::warn!(%member, error = %err, "couldn't read the plan after a refresh; keeping the old one");
                return;
            }
        };
        let stored = plan.stored_plan();
        if let Err(err) = self
            .store
            .update_claude_plan(
                member,
                generation,
                stored.as_deref(),
                plan.rate_limit_tier.as_deref(),
            )
            .await
        {
            tracing::warn!(%member, error = %err, "couldn't store the plan after a refresh");
        }
    }
}

#[async_trait]
impl TokenSource for Auth {
    /// The member's access token, refreshed first if it expires within
    /// [`REFRESH_MARGIN`].
    ///
    /// The refresh runs in a task of its own that holds the member's lock,
    /// so it stores the rotated refresh token (or marks the link broken)
    /// even if this call is cancelled. Concurrent calls for one member share
    /// one refresh and all get its result, success or failure. After a
    /// refresh the plan is read again from the profile, after the member's
    /// lock is released, and only the plan is stored (the old plan is kept if
    /// the profile can't be read).
    ///
    /// If the token endpoint says the refresh token is dead (HTTP 400 or 401
    /// with `invalid_grant`, `invalid_client`, `invalid_scope` or
    /// `unauthorized_client`, or an account-on-hold body on 400, 401 or 403),
    /// the link is marked broken, the member is sent on
    /// [`Auth::take_relink_notices`], and [`AuthError::RelinkRequired`] is
    /// returned, then and on every later call until the member logs in
    /// again. Any other refresh failure leaves the link alone: the current
    /// token is returned while it is still valid, and the error otherwise.
    /// For [`REFRESH_BACKOFF`] after such a failure, a still-valid token is
    /// returned without trying again.
    async fn access_token(&self, member: MemberId) -> Result<SecretString, AuthError> {
        let link = self.inner.live_link(member).await?;
        if !needs_refresh(&link, now()) {
            return Ok(link.access_token);
        }
        let mut flight = self.flight(member);
        let outcome = match flight.wait_for(Option::is_some).await {
            Ok(outcome) => outcome.clone(),
            Err(_) => None,
        };
        outcome.unwrap_or(Err(AuthError::RefreshInterrupted))
    }
}

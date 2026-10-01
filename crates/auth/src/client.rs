//! The HTTP calls: code exchange, refresh, profile and revocation, with the
//! request shapes Claude Code 2.1.285 sends.

use std::sync::Arc;
use std::time::Duration;

use reqwest::{Client, StatusCode, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use serde_json::Value;

use crate::config::ALLOWED_SCOPES;
use crate::plan::{PlanInfo, ProfileResponse};
use crate::{AuthError, Endpoint};

/// Claude Code's timeout for the code exchange and refreshes.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
/// Claude Code's timeout for the profile request.
const PROFILE_TIMEOUT: Duration = Duration::from_secs(10);
/// Claude Code's timeout for revocation, which is best effort.
const REVOKE_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Longest `expires_in` accepted, a year, as Claude Code's long-lived
/// tokens have.
const MAX_EXPIRES_IN: u64 = 365 * 24 * 60 * 60;
/// Longest OAuth `error` code kept for an error message.
const MAX_ERROR_CODE: usize = 64;

/// The HTTP clients: one honoring the system proxy settings and one using
/// no proxy, each request taking the one its URL needs.
pub(crate) struct Http {
    proxied: Client,
    direct: Client,
}

impl Http {
    /// The client for `url`: without a proxy where
    /// [`core_types::skips_proxy`] says so (plain `http`, which only tests'
    /// fakes on a loopback IP address use and a proxy would read, codes and
    /// tokens included, or a loopback IP address, which a proxy would reach
    /// on its own host), and with the system's proxy settings otherwise.
    /// Each endpoint is decided on its own.
    fn to(&self, url: &Url) -> &Client {
        if core_types::skips_proxy(url.scheme(), url.host_str().unwrap_or_default()) {
            &self.direct
        } else {
            &self.proxied
        }
    }
}

/// Builds the HTTP clients. They never follow redirects: a 307 or 308 from
/// the token endpoint would otherwise resend a body holding a code or
/// refresh token to wherever it points. `proxy` is a proxy tests add as if
/// the system had it.
pub(crate) fn build_client(proxy: Option<reqwest::Proxy>) -> Result<Http, AuthError> {
    let builder = || {
        Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .user_agent(concat!("agent-core/", env!("CARGO_PKG_VERSION")))
    };
    let build = |builder: reqwest::ClientBuilder| {
        builder.build().map_err(|source| AuthError::Http {
            endpoint: Endpoint::Client,
            source: Arc::new(source.without_url()),
        })
    };
    let proxied = match proxy {
        Some(proxy) => builder().proxy(proxy),
        None => builder(),
    };
    Ok(Http {
        proxied: build(proxied)?,
        direct: build(builder().no_proxy())?,
    })
}

/// Tokens from the token endpoint.
pub(crate) struct Tokens {
    pub(crate) access_token: SecretString,
    /// `None` when a refresh response leaves the refresh token out, meaning
    /// the old one stays valid.
    pub(crate) refresh_token: Option<SecretString>,
    pub(crate) expires_in: Duration,
    pub(crate) grant: Grant,
}

/// What a token response's `scope` says was granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grant {
    /// Only scopes in [`ALLOWED_SCOPES`].
    Allowed,
    /// No `scope`, or a blank one, which RFC 6749 (section 5.1) reads as
    /// what the request asked for: what agentd asked for on a refresh, but
    /// on a login what the authorize URL asked for, which the member could
    /// have changed.
    Unstated,
    /// A scope outside [`ALLOWED_SCOPES`].
    Wider,
    /// A `scope` that isn't a string, which can't be shown to be narrower:
    /// a login counts it as wider, and a refresh, which sent the scopes
    /// itself, as unstated, so a change of format doesn't break every link.
    Unreadable,
}

impl Tokens {
    /// Why a login can't keep these tokens: a grant wider than
    /// [`ALLOWED_SCOPES`] or unreadable ([`AuthError::ScopeRefused`]), or
    /// one that doesn't say ([`AuthError::ScopeUnstated`]), since the member
    /// can add scopes to the authorize URL and the code exchange doesn't
    /// send them again.
    pub(crate) fn login_refusal(&self) -> Option<AuthError> {
        match self.grant {
            Grant::Allowed => None,
            Grant::Unstated => Some(AuthError::ScopeUnstated),
            Grant::Wider | Grant::Unreadable => Some(AuthError::ScopeRefused),
        }
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: SecretString,
    #[serde(default)]
    refresh_token: Option<SecretString>,
    expires_in: f64,
    /// The scopes granted, space-separated. Read as any JSON value, so one
    /// of another type is [`Grant::Unreadable`] rather than failing the
    /// whole response.
    #[serde(default)]
    scope: Option<Value>,
}

impl TokenResponse {
    /// The tokens, checked, with what their `scope` says was granted
    /// ([`Grant`]): the member can add scopes to the authorize URL, and the
    /// server may grant its own, so what was asked for doesn't bound what
    /// came back.
    fn into_tokens(self, endpoint: Endpoint) -> Result<Tokens, AuthError> {
        let invalid = |reason| AuthError::InvalidResponse { endpoint, reason };
        if self.access_token.expose_secret().is_empty() {
            return Err(invalid("empty access_token"));
        }
        let grant = match &self.scope {
            None | Some(Value::Null) => Grant::Unstated,
            Some(Value::String(scope)) if scope.trim_ascii().is_empty() => Grant::Unstated,
            Some(Value::String(scope))
                if scope
                    .split_ascii_whitespace()
                    .all(|granted| ALLOWED_SCOPES.contains(&granted)) =>
            {
                Grant::Allowed
            }
            Some(Value::String(_)) => Grant::Wider,
            Some(_) => Grant::Unreadable,
        };
        let refresh_token = self
            .refresh_token
            .filter(|token| !token.expose_secret().is_empty());
        if !self.expires_in.is_finite() || self.expires_in < 1.0 {
            return Err(invalid("expires_in is not a positive number"));
        }
        let expires_in = Duration::from_secs_f64(self.expires_in.min(MAX_EXPIRES_IN as f64));
        Ok(Tokens {
            access_token: self.access_token,
            refresh_token,
            expires_in,
            grant,
        })
    }
}

#[derive(Serialize)]
struct CodeGrant<'a> {
    grant_type: &'static str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
    state: &'a str,
}

#[derive(Serialize)]
struct RefreshGrant<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    client_id: &'a str,
    scope: &'a str,
}

#[derive(Serialize)]
struct Revocation<'a> {
    token: &'a str,
    token_type_hint: &'static str,
    client_id: &'a str,
}

/// OAuth `error` codes after which a refresh token can't work again, as
/// Claude Code 2.1.285 reads them: `invalid_grant` is its dead refresh token,
/// and the other three are its "expected" refresh failures, which no retry
/// fixes.
const DEAD_TOKEN_CODES: [&str; 4] = [
    "invalid_grant",
    "invalid_client",
    "invalid_scope",
    "unauthorized_client",
];

/// The `error_description` with which the token endpoint says the account is
/// on hold.
const ACCOUNT_ON_HOLD: &str = "account_on_hold";

/// What an error body says, read the way Claude Code 2.1.285 reads it:
/// `error` is the code when it is a string, and `error.type` when it is an
/// object.
struct OAuthErrorBody {
    code: Option<String>,
    description: Option<String>,
}

impl OAuthErrorBody {
    fn parse(body: &[u8]) -> Self {
        let value = serde_json::from_slice::<serde_json::Value>(body).unwrap_or_default();
        let code = match value.get("error") {
            Some(serde_json::Value::String(code)) => Some(code.clone()),
            Some(error) => error
                .get("type")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            None => None,
        };
        let description = value
            .get("error_description")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned);
        Self { code, description }
    }
}

/// The OAuth `error` code of a failed response, if it has one that is safe
/// to put in an error message: short, and only lowercase letters and
/// underscores, as RFC 6749's codes are. `error_description` is never kept.
fn oauth_error_code(body: &[u8]) -> Option<String> {
    let code = OAuthErrorBody::parse(body).code?;
    let safe = !code.is_empty()
        && code.len() <= MAX_ERROR_CODE
        && code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_');
    safe.then_some(code)
}

/// Whether a failed refresh response says the refresh token will never work
/// again: HTTP 400 or 401 with one of [`DEAD_TOKEN_CODES`], or HTTP 400, 401
/// or 403 with an account-on-hold body (`error` `invalid_grant` or
/// `access_denied`, `error_description` `account_on_hold`). Any other
/// response, such as a 403 HTML page from a proxy, is not.
fn token_is_dead(status: StatusCode, body: &[u8]) -> bool {
    let body = OAuthErrorBody::parse(body);
    let code = body.code.as_deref();
    let on_hold = matches!(code, Some("invalid_grant" | "access_denied"))
        && body.description.as_deref() == Some(ACCOUNT_ON_HOLD);
    match status.as_u16() {
        400 | 401 => on_hold || code.is_some_and(|code| DEAD_TOKEN_CODES.contains(&code)),
        403 => on_hold,
        _ => false,
    }
}

/// A failed refresh.
pub(crate) struct RefreshFailure {
    pub(crate) error: AuthError,
    /// Whether the token endpoint said the refresh token is dead
    /// ([`token_is_dead`]), so the link has to be marked broken.
    pub(crate) dead: bool,
}

impl From<AuthError> for RefreshFailure {
    fn from(error: AuthError) -> Self {
        Self { error, dead: false }
    }
}

/// A response read in full: its status and body.
struct Response {
    status: StatusCode,
    body: Vec<u8>,
}

async fn send(request: reqwest::RequestBuilder, endpoint: Endpoint) -> Result<Response, AuthError> {
    let http = |source: reqwest::Error| AuthError::Http {
        endpoint,
        source: Arc::new(source.without_url()),
    };
    let response = request.send().await.map_err(http)?;
    let status = response.status();
    let body = response.bytes().await.map_err(http)?.to_vec();
    Ok(Response { status, body })
}

fn parse<T: DeserializeOwned>(body: &[u8], endpoint: Endpoint) -> Result<T, AuthError> {
    serde_json::from_slice(body).map_err(|_| AuthError::InvalidResponse {
        endpoint,
        reason: "not the expected JSON",
    })
}

fn status_error(endpoint: Endpoint, response: &Response) -> AuthError {
    AuthError::Status {
        endpoint,
        status: response.status.as_u16(),
        error: oauth_error_code(&response.body),
    }
}

/// Parameters of the code exchange.
pub(crate) struct Exchange<'a> {
    pub(crate) code: &'a SecretString,
    pub(crate) state: &'a str,
    pub(crate) verifier: &'a SecretString,
    pub(crate) redirect_uri: &'a Url,
    pub(crate) client_id: &'a str,
}

/// POSTs the `authorization_code` grant as JSON.
pub(crate) async fn exchange_code(
    client: &Http,
    token_url: &Url,
    exchange: Exchange<'_>,
) -> Result<Tokens, AuthError> {
    let body = CodeGrant {
        grant_type: "authorization_code",
        code: exchange.code.expose_secret(),
        redirect_uri: exchange.redirect_uri.as_str(),
        client_id: exchange.client_id,
        code_verifier: exchange.verifier.expose_secret(),
        state: exchange.state,
    };
    let request = client
        .to(token_url)
        .post(token_url.clone())
        .json(&body)
        .timeout(TOKEN_TIMEOUT);
    let response = send(request, Endpoint::Token).await?;
    if !response.status.is_success() {
        return Err(status_error(Endpoint::Token, &response));
    }
    let tokens =
        parse::<TokenResponse>(&response.body, Endpoint::Token)?.into_tokens(Endpoint::Token)?;
    if tokens.refresh_token.is_none() {
        return Err(AuthError::InvalidResponse {
            endpoint: Endpoint::Token,
            reason: "no refresh_token",
        });
    }
    Ok(tokens)
}

/// POSTs the `refresh_token` grant as JSON.
pub(crate) async fn refresh(
    client: &Http,
    token_url: &Url,
    refresh_token: &SecretString,
    client_id: &str,
    scope: &str,
) -> Result<Tokens, RefreshFailure> {
    let body = RefreshGrant {
        grant_type: "refresh_token",
        refresh_token: refresh_token.expose_secret(),
        client_id,
        scope,
    };
    let request = client
        .to(token_url)
        .post(token_url.clone())
        .json(&body)
        .timeout(TOKEN_TIMEOUT);
    let response = send(request, Endpoint::Token).await?;
    if !response.status.is_success() {
        return Err(RefreshFailure {
            error: status_error(Endpoint::Token, &response),
            dead: token_is_dead(response.status, &response.body),
        });
    }
    Ok(parse::<TokenResponse>(&response.body, Endpoint::Token)?.into_tokens(Endpoint::Token)?)
}

/// GETs the profile with the access token and reads the plan from it.
pub(crate) async fn fetch_profile(
    client: &Http,
    profile_url: &Url,
    access_token: &SecretString,
) -> Result<PlanInfo, AuthError> {
    let request = client
        .to(profile_url)
        .get(profile_url.clone())
        .bearer_auth(access_token.expose_secret())
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .timeout(PROFILE_TIMEOUT);
    let response = send(request, Endpoint::Profile).await?;
    if !response.status.is_success() {
        return Err(status_error(Endpoint::Profile, &response));
    }
    Ok(parse::<ProfileResponse>(&response.body, Endpoint::Profile)?.into())
}

/// POSTs a refresh token to the revocation endpoint.
pub(crate) async fn revoke(
    client: &Http,
    revoke_url: &Url,
    refresh_token: &SecretString,
    client_id: &str,
) -> Result<(), AuthError> {
    let body = Revocation {
        token: refresh_token.expose_secret(),
        token_type_hint: "refresh_token",
        client_id,
    };
    let request = client
        .to(revoke_url)
        .post(revoke_url.clone())
        .json(&body)
        .timeout(REVOKE_TIMEOUT);
    let response = send(request, Endpoint::Revoke).await?;
    if !response.status.is_success() {
        return Err(status_error(Endpoint::Revoke, &response));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn each_endpoint_takes_a_proxy_only_for_remote_https() {
        testkit::proxy::assert_proxied_only_elsewhere(
            |base, proxy| {
                build_client(Some(proxy))
                    .unwrap()
                    .to(&Url::parse(base).unwrap())
                    .clone()
            },
            &["https://127.0.0.1:9", "http://[::1]:9"],
        )
        .await;
    }

    fn tokens(json: &str) -> Result<Tokens, AuthError> {
        serde_json::from_str::<TokenResponse>(json)
            .unwrap()
            .into_tokens(Endpoint::Token)
    }

    #[test]
    fn token_responses_are_validated() {
        let ok = tokens(r#"{"access_token":"a","refresh_token":"r","expires_in":28800}"#).unwrap();
        assert_eq!(ok.access_token.expose_secret(), "a");
        assert_eq!(ok.refresh_token.unwrap().expose_secret(), "r");
        assert_eq!(ok.expires_in, Duration::from_secs(28_800));

        let no_refresh =
            tokens(r#"{"access_token":"a","expires_in":60.5,"scope":"user:profile"}"#).unwrap();
        assert!(no_refresh.refresh_token.is_none());
        assert_eq!(no_refresh.expires_in, Duration::from_millis(60_500));

        let empty_refresh =
            tokens(r#"{"access_token":"a","refresh_token":"","expires_in":60}"#).unwrap();
        assert!(empty_refresh.refresh_token.is_none());

        let capped = tokens(r#"{"access_token":"a","expires_in":1e12}"#).unwrap();
        assert_eq!(capped.expires_in, Duration::from_secs(MAX_EXPIRES_IN));

        for json in [
            r#"{"access_token":"","expires_in":60}"#,
            r#"{"access_token":"a","expires_in":0}"#,
            r#"{"access_token":"a","expires_in":-5}"#,
        ] {
            assert!(
                matches!(tokens(json), Err(AuthError::InvalidResponse { .. })),
                "{json}"
            );
        }
    }

    fn grant(scope: &str) -> Grant {
        let json = format!(r#"{{"access_token":"a","expires_in":60,"scope":{scope}}}"#);
        tokens(&json).unwrap().grant
    }

    #[test]
    fn a_grant_wider_than_the_allowed_scopes_is_refused() {
        for scope in [
            r#""user:profile user:inference""#,
            r#""user:inference""#,
            r#"" user:profile  user:inference ""#,
        ] {
            assert_eq!(grant(scope), Grant::Allowed, "{scope}");
        }
        for scope in ["null", r#""""#, r#""  ""#] {
            assert_eq!(grant(scope), Grant::Unstated, "{scope}");
        }
        let unstated = tokens(r#"{"access_token":"a","expires_in":60}"#).unwrap();
        assert_eq!(unstated.grant, Grant::Unstated);
        assert!(matches!(
            unstated.login_refusal(),
            Some(AuthError::ScopeUnstated)
        ));
        for scope in [r#"["user:profile"]"#, "7", "{}"] {
            assert_eq!(grant(scope), Grant::Unreadable, "{scope}");
        }
        for scope in [
            r#""user:profile user:inference user:sessions:claude_code""#,
            r#""user:sessions:claude_code""#,
            r#""user:inference org:create_api_key""#,
            r#""User:Profile""#,
            r#""user:profile,user:inference""#,
            r#"["user:profile"]"#,
            "7",
            "{}",
        ] {
            let wide = tokens(&format!(
                r#"{{"access_token":"a","expires_in":60,"scope":{scope}}}"#
            ))
            .unwrap();
            let err = wide.login_refusal().unwrap();
            assert!(matches!(err, AuthError::ScopeRefused), "{scope}: {err:?}");
            for allowed in ALLOWED_SCOPES {
                assert!(err.to_string().contains(allowed), "{err}");
            }
            assert!(!err.to_string().contains("sessions"), "{err}");
        }
        let allowed = tokens(r#"{"access_token":"a","expires_in":60,"scope":"user:profile"}"#);
        assert!(allowed.unwrap().login_refusal().is_none());
    }

    #[test]
    fn only_safe_oauth_error_codes_are_kept() {
        assert_eq!(
            oauth_error_code(br#"{"error":"invalid_grant","error_description":"tok-123"}"#),
            Some("invalid_grant".to_owned())
        );
        for body in [
            &br#"{"error":"sk-ant-ort01-abc"}"#[..],
            br#"{"error":"Invalid Grant"}"#,
            br#"{"error":""}"#,
            br#"{"message":"x"}"#,
            b"<html>",
        ] {
            assert_eq!(oauth_error_code(body), None);
        }
        let long = format!(r#"{{"error":"{}"}}"#, "a".repeat(MAX_ERROR_CODE + 1));
        assert_eq!(oauth_error_code(long.as_bytes()), None);
        assert_eq!(
            oauth_error_code(br#"{"error":{"type":"invalid_grant","message":"x"}}"#),
            Some("invalid_grant".to_owned())
        );
    }

    fn dead(status: u16, body: &str) -> bool {
        token_is_dead(StatusCode::from_u16(status).unwrap(), body.as_bytes())
    }

    #[test]
    fn a_token_is_dead_only_on_a_terminal_oauth_error() {
        for code in DEAD_TOKEN_CODES {
            let body = format!(r#"{{"error":"{code}"}}"#);
            assert!(dead(400, &body), "{code}");
            assert!(dead(401, &body), "{code}");
            assert!(!dead(403, &body), "{code}");
            assert!(!dead(500, &body), "{code}");
        }
        assert!(dead(400, r#"{"error":{"type":"invalid_grant"}}"#));
        for (status, body) in [
            (400, r#"{"error":"invalid_request"}"#),
            (400, r#"{"error":"server_error"}"#),
            (401, r#"{"error":"access_denied"}"#),
            (400, "{}"),
            (400, ""),
            (401, "<html>Unauthorized</html>"),
            (403, "<html>Just a moment...</html>"),
            (403, r#"{"error":"forbidden"}"#),
            (429, r#"{"error":"invalid_grant"}"#),
            (503, r#"{"error":"invalid_grant"}"#),
        ] {
            assert!(!dead(status, body), "{status} {body}");
        }
    }

    #[test]
    fn an_account_on_hold_is_a_dead_token() {
        for status in [400, 401, 403] {
            for code in ["invalid_grant", "access_denied"] {
                let body = format!(
                    r#"{{"error":"{code}","error_description":"account_on_hold","error_uri":"https://claude.ai/restricted"}}"#
                );
                assert!(dead(status, &body), "{status} {code}");
            }
        }
        assert!(!dead(
            403,
            r#"{"error":"server_error","error_description":"account_on_hold"}"#
        ));
        assert!(!dead(
            500,
            r#"{"error":"access_denied","error_description":"account_on_hold"}"#
        ));
    }
}

//! [`OAuthConfig`]: the `[claude_oauth]` configuration section.

use reqwest::Url;
use serde::Deserialize;

/// Claude Code's OAuth parameters: the `[claude_oauth]` section of
/// `agentd.toml`.
///
/// They are Claude Code's, not a published API contract, so they are
/// configuration rather than constants. Every key is optional; the defaults
/// ([`OAuthConfig::default`]) are what Claude Code 2.1.285 uses for a
/// claude.ai login with a pasted code.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OAuthConfig {
    /// The authorize page the member opens in their browser.
    pub authorize_url: String,
    /// The token endpoint, for the code exchange and refreshes.
    pub token_url: String,
    /// The token revocation endpoint, called on logout.
    pub revoke_url: String,
    /// Where the authorize page sends the member after approving: Anthropic's
    /// page that shows the `code#state` to paste back.
    pub redirect_uri: String,
    /// The OAuth client ID.
    pub client_id: String,
    /// The scopes to request, separated by spaces.
    pub scopes: String,
    /// The account profile endpoint the plan is read from.
    pub profile_url: String,
}

impl Default for OAuthConfig {
    fn default() -> Self {
        Self {
            authorize_url: "https://claude.com/cai/oauth/authorize".to_owned(),
            token_url: "https://platform.claude.com/v1/oauth/token".to_owned(),
            revoke_url: "https://platform.claude.com/v1/oauth/token/revoke".to_owned(),
            redirect_uri: "https://platform.claude.com/oauth/code/callback".to_owned(),
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".to_owned(),
            scopes: "user:profile user:inference".to_owned(),
            profile_url: "https://api.anthropic.com/api/oauth/profile".to_owned(),
        }
    }
}

/// An invalid [`OAuthConfig`] value, named by its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("claude_oauth.{key}: {reason}")]
pub struct ConfigError {
    /// The key, for example `"token_url"`.
    pub key: &'static str,
    /// What is wrong with its value.
    pub reason: &'static str,
}

impl OAuthConfig {
    /// Checks every value.
    ///
    /// URLs must be absolute `https` URLs without credentials or a fragment.
    /// Plain `http` is accepted only for a loopback host (`localhost`,
    /// `127.0.0.1`, `[::1]`), for tests and local fakes, because tokens and
    /// codes travel in these requests. The client ID and scopes must be
    /// non-empty.
    ///
    /// # Errors
    ///
    /// A [`ConfigError`] naming the first bad key.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.urls()?;
        if self.client_id.trim().is_empty() {
            return Err(ConfigError {
                key: "client_id",
                reason: "must not be empty",
            });
        }
        if self.scope_list().next().is_none() {
            return Err(ConfigError {
                key: "scopes",
                reason: "must name at least one scope",
            });
        }
        Ok(())
    }

    pub(crate) fn urls(&self) -> Result<Urls, ConfigError> {
        Ok(Urls {
            authorize: parse_url("authorize_url", &self.authorize_url)?,
            token: parse_url("token_url", &self.token_url)?,
            revoke: parse_url("revoke_url", &self.revoke_url)?,
            redirect: parse_url("redirect_uri", &self.redirect_uri)?,
            profile: parse_url("profile_url", &self.profile_url)?,
        })
    }

    /// The scopes, one per item, as sent: joined by single spaces.
    pub(crate) fn scope_param(&self) -> String {
        self.scope_list().collect::<Vec<_>>().join(" ")
    }

    fn scope_list(&self) -> impl Iterator<Item = &str> {
        self.scopes.split_whitespace()
    }
}

/// The parsed URLs of a validated [`OAuthConfig`].
#[derive(Debug, Clone)]
pub(crate) struct Urls {
    pub(crate) authorize: Url,
    pub(crate) token: Url,
    pub(crate) revoke: Url,
    pub(crate) redirect: Url,
    pub(crate) profile: Url,
}

fn parse_url(key: &'static str, value: &str) -> Result<Url, ConfigError> {
    let error = |reason| ConfigError { key, reason };
    let url = Url::parse(value).map_err(|_| error("not an absolute URL"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(error("must not contain credentials"));
    }
    if url.fragment().is_some() {
        return Err(error("must not have a fragment"));
    }
    match url.scheme() {
        "https" => Ok(url),
        "http" if is_loopback(&url) => Ok(url),
        "http" => Err(error("must use https unless the host is loopback")),
        _ => Err(error("must be an https URL")),
    }
}

fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || unbracketed
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_claude_code_2_1_285() {
        let config = OAuthConfig::default();
        assert_eq!(
            config.authorize_url,
            "https://claude.com/cai/oauth/authorize"
        );
        assert_eq!(
            config.token_url,
            "https://platform.claude.com/v1/oauth/token"
        );
        assert_eq!(
            config.revoke_url,
            "https://platform.claude.com/v1/oauth/token/revoke"
        );
        assert_eq!(
            config.redirect_uri,
            "https://platform.claude.com/oauth/code/callback"
        );
        assert_eq!(config.client_id, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(config.scopes, "user:profile user:inference");
        assert_eq!(
            config.profile_url,
            "https://api.anthropic.com/api/oauth/profile"
        );
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn missing_keys_take_their_defaults() {
        let config: OAuthConfig =
            serde_json::from_str(r#"{"client_id":"other","scopes":"a  b"}"#).unwrap();
        assert_eq!(config.client_id, "other");
        assert_eq!(config.scope_param(), "a b");
        assert_eq!(config.token_url, OAuthConfig::default().token_url);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        assert!(serde_json::from_str::<OAuthConfig>(r#"{"tokn_url":"x"}"#).is_err());
    }

    fn with(key: &str, value: &str) -> OAuthConfig {
        let mut config = OAuthConfig::default();
        let field = match key {
            "authorize_url" => &mut config.authorize_url,
            "token_url" => &mut config.token_url,
            "revoke_url" => &mut config.revoke_url,
            "redirect_uri" => &mut config.redirect_uri,
            "profile_url" => &mut config.profile_url,
            "client_id" => &mut config.client_id,
            "scopes" => &mut config.scopes,
            _ => unreachable!(),
        };
        *field = value.to_owned();
        config
    }

    #[test]
    fn validation_names_the_bad_key() {
        let cases = [
            ("authorize_url", "claude.com/oauth", "not an absolute URL"),
            (
                "token_url",
                "http://platform.claude.com/v1/oauth/token",
                "must use https unless the host is loopback",
            ),
            ("revoke_url", "ftp://example.com/", "must be an https URL"),
            (
                "redirect_uri",
                "https://user:pw@example.com/",
                "must not contain credentials",
            ),
            (
                "profile_url",
                "https://example.com/profile#x",
                "must not have a fragment",
            ),
            ("client_id", " ", "must not be empty"),
            ("scopes", "  ", "must name at least one scope"),
        ];
        for (key, value, reason) in cases {
            let err = with(key, value).validate().unwrap_err();
            assert_eq!(err.key, key);
            assert_eq!(err.reason, reason, "{key}");
            assert_eq!(err.to_string(), format!("claude_oauth.{key}: {reason}"));
        }
    }

    #[test]
    fn plain_http_is_allowed_only_for_loopback() {
        for url in [
            "http://127.0.0.1:8080/token",
            "http://localhost/token",
            "http://[::1]:9/token",
        ] {
            assert_eq!(with("token_url", url).validate(), Ok(()), "{url}");
        }
        for url in ["http://10.0.0.1/token", "http://localhost.example.com/"] {
            assert!(with("token_url", url).validate().is_err(), "{url}");
        }
    }
}

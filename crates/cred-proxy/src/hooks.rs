//! What the proxy calls out to: [`CommunityKey`] for the community API key,
//! and [`ProxyObserver`] after each forwarded request.

use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use core_types::{CredentialRef, SessionId};
use secrecy::SecretString;

/// Hands out the community API key an admin configured, for turns pointed
/// at [`CredentialRef::Community`]. T26 implements it over the store.
#[async_trait]
pub trait CommunityKey: Send + Sync {
    /// The current community API key.
    ///
    /// # Errors
    ///
    /// [`CommunityKeyError::NotConfigured`] when no key is set, and
    /// [`CommunityKeyError::Unavailable`] when it can't be read now.
    async fn api_key(&self) -> Result<SecretString, CommunityKeyError>;
}

/// The error returned by [`CommunityKey`]. It never carries the key.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CommunityKeyError {
    /// No community API key is configured.
    #[error("no community API key is configured")]
    NotConfigured,
    /// The key couldn't be read, for example because the store failed.
    #[error("the community API key could not be read: {0}")]
    Unavailable(#[source] Box<dyn std::error::Error + Send + Sync>),
}

/// A [`CommunityKey`] that always returns one key, for tests and for
/// wiring before T26.
#[derive(Debug, Clone)]
pub struct FixedKey(SecretString);

impl FixedKey {
    /// A source that returns `key`.
    pub fn new(key: SecretString) -> Self {
        Self(key)
    }
}

#[async_trait]
impl CommunityKey for FixedKey {
    async fn api_key(&self) -> Result<SecretString, CommunityKeyError> {
        Ok(self.0.clone())
    }
}

/// One forwarded request's outcome, as [`ProxyObserver::observe`] gets it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Observation {
    /// The session whose placeholder the request carried.
    pub session: SessionId,
    /// The credential the request was sent with.
    pub credential: CredentialRef,
    /// The upstream's response status.
    pub status: StatusCode,
    /// The upstream's usage headers: every `anthropic-ratelimit-*` header
    /// and `retry-after`.
    pub usage: HeaderMap,
}

/// Told about every request the proxy forwarded, when the upstream's
/// response head arrives. T27's usage meter implements it.
///
/// It runs on the request's task before the response is streamed back, so
/// it must not block; hand anything slow to a task of its own.
pub trait ProxyObserver: Send + Sync {
    /// Records one forwarded request.
    fn observe(&self, observation: &Observation);
}

/// The headers of `headers` that [`Observation::usage`] holds.
pub(crate) fn usage_headers(headers: &HeaderMap) -> HeaderMap {
    headers
        .iter()
        .filter(|(name, _)| {
            name.as_str().starts_with("anthropic-ratelimit-") || name.as_str() == "retry-after"
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;
    use secrecy::ExposeSecret as _;

    use super::*;

    #[test]
    fn usage_headers_keep_rate_limits_and_retry_after_only() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-tokens-remaining", "10"),
            ("retry-after", "3"),
            ("content-type", "text/event-stream"),
            ("request-id", "req_1"),
        ] {
            headers.append(name, HeaderValue::from_static(value));
        }
        let usage = usage_headers(&headers);
        let mut names: Vec<&str> = usage.keys().map(|name| name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "anthropic-ratelimit-tokens-remaining",
                "anthropic-ratelimit-unified-status",
                "retry-after",
            ]
        );
    }

    #[tokio::test]
    async fn fixed_key_returns_its_key_and_hides_it_from_debug() {
        let key = FixedKey::new(SecretString::from("sk-ant-fixed"));
        assert_eq!(key.api_key().await.unwrap().expose_secret(), "sk-ant-fixed");
        assert!(!format!("{key:?}").contains("sk-ant-fixed"));
        let unavailable = CommunityKeyError::Unavailable("store closed".into());
        assert_eq!(
            unavailable.to_string(),
            "the community API key could not be read: store closed"
        );
    }
}

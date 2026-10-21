//! The community API key: what unlinked members' turns run on.
//!
//! A community admin sets it with `/agent admin api-key set <key>` and
//! removes it with `/agent admin api-key clear` (see
//! [`commands`](crate::commands)). The store holds it sealed, in
//! `community_settings`, and nowhere else: it isn't configuration.
//! [`StoreCommunityKey`] hands it to the credential proxy, which is the only
//! place it leaves agentd: in the `x-api-key` header of a request a
//! sandbox sent with an API-key placeholder pointed at
//! [`CredentialRef::Community`](core_types::CredentialRef::Community), to
//! the configured upstream only. Sandboxes only ever hold the placeholder.

use async_trait::async_trait;
use cred_proxy::{CommunityKey, CommunityKeyError};
use secrecy::SecretString;
use store::Store;

/// The longest community API key accepted, in bytes. Anthropic's keys are
/// about a hundred.
pub const MAX_API_KEY_BYTES: usize = 512;

/// The credential proxy's [`CommunityKey`], read from the store on every
/// request, so a key an admin sets or clears takes effect at once, on every
/// agentd instance sharing the store.
#[derive(Debug, Clone)]
pub struct StoreCommunityKey {
    store: Store,
}

impl StoreCommunityKey {
    /// The key `store` holds.
    pub fn new(store: Store) -> Self {
        Self { store }
    }
}

#[async_trait]
impl CommunityKey for StoreCommunityKey {
    async fn api_key(&self) -> Result<SecretString, CommunityKeyError> {
        match self.store.community_api_key().await {
            Ok(Some(key)) => Ok(key),
            Ok(None) => Err(CommunityKeyError::NotConfigured),
            Err(err) => Err(CommunityKeyError::Unavailable(Box::new(err))),
        }
    }
}

/// Whether `key` can be a community API key: 1 to [`MAX_API_KEY_BYTES`]
/// bytes of visible ASCII, which is what an HTTP header value may carry
/// without any character the proxy would have to refuse. It says nothing
/// about whether Anthropic accepts the key.
pub fn is_plausible_api_key(key: &str) -> bool {
    (1..=MAX_API_KEY_BYTES).contains(&key.len()) && key.bytes().all(|b| b.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use core_types::MemberKey;
    use secrecy::ExposeSecret as _;
    use store::Sealer;
    use time::OffsetDateTime;

    use super::*;

    #[tokio::test]
    async fn the_proxy_gets_the_key_the_store_holds_at_the_time() {
        let store =
            Store::open_in_memory(Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap())
                .await
                .unwrap();
        let key = StoreCommunityKey::new(store.clone());
        assert!(matches!(
            key.api_key().await,
            Err(CommunityKeyError::NotConfigured)
        ));
        let admin: MemberKey = "slack:T1:UADMIN".parse().unwrap();
        store
            .set_community_api_key(
                &SecretString::from("sk-ant-api03-one"),
                &admin,
                OffsetDateTime::now_utc(),
            )
            .await
            .unwrap();
        assert_eq!(
            key.api_key().await.unwrap().expose_secret(),
            "sk-ant-api03-one"
        );
        store
            .clear_community_api_key(&admin, OffsetDateTime::now_utc())
            .await
            .unwrap();
        assert!(matches!(
            key.api_key().await,
            Err(CommunityKeyError::NotConfigured)
        ));
        store.close().await;
        let Err(CommunityKeyError::Unavailable(err)) = key.api_key().await else {
            panic!("a closed store makes the key unavailable");
        };
        assert!(err.to_string().starts_with("database error"), "{err}");
    }

    #[test]
    fn a_plausible_key_is_visible_ascii_of_bounded_length() {
        assert!(is_plausible_api_key("sk-ant-api03-AbC_123-xyz"));
        assert!(is_plausible_api_key(&"k".repeat(MAX_API_KEY_BYTES)));
        for key in [
            String::new(),
            "k".repeat(MAX_API_KEY_BYTES + 1),
            "sk-ant\u{7f}".to_owned(),
            "sk-ant-é".to_owned(),
            "sk ant".to_owned(),
            "sk-ant\r\nx-evil: 1".to_owned(),
        ] {
            assert!(!is_plausible_api_key(&key), "{key:?}");
        }
    }
}

//! [`Registry`]: the live placeholders, the session and container address
//! each is bound to, and the credential each is pointed at.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use core_types::{CredentialKind, CredentialRef, SessionId};
use rand::TryRng as _;
use rand::rngs::SysRng;
use secrecy::zeroize::Zeroize as _;
use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

/// Random bytes in a placeholder, after its prefix.
const RANDOM_BYTES: usize = 32;

/// The prefix of a subscription placeholder, which the sandbox gets as
/// `CLAUDE_CODE_OAUTH_TOKEN` and sends as `Authorization: Bearer`.
pub const SUBSCRIPTION_PREFIX: &str = "agentd-sub-";

/// The prefix of an API-key placeholder, which the sandbox gets as
/// `ANTHROPIC_API_KEY` and sends as `x-api-key`.
pub const API_KEY_PREFIX: &str = "agentd-key-";

/// The recognizable prefix of a placeholder of `kind`.
fn prefix(kind: CredentialKind) -> &'static str {
    match kind {
        CredentialKind::Subscription => SUBSCRIPTION_PREFIX,
        CredentialKind::ApiKey => API_KEY_PREFIX,
    }
}

/// The environment variable that carries a placeholder of `kind` into the
/// sandbox. The runner sets exactly one of them: with both set, the CLI
/// sends the API key.
fn env_var(kind: CredentialKind) -> &'static str {
    match kind {
        CredentialKind::Subscription => "CLAUDE_CODE_OAUTH_TOKEN",
        CredentialKind::ApiKey => "ANTHROPIC_API_KEY",
    }
}

/// A placeholder's non-secret handle: the SHA-256 of its text.
///
/// The registry is keyed by it, so a presented token is found by its digest
/// rather than compared with stored tokens, and lookup time says nothing
/// about how close a guess was. Callers keep it to point and revoke the
/// placeholder without holding the secret.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PlaceholderId([u8; 32]);

impl PlaceholderId {
    fn of(token: &str) -> Self {
        Self(Sha256::digest(token.as_bytes()).into())
    }
}

impl fmt::Debug for PlaceholderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PlaceholderId(")?;
        for byte in &self.0[..4] {
            write!(f, "{byte:02x}")?;
        }
        f.write_str("…)")
    }
}

/// A placeholder token, as [`Registry::mint`] returns it: the recognizable
/// prefix of its kind followed by 32 random bytes in base64url.
///
/// Its text goes into the sandbox's environment through
/// [`ExposeSecret::expose_secret`], under [`env_var`](Self::env_var). `Debug`
/// shows only its kind and id.
pub struct Placeholder {
    token: SecretString,
    id: PlaceholderId,
    kind: CredentialKind,
}

impl Placeholder {
    /// The handle to point and revoke it with.
    pub fn id(&self) -> PlaceholderId {
        self.id
    }

    /// The kind of credential it stands for.
    pub fn kind(&self) -> CredentialKind {
        self.kind
    }

    /// The environment variable to put it in: `CLAUDE_CODE_OAUTH_TOKEN` or
    /// `ANTHROPIC_API_KEY`.
    pub fn env_var(&self) -> &'static str {
        env_var(self.kind)
    }
}

impl ExposeSecret<str> for Placeholder {
    fn expose_secret(&self) -> &str {
        self.token.expose_secret()
    }
}

impl fmt::Debug for Placeholder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Placeholder")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// The error returned by [`Registry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    /// The operating system's random number generator failed.
    #[error("the system random number generator failed")]
    Random,
    /// No live placeholder has this id: it was never minted, or it was
    /// revoked.
    #[error("no live placeholder has this id")]
    Unknown,
    /// A placeholder can only be pointed at a credential of its own kind.
    #[error("a {placeholder:?} placeholder can't be pointed at a {credential:?} credential")]
    KindMismatch {
        /// The placeholder's kind.
        placeholder: CredentialKind,
        /// The credential's kind.
        credential: CredentialKind,
    },
}

struct Entry {
    session: SessionId,
    ip: IpAddr,
    kind: CredentialKind,
    credential: Option<CredentialRef>,
}

/// The live placeholders, shared between agentd's turn hooks, which mint,
/// point and revoke them, and the proxy, which checks them.
///
/// It lives in memory only. That is disposable, re-derivable state: agentd
/// reaps every container on restart, and new containers get new
/// placeholders. Clones share one registry.
///
/// A container address belongs to at most one session: minting for an
/// address revokes other sessions' placeholders bound to it, since Docker
/// may give a dead container's address to a new one.
///
/// Once a session has no live placeholder left, however it lost them, its
/// egress tunnels close: the egress proxy watches the session through the
/// registry, so revoking needs no other call.
#[derive(Clone, Default)]
pub struct Registry {
    entries: Arc<Mutex<HashMap<PlaceholderId, Entry>>>,
    watchers: Arc<Mutex<HashMap<SessionId, watch::Sender<()>>>>,
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field("live", &self.lock().len())
            .finish()
    }
}

/// What a request authorized by [`Registry::authorize`] may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Grant {
    pub(crate) id: PlaceholderId,
    pub(crate) session: SessionId,
    pub(crate) credential: CredentialRef,
}

/// Why [`Registry::authorize`] refused a presented token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Denial {
    Unknown,
    OtherSource(SessionId),
    WrongKind(SessionId),
    NotPointed(SessionId),
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<PlaceholderId, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Mints a placeholder of `kind` for `session`'s container at
    /// `container_ip`. It is not pointed at any credential until
    /// [`point`](Self::point).
    ///
    /// Placeholders of other sessions bound to the same address are revoked.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Random`] if the system random number generator
    /// fails.
    pub fn mint(
        &self,
        session: SessionId,
        container_ip: IpAddr,
        kind: CredentialKind,
    ) -> Result<Placeholder, RegistryError> {
        let ip = container_ip.to_canonical();
        let mut bytes = [0u8; RANDOM_BYTES];
        SysRng
            .try_fill_bytes(&mut bytes)
            .map_err(|_| RegistryError::Random)?;
        let mut token = String::with_capacity(prefix(kind).len() + RANDOM_BYTES * 4 / 3 + 1);
        token.push_str(prefix(kind));
        URL_SAFE_NO_PAD.encode_string(bytes, &mut token);
        bytes.zeroize();
        let id = PlaceholderId::of(&token);
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|_, entry| entry.ip != ip || entry.session == session);
        let displaced = before - entries.len();
        if displaced > 0 {
            self.release_watchers(&entries);
            tracing::warn!(
                %session,
                %ip,
                displaced,
                "revoked placeholders another session still had on this address"
            );
        }
        entries.insert(
            id,
            Entry {
                session,
                ip,
                kind,
                credential: None,
            },
        );
        Ok(Placeholder {
            token: SecretString::from(token),
            id,
            kind,
        })
    }

    /// Points the placeholder at `credential`, for the turn that is
    /// starting. A request reads the pointer once, when it arrives, so a
    /// request in flight keeps the credential it started with.
    ///
    /// # Errors
    ///
    /// [`RegistryError::Unknown`] if the placeholder was revoked, and
    /// [`RegistryError::KindMismatch`] if `credential` is of the other
    /// kind; the pointer is unchanged then.
    pub fn point(&self, id: PlaceholderId, credential: CredentialRef) -> Result<(), RegistryError> {
        let mut entries = self.lock();
        let entry = entries.get_mut(&id).ok_or(RegistryError::Unknown)?;
        if credential.kind() != entry.kind {
            return Err(RegistryError::KindMismatch {
                placeholder: entry.kind,
                credential: credential.kind(),
            });
        }
        entry.credential = Some(credential);
        Ok(())
    }

    /// Clears the placeholder's pointer, for the turn that ended, however
    /// it ended. Until the next [`point`](Self::point), requests carrying it
    /// are refused. A request authorized before the call keeps its
    /// credential. Returns whether it was live; a placeholder already
    /// revoked, as when its container died mid-turn, has nothing to clear.
    pub fn unpoint(&self, id: PlaceholderId) -> bool {
        match self.lock().get_mut(&id) {
            Some(entry) => {
                entry.credential = None;
                true
            }
            None => false,
        }
    }

    /// Revokes the placeholder. Returns whether it was live. If it was its
    /// session's last, the session's egress tunnels close.
    pub fn revoke(&self, id: PlaceholderId) -> bool {
        let mut entries = self.lock();
        let live = entries.remove(&id).is_some();
        if live {
            self.release_watchers(&entries);
        }
        live
    }

    /// Revokes every placeholder of `session`, before its container is
    /// stopped and again when it dies, and closes the session's egress
    /// tunnels. Returns how many were live.
    pub fn revoke_session(&self, session: SessionId) -> usize {
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|_, entry| entry.session != session);
        self.release_watchers(&entries);
        before - entries.len()
    }

    /// Drops the watch senders of sessions with no placeholder left in
    /// `entries`, which ends their receivers. Called with the entries
    /// locked, so a session can't be watched after its last revocation.
    fn release_watchers(&self, entries: &HashMap<PlaceholderId, Entry>) {
        self.watchers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|session, _| entries.values().any(|entry| entry.session == *session));
    }

    /// The session whose live placeholders are bound to `ip`, and a
    /// receiver whose sender is dropped once that session has no live
    /// placeholder left.
    pub(crate) fn watch_source(&self, ip: IpAddr) -> Option<(SessionId, watch::Receiver<()>)> {
        let ip = ip.to_canonical();
        let entries = self.lock();
        let session = entries.values().find(|entry| entry.ip == ip)?.session;
        let receiver = self
            .watchers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(session)
            .or_insert_with(|| watch::Sender::new(()))
            .subscribe();
        Some((session, receiver))
    }

    /// The session whose live placeholders are bound to `ip`, if any. An
    /// address belongs to at most one session.
    pub(crate) fn session_at(&self, ip: IpAddr) -> Option<SessionId> {
        let ip = ip.to_canonical();
        self.lock()
            .values()
            .find(|entry| entry.ip == ip)
            .map(|entry| entry.session)
    }

    /// Checks `token`, presented from `ip` in the header for `kind`.
    pub(crate) fn authorize(
        &self,
        token: &str,
        ip: IpAddr,
        kind: CredentialKind,
    ) -> Result<Grant, Denial> {
        let id = PlaceholderId::of(token);
        let entries = self.lock();
        let entry = entries.get(&id).ok_or(Denial::Unknown)?;
        if entry.ip != ip.to_canonical() {
            return Err(Denial::OtherSource(entry.session));
        }
        if entry.kind != kind {
            return Err(Denial::WrongKind(entry.session));
        }
        let credential = entry.credential.ok_or(Denial::NotPointed(entry.session))?;
        Ok(Grant {
            id,
            session: entry.session,
            credential,
        })
    }

    /// Whether the placeholder is still live and bound to `ip`.
    pub(crate) fn is_live(&self, id: PlaceholderId, ip: IpAddr) -> bool {
        self.lock()
            .get(&id)
            .is_some_and(|entry| entry.ip == ip.to_canonical())
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use core_types::MemberId;

    use super::*;

    const IP: IpAddr = IpAddr::V4(Ipv4Addr::new(172, 30, 0, 5));
    const OTHER_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(172, 30, 0, 6));

    fn member() -> CredentialRef {
        CredentialRef::Member(MemberId::new_v4())
    }

    #[test]
    fn placeholders_carry_their_kind_prefix_and_32_random_bytes() {
        let registry = Registry::new();
        let session = SessionId::new_v4();
        let sub = registry
            .mint(session, IP, CredentialKind::Subscription)
            .unwrap();
        let key = registry.mint(session, IP, CredentialKind::ApiKey).unwrap();
        let sub_text = sub.expose_secret();
        assert!(sub_text.starts_with("agentd-sub-"), "{sub:?}");
        assert!(key.expose_secret().starts_with("agentd-key-"));
        let random = URL_SAFE_NO_PAD
            .decode(&sub_text[SUBSCRIPTION_PREFIX.len()..])
            .unwrap();
        assert_eq!(random.len(), RANDOM_BYTES);
        assert_ne!(sub.id(), key.id());
        assert_eq!(sub.env_var(), "CLAUDE_CODE_OAUTH_TOKEN");
        assert_eq!(key.env_var(), "ANTHROPIC_API_KEY");
        assert_eq!(key.kind(), CredentialKind::ApiKey);
    }

    #[test]
    fn debug_never_shows_the_token() {
        let registry = Registry::new();
        let placeholder = registry
            .mint(SessionId::new_v4(), IP, CredentialKind::Subscription)
            .unwrap();
        let text = placeholder.expose_secret();
        let random = &text[SUBSCRIPTION_PREFIX.len()..];
        for debug in [format!("{placeholder:?}"), format!("{registry:?}")] {
            assert!(!debug.contains(random), "{debug}");
        }
        assert!(format!("{:?}", placeholder.id()).starts_with("PlaceholderId("));
        assert_eq!(format!("{registry:?}"), "Registry { live: 1 }");
    }

    #[test]
    fn authorize_checks_existence_address_kind_and_pointer_in_that_order() {
        let registry = Registry::new();
        let session = SessionId::new_v4();
        let placeholder = registry
            .mint(session, IP, CredentialKind::Subscription)
            .unwrap();
        let token = placeholder.expose_secret();
        let sub = CredentialKind::Subscription;
        assert_eq!(
            registry.authorize("agentd-sub-guess", IP, sub),
            Err(Denial::Unknown)
        );
        assert_eq!(
            registry.authorize(token, OTHER_IP, CredentialKind::ApiKey),
            Err(Denial::OtherSource(session))
        );
        assert_eq!(
            registry.authorize(token, IP, CredentialKind::ApiKey),
            Err(Denial::WrongKind(session))
        );
        assert_eq!(
            registry.authorize(token, IP, sub),
            Err(Denial::NotPointed(session))
        );
        let credential = member();
        registry.point(placeholder.id(), credential).unwrap();
        assert_eq!(
            registry.authorize(token, IP, sub),
            Ok(Grant {
                id: placeholder.id(),
                session,
                credential,
            })
        );
    }

    #[test]
    fn a_placeholder_is_never_pointed_at_the_other_kind() {
        let registry = Registry::new();
        let session = SessionId::new_v4();
        let sub = registry
            .mint(session, IP, CredentialKind::Subscription)
            .unwrap();
        let key = registry.mint(session, IP, CredentialKind::ApiKey).unwrap();
        assert_eq!(
            registry.point(sub.id(), CredentialRef::Community),
            Err(RegistryError::KindMismatch {
                placeholder: CredentialKind::Subscription,
                credential: CredentialKind::ApiKey,
            })
        );
        assert!(matches!(
            registry.point(key.id(), member()),
            Err(RegistryError::KindMismatch { .. })
        ));
        registry.point(key.id(), CredentialRef::Community).unwrap();
        assert_eq!(
            registry.authorize(sub.expose_secret(), IP, CredentialKind::Subscription),
            Err(Denial::NotPointed(session))
        );
    }

    #[test]
    fn unpoint_clears_the_pointer_until_the_next_point() {
        let registry = Registry::new();
        let session = SessionId::new_v4();
        let placeholder = registry.mint(session, IP, CredentialKind::ApiKey).unwrap();
        let token = placeholder.expose_secret();
        let key = CredentialKind::ApiKey;
        assert!(registry.unpoint(placeholder.id()));
        registry
            .point(placeholder.id(), CredentialRef::Community)
            .unwrap();
        assert!(registry.authorize(token, IP, key).is_ok());
        assert!(registry.unpoint(placeholder.id()));
        assert_eq!(
            registry.authorize(token, IP, key),
            Err(Denial::NotPointed(session))
        );
        registry
            .point(placeholder.id(), CredentialRef::Community)
            .unwrap();
        assert!(registry.authorize(token, IP, key).is_ok());
        assert!(registry.revoke(placeholder.id()));
        assert!(!registry.unpoint(placeholder.id()));
    }

    #[test]
    fn revoke_and_revoke_session_remove_placeholders() {
        let registry = Registry::new();
        let session = SessionId::new_v4();
        let other = SessionId::new_v4();
        let first = registry
            .mint(session, IP, CredentialKind::Subscription)
            .unwrap();
        let second = registry.mint(session, IP, CredentialKind::ApiKey).unwrap();
        let kept = registry
            .mint(other, OTHER_IP, CredentialKind::ApiKey)
            .unwrap();
        assert!(registry.revoke(first.id()));
        assert!(!registry.revoke(first.id()));
        assert_eq!(
            registry.point(first.id(), member()),
            Err(RegistryError::Unknown)
        );
        assert!(registry.is_live(second.id(), IP));
        assert!(!registry.is_live(second.id(), OTHER_IP));
        assert_eq!(registry.revoke_session(session), 1);
        assert_eq!(registry.revoke_session(session), 0);
        assert!(!registry.is_live(second.id(), IP));
        assert_eq!(registry.session_at(IP), None);
        assert_eq!(registry.session_at(OTHER_IP), Some(other));
        assert!(registry.is_live(kept.id(), OTHER_IP));
    }

    #[test]
    fn minting_on_an_address_revokes_other_sessions_there() {
        let registry = Registry::new();
        let old = SessionId::new_v4();
        let new = SessionId::new_v4();
        let stale = registry
            .mint(old, IP, CredentialKind::Subscription)
            .unwrap();
        let same_session = registry
            .mint(new, IP, CredentialKind::Subscription)
            .unwrap();
        let again = registry.mint(new, IP, CredentialKind::ApiKey).unwrap();
        assert!(!registry.is_live(stale.id(), IP));
        assert!(registry.is_live(same_session.id(), IP));
        assert!(registry.is_live(again.id(), IP));
    }

    #[test]
    fn ipv4_mapped_addresses_match_their_ipv4_form() {
        let registry = Registry::new();
        let session = SessionId::new_v4();
        let mapped = IpAddr::V6(Ipv4Addr::new(172, 30, 0, 5).to_ipv6_mapped());
        let placeholder = registry
            .mint(session, mapped, CredentialKind::ApiKey)
            .unwrap();
        registry
            .point(placeholder.id(), CredentialRef::Community)
            .unwrap();
        assert_eq!(registry.session_at(IP), Some(session));
        assert!(
            registry
                .authorize(placeholder.expose_secret(), IP, CredentialKind::ApiKey)
                .is_ok()
        );
        assert_eq!(registry.session_at(IpAddr::V6(Ipv6Addr::LOCALHOST)), None);
    }

    #[test]
    fn a_session_watch_ends_when_its_last_placeholder_goes() {
        let registry = Registry::new();
        let session = SessionId::new_v4();
        assert!(registry.watch_source(IP).is_none());
        let first = registry
            .mint(session, IP, CredentialKind::Subscription)
            .unwrap();
        let second = registry.mint(session, IP, CredentialKind::ApiKey).unwrap();
        let (watched, watch) = registry.watch_source(IP).unwrap();
        assert_eq!(watched, session);
        registry.unpoint(first.id());
        assert!(registry.revoke(first.id()));
        assert!(watch.has_changed().is_ok());
        assert!(registry.revoke(second.id()));
        assert!(watch.has_changed().is_err());
        assert!(registry.watch_source(IP).is_none());

        registry
            .mint(session, IP, CredentialKind::Subscription)
            .unwrap();
        let (_, watch) = registry.watch_source(IP).unwrap();
        let (_, other_watch) = {
            registry
                .mint(SessionId::new_v4(), OTHER_IP, CredentialKind::Subscription)
                .unwrap();
            registry.watch_source(OTHER_IP).unwrap()
        };
        assert_eq!(registry.revoke_session(session), 1);
        assert!(watch.has_changed().is_err());
        assert!(other_watch.has_changed().is_ok());

        registry
            .mint(SessionId::new_v4(), OTHER_IP, CredentialKind::Subscription)
            .unwrap();
        assert!(other_watch.has_changed().is_err());
    }
}

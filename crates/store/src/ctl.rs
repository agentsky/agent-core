//! `ctl_tokens` and `scope_locks`: agentctl's per-process tokens, and the
//! leases on each volume's `shared/` lock.

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use core_types::{
    AgentId, ConsentId, ConvRef, Hop, LeaseId, MessageId, Requester, SessionId, Side, ThreadKey,
    TurnId, TurnKind, VolumeKey,
};
use time::OffsetDateTime;

use crate::{Result, Store, StoreError, from_unix, parse_column, to_unix};

const TOKENS: &str = "ctl_tokens";
const LOCKS: &str = "scope_locks";

/// The SHA-256 digest of an agentctl token. The store keeps only this, never
/// the token.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TokenHash(pub [u8; 32]);

impl fmt::Debug for TokenHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenHash(")?;
        for byte in &self.0[..4] {
            write!(f, "{byte:02x}")?;
        }
        f.write_str("…)")
    }
}

/// A token to store for a new `claude` process, with no turn running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCtlToken {
    /// The digest of the token.
    pub hash: TokenHash,
    /// The process's session.
    pub session: SessionId,
    /// The agent the session belongs to.
    pub agent: AgentId,
    /// The volume the session mounts, which names its `shared/` lock.
    pub volume: VolumeKey,
    /// The container's address on the sandbox network. Requests from any
    /// other address are refused.
    pub container_ip: IpAddr,
}

/// A stored agentctl token, as [`Store::ctl_token`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtlToken {
    /// The process's session.
    pub session: SessionId,
    /// The agent the session belongs to.
    pub agent: AgentId,
    /// The volume the session mounts.
    pub volume: VolumeKey,
    /// The container's address on the sandbox network.
    pub container_ip: IpAddr,
    /// The turn running now, or `None` between turns.
    pub turn: Option<CtlTurn>,
}

/// The turn a token's process is running: everything agentctl's rules
/// check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CtlTurn {
    /// The turn.
    pub id: TurnId,
    /// Who caused the turn, and pays for it.
    pub requester: Requester,
    /// How many agent-to-agent hops led to it.
    pub hop: Hop,
    /// A normal turn or a private task.
    pub kind: TurnKind,
    /// Which side of the agent it runs on, which decides where it may post.
    pub side: Side,
    /// The thread the turn replies in: the current conversation.
    pub thread: ThreadKey,
    /// The message that started the turn, which `agentctl react` without a
    /// message id reacts to. `None` when nothing on the surface started it,
    /// as for a private task.
    pub trigger: Option<MessageId>,
}

/// A lease on a volume's `shared/` lock, as
/// [`Store::acquire_scope_lock`] grants it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeLease {
    /// The lease, to renew and release.
    pub lease: LeaseId,
    /// When it runs out unless renewed.
    pub expires_at: OffsetDateTime,
}

/// What [`Store::purge_ctl`] deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CtlPurged {
    /// Rows of `ctl_tokens`.
    pub tokens: u64,
    /// Rows of `scope_locks`.
    pub locks: u64,
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    session_id: String,
    agent_id: String,
    volume_key: String,
    container_ip: String,
    turn_id: Option<String>,
    requester_member: Option<String>,
    requester_key: Option<String>,
    hop: Option<i64>,
    kind: Option<String>,
    consent_id: Option<String>,
    side: Option<String>,
    conversation: Option<String>,
    thread_root: Option<String>,
    trigger_message: Option<String>,
}

fn corrupt(column: &'static str) -> StoreError {
    StoreError::Corrupt {
        table: TOKENS,
        column,
    }
}

impl TokenRow {
    fn into_token(self) -> Result<CtlToken> {
        let turn =
            match self.turn_id {
                None => None,
                Some(id) => Some(CtlTurn {
                    id: parse_column(&id, TOKENS, "turn_id")?,
                    requester: Requester {
                        member: self
                            .requester_member
                            .map(|member| parse_column(&member, TOKENS, "requester_member"))
                            .transpose()?,
                        key: parse_column(
                            &self.requester_key.ok_or(corrupt("requester_key"))?,
                            TOKENS,
                            "requester_key",
                        )?,
                    },
                    hop: Hop(self
                        .hop
                        .and_then(|hop| u8::try_from(hop).ok())
                        .ok_or(corrupt("hop"))?),
                    kind: match (self.kind.as_deref(), self.consent_id) {
                        (Some("normal"), None) => TurnKind::Normal,
                        (Some("private_task"), Some(consent)) => TurnKind::PrivateTask(
                            parse_column::<ConsentId>(&consent, TOKENS, "consent_id")?,
                        ),
                        _ => return Err(corrupt("kind")),
                    },
                    side: match self.side.as_deref() {
                        Some("owner") => Side::Owner,
                        Some("public") => Side::Public,
                        _ => return Err(corrupt("side")),
                    },
                    thread: ThreadKey {
                        conv: parse_column::<ConvRef>(
                            &self.conversation.ok_or(corrupt("conversation"))?,
                            TOKENS,
                            "conversation",
                        )?,
                        root: self.thread_root.map(MessageId::new),
                    },
                    trigger: self.trigger_message.map(MessageId::new),
                }),
            };
        Ok(CtlToken {
            session: parse_column(&self.session_id, TOKENS, "session_id")?,
            agent: parse_column(&self.agent_id, TOKENS, "agent_id")?,
            volume: parse_column(&self.volume_key, TOKENS, "volume_key")?,
            container_ip: parse_column(&self.container_ip, TOKENS, "container_ip")?,
            turn,
        })
    }
}

fn hashes(rows: Vec<Vec<u8>>) -> Result<Vec<TokenHash>> {
    rows.into_iter()
        .map(|hash| {
            <[u8; 32]>::try_from(hash)
                .map(TokenHash)
                .map_err(|_| corrupt("hash"))
        })
        .collect()
}

fn ttl_seconds(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX).max(1)
}

impl Store {
    /// Stores the token of a new `claude` process, with no turn running.
    ///
    /// A session runs one process at a time, so a token already stored for
    /// the same session is deleted in the same transaction: the newest
    /// process's token is the only one that works. Returns the digests of
    /// the tokens it replaced.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, including when `hash` is
    /// already stored.
    pub async fn put_ctl_token(&self, token: &NewCtlToken) -> Result<Vec<TokenHash>> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let replaced: Vec<Vec<u8>> =
            sqlx::query_scalar("DELETE FROM ctl_tokens WHERE session_id = ? RETURNING hash")
                .bind(token.session.to_string())
                .fetch_all(&mut *tx)
                .await?;
        sqlx::query(
            "INSERT INTO ctl_tokens (hash, session_id, agent_id, volume_key, container_ip) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&token.hash.0[..])
        .bind(token.session.to_string())
        .bind(token.agent.to_string())
        .bind(token.volume.to_string())
        .bind(token.container_ip.to_canonical().to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        hashes(replaced)
    }

    /// The token with digest `hash`, if it is stored.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a column doesn't parse.
    pub async fn ctl_token(&self, hash: &TokenHash) -> Result<Option<CtlToken>> {
        let row: Option<TokenRow> = sqlx::query_as(
            "SELECT session_id, agent_id, volume_key, container_ip, turn_id, requester_member, \
             requester_key, hop, kind, consent_id, side, conversation, thread_root, \
             trigger_message FROM ctl_tokens WHERE hash = ?",
        )
        .bind(&hash.0[..])
        .fetch_optional(&self.pool)
        .await?;
        row.map(TokenRow::into_token).transpose()
    }

    /// Records `turn` as the token's current turn, replacing any other, or
    /// clears it with `None`. Returns false when no such token is stored.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn set_ctl_turn(&self, hash: &TokenHash, turn: Option<&CtlTurn>) -> Result<bool> {
        let (kind, consent) = match turn.map(|turn| turn.kind) {
            None => (None, None),
            Some(TurnKind::Normal) => (Some("normal"), None),
            Some(TurnKind::PrivateTask(consent)) => (Some("private_task"), Some(consent)),
        };
        let side = turn.map(|turn| match turn.side {
            Side::Owner => "owner",
            Side::Public => "public",
        });
        let result = sqlx::query(
            "UPDATE ctl_tokens SET turn_id = ?, requester_member = ?, requester_key = ?, \
             hop = ?, kind = ?, consent_id = ?, side = ?, conversation = ?, thread_root = ?, \
             trigger_message = ? WHERE hash = ?",
        )
        .bind(turn.map(|turn| turn.id.to_string()))
        .bind(turn.and_then(|turn| turn.requester.member.map(|member| member.to_string())))
        .bind(turn.map(|turn| turn.requester.key.to_string()))
        .bind(turn.map(|turn| i64::from(turn.hop.0)))
        .bind(kind)
        .bind(consent.map(|consent| consent.to_string()))
        .bind(side)
        .bind(turn.map(|turn| turn.thread.conv.to_string()))
        .bind(turn.and_then(|turn| {
            turn.thread
                .root
                .as_ref()
                .map(|root| root.as_str().to_owned())
        }))
        .bind(turn.and_then(|turn| turn.trigger.as_ref().map(|msg| msg.as_str().to_owned())))
        .bind(&hash.0[..])
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes the token with digest `hash`. Returns false when it wasn't
    /// stored, so revoking twice is harmless.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn delete_ctl_token(&self, hash: &TokenHash) -> Result<bool> {
        let result = sqlx::query("DELETE FROM ctl_tokens WHERE hash = ?")
            .bind(&hash.0[..])
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes every agentctl token and every scope lock, as agentd does at
    /// startup: the processes and containers they belong to are gone, and
    /// Docker may give their addresses to new containers.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn purge_ctl(&self) -> Result<CtlPurged> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let tokens = sqlx::query("DELETE FROM ctl_tokens")
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let locks = sqlx::query("DELETE FROM scope_locks")
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(CtlPurged { tokens, locks })
    }

    /// Takes `volume`'s `shared/` lock for `holder` under a new lease until
    /// `now + ttl`, if no unexpired lease holds it. Returns `None` when one
    /// does, whichever session it belongs to, `holder`'s included.
    ///
    /// It is one statement: an insert that, on the volume's unique key,
    /// takes over the existing row only if it has expired. Times are whole
    /// seconds, and `ttl` is at least one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn acquire_scope_lock(
        &self,
        volume: &VolumeKey,
        holder: SessionId,
        now: OffsetDateTime,
        ttl: Duration,
    ) -> Result<Option<ScopeLease>> {
        let lease = LeaseId::new_v4();
        let now = to_unix(now);
        let expires_at = now.saturating_add(ttl_seconds(ttl));
        let granted: Option<String> = sqlx::query_scalar(
            "INSERT INTO scope_locks (lease_id, volume_key, holder_session, expires_at) \
             VALUES (?, ?, ?, ?) \
             ON CONFLICT (volume_key) DO UPDATE SET lease_id = excluded.lease_id, \
             holder_session = excluded.holder_session, expires_at = excluded.expires_at \
             WHERE scope_locks.expires_at <= ? \
             RETURNING lease_id",
        )
        .bind(lease.to_string())
        .bind(volume.to_string())
        .bind(holder.to_string())
        .bind(expires_at)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        match granted {
            None => Ok(None),
            Some(granted) => Ok(Some(ScopeLease {
                lease: parse_column(&granted, LOCKS, "lease_id")?,
                expires_at: from_unix(expires_at, LOCKS, "expires_at")?,
            })),
        }
    }

    /// Extends `lease` on `volume`'s lock to `now + ttl`, if it is the
    /// volume's current lease, `holder` holds it, and it hasn't expired.
    /// Returns the new expiry, or `None` if the lease doesn't hold the lock,
    /// in which case nothing changes.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn renew_scope_lock(
        &self,
        volume: &VolumeKey,
        holder: SessionId,
        lease: LeaseId,
        now: OffsetDateTime,
        ttl: Duration,
    ) -> Result<Option<OffsetDateTime>> {
        let now = to_unix(now);
        let expires_at = now.saturating_add(ttl_seconds(ttl));
        let renewed: Option<i64> = sqlx::query_scalar(
            "UPDATE scope_locks SET expires_at = ? \
             WHERE lease_id = ? AND volume_key = ? AND holder_session = ? AND expires_at > ? \
             RETURNING expires_at",
        )
        .bind(expires_at)
        .bind(lease.to_string())
        .bind(volume.to_string())
        .bind(holder.to_string())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        renewed
            .map(|at| from_unix(at, LOCKS, "expires_at"))
            .transpose()
    }

    /// Gives up `lease` on `volume`'s lock, if it is the volume's current
    /// lease and `holder` holds it. Returns false, changing nothing, for any
    /// other lease.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn release_scope_lock(
        &self,
        volume: &VolumeKey,
        holder: SessionId,
        lease: LeaseId,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM scope_locks WHERE lease_id = ? AND volume_key = ? AND holder_session = ?",
        )
        .bind(lease.to_string())
        .bind(volume.to_string())
        .bind(holder.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv6Addr;

    use core_types::{MemberId, ScopeKey, SurfaceKind};

    use super::*;
    use crate::test_util::*;

    fn conv() -> ConvRef {
        ConvRef {
            surface: SurfaceKind::RocketChat,
            team: "chat.example.org".into(),
            conversation: "room1".into(),
        }
    }

    fn volume(agent: AgentId) -> VolumeKey {
        VolumeKey {
            agent,
            scope: ScopeKey::Channel(conv()),
        }
    }

    fn new_token(byte: u8, session: SessionId) -> NewCtlToken {
        let agent = AgentId::new_v4();
        NewCtlToken {
            hash: TokenHash([byte; 32]),
            session,
            agent,
            volume: volume(agent),
            container_ip: "172.30.0.7".parse().unwrap(),
        }
    }

    fn turn(kind: TurnKind, side: Side) -> CtlTurn {
        CtlTurn {
            id: TurnId::new_v4(),
            requester: Requester {
                member: Some(MemberId::new_v4()),
                key: member_key("u1"),
            },
            hop: Hop(3),
            kind,
            side,
            thread: ThreadKey {
                conv: conv(),
                root: Some(MessageId::new("root1")),
            },
            trigger: Some(MessageId::new("msg1")),
        }
    }

    const TTL: Duration = Duration::from_secs(30);

    #[tokio::test]
    async fn a_token_round_trips_with_and_without_a_turn() {
        let store = memory_store().await;
        let new = new_token(1, SessionId::new_v4());
        assert!(store.put_ctl_token(&new).await.unwrap().is_empty());
        let stored = store.ctl_token(&new.hash).await.unwrap().unwrap();
        assert_eq!(
            stored,
            CtlToken {
                session: new.session,
                agent: new.agent,
                volume: new.volume.clone(),
                container_ip: new.container_ip,
                turn: None,
            }
        );

        for turn in [
            turn(TurnKind::Normal, Side::Public),
            turn(TurnKind::PrivateTask(ConsentId::new_v4()), Side::Owner),
            CtlTurn {
                requester: Requester {
                    member: None,
                    key: member_key("u2"),
                },
                hop: Hop(255),
                thread: ThreadKey {
                    conv: conv(),
                    root: None,
                },
                trigger: None,
                ..turn(TurnKind::Normal, Side::Owner)
            },
        ] {
            assert!(store.set_ctl_turn(&new.hash, Some(&turn)).await.unwrap());
            let stored = store.ctl_token(&new.hash).await.unwrap().unwrap();
            assert_eq!(stored.turn, Some(turn));
        }
        assert!(store.set_ctl_turn(&new.hash, None).await.unwrap());
        assert_eq!(
            store.ctl_token(&new.hash).await.unwrap().unwrap().turn,
            None
        );
    }

    #[tokio::test]
    async fn unknown_tokens_are_absent_and_revoking_is_idempotent() {
        let store = memory_store().await;
        let new = new_token(1, SessionId::new_v4());
        let unknown = TokenHash([9; 32]);
        assert_eq!(store.ctl_token(&unknown).await.unwrap(), None);
        assert!(!store.set_ctl_turn(&unknown, None).await.unwrap());
        store.put_ctl_token(&new).await.unwrap();
        assert!(store.delete_ctl_token(&new.hash).await.unwrap());
        assert!(!store.delete_ctl_token(&new.hash).await.unwrap());
        assert_eq!(store.ctl_token(&new.hash).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_new_token_for_a_session_replaces_the_old_one() {
        let store = memory_store().await;
        let session = SessionId::new_v4();
        let first = new_token(1, session);
        let second = new_token(2, session);
        let other = new_token(3, SessionId::new_v4());
        store.put_ctl_token(&first).await.unwrap();
        store.put_ctl_token(&other).await.unwrap();
        assert_eq!(
            store.put_ctl_token(&second).await.unwrap(),
            vec![first.hash]
        );
        assert_eq!(store.ctl_token(&first.hash).await.unwrap(), None);
        assert!(store.ctl_token(&second.hash).await.unwrap().is_some());
        assert!(store.ctl_token(&other.hash).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_duplicate_hash_is_refused() {
        let store = memory_store().await;
        store
            .put_ctl_token(&new_token(1, SessionId::new_v4()))
            .await
            .unwrap();
        let err = store
            .put_ctl_token(&new_token(1, SessionId::new_v4()))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Database(_)), "{err:?}");
    }

    #[tokio::test]
    async fn ipv4_mapped_addresses_are_stored_as_ipv4() {
        let store = memory_store().await;
        let mut new = new_token(1, SessionId::new_v4());
        new.container_ip = IpAddr::V6(Ipv6Addr::from([0, 0, 0, 0, 0, 0xffff, 0xac1e, 0x0007]));
        store.put_ctl_token(&new).await.unwrap();
        let stored = store.ctl_token(&new.hash).await.unwrap().unwrap();
        assert_eq!(stored.container_ip, "172.30.0.7".parse::<IpAddr>().unwrap());
    }

    #[tokio::test]
    async fn purge_deletes_every_token_and_lock() {
        let store = memory_store().await;
        let a = new_token(1, SessionId::new_v4());
        let b = new_token(2, SessionId::new_v4());
        store.put_ctl_token(&a).await.unwrap();
        store.put_ctl_token(&b).await.unwrap();
        store
            .acquire_scope_lock(&a.volume, a.session, at(1_000), TTL)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store.purge_ctl().await.unwrap(),
            CtlPurged {
                tokens: 2,
                locks: 1
            }
        );
        assert_eq!(store.ctl_token(&a.hash).await.unwrap(), None);
        assert!(
            store
                .acquire_scope_lock(&a.volume, b.session, at(1_000), TTL)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn corrupt_rows_are_reported() {
        let store = memory_store().await;
        let new = new_token(1, SessionId::new_v4());
        store.put_ctl_token(&new).await.unwrap();
        sqlx::query("UPDATE ctl_tokens SET container_ip = 'nope'")
            .execute(&store.pool)
            .await
            .unwrap();
        let err = store.ctl_token(&new.hash).await.unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Corrupt {
                    table: "ctl_tokens",
                    column: "container_ip"
                }
            ),
            "{err:?}"
        );
        assert_eq!(
            hashes(vec![vec![0; 3]]).unwrap_err().to_string(),
            "ctl_tokens.hash holds an invalid value"
        );
    }

    #[tokio::test]
    async fn a_half_set_turn_is_refused_by_the_schema() {
        let store = memory_store().await;
        let new = new_token(1, SessionId::new_v4());
        store.put_ctl_token(&new).await.unwrap();
        let err = sqlx::query("UPDATE ctl_tokens SET turn_id = 'x'")
            .execute(&store.pool)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");
        let err = sqlx::query(
            "UPDATE ctl_tokens SET turn_id = 't', requester_key = 'k', hop = 0, \
             kind = 'private_task', side = 'owner', conversation = 'c'",
        )
        .execute(&store.pool)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");
    }

    #[test]
    fn token_hash_debug_shows_only_a_prefix() {
        assert_eq!(
            format!("{:?}", TokenHash([0xab; 32])),
            "TokenHash(abababab…)"
        );
    }

    #[tokio::test]
    async fn a_lease_is_exclusive_even_within_one_session() {
        let store = memory_store().await;
        let agent = AgentId::new_v4();
        let (a, b) = (SessionId::new_v4(), SessionId::new_v4());
        let held = store
            .acquire_scope_lock(&volume(agent), a, at(1_000), TTL)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(held.expires_at, at(1_030));
        for holder in [a, b] {
            assert_eq!(
                store
                    .acquire_scope_lock(&volume(agent), holder, at(1_029), TTL)
                    .await
                    .unwrap(),
                None
            );
        }
        let other_volume = volume(AgentId::new_v4());
        assert!(
            store
                .acquire_scope_lock(&other_volume, b, at(1_000), TTL)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .release_scope_lock(&volume(agent), a, held.lease)
                .await
                .unwrap()
        );
        let next = store
            .acquire_scope_lock(&volume(agent), a, at(1_001), TTL)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(next.lease, held.lease);
    }

    #[tokio::test]
    async fn an_expired_lease_is_taken_over_and_can_no_longer_act() {
        let store = memory_store().await;
        let key = volume(AgentId::new_v4());
        let (a, b) = (SessionId::new_v4(), SessionId::new_v4());
        let dead = store
            .acquire_scope_lock(&key, a, at(1_000), TTL)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .renew_scope_lock(&key, a, dead.lease, at(1_030), TTL)
                .await
                .unwrap(),
            None,
            "an expired lease can't be renewed"
        );
        let live = store
            .acquire_scope_lock(&key, b, at(1_030), TTL)
            .await
            .unwrap()
            .unwrap();
        assert!(!store.release_scope_lock(&key, a, dead.lease).await.unwrap());
        assert_eq!(
            store
                .renew_scope_lock(&key, a, dead.lease, at(1_031), TTL)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .renew_scope_lock(&key, b, live.lease, at(1_040), TTL)
                .await
                .unwrap(),
            Some(at(1_070))
        );
        assert_eq!(
            store
                .acquire_scope_lock(&key, a, at(1_069), TTL)
                .await
                .unwrap(),
            None,
            "the renewal held"
        );
    }

    #[tokio::test]
    async fn renew_and_release_must_match_volume_holder_and_lease() {
        let store = memory_store().await;
        let key = volume(AgentId::new_v4());
        let other = volume(AgentId::new_v4());
        let (a, b) = (SessionId::new_v4(), SessionId::new_v4());
        let held = store
            .acquire_scope_lock(&key, a, at(1_000), TTL)
            .await
            .unwrap()
            .unwrap();
        let stranger = LeaseId::new_v4();
        for (volume, holder, lease) in [
            (&key, a, stranger),
            (&key, b, held.lease),
            (&other, a, held.lease),
        ] {
            assert_eq!(
                store
                    .renew_scope_lock(volume, holder, lease, at(1_010), TTL)
                    .await
                    .unwrap(),
                None
            );
            assert!(
                !store
                    .release_scope_lock(volume, holder, lease)
                    .await
                    .unwrap()
            );
        }
        assert_eq!(
            store
                .acquire_scope_lock(&key, b, at(1_010), TTL)
                .await
                .unwrap(),
            None,
            "the lease is untouched"
        );
    }

    #[tokio::test]
    async fn a_zero_ttl_still_lasts_a_second() {
        let store = memory_store().await;
        let key = volume(AgentId::new_v4());
        let held = store
            .acquire_scope_lock(&key, SessionId::new_v4(), at(1_000), Duration::ZERO)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(held.expires_at, at(1_001));
    }

    #[tokio::test]
    async fn concurrent_acquires_grant_one_lease() {
        let dir = TempDir::new();
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let key = volume(AgentId::new_v4());
        let now = OffsetDateTime::now_utc();
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            let key = key.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .acquire_scope_lock(&key, SessionId::new_v4(), now, TTL)
                    .await
                    .unwrap()
            }));
        }
        let mut granted = 0;
        for task in tasks {
            granted += usize::from(task.await.unwrap().is_some());
        }
        assert_eq!(granted, 1);
    }
}

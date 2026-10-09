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

use crate::{Result, Store, StoreError, from_unix, parse_column, to_unix, ttl_seconds};

const TOKENS: &str = "ctl_tokens";
const LOCKS: &str = "scope_locks";

/// Deletes every lease held by the session of the token whose digest is
/// bound.
const DELETE_TOKEN_LEASES: &str = "DELETE FROM scope_locks WHERE holder_session = \
     (SELECT session_id FROM ctl_tokens WHERE hash = ?)";

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

impl Store {
    /// Stores the token of a new `claude` process, with no turn running.
    ///
    /// A session runs one process at a time, and a container address holds
    /// one container at a time, so every token already stored for the same
    /// session or the same `container_ip` is deleted in the same
    /// transaction: the newest process's token is the only one that works
    /// for its session and from its address. A token whose revocation failed
    /// when its container stopped can't be presented from a new container
    /// Docker gives the address to. The leases of the replaced tokens'
    /// sessions go with them, since their processes can no longer renew
    /// them. Returns the digests of the tokens it replaced.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, including when `hash` is
    /// already stored.
    pub async fn put_ctl_token(&self, token: &NewCtlToken) -> Result<Vec<TokenHash>> {
        let session = token.session.to_string();
        let container_ip = token.container_ip.to_canonical().to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "DELETE FROM scope_locks WHERE holder_session = ? OR holder_session IN \
             (SELECT session_id FROM ctl_tokens WHERE container_ip = ?)",
        )
        .bind(&session)
        .bind(&container_ip)
        .execute(&mut *tx)
        .await?;
        let replaced: Vec<Vec<u8>> = sqlx::query_scalar(
            "DELETE FROM ctl_tokens WHERE session_id = ? OR container_ip = ? RETURNING hash",
        )
        .bind(&session)
        .bind(&container_ip)
        .fetch_all(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO ctl_tokens (hash, session_id, agent_id, volume_key, container_ip) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&token.hash.0[..])
        .bind(&session)
        .bind(token.agent.to_string())
        .bind(token.volume.to_string())
        .bind(&container_ip)
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
    /// Either way, every lease the token's session holds is deleted in the
    /// same transaction: a lease lasts no longer than the turn that took it,
    /// so the lock is free as soon as the turn ends.
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
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
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
        .execute(&mut *tx)
        .await?;
        sqlx::query(DELETE_TOKEN_LEASES)
            .bind(&hash.0[..])
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes the token with digest `hash`, and every lease its session
    /// holds. Returns false when it wasn't stored, so revoking twice is
    /// harmless.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn delete_ctl_token(&self, hash: &TokenHash) -> Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(DELETE_TOKEN_LEASES)
            .bind(&hash.0[..])
            .execute(&mut *tx)
            .await?;
        let result = sqlx::query("DELETE FROM ctl_tokens WHERE hash = ?")
            .bind(&hash.0[..])
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
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

    /// Takes the `shared/` lock of the volume of the token with digest
    /// `token` for its session, under `lease` until `now + ttl`, if `turn`
    /// is still the token's turn and either no unexpired lease holds the
    /// lock or the session already holds it under `lease`, which is then
    /// extended. Returns the new expiry, or `None`, changing nothing,
    /// otherwise: another lease holds the lock, whichever session it
    /// belongs to, the token's own included, or `lease` names a lease of
    /// another session or volume.
    ///
    /// The caller picks `lease`, so an acquire it retries after losing the
    /// answer gets back the lease an earlier attempt was granted.
    ///
    /// It is one statement: an insert, from the token's row only while it
    /// records `turn`, that on the volume's unique key takes over the
    /// existing row only if it has expired or is the same session's under
    /// `lease`, and never when `lease` names another volume's row, which
    /// it leaves alone rather than failing on the lease's own key. So an
    /// acquire authorized under a turn that has since been replaced or ended
    /// grants nothing, even when it lands after
    /// [`set_ctl_turn`](Self::set_ctl_turn) deleted the session's leases.
    /// Times are whole seconds, and `ttl` is at least one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn acquire_scope_lock(
        &self,
        token: &TokenHash,
        turn: TurnId,
        lease: LeaseId,
        now: OffsetDateTime,
        ttl: Duration,
    ) -> Result<Option<OffsetDateTime>> {
        let now = to_unix(now);
        let expires_at = now.saturating_add(ttl_seconds(ttl));
        let granted: Option<i64> = sqlx::query_scalar(
            "INSERT INTO scope_locks (lease_id, volume_key, holder_session, expires_at) \
             SELECT ?, volume_key, session_id, ? FROM ctl_tokens WHERE hash = ? AND turn_id = ? \
             ON CONFLICT (volume_key) DO UPDATE SET lease_id = excluded.lease_id, \
             holder_session = excluded.holder_session, expires_at = excluded.expires_at \
             WHERE (scope_locks.expires_at <= ? OR (scope_locks.lease_id = excluded.lease_id \
             AND scope_locks.holder_session = excluded.holder_session)) \
             AND NOT EXISTS (SELECT 1 FROM scope_locks AS other \
             WHERE other.lease_id = excluded.lease_id AND other.volume_key <> excluded.volume_key) \
             ON CONFLICT DO NOTHING \
             RETURNING expires_at",
        )
        .bind(lease.to_string())
        .bind(expires_at)
        .bind(&token.0[..])
        .bind(turn.to_string())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        granted
            .map(|at| from_unix(at, LOCKS, "expires_at"))
            .transpose()
    }

    /// Extends `lease` to `now + ttl`, if `turn` is still the turn of the
    /// token with digest `token`, and `lease` is the current, unexpired
    /// lease on the token's volume, held by its session. Returns the new
    /// expiry, or `None`, changing nothing, otherwise.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn renew_scope_lock(
        &self,
        token: &TokenHash,
        turn: TurnId,
        lease: LeaseId,
        now: OffsetDateTime,
        ttl: Duration,
    ) -> Result<Option<OffsetDateTime>> {
        let now = to_unix(now);
        let expires_at = now.saturating_add(ttl_seconds(ttl));
        let renewed: Option<i64> = sqlx::query_scalar(
            "UPDATE scope_locks SET expires_at = ? \
             WHERE lease_id = ? AND expires_at > ? AND (volume_key, holder_session) IN \
             (SELECT volume_key, session_id FROM ctl_tokens WHERE hash = ? AND turn_id = ?) \
             RETURNING expires_at",
        )
        .bind(expires_at)
        .bind(lease.to_string())
        .bind(now)
        .bind(&token.0[..])
        .bind(turn.to_string())
        .fetch_optional(&self.pool)
        .await?;
        renewed
            .map(|at| from_unix(at, LOCKS, "expires_at"))
            .transpose()
    }

    /// Gives up `lease`, if it is the current lease on the volume of the
    /// token with digest `token`, held by its session. Returns false,
    /// changing nothing, for any other lease.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn release_scope_lock(&self, token: &TokenHash, lease: LeaseId) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM scope_locks WHERE lease_id = ? AND (volume_key, holder_session) IN \
             (SELECT volume_key, session_id FROM ctl_tokens WHERE hash = ?)",
        )
        .bind(lease.to_string())
        .bind(&token.0[..])
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
            container_ip: IpAddr::from([172, 30, 0, byte]),
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
    async fn a_new_token_from_an_address_replaces_the_one_bound_to_it() {
        let store = memory_store().await;
        let address: IpAddr = "172.30.0.7".parse().unwrap();
        let key = volume(AgentId::new_v4());
        let stale = NewCtlToken {
            volume: key.clone(),
            container_ip: address,
            ..new_token(1, SessionId::new_v4())
        };
        let other = new_token(3, SessionId::new_v4());
        store.put_ctl_token(&stale).await.unwrap();
        store.put_ctl_token(&other).await.unwrap();
        let holder = Holder::begin(&store, stale.hash).await;
        holder.acquire(&store, 1_000).await.unwrap();
        let probe = Holder::new(&store, 4, &key).await;
        assert!(!free(&store, probe).await);

        let fresh = NewCtlToken {
            volume: key.clone(),
            container_ip: address,
            ..new_token(2, SessionId::new_v4())
        };
        assert_eq!(store.put_ctl_token(&fresh).await.unwrap(), vec![stale.hash]);
        assert_eq!(store.ctl_token(&stale.hash).await.unwrap(), None);
        assert_eq!(
            store.ctl_token(&fresh.hash).await.unwrap().unwrap().session,
            fresh.session
        );
        assert!(store.ctl_token(&other.hash).await.unwrap().is_some());
        assert!(free(&store, probe).await, "the stale token's lease is gone");
    }

    #[tokio::test]
    async fn a_duplicate_hash_is_refused() {
        let store = memory_store().await;
        store
            .put_ctl_token(&new_token(1, SessionId::new_v4()))
            .await
            .unwrap();
        let elsewhere = NewCtlToken {
            container_ip: IpAddr::from([172, 30, 0, 2]),
            ..new_token(1, SessionId::new_v4())
        };
        let err = store.put_ctl_token(&elsewhere).await.unwrap_err();
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

    /// A request authorized under a token's turn.
    #[derive(Debug, Clone, Copy)]
    struct Holder {
        hash: TokenHash,
        turn: TurnId,
    }

    impl Holder {
        /// Stores a token for a new session on `volume` and begins a turn on
        /// it.
        async fn new(store: &Store, byte: u8, volume: &VolumeKey) -> Self {
            let token = NewCtlToken {
                volume: volume.clone(),
                ..new_token(byte, SessionId::new_v4())
            };
            store.put_ctl_token(&token).await.unwrap();
            Self::begin(store, token.hash).await
        }

        /// Begins a new turn on the token with digest `hash`.
        async fn begin(store: &Store, hash: TokenHash) -> Self {
            let running = turn(TurnKind::Normal, Side::Public);
            assert!(store.set_ctl_turn(&hash, Some(&running)).await.unwrap());
            Self {
                hash,
                turn: running.id,
            }
        }

        /// Acquires under a new lease, and returns it if granted.
        async fn acquire(self, store: &Store, now: i64) -> Option<LeaseId> {
            let lease = LeaseId::new_v4();
            self.acquire_as(store, lease, now).await.map(|_| lease)
        }

        async fn acquire_as(
            self,
            store: &Store,
            lease: LeaseId,
            now: i64,
        ) -> Option<OffsetDateTime> {
            store
                .acquire_scope_lock(&self.hash, self.turn, lease, at(now), TTL)
                .await
                .unwrap()
        }

        async fn renew(self, store: &Store, lease: LeaseId, now: i64) -> Option<OffsetDateTime> {
            store
                .renew_scope_lock(&self.hash, self.turn, lease, at(now), TTL)
                .await
                .unwrap()
        }

        async fn release(self, store: &Store, lease: LeaseId) -> bool {
            store.release_scope_lock(&self.hash, lease).await.unwrap()
        }
    }

    /// Whether `probe`, another session on the volume, can take the lock at
    /// 1,000 s. It gives the lock back if it can.
    async fn free(store: &Store, probe: Holder) -> bool {
        match probe.acquire(store, 1_000).await {
            Some(lease) => probe.release(store, lease).await,
            None => false,
        }
    }

    #[tokio::test]
    async fn a_sessions_leases_end_with_its_turn_and_its_token() {
        let store = memory_store().await;
        let elsewhere = Holder::new(&store, 9, &volume(AgentId::new_v4())).await;
        let kept = elsewhere.acquire(&store, 1_000).await.unwrap();
        let first = NewCtlToken {
            volume: volume(AgentId::new_v4()),
            ..new_token(1, SessionId::new_v4())
        };
        let second = NewCtlToken {
            hash: TokenHash([2; 32]),
            ..first.clone()
        };
        store.put_ctl_token(&first).await.unwrap();
        let probe = Holder::new(&store, 3, &first.volume).await;

        let holder = Holder::begin(&store, first.hash).await;
        holder.acquire(&store, 1_000).await.unwrap();
        assert!(!free(&store, probe).await);
        Holder::begin(&store, first.hash).await;
        assert!(free(&store, probe).await, "a new turn");

        let holder = Holder::begin(&store, first.hash).await;
        holder.acquire(&store, 1_000).await.unwrap();
        assert!(store.set_ctl_turn(&first.hash, None).await.unwrap());
        assert!(free(&store, probe).await, "the turn ended");

        let holder = Holder::begin(&store, first.hash).await;
        holder.acquire(&store, 1_000).await.unwrap();
        store.put_ctl_token(&second).await.unwrap();
        assert!(free(&store, probe).await, "a new token");

        let holder = Holder::begin(&store, second.hash).await;
        holder.acquire(&store, 1_000).await.unwrap();
        assert!(store.delete_ctl_token(&second.hash).await.unwrap());
        assert!(free(&store, probe).await, "revoked");
        assert!(
            elsewhere.renew(&store, kept, 1_010).await.is_some(),
            "another session's lease is untouched"
        );
    }

    #[tokio::test]
    async fn an_acquire_or_renew_authorized_under_a_replaced_turn_does_nothing() {
        let store = memory_store().await;
        let key = volume(AgentId::new_v4());
        let first = Holder::new(&store, 1, &key).await;
        let probe = Holder::new(&store, 2, &key).await;

        let second = Holder::begin(&store, first.hash).await;
        assert_eq!(
            first.acquire(&store, 1_000).await,
            None,
            "an acquire authorized under the first turn, landing after the second began"
        );
        assert!(
            probe.acquire(&store, 1_000).await.is_some(),
            "the lock stayed free"
        );
        store.set_ctl_turn(&probe.hash, None).await.unwrap();

        let held = second.acquire(&store, 1_000).await.unwrap();
        let third = Holder::begin(&store, first.hash).await;
        assert_eq!(second.renew(&store, held, 1_010).await, None);
        assert_eq!(third.renew(&store, held, 1_010).await, None);

        let held = third.acquire(&store, 1_000).await.unwrap();
        assert_eq!(
            second.renew(&store, held, 1_010).await,
            None,
            "a renewal authorized under an earlier turn"
        );
        assert_eq!(third.renew(&store, held, 1_010).await, Some(at(1_040)));
        assert!(store.set_ctl_turn(&first.hash, None).await.unwrap());
        assert_eq!(third.acquire(&store, 1_000).await, None, "between turns");
    }

    #[tokio::test]
    async fn purge_deletes_every_token_and_lock() {
        let store = memory_store().await;
        let key = volume(AgentId::new_v4());
        let a = Holder::new(&store, 1, &key).await;
        Holder::new(&store, 2, &volume(AgentId::new_v4())).await;
        a.acquire(&store, 1_000).await.unwrap();
        assert_eq!(
            store.purge_ctl().await.unwrap(),
            CtlPurged {
                tokens: 2,
                locks: 1
            }
        );
        assert_eq!(store.ctl_token(&a.hash).await.unwrap(), None);
        let b = Holder::new(&store, 3, &key).await;
        assert!(b.acquire(&store, 1_000).await.is_some());
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
        let key = volume(AgentId::new_v4());
        let (a, b) = (
            Holder::new(&store, 1, &key).await,
            Holder::new(&store, 2, &key).await,
        );
        let held = LeaseId::new_v4();
        assert_eq!(a.acquire_as(&store, held, 1_000).await, Some(at(1_030)));
        for holder in [a, b] {
            assert_eq!(holder.acquire(&store, 1_029).await, None);
        }
        let elsewhere = Holder::new(&store, 3, &volume(AgentId::new_v4())).await;
        assert!(elsewhere.acquire(&store, 1_000).await.is_some());
        assert!(a.release(&store, held).await);
        assert!(a.acquire(&store, 1_001).await.is_some());
    }

    #[tokio::test]
    async fn an_acquire_repeated_under_its_lease_by_its_session_holds_it_again() {
        let store = memory_store().await;
        let key = volume(AgentId::new_v4());
        let (a, b) = (
            Holder::new(&store, 1, &key).await,
            Holder::new(&store, 2, &key).await,
        );
        let elsewhere = Holder::new(&store, 3, &volume(AgentId::new_v4())).await;
        let lease = LeaseId::new_v4();
        assert_eq!(a.acquire_as(&store, lease, 1_000).await, Some(at(1_030)));
        assert_eq!(
            a.acquire_as(&store, lease, 1_010).await,
            Some(at(1_040)),
            "the same lease and session hold it again, for longer"
        );
        assert_eq!(
            b.acquire_as(&store, lease, 1_020).await,
            None,
            "another session naming the lease"
        );
        assert_eq!(
            elsewhere.acquire_as(&store, lease, 1_020).await,
            None,
            "a session on another volume naming the lease"
        );
        assert_eq!(a.acquire(&store, 1_020).await, None, "another lease");
        assert_eq!(a.renew(&store, lease, 1_039).await, Some(at(1_069)));
        let taken = b.acquire(&store, 1_069).await.unwrap();
        assert_eq!(
            a.acquire_as(&store, lease, 1_070).await,
            None,
            "an expired lease taken over"
        );
        assert!(b.release(&store, taken).await);
        assert_eq!(
            a.acquire_as(&store, lease, 1_070).await,
            Some(at(1_100)),
            "a free lock"
        );
        let theirs = elsewhere.acquire(&store, 1_090).await.unwrap();
        assert_eq!(
            a.acquire_as(&store, theirs, 1_100).await,
            None,
            "a lease of another volume, over this volume's expired row"
        );
        assert_eq!(
            elsewhere.renew(&store, theirs, 1_100).await,
            Some(at(1_130)),
            "the other volume's lease is untouched"
        );
        assert!(
            a.release(&store, lease).await,
            "this volume's row is unchanged"
        );
    }

    #[tokio::test]
    async fn an_expired_lease_is_taken_over_and_can_no_longer_act() {
        let store = memory_store().await;
        let key = volume(AgentId::new_v4());
        let (a, b) = (
            Holder::new(&store, 1, &key).await,
            Holder::new(&store, 2, &key).await,
        );
        let dead = a.acquire(&store, 1_000).await.unwrap();
        assert_eq!(
            a.renew(&store, dead, 1_030).await,
            None,
            "an expired lease can't be renewed"
        );
        let live = b.acquire(&store, 1_030).await.unwrap();
        assert!(!a.release(&store, dead).await);
        assert_eq!(a.renew(&store, dead, 1_031).await, None);
        assert_eq!(b.renew(&store, live, 1_040).await, Some(at(1_070)));
        assert_eq!(a.acquire(&store, 1_069).await, None, "the renewal held");
    }

    #[tokio::test]
    async fn renew_and_release_must_match_volume_holder_and_lease() {
        let store = memory_store().await;
        let key = volume(AgentId::new_v4());
        let a = Holder::new(&store, 1, &key).await;
        let b = Holder::new(&store, 2, &key).await;
        let other = Holder::new(&store, 3, &volume(AgentId::new_v4())).await;
        let held = a.acquire(&store, 1_000).await.unwrap();
        let stranger = LeaseId::new_v4();
        for (holder, lease) in [(a, stranger), (b, held), (other, held)] {
            assert_eq!(holder.renew(&store, lease, 1_010).await, None);
            assert!(!holder.release(&store, lease).await);
        }
        assert_eq!(
            b.acquire(&store, 1_010).await,
            None,
            "the lease is untouched"
        );
    }

    #[tokio::test]
    async fn a_zero_ttl_still_lasts_a_second() {
        let store = memory_store().await;
        let holder = Holder::new(&store, 1, &volume(AgentId::new_v4())).await;
        let held = store
            .acquire_scope_lock(
                &holder.hash,
                holder.turn,
                LeaseId::new_v4(),
                at(1_000),
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(held, Some(at(1_001)));
    }

    #[tokio::test]
    async fn concurrent_acquires_grant_one_lease() {
        let dir = TempDir::new("store-test");
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let key = volume(AgentId::new_v4());
        let now = OffsetDateTime::now_utc();
        let mut tasks = Vec::new();
        for byte in 0..8 {
            let holder = Holder::new(&store, byte, &key).await;
            let store = store.clone();
            tasks.push(tokio::spawn(async move {
                store
                    .acquire_scope_lock(&holder.hash, holder.turn, LeaseId::new_v4(), now, TTL)
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

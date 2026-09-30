//! `sessions`: one Claude Code session per agent and thread, and private
//! tasks' sessions.

use core_types::{
    AgentId, ConsentId, ConvRef, ConversationId, MessageId, ScopeKey, SessionId, SurfaceKind,
    TeamId, ThreadKey, VolumeKey,
};
use sqlx::SqliteConnection;
use time::OffsetDateTime;

use crate::{Result, Store, StoreError, from_unix, parse_column, to_unix};

const TABLE: &str = "sessions";

/// How many [`reset_session`](Store::reset_session) calls write at once, per
/// [`Store`] and its clones. SQLite has one writer, so more would only
/// hold more of the pool's connections waiting for it.
pub const RESETS_AT_ONCE: usize = 2;

/// The columns every query reads, in [`Row`]'s order.
macro_rules! columns {
    () => {
        "id, agent_id, surface, team_id, conversation, thread_root, scope_key, kind, \
         consent_id, started, maybe_started, created_at, last_turn_at, reset_at"
    };
}

/// The condition for an agent's session in use, binding the agent id and
/// the warm session ids as a JSON array, in that order.
macro_rules! in_use {
    () => {
        "agent_id = ? AND reset_at IS NULL \
         AND (started OR maybe_started OR last_turn_at IS NOT NULL \
              OR id IN (SELECT value FROM json_each(?)))"
    };
}

/// `ids` as a JSON array of strings, for `json_each`.
fn ids_json(ids: &[SessionId]) -> sqlx::types::Json<Vec<String>> {
    sqlx::types::Json(ids.iter().map(ToString::to_string).collect())
}

/// What kind of session a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionKind {
    /// A DM's or a channel thread's session, found by its agent and thread.
    Normal,
    /// A private task's session, run under the consent it names. It is
    /// never found by thread: its thread is where the task's result goes.
    Private(ConsentId),
}

/// A `sessions` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    /// The session id, passed to the CLI as `--session-id` or `--resume`.
    pub id: SessionId,
    /// The agent whose session it is.
    pub agent: AgentId,
    /// A normal session's thread, or where a private task's result goes.
    /// A DM's session has no root.
    pub thread: ThreadKey,
    /// The scope whose volume the session mounts.
    pub scope: ScopeKey,
    /// A normal session or a private task's.
    pub kind: SessionKind,
    /// Whether the CLI has read one of the session's messages, so it has a
    /// transcript and the next process must resume it.
    pub started: bool,
    /// Whether a turn went to the CLI of the unstarted session without its
    /// outcome being recorded, so it may have a transcript: the next
    /// process tries `--resume` first.
    pub maybe_started: bool,
    /// When the row was created.
    pub created_at: OffsetDateTime,
    /// When its last turn ended, if one has.
    pub last_turn_at: Option<OffsetDateTime>,
    /// When it was reset. A reset session takes no more turns.
    pub reset_at: Option<OffsetDateTime>,
}

impl Session {
    /// The volume the session mounts: its agent's, for its scope.
    pub fn volume(&self) -> VolumeKey {
        VolumeKey {
            agent: self.agent,
            scope: self.scope.clone(),
        }
    }

    /// Whether the next process should resume the session's transcript
    /// rather than start it with `--session-id`.
    pub fn resumes(&self) -> bool {
        self.started || self.maybe_started
    }
}

/// What [`Store::session_for_thread`] found or made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSession {
    /// The thread's live session.
    pub session: Session,
    /// A live session of the thread that was reset because its scope
    /// wasn't the one asked for. Its warm container, if any, should be
    /// stopped.
    pub replaced: Option<SessionId>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    agent_id: String,
    surface: String,
    team_id: String,
    conversation: String,
    thread_root: String,
    scope_key: String,
    kind: String,
    consent_id: Option<String>,
    started: i64,
    maybe_started: i64,
    created_at: i64,
    last_turn_at: Option<i64>,
    reset_at: Option<i64>,
}

fn corrupt(column: &'static str) -> StoreError {
    StoreError::Corrupt {
        table: TABLE,
        column,
    }
}

fn flag(value: i64, column: &'static str) -> Result<bool> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(corrupt(column)),
    }
}

impl Row {
    fn into_session(self) -> Result<Session> {
        let kind = match (self.kind.as_str(), self.consent_id) {
            ("normal", None) => SessionKind::Normal,
            ("private", Some(consent)) => {
                SessionKind::Private(parse_column(&consent, TABLE, "consent_id")?)
            }
            _ => return Err(corrupt("kind")),
        };
        Ok(Session {
            id: parse_column(&self.id, TABLE, "id")?,
            agent: parse_column(&self.agent_id, TABLE, "agent_id")?,
            thread: ThreadKey {
                conv: ConvRef {
                    surface: parse_column::<SurfaceKind>(&self.surface, TABLE, "surface")?,
                    team: TeamId::new(self.team_id),
                    conversation: ConversationId::new(self.conversation),
                },
                root: (!self.thread_root.is_empty()).then(|| MessageId::new(self.thread_root)),
            },
            scope: parse_column(&self.scope_key, TABLE, "scope_key")?,
            kind,
            started: flag(self.started, "started")?,
            maybe_started: flag(self.maybe_started, "maybe_started")?,
            created_at: from_unix(self.created_at, TABLE, "created_at")?,
            last_turn_at: self
                .last_turn_at
                .map(|at| from_unix(at, TABLE, "last_turn_at"))
                .transpose()?,
            reset_at: self
                .reset_at
                .map(|at| from_unix(at, TABLE, "reset_at"))
                .transpose()?,
        })
    }
}

/// The `thread_root` column for a thread: its root's id, or `''` for a
/// conversation's top level.
fn root_column(thread: &ThreadKey) -> &str {
    thread.root.as_ref().map_or("", MessageId::as_str)
}

/// The thread's live normal session, read inside a transaction.
async fn live_for_thread(
    conn: &mut SqliteConnection,
    agent: AgentId,
    thread: &ThreadKey,
) -> Result<Option<Session>> {
    let row: Option<Row> = sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM sessions WHERE agent_id = ? AND surface = ? AND team_id = ? \
         AND conversation = ? AND thread_root = ? AND kind = 'normal' AND reset_at IS NULL"
    ))
    .bind(agent.to_string())
    .bind(thread.conv.surface.as_str())
    .bind(thread.conv.team.as_str())
    .bind(thread.conv.conversation.as_str())
    .bind(root_column(thread))
    .fetch_optional(conn)
    .await?;
    row.map(Row::into_session).transpose()
}

/// Inserts a new, unstarted session and returns it.
async fn insert(
    conn: &mut SqliteConnection,
    agent: AgentId,
    thread: &ThreadKey,
    scope: &ScopeKey,
    kind: SessionKind,
    now: OffsetDateTime,
) -> Result<Session> {
    let (kind, consent) = match kind {
        SessionKind::Normal => ("normal", None),
        SessionKind::Private(consent) => ("private", Some(consent.to_string())),
    };
    let row: Row = sqlx::query_as(concat!(
        "INSERT INTO sessions (id, agent_id, surface, team_id, conversation, thread_root, \
         scope_key, kind, consent_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         RETURNING ",
        columns!()
    ))
    .bind(SessionId::new_v4().to_string())
    .bind(agent.to_string())
    .bind(thread.conv.surface.as_str())
    .bind(thread.conv.team.as_str())
    .bind(thread.conv.conversation.as_str())
    .bind(root_column(thread))
    .bind(scope.to_string())
    .bind(kind)
    .bind(consent)
    .bind(to_unix(now))
    .fetch_one(conn)
    .await?;
    row.into_session()
}

async fn mark_reset(conn: &mut SqliteConnection, id: SessionId, now: OffsetDateTime) -> Result<()> {
    sqlx::query("UPDATE sessions SET reset_at = ? WHERE id = ? AND reset_at IS NULL")
        .bind(to_unix(now))
        .bind(id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

impl Store {
    /// The live normal session of `agent` in `thread`, created with a new
    /// v4 id if there is none. A thread with no root is a conversation's
    /// top level, a DM's one continuous session; a root with an empty id
    /// names the same session.
    ///
    /// A live session of the thread on another scope is reset and replaced,
    /// and named in [`ThreadSession::replaced`]: a session never changes
    /// volume, so a DM that stops being the owner's never reaches the
    /// agent's `Private` volume through its old session.
    ///
    /// It runs in one `BEGIN IMMEDIATE` transaction, so concurrent calls for
    /// one thread return one session.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if a query fails, [`StoreError::Corrupt`] if
    /// the row doesn't parse.
    pub async fn session_for_thread(
        &self,
        agent: AgentId,
        thread: &ThreadKey,
        scope: &ScopeKey,
        now: OffsetDateTime,
    ) -> Result<ThreadSession> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let mut replaced = None;
        if let Some(session) = live_for_thread(&mut tx, agent, thread).await? {
            if session.scope == *scope {
                tx.commit().await?;
                return Ok(ThreadSession {
                    session,
                    replaced: None,
                });
            }
            mark_reset(&mut tx, session.id, now).await?;
            replaced = Some(session.id);
        }
        let session = insert(&mut tx, agent, thread, scope, SessionKind::Normal, now).await?;
        tx.commit().await?;
        Ok(ThreadSession { session, replaced })
    }

    /// A new private task's session for `agent`, on its `Private` scope,
    /// with a new v4 id. `thread` is where the task's result is posted.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn create_private_session(
        &self,
        agent: AgentId,
        consent: ConsentId,
        thread: &ThreadKey,
        now: OffsetDateTime,
    ) -> Result<Session> {
        let mut conn = self.pool.acquire().await?;
        insert(
            &mut conn,
            agent,
            thread,
            &ScopeKey::Private,
            SessionKind::Private(consent),
            now,
        )
        .await
    }

    /// The session with id `id`, reset or not.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the row doesn't parse.
    pub async fn session(&self, id: SessionId) -> Result<Option<Session>> {
        let row: Option<Row> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM sessions WHERE id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(Row::into_session).transpose()
    }

    /// The sessions of `agent` in use, normal and private, most recently
    /// active first (by the end of the last turn, or the creation), at most
    /// `limit` of them, or all with `None`. A session in use is live (not
    /// reset) and has had a turn finish, has had one go to its CLI, or is
    /// one of `warm`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn sessions_in_use(
        &self,
        agent: AgentId,
        warm: &[SessionId],
        limit: Option<usize>,
    ) -> Result<Vec<Session>> {
        let limit = limit.map_or(-1, |limit| i64::try_from(limit).unwrap_or(i64::MAX));
        let rows: Vec<Row> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM sessions WHERE ",
            in_use!(),
            " ORDER BY COALESCE(last_turn_at, created_at) DESC, created_at DESC, id LIMIT ?"
        ))
        .bind(agent.to_string())
        .bind(ids_json(warm))
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Row::into_session).collect()
    }

    /// How many sessions of `agent` are in use, as
    /// [`sessions_in_use`](Self::sessions_in_use) counts them.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn count_sessions_in_use(&self, agent: AgentId, warm: &[SessionId]) -> Result<usize> {
        let count: i64 =
            sqlx::query_scalar(concat!("SELECT COUNT(*) FROM sessions WHERE ", in_use!()))
                .bind(agent.to_string())
                .bind(ids_json(warm))
                .fetch_one(&self.pool)
                .await?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Resets session `id`: marks it reset and, for a normal session,
    /// inserts its replacement for the same thread and scope, unstarted and
    /// with a new v4 id, in the same transaction. Returns the replacement;
    /// `None` for a private task's session, and for a session that is
    /// unknown or already reset.
    ///
    /// It waits for one of [`RESETS_AT_ONCE`] permits before it takes a
    /// connection, so a reset of thousands of sessions leaves the rest of
    /// the pool to other work.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if a query fails, [`StoreError::Corrupt`] if
    /// the row doesn't parse.
    pub async fn reset_session(
        &self,
        id: SessionId,
        now: OffsetDateTime,
    ) -> Result<Option<Session>> {
        let _permit = self.resets.acquire().await;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row: Option<Row> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM sessions WHERE id = ? AND reset_at IS NULL"
        ))
        .bind(id.to_string())
        .fetch_optional(&mut *tx)
        .await?;
        let Some(old) = row.map(Row::into_session).transpose()? else {
            tx.commit().await?;
            return Ok(None);
        };
        mark_reset(&mut tx, old.id, now).await?;
        let replacement = match old.kind {
            SessionKind::Normal => Some(
                insert(
                    &mut tx,
                    old.agent,
                    &old.thread,
                    &old.scope,
                    SessionKind::Normal,
                    now,
                )
                .await?,
            ),
            SessionKind::Private(_) => None,
        };
        tx.commit().await?;
        Ok(replacement)
    }

    /// Records that a turn is going to the CLI of session `id`: if the
    /// session hasn't started, it may have from now on, until
    /// [`record_session_turn`](Self::record_session_turn) says otherwise.
    /// Returns false if the session is unknown.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn mark_session_turn_pending(&self, id: SessionId) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE sessions SET maybe_started = 1 - started WHERE id = ? AND reset_at IS NULL",
        )
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Records the end of a turn of session `id` at `now`. `init_seen`
    /// says whether the CLI read the turn's message: the session has
    /// started if it did, and is then known to have a transcript.
    /// Otherwise whether it may have started is left as it was.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn record_session_turn(
        &self,
        id: SessionId,
        init_seen: bool,
        now: OffsetDateTime,
    ) -> Result<()> {
        sqlx::query(
            "UPDATE sessions SET started = MAX(started, ?1), \
             maybe_started = CASE WHEN ?1 = 1 THEN 0 ELSE maybe_started END, \
             last_turn_at = ?2 WHERE id = ?3",
        )
        .bind(i64::from(init_seen))
        .bind(to_unix(now))
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Records that session `id` has no transcript, because the CLI
    /// refused to resume it: the next process starts it with
    /// `--session-id` under the same id.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn mark_session_unstarted(&self, id: SessionId) -> Result<()> {
        sqlx::query("UPDATE sessions SET started = 0, maybe_started = 0 WHERE id = ?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use core_types::{ConvKind, SurfaceKind};

    use super::*;
    use crate::test_util::*;

    fn conv(conversation: &str) -> ConvRef {
        ConvRef {
            surface: SurfaceKind::Slack,
            team: TeamId::new("T1"),
            conversation: ConversationId::new(conversation),
        }
    }

    fn dm() -> ThreadKey {
        ThreadKey {
            conv: conv("D1"),
            root: None,
        }
    }

    fn thread(root: &str) -> ThreadKey {
        ThreadKey {
            conv: conv("C1"),
            root: Some(MessageId::new(root)),
        }
    }

    fn channel() -> ScopeKey {
        ScopeKey::for_conversation(ConvKind::Channel, conv("C1"))
    }

    #[tokio::test]
    async fn a_thread_gets_one_session_and_keeps_it() {
        let store = memory_store().await;
        let agent = AgentId::new_v4();
        let first = store
            .session_for_thread(agent, &thread("1.1"), &channel(), at(100))
            .await
            .unwrap();
        assert_eq!(first.replaced, None);
        let session = &first.session;
        assert_eq!(session.id.as_uuid().get_version_num(), 4);
        assert_eq!(session.agent, agent);
        assert_eq!(session.thread, thread("1.1"));
        assert_eq!(session.scope, channel());
        assert_eq!(session.kind, SessionKind::Normal);
        assert!(!session.started && !session.maybe_started && !session.resumes());
        assert_eq!(session.created_at, at(100));
        assert_eq!(session.last_turn_at, None);
        assert_eq!(session.reset_at, None);
        assert_eq!(
            session.volume(),
            VolumeKey {
                agent,
                scope: channel()
            }
        );
        let again = store
            .session_for_thread(agent, &thread("1.1"), &channel(), at(200))
            .await
            .unwrap();
        assert_eq!(again.session, first.session);
        assert_eq!(
            store.session(session.id).await.unwrap().as_ref(),
            Some(session)
        );
    }

    #[tokio::test]
    async fn threads_agents_and_surfaces_get_their_own_sessions() {
        let store = memory_store().await;
        let agent = AgentId::new_v4();
        let one = store
            .session_for_thread(agent, &thread("1.1"), &channel(), at(1))
            .await
            .unwrap()
            .session;
        let two = store
            .session_for_thread(agent, &thread("2.2"), &channel(), at(1))
            .await
            .unwrap()
            .session;
        let other_agent = store
            .session_for_thread(AgentId::new_v4(), &thread("1.1"), &channel(), at(1))
            .await
            .unwrap()
            .session;
        let mut rocket = thread("1.1");
        rocket.conv.surface = SurfaceKind::RocketChat;
        let other_surface = store
            .session_for_thread(agent, &rocket, &channel(), at(1))
            .await
            .unwrap()
            .session;
        let top = ThreadKey {
            conv: conv("C1"),
            root: None,
        };
        let top_level = store
            .session_for_thread(agent, &top, &channel(), at(1))
            .await
            .unwrap()
            .session;
        let ids = [
            one.id,
            two.id,
            other_agent.id,
            other_surface.id,
            top_level.id,
        ];
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(top_level.thread.root, None);
    }

    #[tokio::test]
    async fn two_dm_lookups_create_one_session() {
        let dir = TempDir::new();
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let agent = AgentId::new_v4();
        let scope = ScopeKey::Private;
        let lookups: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                let scope = scope.clone();
                tokio::spawn(async move {
                    store
                        .session_for_thread(agent, &dm(), &scope, at(5))
                        .await
                        .unwrap()
                        .session
                        .id
                })
            })
            .collect();
        let mut ids = Vec::new();
        for lookup in lookups {
            ids.push(lookup.await.unwrap());
        }
        ids.dedup();
        assert_eq!(ids.len(), 1, "{ids:?}");
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
        let dm_row: String = sqlx::query_scalar("SELECT thread_root FROM sessions")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(dm_row, "");
    }

    #[tokio::test]
    async fn a_reset_session_is_replaced_without_a_conflict() {
        let store = memory_store().await;
        let agent = AgentId::new_v4();
        let old = store
            .session_for_thread(agent, &dm(), &ScopeKey::Private, at(1))
            .await
            .unwrap()
            .session;
        store.mark_session_turn_pending(old.id).await.unwrap();
        store
            .record_session_turn(old.id, true, at(2))
            .await
            .unwrap();
        let new = store
            .reset_session(old.id, at(3))
            .await
            .unwrap()
            .expect("a replacement");
        assert_ne!(new.id, old.id);
        assert_eq!(new.thread, old.thread);
        assert_eq!(new.scope, old.scope);
        assert!(!new.started && !new.maybe_started);
        assert_eq!(new.created_at, at(3));
        let old_now = store.session(old.id).await.unwrap().unwrap();
        assert_eq!(old_now.reset_at, Some(at(3)));
        assert!(old_now.started);
        let found = store
            .session_for_thread(agent, &dm(), &ScopeKey::Private, at(4))
            .await
            .unwrap();
        assert_eq!(found.session, new);
        assert_eq!(found.replaced, None);
        assert_eq!(store.reset_session(old.id, at(5)).await.unwrap(), None);
        assert_eq!(
            store
                .reset_session(SessionId::new_v4(), at(5))
                .await
                .unwrap(),
            None
        );
        let again = store.reset_session(new.id, at(6)).await.unwrap().unwrap();
        assert_ne!(again.id, new.id);
    }

    #[tokio::test]
    async fn at_most_a_few_resets_write_at_once() {
        use sqlx::Connection;

        let dir = TempDir::new();
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let agent = AgentId::new_v4();
        let mut ids = Vec::new();
        for n in 0..3 * RESETS_AT_ONCE {
            let found = store
                .session_for_thread(agent, &thread(&format!("{n}.1")), &channel(), at(1))
                .await
                .unwrap();
            ids.push(found.session.id);
        }
        let mut writer = SqliteConnection::connect(&dir.db_url()).await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut writer)
            .await
            .unwrap();
        let settled = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while usize::try_from(store.pool.size()).unwrap() > store.pool.num_idle() {
            assert!(
                tokio::time::Instant::now() < settled,
                "the setup's connections go back to the pool"
            );
            tokio::task::yield_now().await;
        }
        let resets: Vec<_> = ids
            .into_iter()
            .map(|id| {
                let store = store.clone();
                tokio::spawn(async move { store.reset_session(id, at(2)).await })
            })
            .collect();
        tokio::task::yield_now().await;
        let in_use = usize::try_from(store.pool.size()).unwrap() - store.pool.num_idle();
        assert_eq!(
            in_use, RESETS_AT_ONCE,
            "only the resets holding a permit wait for the writer with a connection"
        );
        sqlx::query("COMMIT").execute(&mut writer).await.unwrap();
        for reset in resets {
            assert!(reset.await.unwrap().unwrap().is_some());
        }
    }

    #[tokio::test]
    async fn a_thread_on_another_scope_replaces_its_session() {
        let store = memory_store().await;
        let agent = AgentId::new_v4();
        let owner = store
            .session_for_thread(agent, &dm(), &ScopeKey::Private, at(1))
            .await
            .unwrap()
            .session;
        let public = ScopeKey::for_conversation(ConvKind::Dm, conv("D1"));
        let found = store
            .session_for_thread(agent, &dm(), &public, at(2))
            .await
            .unwrap();
        assert_eq!(found.replaced, Some(owner.id));
        assert_ne!(found.session.id, owner.id);
        assert_eq!(found.session.scope, public);
        assert_eq!(
            store.session(owner.id).await.unwrap().unwrap().reset_at,
            Some(at(2))
        );
    }

    #[tokio::test]
    async fn private_sessions_are_always_new_and_never_found_by_thread() {
        let store = memory_store().await;
        let agent = AgentId::new_v4();
        let consent = ConsentId::new_v4();
        let one = store
            .create_private_session(agent, consent, &thread("1.1"), at(1))
            .await
            .unwrap();
        let two = store
            .create_private_session(agent, consent, &thread("1.1"), at(1))
            .await
            .unwrap();
        assert_ne!(one.id, two.id);
        assert_eq!(one.kind, SessionKind::Private(consent));
        assert_eq!(one.scope, ScopeKey::Private);
        assert_eq!(one.thread, thread("1.1"));
        let normal = store
            .session_for_thread(agent, &thread("1.1"), &ScopeKey::Private, at(1))
            .await
            .unwrap();
        assert_ne!(normal.session.id, one.id);
        assert_ne!(normal.session.id, two.id);
        assert_eq!(store.reset_session(one.id, at(2)).await.unwrap(), None);
        assert_eq!(
            store.session(one.id).await.unwrap().unwrap().reset_at,
            Some(at(2))
        );
    }

    #[tokio::test]
    async fn started_follows_what_the_cli_read() {
        let store = memory_store().await;
        let session = store
            .session_for_thread(AgentId::new_v4(), &dm(), &ScopeKey::Private, at(1))
            .await
            .unwrap()
            .session;
        let read = |store: Store| async move { store.session(session.id).await.unwrap().unwrap() };

        assert!(store.mark_session_turn_pending(session.id).await.unwrap());
        let pending = read(store.clone()).await;
        assert!(!pending.started && pending.maybe_started && pending.resumes());

        store
            .record_session_turn(session.id, false, at(2))
            .await
            .unwrap();
        let unknown = read(store.clone()).await;
        assert!(!unknown.started && unknown.maybe_started);
        assert_eq!(unknown.last_turn_at, Some(at(2)));

        store
            .record_session_turn(session.id, true, at(3))
            .await
            .unwrap();
        let started = read(store.clone()).await;
        assert!(started.started && !started.maybe_started);

        assert!(store.mark_session_turn_pending(session.id).await.unwrap());
        store
            .record_session_turn(session.id, false, at(4))
            .await
            .unwrap();
        let still = read(store.clone()).await;
        assert!(still.started && !still.maybe_started, "{still:?}");
        assert_eq!(still.last_turn_at, Some(at(4)));

        store.mark_session_unstarted(session.id).await.unwrap();
        let refused = read(store.clone()).await;
        assert!(!refused.started && !refused.maybe_started && !refused.resumes());

        assert!(
            !store
                .mark_session_turn_pending(SessionId::new_v4())
                .await
                .unwrap()
        );
        store.reset_session(session.id, at(5)).await.unwrap();
        assert!(!store.mark_session_turn_pending(session.id).await.unwrap());
    }

    #[tokio::test]
    async fn sessions_in_use_are_the_live_ones_that_ran_or_are_warm_most_recent_first() {
        let store = memory_store().await;
        let agent = AgentId::new_v4();
        let old = store
            .session_for_thread(agent, &thread("1.1"), &channel(), at(1))
            .await
            .unwrap()
            .session;
        store
            .record_session_turn(old.id, true, at(5))
            .await
            .unwrap();
        let dm = store
            .session_for_thread(agent, &dm(), &ScopeKey::Private, at(2))
            .await
            .unwrap()
            .session;
        store.record_session_turn(dm.id, true, at(9)).await.unwrap();
        let unused = store
            .session_for_thread(agent, &thread("2.2"), &channel(), at(7))
            .await
            .unwrap()
            .session;
        let warm = store
            .session_for_thread(agent, &thread("4.4"), &channel(), at(3))
            .await
            .unwrap()
            .session;
        let pending = store
            .session_for_thread(agent, &thread("5.5"), &channel(), at(4))
            .await
            .unwrap()
            .session;
        assert!(store.mark_session_turn_pending(pending.id).await.unwrap());
        let task = store
            .create_private_session(agent, ConsentId::new_v4(), &thread("1.1"), at(3))
            .await
            .unwrap();
        store
            .record_session_turn(task.id, false, at(8))
            .await
            .unwrap();
        let reset = store
            .session_for_thread(agent, &thread("3.3"), &channel(), at(4))
            .await
            .unwrap()
            .session;
        store
            .record_session_turn(reset.id, true, at(10))
            .await
            .unwrap();
        let replacement = store.reset_session(reset.id, at(6)).await.unwrap().unwrap();
        let other = store
            .session_for_thread(AgentId::new_v4(), &thread("1.1"), &channel(), at(8))
            .await
            .unwrap()
            .session;
        store
            .record_session_turn(other.id, true, at(8))
            .await
            .unwrap();

        let ids = |sessions: Vec<Session>| -> Vec<SessionId> {
            sessions.into_iter().map(|session| session.id).collect()
        };
        let warm_ids = [warm.id, reset.id, other.id];
        assert_eq!(
            ids(store.sessions_in_use(agent, &warm_ids, None).await.unwrap()),
            [dm.id, task.id, old.id, pending.id, warm.id]
        );
        assert_eq!(
            store.count_sessions_in_use(agent, &warm_ids).await.unwrap(),
            5
        );
        assert_eq!(
            ids(store
                .sessions_in_use(agent, &warm_ids, Some(2))
                .await
                .unwrap()),
            [dm.id, task.id]
        );
        assert_eq!(
            ids(store.sessions_in_use(agent, &[], None).await.unwrap()),
            [dm.id, task.id, old.id, pending.id]
        );
        assert_eq!(store.count_sessions_in_use(agent, &[]).await.unwrap(), 4);
        for left_out in [unused.id, replacement.id] {
            assert!(
                !ids(store.sessions_in_use(agent, &warm_ids, None).await.unwrap())
                    .contains(&left_out)
            );
        }
        assert!(
            store
                .sessions_in_use(AgentId::new_v4(), &warm_ids, None)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_corrupt_row_is_reported_by_column() {
        let store = memory_store().await;
        let session = store
            .session_for_thread(AgentId::new_v4(), &dm(), &ScopeKey::Private, at(1))
            .await
            .unwrap()
            .session;
        sqlx::query("UPDATE sessions SET scope_key = 'nonsense' WHERE id = ?")
            .bind(session.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store.session(session.id).await,
            Err(StoreError::Corrupt {
                table: "sessions",
                column: "scope_key"
            })
        ));
        assert!(matches!(
            flag(2, "started"),
            Err(StoreError::Corrupt { .. })
        ));
    }
}

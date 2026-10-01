//! Persistence and encryption at rest for agent-core.
//!
//! [`Store`] wraps a SQLite database through `sqlx`. It owns every encrypted
//! column: callers pass and receive [`SecretString`](secrecy::SecretString)s,
//! and the store seals them with its [`Sealer`] on the way in and opens them on
//! the way out, so no caller can forget to.
//!
//! Conventions every table follows (see the foundation migration):
//!
//! - IDs are `TEXT` in the lowercase hyphenated UUID form that `core-types`
//!   writes.
//! - Timestamps are `INTEGER` Unix seconds, so sub-second precision is
//!   dropped on the way in.
//! - Encrypted columns end in `_enc`, and their associated data is
//!   `table/column/primary key`.
//!
//! Repository methods are grouped by table: [`members`](Store::ensure_member),
//! [`claude_links`](Store::put_claude_link) and their
//! [relink notices](Store::claim_relink_notice),
//! [`pending_logins`](Store::put_pending_login),
//! [`processed_events`](Store::mark_event_processed),
//! [`ctl_tokens`](Store::put_ctl_token),
//! [`scope_locks`](Store::acquire_scope_lock),
//! [`volumes`](Store::put_volume), [`sessions`](Store::session_for_thread),
//! [`message_refs`](Store::record_message_ref),
//! [`slack_config_tokens`](Store::put_slack_config_token),
//! [`agents`](Store::create_agent) with their bindings,
//! [retirements](Store::claim_retirement) and
//! [Slack apps](Store::set_slack_app),
//! [`agent_skills`](Store::put_skill),
//! [`consents`](Store::create_consent),
//! [`community_settings`](Store::set_community_api_key),
//! [`failure_notices`](Store::claim_failure_notice), the usage meter
//! ([`usage`](Store::record_turn_usage), with `thread_usage` and
//! `limit_notices`), [`agent_policies`](Store::agent_settings) and
//! [`bans`](Store::ban_member).

#![warn(missing_docs)]

use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::migrate::Migrator;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};
use time::OffsetDateTime;
use tokio::sync::Semaphore;

mod agents;
mod claude_links;
mod community;
mod consents;
mod ctl;
mod events;
mod failure_notices;
mod members;
mod message_refs;
mod pending_logins;
mod policies;
mod relink_notices;
mod seal;
mod sessions;
mod skills;
mod slack_apps;
mod slack_config_tokens;
mod usage;
mod volumes;

pub use agents::{
    ActiveBot, Agent, AgentBinding, AgentCreation, AgentState, BindingState, DirectoryEntry,
    NewAgent, PendingRetirement, Visibility,
};
pub use claude_links::{ClaudeLink, ClaudeLinkStatus, ClaudeTokens, NewClaudeLink};
pub use community::CommunityKeyStatus;
pub use consents::{Consent, ConsentState, NewConsent};
pub use ctl::{CtlPurged, CtlToken, CtlTurn, NewCtlToken, ScopeLease, TokenHash};
pub use events::{PROCESSED_EVENT_RETENTION, Swept};
pub use message_refs::{MessageRef, NewMessageRef};
pub use pending_logins::PendingLogin;
pub use policies::{AgentSettings, Ban, NO_RULES};
pub use relink_notices::PendingRelinkNotice;
pub use seal::{KeyError, SealError, Sealer};
pub use sessions::{RESETS_AT_ONCE, Session, SessionKind, ThreadSession};
pub use skills::{AgentSkill, NewSkill, SkillState};
pub use slack_apps::{InstallReminder, NewSlackApp, SlackAppBinding, SlackAppKeys};
pub use slack_config_tokens::{
    NewSlackConfigToken, SlackConfigToken, SlackConfigTokenRef, SlackConfigTokenStatus,
};
pub use usage::{
    CostUnknown, LimitWindow, MemberUsage, THREAD_USAGE_RETENTION, ThreadSpend, TurnUsage,
    UsageTotals,
};
pub use volumes::Volume;

use seal::Aad;

/// How long a connection waits for another connection's write lock before
/// failing with `SQLITE_BUSY`.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// The error returned by [`Store`] methods.
///
/// No variant carries a secret: sealed values are reported by table and
/// column only, and SQL errors never include bound values.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database failed, or the connection URL is invalid.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// Migrations failed to apply.
    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    /// A secret column couldn't be sealed or opened.
    #[error("{table}.{column}: {source}")]
    Seal {
        /// The table.
        table: &'static str,
        /// The column.
        column: &'static str,
        /// What went wrong.
        source: SealError,
    },
    /// A stored value doesn't parse as its type, for example an ID that is
    /// not a canonical UUID.
    #[error("{table}.{column} holds an invalid value")]
    Corrupt {
        /// The table.
        table: &'static str,
        /// The column.
        column: &'static str,
    },
}

/// A `Result` whose error is [`StoreError`].
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// The database: a connection pool, migrated to the latest schema, and the
/// [`Sealer`] for its encrypted columns.
///
/// Cloning is cheap and shares the pool.
#[derive(Clone)]
pub struct Store {
    pool: SqlitePool,
    sealer: Arc<Sealer>,
    resets: Arc<Semaphore>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").finish_non_exhaustive()
    }
}

impl Store {
    /// Opens the database at `url`, such as `sqlite:///var/lib/agentd/agentd.db`,
    /// creating the file if it's missing, and runs any pending migrations.
    ///
    /// Every connection has `foreign_keys=ON` and a
    /// [`busy_timeout`](BUSY_TIMEOUT). A file database is put in WAL mode.
    /// An in-memory URL (`sqlite::memory:`, or `mode=memory`) gets what
    /// [`open_in_memory`](Self::open_in_memory) does.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the URL is invalid or the database can't
    /// be opened, [`StoreError::Migrate`] if migrations fail.
    pub async fn open(url: &str, sealer: Sealer) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT);
        if is_memory_url(url) {
            return Self::connect(memory_pool(), options, sealer).await;
        }
        let options = options.journal_mode(SqliteJournalMode::Wal);
        Self::connect(SqlitePoolOptions::new(), options, sealer).await
    }

    /// Opens a new, empty, migrated in-memory database, for tests.
    ///
    /// Each call gets its own database. The pool holds a single connection
    /// that is never closed, because an in-memory database lives only as long
    /// as its connection; concurrent callers queue for it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if SQLite fails, [`StoreError::Migrate`] if
    /// migrations fail.
    pub async fn open_in_memory(sealer: Sealer) -> Result<Self> {
        let options = SqliteConnectOptions::new()
            .in_memory(true)
            .foreign_keys(true)
            .busy_timeout(BUSY_TIMEOUT);
        Self::connect(memory_pool(), options, sealer).await
    }

    async fn connect(
        pool: SqlitePoolOptions,
        options: SqliteConnectOptions,
        sealer: Sealer,
    ) -> Result<Self> {
        let pool = pool.connect_with(options).await?;
        MIGRATOR.run(&pool).await?;
        Ok(Self {
            pool,
            sealer: Arc::new(sealer),
            resets: Arc::new(Semaphore::new(RESETS_AT_ONCE)),
        })
    }

    /// Checks that the database answers a trivial query, for health checks.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if no connection can be had or the query
    /// fails, including after [`close`](Self::close).
    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Closes the pool: waits for connections in use to be returned, then
    /// closes every connection. Later calls on this store or its clones fail
    /// with [`StoreError::Database`].
    pub async fn close(&self) {
        self.pool.close().await;
    }

    fn seal(&self, aad: Aad<'_>, value: &secrecy::SecretString) -> Result<Vec<u8>> {
        self.sealer
            .seal(aad, value)
            .map_err(|source| StoreError::Seal {
                table: aad.table,
                column: aad.column,
                source,
            })
    }

    fn open_sealed(&self, aad: Aad<'_>, sealed: &[u8]) -> Result<secrecy::SecretString> {
        self.sealer
            .open(aad, sealed)
            .map_err(|source| StoreError::Seal {
                table: aad.table,
                column: aad.column,
                source,
            })
    }
}

/// Pool options for an in-memory database: one connection, never closed.
fn memory_pool() -> SqlitePoolOptions {
    SqlitePoolOptions::new()
        .max_connections(1)
        .min_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
}

/// Whether `url` names an in-memory database, by the rules `sqlx` parses
/// SQLite URLs with: the database part is `:memory:`, or a query parameter
/// is `mode=memory`.
fn is_memory_url(url: &str) -> bool {
    let rest = url
        .strip_prefix("sqlite://")
        .or_else(|| url.strip_prefix("sqlite:"))
        .unwrap_or(url);
    let (database, params) = rest.split_once('?').unwrap_or((rest, ""));
    database == ":memory:" || params.split('&').any(|param| param == "mode=memory")
}

fn to_unix(at: OffsetDateTime) -> i64 {
    at.unix_timestamp()
}

fn from_unix(seconds: i64, table: &'static str, column: &'static str) -> Result<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(seconds).map_err(|_| StoreError::Corrupt { table, column })
}

fn parse_column<T: FromStr>(value: &str, table: &'static str, column: &'static str) -> Result<T> {
    value
        .parse()
        .map_err(|_| StoreError::Corrupt { table, column })
}

#[cfg(test)]
pub(crate) mod test_util {
    use std::path::PathBuf;

    use core_types::{MemberKey, SurfaceKind, TeamId, UserId};

    use super::*;

    pub(crate) fn sealer() -> Sealer {
        Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap()
    }

    pub(crate) async fn memory_store() -> Store {
        Store::open_in_memory(sealer()).await.unwrap()
    }

    pub(crate) fn member_key(user: &str) -> MemberKey {
        MemberKey {
            surface: SurfaceKind::RocketChat,
            team: TeamId::new("chat.example.org"),
            user: UserId::new(user),
        }
    }

    pub(crate) fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(seconds).unwrap()
    }

    /// A new Rocket.Chat agent of `owner` named `name`.
    pub(crate) async fn agent(
        store: &Store,
        owner: core_types::MemberId,
        name: &str,
    ) -> core_types::AgentId {
        let team = TeamId::new("T1");
        let new = crate::NewAgent {
            owner,
            name,
            persona: "p",
            visibility: crate::Visibility::Public,
            surface: SurfaceKind::RocketChat,
            team: &team,
        };
        match store.create_agent(&new, 10, at(1)).await.unwrap() {
            crate::AgentCreation::Created(agent, _) => agent.id,
            other => panic!("{other:?}"),
        }
    }

    /// A directory under the system temp directory, removed on drop.
    pub(crate) struct TempDir(PathBuf);

    impl TempDir {
        pub(crate) fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("store-test-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        pub(crate) fn db_url(&self) -> String {
            format!("sqlite://{}", self.0.join("agentd.db").display())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use secrecy::{ExposeSecret, SecretString};

    use super::test_util::*;
    use super::*;

    async fn pragma(store: &Store, name: &'static str) -> String {
        let sql = match name {
            "journal_mode" => "PRAGMA journal_mode",
            "foreign_keys" => "SELECT CAST(foreign_keys AS TEXT) FROM pragma_foreign_keys",
            "busy_timeout" => "SELECT CAST(timeout AS TEXT) FROM pragma_busy_timeout",
            _ => unreachable!(),
        };
        sqlx::query_scalar(sql)
            .fetch_one(&store.pool)
            .await
            .unwrap()
    }

    async fn table_names(store: &Store) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
             ORDER BY name",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn open_sets_wal_foreign_keys_and_busy_timeout() {
        let dir = TempDir::new();
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        assert_eq!(pragma(&store, "journal_mode").await, "wal");
        assert_eq!(pragma(&store, "foreign_keys").await, "1");
        assert_eq!(pragma(&store, "busy_timeout").await, "5000");
    }

    #[tokio::test]
    async fn the_migration_applies_to_an_empty_database() {
        let dir = TempDir::new();
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        assert_eq!(
            table_names(&store).await,
            [
                "_sqlx_migrations",
                "agent_bindings",
                "agent_policies",
                "agent_skills",
                "agents",
                "bans",
                "claude_link_generations",
                "claude_links",
                "community_settings",
                "consents",
                "ctl_tokens",
                "failure_notices",
                "limit_notices",
                "members",
                "message_refs",
                "pending_logins",
                "processed_events",
                "scope_locks",
                "sessions",
                "slack_config_tokens",
                "surface_identities",
                "thread_usage",
                "usage",
                "volumes",
            ]
        );
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let dir = TempDir::new();
        let key = Sealer::generate_key().unwrap();
        let store = Store::open(&dir.db_url(), Sealer::from_base64(&key).unwrap())
            .await
            .unwrap();
        let member = store
            .ensure_member(&member_key("u1"), "Ada", at(1_000))
            .await
            .unwrap();
        MIGRATOR.run(&store.pool).await.unwrap();
        drop(store);

        let store = Store::open(&dir.db_url(), Sealer::from_base64(&key).unwrap())
            .await
            .unwrap();
        let applied: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM _sqlx_migrations")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(applied, i64::try_from(MIGRATOR.iter().count()).unwrap());
        assert_eq!(
            store.member_for_identity(&member_key("u1")).await.unwrap(),
            Some(member)
        );
    }

    #[tokio::test]
    async fn foreign_keys_are_enforced() {
        let store = memory_store().await;
        assert_eq!(pragma(&store, "foreign_keys").await, "1");
        let err = store
            .put_pending_login(
                "state",
                core_types::MemberId::new_v4(),
                &SecretString::from("verifier"),
                at(1_000),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Database(_)), "{err:?}");
    }

    #[tokio::test]
    async fn in_memory_stores_are_separate_and_keep_their_data() {
        let a = memory_store().await;
        let b = memory_store().await;
        let key = member_key("u1");
        let member = a.ensure_member(&key, "Ada", at(1_000)).await.unwrap();
        assert_eq!(b.member_for_identity(&key).await.unwrap(), None);
        let (x, y) = tokio::join!(a.member_for_identity(&key), a.member_for_identity(&key));
        assert_eq!(x.unwrap(), Some(member));
        assert_eq!(y.unwrap(), Some(member));
    }

    #[tokio::test]
    async fn an_in_memory_url_opens_an_in_memory_database() {
        let store = Store::open("sqlite::memory:", sealer()).await.unwrap();
        assert_eq!(pragma(&store, "journal_mode").await, "memory");
        let member = store
            .ensure_member(&member_key("u1"), "Ada", at(1_000))
            .await
            .unwrap();
        assert_eq!(
            store.member_for_identity(&member_key("u1")).await.unwrap(),
            Some(member)
        );
    }

    #[tokio::test]
    async fn ping_succeeds_until_the_store_is_closed() {
        let store = memory_store().await;
        store.ping().await.unwrap();
        let clone = store.clone();
        store.close().await;
        let err = clone.ping().await.unwrap_err();
        assert!(matches!(err, StoreError::Database(_)), "{err:?}");
    }

    #[test]
    fn memory_urls_are_recognized() {
        for url in [
            "sqlite::memory:",
            "sqlite://:memory:",
            ":memory:",
            "sqlite://?mode=memory",
            "sqlite://shared?mode=memory&cache=shared",
        ] {
            assert!(is_memory_url(url), "{url}");
        }
        for url in [
            "sqlite:///var/lib/agentd/agentd.db",
            "sqlite://agentd.db?mode=rwc",
            "sqlite:memory.db",
        ] {
            assert!(!is_memory_url(url), "{url}");
        }
    }

    #[tokio::test]
    async fn an_invalid_url_is_a_database_error() {
        let err = Store::open("sqlite://x.db?mode=bogus", sealer())
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Database(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_wrong_key_fails_to_read_secrets() {
        let dir = TempDir::new();
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let member = store
            .ensure_member(&member_key("u1"), "Ada", at(1_000))
            .await
            .unwrap();
        store
            .put_claude_link(member, &claude_links::tests::new_link("a", "r"), at(1_000))
            .await
            .unwrap();
        store
            .put_pending_login("state", member, &SecretString::from("v"), at(1_000))
            .await
            .unwrap();
        drop(store);

        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let err = store.get_claude_link(member).await.unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Seal {
                    table: "claude_links",
                    column: "access_token_enc",
                    source: SealError::Decrypt,
                }
            ),
            "{err:?}"
        );
        let err = store.take_pending_login("state").await.unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Seal {
                    source: SealError::Decrypt,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn errors_never_contain_secrets() {
        let store = memory_store().await;
        let member = store
            .ensure_member(&member_key("u1"), "Ada", at(1_000))
            .await
            .unwrap();
        let other = store
            .ensure_member(&member_key("u2"), "Bob", at(1_000))
            .await
            .unwrap();
        let secret = SecretString::from("sk-ant-oat01-supersecret");
        store
            .put_pending_login("state", member, &secret, at(1_000))
            .await
            .unwrap();
        let err = store
            .put_pending_login("state", other, &secret, at(1_000))
            .await
            .unwrap_err();
        assert!(!format!("{err} {err:?}").contains(secret.expose_secret()));
        assert!(!format!("{store:?}").contains(secret.expose_secret()));
    }

    #[test]
    fn corrupt_timestamps_and_ids_are_reported() {
        let err = from_unix(i64::MAX, "t", "c").unwrap_err();
        assert_eq!(err.to_string(), "t.c holds an invalid value");
        let err = parse_column::<core_types::MemberId>("nope", "t", "c").unwrap_err();
        assert!(matches!(
            err,
            StoreError::Corrupt {
                table: "t",
                column: "c"
            }
        ));
    }
}

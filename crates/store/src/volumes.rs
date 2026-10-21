//! `volumes`: which scope each volume directory holds.

use core_types::{AgentId, ScopeKey, VolumeKey};
use time::OffsetDateTime;

use crate::{Result, Store, from_unix, parse_column, to_unix};

const TABLE: &str = "volumes";

/// A volume row: the directory that holds one agent's files for one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// The agent and scope the volume serves.
    pub key: VolumeKey,
    /// The directory, relative to agentd's data directory.
    pub path: String,
    /// When the row was first written.
    pub created_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct Row {
    agent_id: String,
    scope_key: String,
    path: String,
    created_at: i64,
}

impl Row {
    fn into_volume(self) -> Result<Volume> {
        Ok(Volume {
            key: VolumeKey {
                agent: parse_column::<AgentId>(&self.agent_id, TABLE, "agent_id")?,
                scope: parse_column::<ScopeKey>(&self.scope_key, TABLE, "scope_key")?,
            },
            path: self.path,
            created_at: from_unix(self.created_at, TABLE, "created_at")?,
        })
    }
}

impl Store {
    /// Records that the directory `path` (relative to the data directory)
    /// holds the volume `key`, and returns the row.
    ///
    /// Recording a key again replaces its path and keeps its `created_at`.
    /// It is one statement, so concurrent calls for one key leave one row.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if another key
    /// already records `path`, or the query fails.
    pub async fn put_volume(&self, key: &VolumeKey, path: &str) -> Result<Volume> {
        let row: Row = sqlx::query_as(
            "INSERT INTO volumes (agent_id, scope_key, path, created_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (agent_id, scope_key) DO UPDATE SET path = excluded.path \
             RETURNING agent_id, scope_key, path, created_at",
        )
        .bind(key.agent.to_string())
        .bind(key.scope.to_string())
        .bind(path)
        .bind(to_unix(OffsetDateTime::now_utc()))
        .fetch_one(&self.pool)
        .await?;
        row.into_volume()
    }

    /// The row of the volume `key`, if one was recorded.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails, [`StoreError::Corrupt`](crate::StoreError::Corrupt) if the row
    /// doesn't parse.
    pub async fn volume(&self, key: &VolumeKey) -> Result<Option<Volume>> {
        let row: Option<Row> = sqlx::query_as(
            "SELECT agent_id, scope_key, path, created_at FROM volumes \
             WHERE agent_id = ? AND scope_key = ?",
        )
        .bind(key.agent.to_string())
        .bind(key.scope.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(Row::into_volume).transpose()
    }

    /// The row whose directory is `path` (relative to the data directory),
    /// which tells which key a digest-named directory holds.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails, [`StoreError::Corrupt`](crate::StoreError::Corrupt) if the row
    /// doesn't parse.
    pub async fn volume_by_path(&self, path: &str) -> Result<Option<Volume>> {
        let row: Option<Row> = sqlx::query_as(
            "SELECT agent_id, scope_key, path, created_at FROM volumes WHERE path = ?",
        )
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        row.map(Row::into_volume).transpose()
    }
}

#[cfg(test)]
mod tests {
    use core_types::{ConvRef, SurfaceKind, TeamId};

    use super::*;
    use crate::StoreError;
    use crate::test_util::*;

    fn channel_key(agent: AgentId) -> VolumeKey {
        VolumeKey {
            agent,
            scope: ScopeKey::Channel(ConvRef {
                surface: SurfaceKind::RocketChat,
                team: TeamId::new("host:3000"),
                conversation: "a%b".into(),
            }),
        }
    }

    #[tokio::test]
    async fn a_volume_round_trips_and_keeps_its_created_at() {
        let store = memory_store().await;
        let key = channel_key(AgentId::new_v4());
        assert_eq!(store.volume(&key).await.unwrap(), None);
        let first = store.put_volume(&key, "volumes/a/1").await.unwrap();
        assert_eq!(first.key, key);
        assert_eq!(first.path, "volumes/a/1");
        sqlx::query("UPDATE volumes SET created_at = 1000")
            .execute(&store.pool)
            .await
            .unwrap();
        let again = store.put_volume(&key, "volumes/a/2").await.unwrap();
        assert_eq!(again.path, "volumes/a/2");
        assert_eq!(again.created_at, at(1_000));
        assert_eq!(store.volume(&key).await.unwrap(), Some(again.clone()));
        assert_eq!(
            store.volume_by_path("volumes/a/2").await.unwrap(),
            Some(again)
        );
        assert_eq!(store.volume_by_path("volumes/a/1").await.unwrap(), None);
    }

    #[tokio::test]
    async fn two_agents_in_one_channel_have_two_rows() {
        let store = memory_store().await;
        let a = channel_key(AgentId::new_v4());
        let b = channel_key(AgentId::new_v4());
        store.put_volume(&a, "volumes/a/x").await.unwrap();
        store.put_volume(&b, "volumes/b/x").await.unwrap();
        assert_eq!(store.volume(&a).await.unwrap().unwrap().path, "volumes/a/x");
        assert_eq!(store.volume(&b).await.unwrap().unwrap().path, "volumes/b/x");
    }

    #[tokio::test]
    async fn one_path_can_hold_only_one_key() {
        let store = memory_store().await;
        let a = channel_key(AgentId::new_v4());
        let b = VolumeKey {
            agent: a.agent,
            scope: ScopeKey::Private,
        };
        store.put_volume(&a, "volumes/a/x").await.unwrap();
        let err = store.put_volume(&b, "volumes/a/x").await.unwrap_err();
        assert!(matches!(err, StoreError::Database(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_corrupt_scope_key_is_reported() {
        let store = memory_store().await;
        sqlx::query(
            "INSERT INTO volumes (agent_id, scope_key, path, created_at) \
             VALUES (?, 'bogus', 'volumes/x', 0)",
        )
        .bind(AgentId::new_v4().to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        let err = store.volume_by_path("volumes/x").await.unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Corrupt {
                    table: "volumes",
                    column: "scope_key"
                }
            ),
            "{err:?}"
        );
    }
}

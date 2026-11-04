//! `members` and `surface_identities`.

use core_types::{MemberId, MemberKey};
use sqlx::SqliteConnection;
use time::OffsetDateTime;

use crate::{Result, Store, parse_column, to_unix};

impl Store {
    /// The member that owns the surface identity `key`, if any.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn member_for_identity(&self, key: &MemberKey) -> Result<Option<MemberId>> {
        let mut conn = self.pool.acquire().await?;
        lookup(&mut conn, key).await
    }

    /// The member that owns the surface identity `key`, created with
    /// `display_name` if there is none yet.
    ///
    /// The display name is only used when the member is created; an existing
    /// member keeps theirs. Concurrent calls for one identity return one
    /// member.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if a query
    /// fails.
    pub async fn ensure_member(&self, key: &MemberKey, display_name: &str) -> Result<MemberId> {
        if let Some(member) = self.member_for_identity(key).await? {
            return Ok(member);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        if let Some(member) = lookup(&mut tx, key).await? {
            tx.commit().await?;
            return Ok(member);
        }
        let member = MemberId::new_v4();
        sqlx::query("INSERT INTO members (id, display_name, created_at) VALUES (?, ?, ?)")
            .bind(member.to_string())
            .bind(display_name)
            .bind(to_unix(OffsetDateTime::now_utc()))
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO surface_identities (surface, team_id, user_id, member_id) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(key.surface.as_str())
        .bind(key.team.as_str())
        .bind(key.user.as_str())
        .bind(member.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(member)
    }
}

async fn lookup(conn: &mut SqliteConnection, key: &MemberKey) -> Result<Option<MemberId>> {
    let member: Option<String> = sqlx::query_scalar(
        "SELECT member_id FROM surface_identities \
         WHERE surface = ? AND team_id = ? AND user_id = ?",
    )
    .bind(key.surface.as_str())
    .bind(key.team.as_str())
    .bind(key.user.as_str())
    .fetch_optional(conn)
    .await?;
    member
        .map(|id| parse_column(&id, "surface_identities", "member_id"))
        .transpose()
}

#[cfg(test)]
mod tests {
    use core_types::{MemberKey, SurfaceKind, TeamId, UserId};

    use crate::StoreError;
    use crate::test_util::*;

    #[tokio::test]
    async fn member_for_identity_is_none_for_an_unknown_identity() {
        let store = memory_store().await;
        assert_eq!(
            store.member_for_identity(&member_key("u1")).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn ensure_member_creates_once_and_then_finds() {
        let store = memory_store().await;
        let member = store.ensure_member(&member_key("u1"), "Ada").await.unwrap();
        assert_eq!(
            store.member_for_identity(&member_key("u1")).await.unwrap(),
            Some(member)
        );
        assert_eq!(
            store
                .ensure_member(&member_key("u1"), "Renamed")
                .await
                .unwrap(),
            member
        );
        let name: String = sqlx::query_scalar("SELECT display_name FROM members WHERE id = ?")
            .bind(member.to_string())
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(name, "Ada");
    }

    #[tokio::test]
    async fn identities_differ_by_surface_team_and_user() {
        let store = memory_store().await;
        let base = member_key("u1");
        let other_user = member_key("u2");
        let other_team = MemberKey {
            team: TeamId::new("other.example.org"),
            ..base.clone()
        };
        let other_surface = MemberKey {
            surface: SurfaceKind::Slack,
            ..base.clone()
        };
        let mut members = Vec::new();
        for key in [&base, &other_user, &other_team, &other_surface] {
            members.push(store.ensure_member(key, "x").await.unwrap());
        }
        members.sort();
        members.dedup();
        assert_eq!(members.len(), 4);
        let bare_user = MemberKey {
            user: UserId::new("U1"),
            ..other_surface
        };
        assert_eq!(store.member_for_identity(&bare_user).await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_ensure_member_creates_one_member() {
        let dir = TempDir::new("store-test");
        let store = crate::Store::open(&dir.db_url(), sealer()).await.unwrap();
        let key = member_key("u1");
        let (a, b, c, d) = tokio::join!(
            store.ensure_member(&key, "Ada"),
            store.ensure_member(&key, "Ada"),
            store.ensure_member(&key, "Ada"),
            store.ensure_member(&key, "Ada"),
        );
        let a = a.unwrap();
        assert_eq!([b.unwrap(), c.unwrap(), d.unwrap()], [a, a, a]);
        let members: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM members")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(members, 1);
    }

    #[tokio::test]
    async fn a_corrupt_member_id_is_reported() {
        let store = memory_store().await;
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO surface_identities (surface, team_id, user_id, member_id) \
             VALUES ('rocketchat', 'chat.example.org', 'u1', 'not-a-uuid')",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let err = store
            .member_for_identity(&member_key("u1"))
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Corrupt {
                    table: "surface_identities",
                    column: "member_id"
                }
            ),
            "{err:?}"
        );
    }
}

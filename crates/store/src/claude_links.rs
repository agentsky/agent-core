//! `claude_links`: each member's Claude subscription tokens.

use core_types::MemberId;
use secrecy::SecretString;
use time::OffsetDateTime;

use crate::seal::Aad;
use crate::{Result, Store, from_unix, to_unix};

const TABLE: &str = "claude_links";
const ACCESS: &str = "access_token_enc";
const REFRESH: &str = "refresh_token_enc";

/// Tokens and plan to store for a member, from a login or a refresh.
#[derive(Debug)]
pub struct NewClaudeLink {
    /// The OAuth access token.
    pub access_token: SecretString,
    /// The OAuth refresh token.
    pub refresh_token: SecretString,
    /// When the access token expires.
    pub expires_at: OffsetDateTime,
    /// The plan from the profile (`organization.organization_type`), if it
    /// has been read.
    pub plan: Option<String>,
    /// `organization.rate_limit_tier` from the profile, if known.
    pub rate_limit_tier: Option<String>,
}

/// A member's stored Claude link.
#[derive(Debug)]
pub struct ClaudeLink {
    /// The member.
    pub member: MemberId,
    /// The OAuth access token.
    pub access_token: SecretString,
    /// The OAuth refresh token.
    pub refresh_token: SecretString,
    /// When the access token expires.
    pub expires_at: OffsetDateTime,
    /// The plan from the profile, if it has been read.
    pub plan: Option<String>,
    /// The rate limit tier from the profile, if known.
    pub rate_limit_tier: Option<String>,
    /// When a refresh last failed, if the link has been broken since it was
    /// last stored.
    pub broken_at: Option<OffsetDateTime>,
    /// When the link was last stored.
    pub updated_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct Row {
    access_token_enc: Vec<u8>,
    refresh_token_enc: Vec<u8>,
    expires_at: i64,
    plan: Option<String>,
    rate_limit_tier: Option<String>,
    broken_at: Option<i64>,
    updated_at: i64,
}

fn aad<'a>(column: &'static str, key: &'a str) -> Aad<'a> {
    Aad {
        table: TABLE,
        column,
        key,
    }
}

impl Store {
    /// Stores `link` as `member`'s Claude link, replacing any existing one,
    /// and clears [`broken_at`](ClaudeLink::broken_at).
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the member
    /// doesn't exist or the query fails.
    pub async fn put_claude_link(&self, member: MemberId, link: &NewClaudeLink) -> Result<()> {
        let key = member.to_string();
        let access = self.seal(aad(ACCESS, &key), &link.access_token)?;
        let refresh = self.seal(aad(REFRESH, &key), &link.refresh_token)?;
        sqlx::query(
            "INSERT INTO claude_links (member_id, access_token_enc, refresh_token_enc, \
             expires_at, plan, rate_limit_tier, broken_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, NULL, ?) \
             ON CONFLICT (member_id) DO UPDATE SET \
             access_token_enc = excluded.access_token_enc, \
             refresh_token_enc = excluded.refresh_token_enc, \
             expires_at = excluded.expires_at, \
             plan = excluded.plan, \
             rate_limit_tier = excluded.rate_limit_tier, \
             broken_at = NULL, \
             updated_at = excluded.updated_at",
        )
        .bind(&key)
        .bind(access)
        .bind(refresh)
        .bind(to_unix(link.expires_at))
        .bind(&link.plan)
        .bind(&link.rate_limit_tier)
        .bind(to_unix(OffsetDateTime::now_utc()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// `member`'s Claude link, if they have one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Seal`](crate::StoreError::Seal) if a token fails to
    /// decrypt (a wrong key, or a value moved from another row),
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn get_claude_link(&self, member: MemberId) -> Result<Option<ClaudeLink>> {
        let key = member.to_string();
        let row: Option<Row> = sqlx::query_as(
            "SELECT access_token_enc, refresh_token_enc, expires_at, plan, rate_limit_tier, \
             broken_at, updated_at FROM claude_links WHERE member_id = ?",
        )
        .bind(&key)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(ClaudeLink {
            member,
            access_token: self.open_sealed(aad(ACCESS, &key), &row.access_token_enc)?,
            refresh_token: self.open_sealed(aad(REFRESH, &key), &row.refresh_token_enc)?,
            expires_at: from_unix(row.expires_at, TABLE, "expires_at")?,
            plan: row.plan,
            rate_limit_tier: row.rate_limit_tier,
            broken_at: row
                .broken_at
                .map(|at| from_unix(at, TABLE, "broken_at"))
                .transpose()?,
            updated_at: from_unix(row.updated_at, TABLE, "updated_at")?,
        }))
    }

    /// Deletes `member`'s Claude link. Returns whether there was one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn delete_claude_link(&self, member: MemberId) -> Result<bool> {
        let result = sqlx::query("DELETE FROM claude_links WHERE member_id = ?")
            .bind(member.to_string())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Records that refreshing `member`'s link failed at `at`.
    ///
    /// Returns true only when this call set
    /// [`broken_at`](ClaudeLink::broken_at), that is when the link was not
    /// already broken, so the member is told once per failure. Returns false
    /// if the link was already broken or doesn't exist.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn mark_claude_link_broken(
        &self,
        member: MemberId,
        at: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE claude_links SET broken_at = ? WHERE member_id = ? AND broken_at IS NULL",
        )
        .bind(to_unix(at))
        .bind(member.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use secrecy::ExposeSecret;

    use super::*;
    use crate::test_util::*;
    use crate::{SealError, StoreError};

    pub(crate) fn new_link(access: &str, refresh: &str) -> NewClaudeLink {
        NewClaudeLink {
            access_token: SecretString::from(access),
            refresh_token: SecretString::from(refresh),
            expires_at: at(2_000_000_000),
            plan: Some("claude_max".to_owned()),
            rate_limit_tier: Some("default_claude_max_20x".to_owned()),
        }
    }

    async fn store_with_member(user: &str) -> (Store, MemberId) {
        let store = memory_store().await;
        let member = store.ensure_member(&member_key(user), "Ada").await.unwrap();
        (store, member)
    }

    fn assert_decrypt_error(err: StoreError, expected_column: &str) {
        match err {
            StoreError::Seal {
                table,
                column,
                source,
            } => {
                assert_eq!(table, TABLE);
                assert_eq!(column, expected_column);
                assert_eq!(source, SealError::Decrypt);
            }
            other => panic!("expected a decrypt error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn put_then_get_round_trips() {
        let (store, member) = store_with_member("u1").await;
        let before = OffsetDateTime::now_utc().unix_timestamp();
        store
            .put_claude_link(member, &new_link("access-1", "refresh-1"))
            .await
            .unwrap();
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.member, member);
        assert_eq!(link.access_token.expose_secret(), "access-1");
        assert_eq!(link.refresh_token.expose_secret(), "refresh-1");
        assert_eq!(link.expires_at, at(2_000_000_000));
        assert_eq!(link.plan.as_deref(), Some("claude_max"));
        assert_eq!(
            link.rate_limit_tier.as_deref(),
            Some("default_claude_max_20x")
        );
        assert_eq!(link.broken_at, None);
        assert!(link.updated_at.unix_timestamp() >= before);
    }

    #[tokio::test]
    async fn get_is_none_without_a_link() {
        let (store, member) = store_with_member("u1").await;
        assert!(store.get_claude_link(member).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tokens_are_stored_encrypted() {
        let (store, member) = store_with_member("u1").await;
        store
            .put_claude_link(member, &new_link("access-plain", "refresh-plain"))
            .await
            .unwrap();
        let (access, refresh): (Vec<u8>, Vec<u8>) =
            sqlx::query_as("SELECT access_token_enc, refresh_token_enc FROM claude_links")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert!(!access.windows(12).any(|w| w == b"access-plain"));
        assert!(!refresh.windows(13).any(|w| w == b"refresh-plain"));
    }

    #[tokio::test]
    async fn put_replaces_and_clears_broken_at() {
        let (store, member) = store_with_member("u1").await;
        store
            .put_claude_link(member, &new_link("a1", "r1"))
            .await
            .unwrap();
        assert!(
            store
                .mark_claude_link_broken(member, at(1_000))
                .await
                .unwrap()
        );
        let replacement = NewClaudeLink {
            plan: None,
            rate_limit_tier: None,
            ..new_link("a2", "r2")
        };
        store.put_claude_link(member, &replacement).await.unwrap();
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.access_token.expose_secret(), "a2");
        assert_eq!(link.refresh_token.expose_secret(), "r2");
        assert_eq!(link.plan, None);
        assert_eq!(link.rate_limit_tier, None);
        assert_eq!(link.broken_at, None);
    }

    #[tokio::test]
    async fn mark_broken_reports_only_the_first_failure() {
        let (store, member) = store_with_member("u1").await;
        assert!(
            !store
                .mark_claude_link_broken(member, at(1_000))
                .await
                .unwrap()
        );
        store
            .put_claude_link(member, &new_link("a", "r"))
            .await
            .unwrap();
        assert!(
            store
                .mark_claude_link_broken(member, at(1_000))
                .await
                .unwrap()
        );
        assert!(
            !store
                .mark_claude_link_broken(member, at(2_000))
                .await
                .unwrap()
        );
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.broken_at, Some(at(1_000)));
    }

    #[tokio::test]
    async fn delete_removes_the_link() {
        let (store, member) = store_with_member("u1").await;
        assert!(!store.delete_claude_link(member).await.unwrap());
        store
            .put_claude_link(member, &new_link("a", "r"))
            .await
            .unwrap();
        assert!(store.delete_claude_link(member).await.unwrap());
        assert!(store.get_claude_link(member).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn put_needs_an_existing_member() {
        let store = memory_store().await;
        let err = store
            .put_claude_link(MemberId::new_v4(), &new_link("a", "r"))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Database(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_token_moved_to_another_member_fails_to_decrypt() {
        let (store, alice) = store_with_member("alice").await;
        let bob = store
            .ensure_member(&member_key("bob"), "Bob")
            .await
            .unwrap();
        store
            .put_claude_link(alice, &new_link("alice-access", "alice-refresh"))
            .await
            .unwrap();
        store
            .put_claude_link(bob, &new_link("bob-access", "bob-refresh"))
            .await
            .unwrap();
        sqlx::query(
            "UPDATE claude_links SET access_token_enc = \
             (SELECT access_token_enc FROM claude_links WHERE member_id = ?) \
             WHERE member_id = ?",
        )
        .bind(alice.to_string())
        .bind(bob.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        assert_decrypt_error(store.get_claude_link(bob).await.unwrap_err(), ACCESS);
        let alice_link = store.get_claude_link(alice).await.unwrap().unwrap();
        assert_eq!(alice_link.access_token.expose_secret(), "alice-access");
    }

    #[tokio::test]
    async fn a_token_moved_to_another_column_fails_to_decrypt() {
        let (store, member) = store_with_member("u1").await;
        store
            .put_claude_link(member, &new_link("access", "refresh"))
            .await
            .unwrap();
        sqlx::query("UPDATE claude_links SET refresh_token_enc = access_token_enc")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_decrypt_error(store.get_claude_link(member).await.unwrap_err(), REFRESH);
    }

    #[tokio::test]
    async fn a_corrupt_timestamp_is_reported() {
        let (store, member) = store_with_member("u1").await;
        store
            .put_claude_link(member, &new_link("a", "r"))
            .await
            .unwrap();
        sqlx::query("UPDATE claude_links SET broken_at = ?")
            .bind(i64::MAX)
            .execute(&store.pool)
            .await
            .unwrap();
        let err = store.get_claude_link(member).await.unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Corrupt {
                    table: TABLE,
                    column: "broken_at"
                }
            ),
            "{err:?}"
        );
    }
}

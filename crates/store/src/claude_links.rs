//! `claude_links`: each member's Claude subscription tokens.

use core_types::MemberId;
use secrecy::SecretString;
use time::OffsetDateTime;

use crate::seal::Aad;
use crate::{Result, Store, from_unix, to_unix};

const TABLE: &str = "claude_links";
const ACCESS: &str = "access_token_enc";
const REFRESH: &str = "refresh_token_enc";

/// Tokens and plan to store for a member at login.
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

/// Tokens from a refresh, for
/// [`update_claude_tokens`](Store::update_claude_tokens).
#[derive(Debug)]
pub struct ClaudeTokens {
    /// The OAuth access token.
    pub access_token: SecretString,
    /// The OAuth refresh token.
    pub refresh_token: SecretString,
    /// When the access token expires.
    pub expires_at: OffsetDateTime,
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
    /// Which login stored the link. Every
    /// [`put_claude_link`](Store::put_claude_link) gives the link a
    /// generation no earlier link of any member had, and writes that act on
    /// an existing link take the generation they read, so they don't touch
    /// a newer login's link.
    pub generation: i64,
}

/// What a member's link says without its tokens, as
/// [`claude_link_status`](Store::claude_link_status) returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeLinkStatus {
    /// The plan from the profile, if it has been read.
    pub plan: Option<String>,
    /// The rate limit tier from the profile, if known.
    pub rate_limit_tier: Option<String>,
    /// When a refresh was refused, if the link is broken.
    pub broken_at: Option<OffsetDateTime>,
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
    generation: i64,
}

fn aad<'a>(column: &'static str, key: &'a str) -> Aad<'a> {
    Aad {
        table: TABLE,
        column,
        key,
    }
}

impl Store {
    /// Stores `link` as `member`'s Claude link at `now`, replacing any
    /// existing one, and clears [`broken_at`](ClaudeLink::broken_at) and the
    /// [relink notice](Store::claim_relink_notice).
    ///
    /// Returns the link's new [`generation`](ClaudeLink::generation). It is
    /// one `BEGIN IMMEDIATE` transaction that takes the next generation and
    /// writes the link.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the member
    /// doesn't exist or a query fails.
    pub async fn put_claude_link(
        &self,
        member: MemberId,
        link: &NewClaudeLink,
        now: OffsetDateTime,
    ) -> Result<i64> {
        let key = member.to_string();
        let access = self.seal(aad(ACCESS, &key), &link.access_token)?;
        let refresh = self.seal(aad(REFRESH, &key), &link.refresh_token)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let generation: i64 = sqlx::query_scalar(
            "UPDATE claude_link_generations SET last = last + 1 WHERE id = 1 RETURNING last",
        )
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO claude_links (member_id, access_token_enc, refresh_token_enc, \
             expires_at, plan, rate_limit_tier, broken_at, updated_at, generation) \
             VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?) \
             ON CONFLICT (member_id) DO UPDATE SET \
             access_token_enc = excluded.access_token_enc, \
             refresh_token_enc = excluded.refresh_token_enc, \
             expires_at = excluded.expires_at, \
             plan = excluded.plan, \
             rate_limit_tier = excluded.rate_limit_tier, \
             broken_at = NULL, \
             relink_notified_at = NULL, \
             relink_attempts = 0, \
             relink_next_attempt_at = NULL, \
             updated_at = excluded.updated_at, \
             generation = excluded.generation",
        )
        .bind(&key)
        .bind(access)
        .bind(refresh)
        .bind(to_unix(link.expires_at))
        .bind(&link.plan)
        .bind(&link.rate_limit_tier)
        .bind(to_unix(now))
        .bind(generation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(generation)
    }

    /// Replaces the tokens of `member`'s link at `now` and clears
    /// [`broken_at`](ClaudeLink::broken_at) and the
    /// [relink notice](Store::claim_relink_notice), if the link is still the
    /// one of `generation`. The plan is left alone.
    ///
    /// Returns false, and stores nothing, if the member has no link or a
    /// newer login replaced it. A token refresh stores its result this way,
    /// so a refresh that finishes after the member logged out doesn't link
    /// them again, and one that finishes after they logged in again doesn't
    /// overwrite the new tokens.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn update_claude_tokens(
        &self,
        member: MemberId,
        generation: i64,
        tokens: &ClaudeTokens,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let key = member.to_string();
        let access = self.seal(aad(ACCESS, &key), &tokens.access_token)?;
        let refresh = self.seal(aad(REFRESH, &key), &tokens.refresh_token)?;
        let result = sqlx::query(
            "UPDATE claude_links SET access_token_enc = ?, refresh_token_enc = ?, \
             expires_at = ?, broken_at = NULL, relink_notified_at = NULL, relink_attempts = 0, \
             relink_next_attempt_at = NULL, updated_at = ? \
             WHERE member_id = ? AND generation = ?",
        )
        .bind(access)
        .bind(refresh)
        .bind(to_unix(tokens.expires_at))
        .bind(to_unix(now))
        .bind(&key)
        .bind(generation)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Replaces the plan and rate limit tier of `member`'s link, if the link
    /// is still the one of `generation`. Returns whether it was.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn update_claude_plan(
        &self,
        member: MemberId,
        generation: i64,
        plan: Option<&str>,
        rate_limit_tier: Option<&str>,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE claude_links SET plan = ?, rate_limit_tier = ? \
             WHERE member_id = ? AND generation = ?",
        )
        .bind(plan)
        .bind(rate_limit_tier)
        .bind(member.to_string())
        .bind(generation)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
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
             broken_at, updated_at, generation FROM claude_links WHERE member_id = ?",
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
            broken_at: broken_at(row.broken_at)?,
            updated_at: from_unix(row.updated_at, TABLE, "updated_at")?,
            generation: row.generation,
        }))
    }

    /// The plan and state of `member`'s link, without reading or decrypting
    /// its tokens, if they have a link.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn claude_link_status(&self, member: MemberId) -> Result<Option<ClaudeLinkStatus>> {
        let row: Option<(Option<String>, Option<String>, Option<i64>)> = sqlx::query_as(
            "SELECT plan, rate_limit_tier, broken_at FROM claude_links WHERE member_id = ?",
        )
        .bind(member.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(plan, rate_limit_tier, broken)| {
            Ok(ClaudeLinkStatus {
                plan,
                rate_limit_tier,
                broken_at: broken_at(broken)?,
            })
        })
        .transpose()
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

    /// Records that the token endpoint refused to refresh the link of
    /// `generation` at `at`.
    ///
    /// Returns true only when this call set
    /// [`broken_at`](ClaudeLink::broken_at), that is when the link is still
    /// the one of `generation` and was not already broken, so the member is
    /// told once per failure. Returns false, and changes nothing, if the link
    /// was already broken, doesn't exist, or a newer login replaced it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn mark_claude_link_broken(
        &self,
        member: MemberId,
        generation: i64,
        at: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE claude_links SET broken_at = ? \
             WHERE member_id = ? AND generation = ? AND broken_at IS NULL",
        )
        .bind(to_unix(at))
        .bind(member.to_string())
        .bind(generation)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

fn broken_at(value: Option<i64>) -> Result<Option<OffsetDateTime>> {
    value
        .map(|at| from_unix(at, TABLE, "broken_at"))
        .transpose()
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

    fn tokens(access: &str, refresh: &str, expires_at: i64) -> ClaudeTokens {
        ClaudeTokens {
            access_token: SecretString::from(access),
            refresh_token: SecretString::from(refresh),
            expires_at: at(expires_at),
        }
    }

    async fn store_with_member(user: &str) -> (Store, MemberId) {
        let store = memory_store().await;
        let member = store
            .ensure_member(&member_key(user), "Ada", at(1_000))
            .await
            .unwrap();
        (store, member)
    }

    async fn put(store: &Store, member: MemberId, access: &str, refresh: &str) -> i64 {
        store
            .put_claude_link(member, &new_link(access, refresh), at(1_500))
            .await
            .unwrap()
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
        let generation = put(&store, member, "access-1", "refresh-1").await;
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
        assert_eq!(link.updated_at, at(1_500));
        assert_eq!(link.generation, generation);
    }

    #[tokio::test]
    async fn get_is_none_without_a_link() {
        let (store, member) = store_with_member("u1").await;
        assert!(store.get_claude_link(member).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn tokens_are_stored_encrypted() {
        let (store, member) = store_with_member("u1").await;
        put(&store, member, "access-plain", "refresh-plain").await;
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
        let first = put(&store, member, "a1", "r1").await;
        assert!(
            store
                .mark_claude_link_broken(member, first, at(1_000))
                .await
                .unwrap()
        );
        let replacement = NewClaudeLink {
            plan: None,
            rate_limit_tier: None,
            ..new_link("a2", "r2")
        };
        store
            .put_claude_link(member, &replacement, at(1_600))
            .await
            .unwrap();
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.access_token.expose_secret(), "a2");
        assert_eq!(link.refresh_token.expose_secret(), "r2");
        assert_eq!(link.plan, None);
        assert_eq!(link.rate_limit_tier, None);
        assert_eq!(link.broken_at, None);
        assert_eq!(link.updated_at, at(1_600));
    }

    #[tokio::test]
    async fn every_put_takes_a_new_generation_even_after_a_delete() {
        let (store, alice) = store_with_member("alice").await;
        let bob = store
            .ensure_member(&member_key("bob"), "Bob", at(1_000))
            .await
            .unwrap();
        let first = put(&store, alice, "a1", "r1").await;
        let second = put(&store, alice, "a2", "r2").await;
        let bobs = put(&store, bob, "b1", "r1").await;
        assert!(store.delete_claude_link(alice).await.unwrap());
        let third = put(&store, alice, "a3", "r3").await;
        let mut all = vec![first, second, bobs, third];
        assert!(all.is_sorted());
        all.dedup();
        assert_eq!(all.len(), 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_puts_take_distinct_generations() {
        let dir = TempDir::new("store-test");
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let member = store
            .ensure_member(&member_key("u1"), "Ada", at(1_000))
            .await
            .unwrap();
        let links = [
            new_link("a", "r"),
            new_link("b", "r"),
            new_link("c", "r"),
            new_link("d", "r"),
        ];
        let (a, b, c, d) = tokio::join!(
            store.put_claude_link(member, &links[0], at(1_500)),
            store.put_claude_link(member, &links[1], at(1_500)),
            store.put_claude_link(member, &links[2], at(1_500)),
            store.put_claude_link(member, &links[3], at(1_500)),
        );
        let mut generations = vec![a.unwrap(), b.unwrap(), c.unwrap(), d.unwrap()];
        generations.sort_unstable();
        generations.dedup();
        assert_eq!(generations.len(), 4);
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.generation, generations[3]);
    }

    #[tokio::test]
    async fn mark_broken_reports_only_the_first_failure() {
        let (store, member) = store_with_member("u1").await;
        assert!(
            !store
                .mark_claude_link_broken(member, 1, at(1_000))
                .await
                .unwrap()
        );
        let generation = put(&store, member, "a", "r").await;
        assert!(
            store
                .mark_claude_link_broken(member, generation, at(1_000))
                .await
                .unwrap()
        );
        assert!(
            !store
                .mark_claude_link_broken(member, generation, at(2_000))
                .await
                .unwrap()
        );
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.broken_at, Some(at(1_000)));
    }

    #[tokio::test]
    async fn a_stale_mark_broken_after_a_new_login_changes_nothing() {
        let (store, member) = store_with_member("u1").await;
        let old = put(&store, member, "a1", "r1").await;
        let fresh = put(&store, member, "a2", "r2").await;
        assert!(
            !store
                .mark_claude_link_broken(member, old, at(1_000))
                .await
                .unwrap()
        );
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.broken_at, None);
        assert_eq!(link.generation, fresh);
    }

    #[tokio::test]
    async fn a_new_login_after_mark_broken_is_not_broken() {
        let (store, member) = store_with_member("u1").await;
        let old = put(&store, member, "a1", "r1").await;
        assert!(
            store
                .mark_claude_link_broken(member, old, at(1_000))
                .await
                .unwrap()
        );
        let fresh = put(&store, member, "a2", "r2").await;
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.broken_at, None);
        assert_eq!(link.generation, fresh);
        assert!(
            store
                .mark_claude_link_broken(member, fresh, at(2_000))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn delete_removes_the_link() {
        let (store, member) = store_with_member("u1").await;
        assert!(!store.delete_claude_link(member).await.unwrap());
        put(&store, member, "a", "r").await;
        assert!(store.delete_claude_link(member).await.unwrap());
        assert!(store.get_claude_link(member).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_tokens_replaces_the_tokens_and_keeps_the_plan() {
        let (store, member) = store_with_member("u1").await;
        let generation = put(&store, member, "a1", "r1").await;
        assert!(
            store
                .mark_claude_link_broken(member, generation, at(1_000))
                .await
                .unwrap()
        );
        assert!(
            store
                .update_claude_tokens(
                    member,
                    generation,
                    &tokens("a2", "r2", 2_100_000_000),
                    at(1_700)
                )
                .await
                .unwrap()
        );
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.access_token.expose_secret(), "a2");
        assert_eq!(link.refresh_token.expose_secret(), "r2");
        assert_eq!(link.expires_at, at(2_100_000_000));
        assert_eq!(link.plan.as_deref(), Some("claude_max"));
        assert_eq!(link.broken_at, None);
        assert_eq!(link.updated_at, at(1_700));
        assert_eq!(link.generation, generation);
    }

    #[tokio::test]
    async fn update_tokens_never_creates_a_link() {
        let (store, member) = store_with_member("u1").await;
        let update = tokens("a", "r", 2_000_000_000);
        assert!(
            !store
                .update_claude_tokens(member, 1, &update, at(1_700))
                .await
                .unwrap()
        );
        assert!(store.get_claude_link(member).await.unwrap().is_none());
        let generation = put(&store, member, "a", "r").await;
        assert!(store.delete_claude_link(member).await.unwrap());
        assert!(
            !store
                .update_claude_tokens(member, generation, &update, at(1_700))
                .await
                .unwrap()
        );
        assert!(store.get_claude_link(member).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn update_tokens_of_an_older_login_changes_nothing() {
        let (store, member) = store_with_member("u1").await;
        let old = put(&store, member, "a1", "r1").await;
        put(&store, member, "a2", "r2").await;
        assert!(
            !store
                .update_claude_tokens(member, old, &tokens("a3", "r3", 2_000_000_000), at(1_700))
                .await
                .unwrap()
        );
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.access_token.expose_secret(), "a2");
        assert_eq!(link.refresh_token.expose_secret(), "r2");
    }

    #[tokio::test]
    async fn update_tokens_touches_only_that_members_link() {
        let (store, alice) = store_with_member("alice").await;
        let bob = store
            .ensure_member(&member_key("bob"), "Bob", at(1_000))
            .await
            .unwrap();
        let generation = put(&store, alice, "a1", "r1").await;
        put(&store, bob, "a1", "r1").await;
        assert!(
            store
                .update_claude_tokens(alice, generation, &tokens("a2", "r2", 2_000), at(1_700))
                .await
                .unwrap()
        );
        let bob_link = store.get_claude_link(bob).await.unwrap().unwrap();
        assert_eq!(bob_link.access_token.expose_secret(), "a1");
        let alice_link = store.get_claude_link(alice).await.unwrap().unwrap();
        assert_eq!(alice_link.access_token.expose_secret(), "a2");
    }

    #[tokio::test]
    async fn update_plan_writes_only_the_plan_columns_of_that_generation() {
        let (store, member) = store_with_member("u1").await;
        let old = put(&store, member, "a1", "r1").await;
        assert!(
            store
                .update_claude_plan(member, old, Some("claude_pro"), None)
                .await
                .unwrap()
        );
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.access_token.expose_secret(), "a1");
        assert_eq!(link.plan.as_deref(), Some("claude_pro"));
        assert_eq!(link.rate_limit_tier, None);
        assert_eq!(link.updated_at, at(1_500));

        put(&store, member, "a2", "r2").await;
        assert!(
            !store
                .update_claude_plan(member, old, Some("claude_team"), None)
                .await
                .unwrap()
        );
        let link = store.get_claude_link(member).await.unwrap().unwrap();
        assert_eq!(link.plan.as_deref(), Some("claude_max"));
    }

    #[tokio::test]
    async fn status_reads_no_token() {
        let (store, member) = store_with_member("u1").await;
        assert_eq!(store.claude_link_status(member).await.unwrap(), None);
        let generation = put(&store, member, "a", "r").await;
        sqlx::query("UPDATE claude_links SET access_token_enc = x'00', refresh_token_enc = x'00'")
            .execute(&store.pool)
            .await
            .unwrap();
        let status = store.claude_link_status(member).await.unwrap().unwrap();
        assert_eq!(
            status,
            ClaudeLinkStatus {
                plan: Some("claude_max".to_owned()),
                rate_limit_tier: Some("default_claude_max_20x".to_owned()),
                broken_at: None,
            }
        );
        store
            .mark_claude_link_broken(member, generation, at(1_800))
            .await
            .unwrap();
        let status = store.claude_link_status(member).await.unwrap().unwrap();
        assert_eq!(status.broken_at, Some(at(1_800)));
    }

    #[tokio::test]
    async fn put_needs_an_existing_member() {
        let store = memory_store().await;
        let err = store
            .put_claude_link(MemberId::new_v4(), &new_link("a", "r"), at(1_500))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Database(_)), "{err:?}");
    }

    #[tokio::test]
    async fn a_token_moved_to_another_member_fails_to_decrypt() {
        let (store, alice) = store_with_member("alice").await;
        let bob = store
            .ensure_member(&member_key("bob"), "Bob", at(1_000))
            .await
            .unwrap();
        put(&store, alice, "alice-access", "alice-refresh").await;
        put(&store, bob, "bob-access", "bob-refresh").await;
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
        put(&store, member, "access", "refresh").await;
        sqlx::query("UPDATE claude_links SET refresh_token_enc = access_token_enc")
            .execute(&store.pool)
            .await
            .unwrap();
        assert_decrypt_error(store.get_claude_link(member).await.unwrap_err(), REFRESH);
    }

    #[tokio::test]
    async fn a_corrupt_timestamp_is_reported() {
        let (store, member) = store_with_member("u1").await;
        put(&store, member, "a", "r").await;
        sqlx::query("UPDATE claude_links SET broken_at = ?")
            .bind(i64::MAX)
            .execute(&store.pool)
            .await
            .unwrap();
        for err in [
            store.get_claude_link(member).await.unwrap_err(),
            store.claude_link_status(member).await.unwrap_err(),
        ] {
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
}

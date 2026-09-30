//! Relink notices: telling a member, once, that their Claude link broke.
//!
//! `claude_links.relink_notified_at` records that the member was told about
//! the link's current break. It is cleared whenever the link is stored or
//! refreshed, so every time [`broken_at`](crate::ClaudeLink::broken_at) goes
//! from empty to set there is exactly one notice to claim, and the claim is a
//! conditional `UPDATE`, so exactly one caller gets it across processes and
//! restarts.

use core_types::MemberId;
use time::OffsetDateTime;

use crate::{Result, Store, parse_column, to_unix};

/// A broken link whose member hasn't been told yet, from
/// [`Store::pending_relink_notices`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingRelinkNotice {
    /// The member to tell.
    pub member: MemberId,
    /// The link's [`generation`](crate::ClaudeLink::generation), which the
    /// claim and release take so they never act on a newer login's link.
    pub generation: i64,
}

impl Store {
    /// Every broken link whose member hasn't been told, oldest break first.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails, [`StoreError::Corrupt`](crate::StoreError::Corrupt) if a member
    /// id doesn't parse.
    pub async fn pending_relink_notices(&self) -> Result<Vec<PendingRelinkNotice>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT member_id, generation FROM claude_links \
             WHERE broken_at IS NOT NULL AND relink_notified_at IS NULL \
             ORDER BY broken_at, member_id",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(member, generation)| {
                Ok(PendingRelinkNotice {
                    member: parse_column(&member, "claude_links", "member_id")?,
                    generation,
                })
            })
            .collect()
    }

    /// Claims the notice for the break of `member`'s link of `generation`,
    /// at `now`. Returns true only for the one call that claims it; false if
    /// the link isn't broken, was claimed already, or a newer login replaced
    /// it.
    ///
    /// Claim before sending, and [release](Self::release_relink_notice) if
    /// nothing could be sent, so a notice is sent at most once and retried
    /// only when it wasn't.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn claim_relink_notice(
        &self,
        member: MemberId,
        generation: i64,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE claude_links SET relink_notified_at = ? \
             WHERE member_id = ? AND generation = ? \
             AND broken_at IS NOT NULL AND relink_notified_at IS NULL",
        )
        .bind(to_unix(now))
        .bind(member.to_string())
        .bind(generation)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Gives back a claim on `member`'s link of `generation` whose notice
    /// couldn't be sent, so it is pending again. Returns whether there was
    /// such a claim.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn release_relink_notice(&self, member: MemberId, generation: i64) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE claude_links SET relink_notified_at = NULL \
             WHERE member_id = ? AND generation = ? AND relink_notified_at IS NOT NULL",
        )
        .bind(member.to_string())
        .bind(generation)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use crate::claude_links::tests::new_link;
    use crate::test_util::*;

    use super::*;

    async fn broken_link(store: &Store, user: &str, at_secs: i64) -> PendingRelinkNotice {
        let member = store
            .ensure_member(&member_key(user), "Ada", at(1_000))
            .await
            .unwrap();
        let generation = store
            .put_claude_link(member, &new_link("a", "r"), at(1_000))
            .await
            .unwrap();
        assert!(
            store
                .mark_claude_link_broken(member, generation, at(at_secs))
                .await
                .unwrap()
        );
        PendingRelinkNotice { member, generation }
    }

    #[tokio::test]
    async fn only_broken_unclaimed_links_are_pending_oldest_first() {
        let store = memory_store().await;
        let later = broken_link(&store, "u1", 3_000).await;
        let earlier = broken_link(&store, "u2", 2_000).await;
        let healthy = store
            .ensure_member(&member_key("u3"), "Bob", at(1_000))
            .await
            .unwrap();
        store
            .put_claude_link(healthy, &new_link("a", "r"), at(1_000))
            .await
            .unwrap();
        assert_eq!(
            store.pending_relink_notices().await.unwrap(),
            [earlier, later]
        );
    }

    #[tokio::test]
    async fn a_notice_is_claimed_once() {
        let store = memory_store().await;
        let notice = broken_link(&store, "u1", 2_000).await;
        let (a, b) = tokio::join!(
            store.claim_relink_notice(notice.member, notice.generation, at(2_001)),
            store.claim_relink_notice(notice.member, notice.generation, at(2_001)),
        );
        assert!(a.unwrap() ^ b.unwrap());
        assert!(store.pending_relink_notices().await.unwrap().is_empty());
        assert!(
            !store
                .claim_relink_notice(notice.member, notice.generation, at(2_002))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_released_claim_is_pending_again() {
        let store = memory_store().await;
        let notice = broken_link(&store, "u1", 2_000).await;
        assert!(
            !store
                .release_relink_notice(notice.member, notice.generation)
                .await
                .unwrap()
        );
        store
            .claim_relink_notice(notice.member, notice.generation, at(2_001))
            .await
            .unwrap();
        assert!(
            store
                .release_relink_notice(notice.member, notice.generation)
                .await
                .unwrap()
        );
        assert_eq!(store.pending_relink_notices().await.unwrap(), [notice]);
    }

    #[tokio::test]
    async fn a_healthy_or_replaced_link_has_no_notice_to_claim() {
        let store = memory_store().await;
        let member = store
            .ensure_member(&member_key("u1"), "Ada", at(1_000))
            .await
            .unwrap();
        let generation = store
            .put_claude_link(member, &new_link("a", "r"), at(1_000))
            .await
            .unwrap();
        assert!(
            !store
                .claim_relink_notice(member, generation, at(1_001))
                .await
                .unwrap()
        );
        store
            .mark_claude_link_broken(member, generation, at(1_002))
            .await
            .unwrap();
        let newer = store
            .put_claude_link(member, &new_link("b", "s"), at(1_003))
            .await
            .unwrap();
        assert!(
            !store
                .claim_relink_notice(member, generation, at(1_004))
                .await
                .unwrap()
        );
        store
            .mark_claude_link_broken(member, newer, at(1_005))
            .await
            .unwrap();
        assert!(
            store
                .claim_relink_notice(member, newer, at(1_006))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_new_login_or_refresh_resets_the_notice_for_the_next_break() {
        let store = memory_store().await;
        let notice = broken_link(&store, "u1", 2_000).await;
        store
            .claim_relink_notice(notice.member, notice.generation, at(2_001))
            .await
            .unwrap();
        let generation = store
            .put_claude_link(notice.member, &new_link("b", "s"), at(2_002))
            .await
            .unwrap();
        store
            .mark_claude_link_broken(notice.member, generation, at(2_003))
            .await
            .unwrap();
        assert_eq!(
            store.pending_relink_notices().await.unwrap(),
            [PendingRelinkNotice {
                member: notice.member,
                generation
            }]
        );
        store
            .claim_relink_notice(notice.member, generation, at(2_004))
            .await
            .unwrap();
        let tokens = crate::ClaudeTokens {
            access_token: "c".into(),
            refresh_token: "t".into(),
            expires_at: at(9_000),
        };
        assert!(
            store
                .update_claude_tokens(notice.member, generation, &tokens, at(2_005))
                .await
                .unwrap()
        );
        let notified: Option<i64> =
            sqlx::query_scalar("SELECT relink_notified_at FROM claude_links")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(notified, None);
    }
}

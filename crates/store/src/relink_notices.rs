//! Relink notices: telling a member that their Claude link broke.
//!
//! `claude_links.relink_notified_at` records that the member was told about
//! the link's current break. It is cleared whenever the link is stored or
//! refreshed, so every time [`broken_at`](crate::ClaudeLink::broken_at) goes
//! from empty to set there is a new notice owed.
//!
//! Sending is at least once, with bounded retries. A sender
//! [claims](Store::claim_relink_notice) the notice with a conditional
//! `UPDATE`, so one caller at a time gets it across processes and restarts.
//! The claim is a lease: it counts an attempt and sets
//! `relink_next_attempt_at` to the lease's end, after which the notice is
//! pending again if the sender died before it could
//! [mark it sent](Store::mark_relink_notice_sent). A sender that failed
//! [defers](Store::defer_relink_notice) the next attempt instead. After
//! `max_attempts` claims the notice is no longer pending.

use core_types::MemberId;
use time::OffsetDateTime;

use crate::{Result, Store, StoreError, parse_column, to_unix};

/// A broken link whose member hasn't been told yet, from
/// [`Store::pending_relink_notices`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingRelinkNotice {
    /// The member to tell.
    pub member: MemberId,
    /// The link's [`generation`](crate::ClaudeLink::generation), which the
    /// other relink calls take so they never act on a newer login's link.
    pub generation: i64,
}

/// The conditions under which a notice may be claimed at `now` (bound
/// first) with fewer than `max_attempts` claims so far (bound second).
macro_rules! claimable {
    () => {
        "broken_at IS NOT NULL AND relink_notified_at IS NULL \
         AND (relink_next_attempt_at IS NULL OR relink_next_attempt_at <= ?) \
         AND relink_attempts < ?"
    };
}

impl Store {
    /// Every notice that may be claimed at `now`, oldest break first: the
    /// link is broken, its member hasn't been told, no lease or backoff runs
    /// past `now`, and it was claimed fewer than `max_attempts` times.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query
    /// fails, [`StoreError::Corrupt`] if a member
    /// id doesn't parse.
    pub async fn pending_relink_notices(
        &self,
        now: OffsetDateTime,
        max_attempts: u32,
    ) -> Result<Vec<PendingRelinkNotice>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(concat!(
            "SELECT member_id, generation FROM claude_links WHERE ",
            claimable!(),
            " ORDER BY broken_at, member_id"
        ))
        .bind(to_unix(now))
        .bind(i64::from(max_attempts))
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

    /// Claims the notice for the break of `member`'s link of `generation`
    /// at `now`, with a lease until `lease_until`, if it may be claimed (as
    /// for [`pending_relink_notices`](Self::pending_relink_notices)).
    /// Returns which attempt this is, counting from 1, only for the one
    /// call that claims it; `None` if the link isn't broken, its member was
    /// told, another claim or a backoff runs, the attempts ran out, or a
    /// newer login replaced the link.
    ///
    /// Claim before sending, then [mark it sent](Self::mark_relink_notice_sent)
    /// or [defer](Self::defer_relink_notice) it. A claim neither follows,
    /// because its sender died, is pending again once its lease ends.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query
    /// fails, [`StoreError::Corrupt`] if the
    /// attempt count is negative.
    pub async fn claim_relink_notice(
        &self,
        member: MemberId,
        generation: i64,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
        max_attempts: u32,
    ) -> Result<Option<u32>> {
        let attempt: Option<i64> = sqlx::query_scalar(concat!(
            "UPDATE claude_links \
             SET relink_attempts = relink_attempts + 1, relink_next_attempt_at = ? \
             WHERE member_id = ? AND generation = ? AND ",
            claimable!(),
            " RETURNING relink_attempts"
        ))
        .bind(to_unix(lease_until))
        .bind(member.to_string())
        .bind(generation)
        .bind(to_unix(now))
        .bind(i64::from(max_attempts))
        .fetch_optional(&self.pool)
        .await?;
        attempt
            .map(|attempt| {
                u32::try_from(attempt).map_err(|_| StoreError::Corrupt {
                    table: "claude_links",
                    column: "relink_attempts",
                })
            })
            .transpose()
    }

    /// Records that the member was told about the break of their link of
    /// `generation`, at `now`, so the notice is no longer owed. Returns
    /// false if the link isn't broken, the member was told already, or a
    /// newer login replaced it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query
    /// fails.
    pub async fn mark_relink_notice_sent(
        &self,
        member: MemberId,
        generation: i64,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE claude_links SET relink_notified_at = ?, relink_next_attempt_at = NULL \
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

    /// Ends a claim on `member`'s link of `generation` whose notice couldn't
    /// be sent: it may be claimed again from `retry_at`, if attempts are
    /// left. Returns false if the notice isn't owed any more.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query
    /// fails.
    pub async fn defer_relink_notice(
        &self,
        member: MemberId,
        generation: i64,
        retry_at: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE claude_links SET relink_next_attempt_at = ? \
             WHERE member_id = ? AND generation = ? \
             AND broken_at IS NOT NULL AND relink_notified_at IS NULL",
        )
        .bind(to_unix(retry_at))
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

    const MAX: u32 = 3;

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

    async fn pending(store: &Store, now: i64) -> Vec<PendingRelinkNotice> {
        store.pending_relink_notices(at(now), MAX).await.unwrap()
    }

    async fn claim(store: &Store, notice: PendingRelinkNotice, now: i64) -> Option<u32> {
        store
            .claim_relink_notice(
                notice.member,
                notice.generation,
                at(now),
                at(now + 600),
                MAX,
            )
            .await
            .unwrap()
    }

    async fn retry_columns(store: &Store) -> (Option<i64>, i64, Option<i64>) {
        sqlx::query_as(
            "SELECT relink_notified_at, relink_attempts, relink_next_attempt_at FROM claude_links",
        )
        .fetch_one(&store.pool)
        .await
        .unwrap()
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
        assert_eq!(pending(&store, 3_000).await, [earlier, later]);
    }

    #[tokio::test]
    async fn a_notice_is_claimed_once_and_stays_sent() {
        let store = memory_store().await;
        let notice = broken_link(&store, "u1", 2_000).await;
        let (a, b) = tokio::join!(claim(&store, notice, 2_001), claim(&store, notice, 2_001));
        assert_eq!(a.or(b), Some(1));
        assert!(a.is_none() || b.is_none());
        assert!(pending(&store, 2_001).await.is_empty());
        assert_eq!(claim(&store, notice, 2_002).await, None);
        assert!(
            store
                .mark_relink_notice_sent(notice.member, notice.generation, at(2_003))
                .await
                .unwrap()
        );
        assert!(pending(&store, 9_000).await.is_empty());
        assert_eq!(claim(&store, notice, 9_000).await, None);
        assert!(
            !store
                .mark_relink_notice_sent(notice.member, notice.generation, at(9_001))
                .await
                .unwrap()
        );
        assert!(
            !store
                .defer_relink_notice(notice.member, notice.generation, at(9_002))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_claim_whose_sender_died_is_pending_again_when_its_lease_ends() {
        let store = memory_store().await;
        let notice = broken_link(&store, "u1", 2_000).await;
        assert_eq!(claim(&store, notice, 2_001).await, Some(1));
        assert!(pending(&store, 2_600).await.is_empty());
        assert_eq!(pending(&store, 2_601).await, [notice]);
        assert_eq!(claim(&store, notice, 2_601).await, Some(2));
    }

    #[tokio::test]
    async fn a_deferred_notice_waits_and_attempts_run_out() {
        let store = memory_store().await;
        let notice = broken_link(&store, "u1", 2_000).await;
        assert!(
            !store
                .defer_relink_notice(notice.member, notice.generation + 1, at(2_000))
                .await
                .unwrap()
        );
        assert_eq!(claim(&store, notice, 2_001).await, Some(1));
        assert!(
            store
                .defer_relink_notice(notice.member, notice.generation, at(2_100))
                .await
                .unwrap()
        );
        assert!(pending(&store, 2_099).await.is_empty());
        assert_eq!(claim(&store, notice, 2_099).await, None);
        assert_eq!(pending(&store, 2_100).await, [notice]);
        assert_eq!(claim(&store, notice, 2_100).await, Some(2));
        store
            .defer_relink_notice(notice.member, notice.generation, at(2_200))
            .await
            .unwrap();
        assert_eq!(claim(&store, notice, 2_200).await, Some(3));
        store
            .defer_relink_notice(notice.member, notice.generation, at(2_300))
            .await
            .unwrap();
        assert!(pending(&store, 9_000).await.is_empty());
        assert_eq!(claim(&store, notice, 9_000).await, None);
        assert_eq!(
            store
                .pending_relink_notices(at(9_000), MAX + 1)
                .await
                .unwrap(),
            [notice]
        );
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
        let old = PendingRelinkNotice { member, generation };
        assert_eq!(claim(&store, old, 1_001).await, None);
        store
            .mark_claude_link_broken(member, generation, at(1_002))
            .await
            .unwrap();
        let newer = store
            .put_claude_link(member, &new_link("b", "s"), at(1_003))
            .await
            .unwrap();
        assert_eq!(claim(&store, old, 1_004).await, None);
        assert!(
            !store
                .mark_relink_notice_sent(member, generation, at(1_004))
                .await
                .unwrap()
        );
        store
            .mark_claude_link_broken(member, newer, at(1_005))
            .await
            .unwrap();
        let newer = PendingRelinkNotice {
            member,
            generation: newer,
        };
        assert_eq!(claim(&store, newer, 1_006).await, Some(1));
    }

    #[tokio::test]
    async fn a_new_login_or_refresh_resets_the_notice_for_the_next_break() {
        let store = memory_store().await;
        let notice = broken_link(&store, "u1", 2_000).await;
        claim(&store, notice, 2_001).await.unwrap();
        store
            .defer_relink_notice(notice.member, notice.generation, at(5_000))
            .await
            .unwrap();
        let generation = store
            .put_claude_link(notice.member, &new_link("b", "s"), at(2_002))
            .await
            .unwrap();
        assert_eq!(retry_columns(&store).await, (None, 0, None));
        store
            .mark_claude_link_broken(notice.member, generation, at(2_003))
            .await
            .unwrap();
        let second = PendingRelinkNotice {
            member: notice.member,
            generation,
        };
        assert_eq!(pending(&store, 2_003).await, [second]);
        assert_eq!(claim(&store, second, 2_004).await, Some(1));
        store
            .mark_relink_notice_sent(notice.member, generation, at(2_004))
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
        assert_eq!(retry_columns(&store).await, (None, 0, None));
    }
}

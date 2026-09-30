//! `failure_notices`: when a requester was last told privately that a turn
//! failed on the credential it ran on, so that telling them is rate-limited
//! across turns, restarts and instances.

use std::time::Duration;

use core_types::MemberKey;
use time::OffsetDateTime;

use crate::{Result, Store, to_unix};

impl Store {
    /// Claims the right to tell `requester` about a failure of `kind` at
    /// `now`: true, and `now` recorded, when they were never told about
    /// that kind or were last told at least `every` before `now`; false
    /// otherwise, and nothing changes. One caller at a time gets the claim.
    ///
    /// Claim before sending. A message that then fails to send is not
    /// retried until `every` has passed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn claim_failure_notice(
        &self,
        requester: &MemberKey,
        kind: &str,
        now: OffsetDateTime,
        every: Duration,
    ) -> Result<bool> {
        let now = to_unix(now);
        let every = i64::try_from(every.as_secs()).unwrap_or(i64::MAX);
        let result = sqlx::query(
            "INSERT INTO failure_notices (requester, kind, sent_at) VALUES (?, ?, ?) \
             ON CONFLICT (requester, kind) DO UPDATE SET sent_at = excluded.sent_at \
             WHERE failure_notices.sent_at <= ?",
        )
        .bind(requester.to_string())
        .bind(kind)
        .bind(now)
        .bind(now.saturating_sub(every))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::*;

    const HOUR: Duration = Duration::from_secs(3_600);

    #[tokio::test]
    async fn a_notice_is_claimed_once_per_requester_and_kind_per_interval() {
        let store = memory_store().await;
        let (bob, carol) = (member_key("bob"), member_key("carol"));
        let claim = |who: &MemberKey, kind: &'static str, seconds: i64| {
            let store = store.clone();
            let who = who.clone();
            async move {
                store
                    .claim_failure_notice(&who, kind, at(seconds), HOUR)
                    .await
                    .unwrap()
            }
        };
        assert!(claim(&bob, "usage_limit/member", 10_000).await);
        assert!(!claim(&bob, "usage_limit/member", 10_001).await);
        assert!(
            claim(&bob, "refused/member", 10_001).await,
            "another kind is claimed on its own"
        );
        assert!(
            claim(&carol, "usage_limit/member", 10_001).await,
            "another requester is claimed on their own"
        );
        assert!(!claim(&bob, "usage_limit/member", 13_599).await);
        assert!(
            claim(&bob, "usage_limit/member", 13_600).await,
            "an hour later it is claimed again"
        );
        assert!(!claim(&bob, "usage_limit/member", 13_601).await);
    }
}

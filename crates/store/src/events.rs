//! `processed_events`, and sweeping expired rows.

use time::{Duration, OffsetDateTime};

use crate::{Result, Store, to_unix};

/// How long a processed event is remembered. Slack retries an event for
/// minutes and Rocket.Chat redelivers on reconnect; a week covers both with a
/// wide margin.
pub const PROCESSED_EVENT_RETENTION: Duration = Duration::days(7);

/// What [`Store::sweep_expired`] deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Swept {
    /// Pending logins past their expiry.
    pub pending_logins: u64,
    /// Processed events older than [`PROCESSED_EVENT_RETENTION`].
    pub processed_events: u64,
}

impl Store {
    /// Records that the event `event_id` from `source` (such as `slack` or
    /// `rocketchat`) is being handled.
    ///
    /// Returns true the first time, and false when the event was already
    /// recorded, so the caller drops a retry or a second bot's copy. Under
    /// concurrent callers exactly one gets true.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn mark_event_processed(&self, source: &str, event_id: &str) -> Result<bool> {
        let result = sqlx::query(
            "INSERT INTO processed_events (source, event_id, seen_at) VALUES (?, ?, ?) \
             ON CONFLICT (source, event_id) DO NOTHING",
        )
        .bind(source)
        .bind(event_id)
        .bind(to_unix(OffsetDateTime::now_utc()))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes what has expired at `now`: pending logins whose expiry is not
    /// after `now`, and processed events seen more than
    /// [`PROCESSED_EVENT_RETENTION`] before it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if a query
    /// fails.
    pub async fn sweep_expired(&self, now: OffsetDateTime) -> Result<Swept> {
        let pending_logins = sqlx::query("DELETE FROM pending_logins WHERE expires_at <= ?")
            .bind(to_unix(now))
            .execute(&self.pool)
            .await?
            .rows_affected();
        let processed_events = sqlx::query("DELETE FROM processed_events WHERE seen_at < ?")
            .bind(to_unix(now - PROCESSED_EVENT_RETENTION))
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(Swept {
            pending_logins,
            processed_events,
        })
    }
}

#[cfg(test)]
mod tests {
    use secrecy::SecretString;

    use super::*;
    use crate::test_util::*;

    #[tokio::test]
    async fn an_event_is_new_only_once() {
        let store = memory_store().await;
        assert!(store.mark_event_processed("slack", "Ev1").await.unwrap());
        assert!(!store.mark_event_processed("slack", "Ev1").await.unwrap());
        assert!(store.mark_event_processed("slack", "Ev2").await.unwrap());
        assert!(
            store
                .mark_event_processed("rocketchat", "Ev1")
                .await
                .unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_marks_let_one_caller_through() {
        let dir = TempDir::new();
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let (a, b, c, d) = tokio::join!(
            store.mark_event_processed("rocketchat", "m1"),
            store.mark_event_processed("rocketchat", "m1"),
            store.mark_event_processed("rocketchat", "m1"),
            store.mark_event_processed("rocketchat", "m1"),
        );
        let firsts = [a, b, c, d]
            .into_iter()
            .filter(|result| *result.as_ref().unwrap())
            .count();
        assert_eq!(firsts, 1);
    }

    #[tokio::test]
    async fn sweep_deletes_expired_pending_logins() {
        let store = memory_store().await;
        let member = store.ensure_member(&member_key("u1"), "Ada").await.unwrap();
        let verifier = SecretString::from("v");
        for (state, expires_at) in [("old", 1_000), ("edge", 2_000), ("live", 3_000)] {
            store
                .put_pending_login(state, member, &verifier, at(expires_at))
                .await
                .unwrap();
        }
        let swept = store.sweep_expired(at(2_000)).await.unwrap();
        assert_eq!(
            swept,
            Swept {
                pending_logins: 2,
                processed_events: 0
            }
        );
        assert!(store.take_pending_login("old").await.unwrap().is_none());
        assert!(store.take_pending_login("edge").await.unwrap().is_none());
        assert!(store.take_pending_login("live").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn sweep_forgets_events_after_the_retention() {
        let store = memory_store().await;
        assert!(store.mark_event_processed("slack", "Ev1").await.unwrap());
        let now = OffsetDateTime::now_utc();
        let swept = store.sweep_expired(now).await.unwrap();
        assert_eq!(swept, Swept::default());
        assert!(!store.mark_event_processed("slack", "Ev1").await.unwrap());

        let later = now + PROCESSED_EVENT_RETENTION + Duration::minutes(1);
        let swept = store.sweep_expired(later).await.unwrap();
        assert_eq!(swept.processed_events, 1);
        assert!(store.mark_event_processed("slack", "Ev1").await.unwrap());
    }
}

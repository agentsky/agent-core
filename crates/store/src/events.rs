//! `processed_events`, and sweeping expired rows.

use time::{Duration, OffsetDateTime};

use crate::{Result, Store, to_unix};

/// How long agentd remembers a processed Rocket.Chat message, which
/// Rocket.Chat redelivers on reconnect: a week, with a wide margin. Each
/// caller of [`Store::mark_event_processed`] says how long its events are
/// remembered.
pub const PROCESSED_EVENT_RETENTION: Duration = Duration::days(7);

/// What [`Store::sweep_expired`] deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Swept {
    /// Pending logins past their expiry.
    pub pending_logins: u64,
    /// Processed events past the retention they were recorded with.
    pub processed_events: u64,
}

impl Store {
    /// Records that the event `event_id` from `source` (such as `slack` or
    /// `rocketchat`) is being handled, as seen at `now`, to be remembered
    /// for `retention`.
    ///
    /// Returns true the first time, and false when the event was already
    /// recorded, so the caller drops a retry or a second bot's copy. Under
    /// concurrent callers exactly one gets true.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn mark_event_processed(
        &self,
        source: &str,
        event_id: &str,
        now: OffsetDateTime,
        retention: Duration,
    ) -> Result<bool> {
        let result = sqlx::query(
            "INSERT INTO processed_events (source, event_id, seen_at, expires_at) \
             VALUES (?, ?, ?, ?) ON CONFLICT (source, event_id) DO NOTHING",
        )
        .bind(source)
        .bind(event_id)
        .bind(to_unix(now))
        .bind(to_unix(now + retention))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes what has expired at `now`: pending logins whose expiry is not
    /// after `now`, and processed events recorded more than their retention
    /// before it.
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
        let processed_events = sqlx::query("DELETE FROM processed_events WHERE expires_at < ?")
            .bind(to_unix(now))
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
        assert!(
            store
                .mark_event_processed("slack", "Ev1", at(1_000), PROCESSED_EVENT_RETENTION)
                .await
                .unwrap()
        );
        assert!(
            !store
                .mark_event_processed("slack", "Ev1", at(1_000), PROCESSED_EVENT_RETENTION)
                .await
                .unwrap()
        );
        assert!(
            store
                .mark_event_processed("slack", "Ev2", at(1_000), PROCESSED_EVENT_RETENTION)
                .await
                .unwrap()
        );
        assert!(
            store
                .mark_event_processed("rocketchat", "Ev1", at(1_000), PROCESSED_EVENT_RETENTION)
                .await
                .unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_marks_let_one_caller_through() {
        let dir = TempDir::new("store-test");
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let (a, b, c, d) = tokio::join!(
            store.mark_event_processed("rocketchat", "m1", at(1_000), PROCESSED_EVENT_RETENTION),
            store.mark_event_processed("rocketchat", "m1", at(1_000), PROCESSED_EVENT_RETENTION),
            store.mark_event_processed("rocketchat", "m1", at(1_000), PROCESSED_EVENT_RETENTION),
            store.mark_event_processed("rocketchat", "m1", at(1_000), PROCESSED_EVENT_RETENTION),
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
        let verifier = SecretString::from("v");
        for (state, expires_at) in [("old", 1_000), ("edge", 2_000), ("live", 3_000)] {
            let member = store
                .ensure_member(&member_key(state), "Ada", at(1_000))
                .await
                .unwrap();
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
        let seen = at(1_000_000);
        assert!(
            store
                .mark_event_processed("slack", "Ev1", seen, PROCESSED_EVENT_RETENTION)
                .await
                .unwrap()
        );
        let swept = store
            .sweep_expired(seen + PROCESSED_EVENT_RETENTION)
            .await
            .unwrap();
        assert_eq!(swept, Swept::default());
        assert!(
            !store
                .mark_event_processed("slack", "Ev1", seen, PROCESSED_EVENT_RETENTION)
                .await
                .unwrap()
        );

        let later = seen + PROCESSED_EVENT_RETENTION + Duration::seconds(1);
        let swept = store.sweep_expired(later).await.unwrap();
        assert_eq!(swept.processed_events, 1);
        assert!(
            store
                .mark_event_processed("slack", "Ev1", later, PROCESSED_EVENT_RETENTION)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn each_event_is_kept_for_the_retention_it_was_recorded_with() {
        let store = memory_store().await;
        let seen = at(1_000_000);
        let hour = Duration::hours(1);
        assert!(
            store
                .mark_event_processed("slack:b:message", "C1:1.1", seen, hour)
                .await
                .unwrap()
        );
        assert!(
            store
                .mark_event_processed("rocketchat", "m1", seen, PROCESSED_EVENT_RETENTION)
                .await
                .unwrap()
        );
        assert_eq!(
            store.sweep_expired(seen + hour).await.unwrap(),
            Swept::default()
        );
        let later = seen + hour + Duration::seconds(1);
        assert_eq!(
            store.sweep_expired(later).await.unwrap().processed_events,
            1
        );
        assert!(
            store
                .mark_event_processed("slack:b:message", "C1:1.1", later, hour)
                .await
                .unwrap()
        );
        assert!(
            !store
                .mark_event_processed("rocketchat", "m1", later, PROCESSED_EVENT_RETENTION)
                .await
                .unwrap()
        );
    }
}

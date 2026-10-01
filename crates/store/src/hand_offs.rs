//! `hand_offs`: the agent-to-agent hand-offs agentd owes, kept until each
//! has been handled so a shutdown or a crash loses none.

use std::time::Duration;

use core_types::AgentId;
use time::OffsetDateTime;

use crate::{Result, Store, from_unix, parse_column, to_unix};

const TABLE: &str = "hand_offs";

/// One hand-off agentd owes, from [`Store::take_due_hand_offs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandOff {
    /// The row's id, for [`Store::finish_hand_off`].
    pub id: i64,
    /// The agent the post mentions.
    pub agent: AgentId,
    /// The event agentd built for it, as JSON agentd owns.
    pub event_json: String,
    /// When it was recorded.
    pub created_at: OffsetDateTime,
}

/// What [`Store::take_due_hand_offs`] took, and how many it dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DueHandOffs {
    /// The hand-offs due, by id.
    pub taken: Vec<HandOff>,
    /// How many rows it deleted as recorded too long ago.
    pub stale: u64,
}

impl Store {
    /// Records a hand-off of the event `event_json` to `agent`, made at
    /// `now`, to be taken by [`take_due_hand_offs`](Self::take_due_hand_offs)
    /// from `due_at` unless it is [finished](Self::finish_hand_off) first.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails, as when the agent doesn't exist.
    pub async fn add_hand_off(
        &self,
        agent: AgentId,
        event_json: &str,
        now: OffsetDateTime,
        due_at: OffsetDateTime,
    ) -> Result<i64> {
        let id = sqlx::query_scalar(
            "INSERT INTO hand_offs (agent_id, event_json, created_at, due_at) \
             VALUES (?, ?, ?, ?) RETURNING id",
        )
        .bind(agent.to_string())
        .bind(event_json)
        .bind(to_unix(now))
        .bind(to_unix(due_at))
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    /// Takes up to `limit` hand-offs due at `now`, oldest due first, and
    /// makes each due again `lease` later, so a taker that never finishes
    /// one leaves it to the next. The rows `held`, whose jobs the caller
    /// still has, are made due `lease` later first, so no one takes them
    /// while the caller lives. Rows recorded before `stale_before` are
    /// deleted instead: a hand-off that old is no longer wanted.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if a query
    /// fails, [`StoreError::Corrupt`](crate::StoreError::Corrupt) if a row
    /// doesn't parse.
    pub async fn take_due_hand_offs(
        &self,
        now: OffsetDateTime,
        lease: Duration,
        stale_before: OffsetDateTime,
        limit: u32,
        held: &[i64],
    ) -> Result<DueHandOffs> {
        let leased = to_unix(now + lease);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE hand_offs SET due_at = ? WHERE id IN (SELECT value FROM json_each(?))")
            .bind(leased)
            .bind(sqlx::types::Json(held))
            .execute(&mut *tx)
            .await?;
        let stale = sqlx::query("DELETE FROM hand_offs WHERE created_at < ?")
            .bind(to_unix(stale_before))
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let rows: Vec<(i64, String, String, i64)> = sqlx::query_as(
            "UPDATE hand_offs SET due_at = ?1 WHERE id IN \
             (SELECT id FROM hand_offs WHERE due_at <= ?2 ORDER BY due_at, id LIMIT ?3) \
             RETURNING id, agent_id, event_json, created_at",
        )
        .bind(leased)
        .bind(to_unix(now))
        .bind(i64::from(limit))
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        let mut taken = rows
            .into_iter()
            .map(|(id, agent, event_json, created_at)| {
                Ok(HandOff {
                    id,
                    agent: parse_column(&agent, TABLE, "agent_id")?,
                    event_json,
                    created_at: from_unix(created_at, TABLE, "created_at")?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        taken.sort_by_key(|hand_off| hand_off.id);
        Ok(DueHandOffs { taken, stale })
    }

    /// Makes the hand-offs `ids` due at `now`: the jobs that held them were
    /// dropped, as a shutdown drops them, and another taker should have
    /// them at once.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn release_hand_offs(&self, ids: &[i64], now: OffsetDateTime) -> Result<()> {
        sqlx::query("UPDATE hand_offs SET due_at = ? WHERE id IN (SELECT value FROM json_each(?))")
            .bind(to_unix(now))
            .bind(sqlx::types::Json(ids))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Deletes the hand-off `id`: its agent's job for it has settled it.
    /// Deleting one already gone does nothing.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn finish_hand_off(&self, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM hand_offs WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;
    use crate::test_util::{agent, at, member_key, memory_store};

    #[tokio::test]
    async fn a_hand_off_is_taken_when_due_until_finished() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("u1"), "Ada", at(1))
            .await
            .unwrap();
        let agent = agent(&store, owner, "helper").await;
        let now = datetime!(2026-10-01 12:00 UTC);
        let soon = now + Duration::from_secs(60);
        let first = store
            .add_hand_off(agent, r#"{"first":1}"#, now, now)
            .await
            .unwrap();
        let later = store.add_hand_off(agent, "{}", now, soon).await.unwrap();
        let lease = Duration::from_secs(300);
        let stale = now - Duration::from_secs(3600);
        let ids = |due: DueHandOffs| due.taken.iter().map(|h| h.id).collect::<Vec<_>>();

        let taken = store
            .take_due_hand_offs(now, lease, stale, 10, &[])
            .await
            .unwrap();
        assert_eq!(
            taken,
            DueHandOffs {
                taken: vec![HandOff {
                    id: first,
                    agent,
                    event_json: r#"{"first":1}"#.into(),
                    created_at: now,
                }],
                stale: 0,
            }
        );
        let none = store
            .take_due_hand_offs(now, lease, stale, 10, &[])
            .await
            .unwrap();
        assert!(none.taken.is_empty(), "a taken row waits out its lease");
        let at_soon = store
            .take_due_hand_offs(soon, lease, stale, 10, &[])
            .await
            .unwrap();
        assert_eq!(ids(at_soon), [later]);
        let after_lease = now + lease;
        let again = store
            .take_due_hand_offs(after_lease, lease, stale, 1, &[])
            .await
            .unwrap();
        assert_eq!(ids(again), [first]);

        store.finish_hand_off(first).await.unwrap();
        store.finish_hand_off(first).await.unwrap();
        let rest = store
            .take_due_hand_offs(after_lease + lease, lease, stale, 10, &[])
            .await
            .unwrap();
        assert_eq!(ids(rest), [later]);
        let fresh_cut = after_lease + lease + Duration::from_secs(1);
        let dropped = store
            .take_due_hand_offs(fresh_cut + lease, lease, fresh_cut, 10, &[])
            .await
            .unwrap();
        assert_eq!(
            dropped,
            DueHandOffs {
                taken: Vec::new(),
                stale: 1,
            },
            "a stale hand-off is dropped and counted"
        );
    }

    #[tokio::test]
    async fn a_held_hand_off_is_kept_from_others_until_released() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("u1"), "Ada", at(1))
            .await
            .unwrap();
        let agent = agent(&store, owner, "helper").await;
        let now = datetime!(2026-10-01 12:00 UTC);
        let lease = Duration::from_secs(300);
        let stale = now - Duration::from_secs(3600);
        let held = store.add_hand_off(agent, "{}", now, now).await.unwrap();
        let free = store.add_hand_off(agent, "{}", now, now).await.unwrap();
        let taken = store
            .take_due_hand_offs(now, lease, stale, 10, &[held])
            .await
            .unwrap();
        assert_eq!(
            taken.taken.iter().map(|h| h.id).collect::<Vec<_>>(),
            [free],
            "the holder's own row is leased again, not taken"
        );
        let later = now + lease;
        let renewed = store
            .take_due_hand_offs(now + lease / 2, lease, stale, 10, &[held])
            .await
            .unwrap();
        assert!(renewed.taken.is_empty());
        let others = store
            .take_due_hand_offs(later, lease, stale, 10, &[])
            .await
            .unwrap();
        assert!(
            others.taken.iter().all(|h| h.id != held),
            "another taker sees a row its holder keeps leasing as not due"
        );
        store.release_hand_offs(&[held], later).await.unwrap();
        let released = store
            .take_due_hand_offs(later, lease, stale, 10, &[])
            .await
            .unwrap();
        assert_eq!(
            released.taken.iter().map(|h| h.id).collect::<Vec<_>>(),
            [held],
            "a released row is due at once"
        );
    }
}

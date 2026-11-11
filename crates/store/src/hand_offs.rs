//! `hand_offs`: the agent-to-agent hand-offs agentd owes, kept until each
//! has been handled so a shutdown or a crash loses none.

use std::time::Duration;

use core_types::{AgentId, ConvRef, ConversationId, SurfaceKind, TeamId};
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

/// A hand-off to record with the post it hands off, by
/// [`Store::record_post`].
#[derive(Debug, Clone, Copy)]
pub struct NewHandOff<'a> {
    /// The agent the post mentions.
    pub agent: AgentId,
    /// The conversation the post is in.
    pub conv: &'a ConvRef,
    /// The event agentd built for it, as JSON agentd owns.
    pub event_json: &'a str,
    /// When it was made.
    pub created_at: OffsetDateTime,
    /// When it may first be taken.
    pub due_at: OffsetDateTime,
}

/// Inserts `new` with `executor`, returning its id.
pub(crate) async fn insert<'e>(
    executor: impl sqlx::SqliteExecutor<'e>,
    new: &NewHandOff<'_>,
) -> Result<i64> {
    let id = sqlx::query_scalar(
        "INSERT INTO hand_offs (agent_id, surface, team_id, conversation, event_json, \
         created_at, due_at) VALUES (?, ?, ?, ?, ?, ?, ?) RETURNING id",
    )
    .bind(new.agent.to_string())
    .bind(new.conv.surface.as_str())
    .bind(new.conv.team.as_str())
    .bind(new.conv.conversation.as_str())
    .bind(new.event_json)
    .bind(to_unix(new.created_at))
    .bind(to_unix(new.due_at))
    .fetch_one(executor)
    .await?;
    Ok(id)
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
    /// Records a hand-off of the event `event_json`, of a post in `conv`, to
    /// `agent`, made at `now`, to be taken by
    /// [`take_due_hand_offs`](Self::take_due_hand_offs) from `due_at` unless
    /// it is [finished](Self::finish_hand_off) first.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails, as when the agent doesn't exist.
    pub async fn add_hand_off(
        &self,
        agent: AgentId,
        conv: &ConvRef,
        event_json: &str,
        now: OffsetDateTime,
        due_at: OffsetDateTime,
    ) -> Result<i64> {
        insert(
            &self.pool,
            &NewHandOff {
                agent,
                conv,
                event_json,
                created_at: now,
                due_at,
            },
        )
        .await
    }

    /// Takes up to `limit` hand-offs due at `now` in conversations `serves`
    /// accepts, oldest due first, and makes each due again `lease` later,
    /// so a taker that never finishes one leaves it to the next. A row in a
    /// conversation `serves` refuses is left as it is, for a taker that
    /// serves it, and takes no place among the `limit`. The rows `held`,
    /// whose jobs the caller still has, are made due `lease` later first,
    /// so no one takes them while the caller lives. A row on a surface this
    /// version doesn't know, as one a newer version wrote, is left as one
    /// `serves` refuses. Rows recorded before `stale_before` that are due
    /// are deleted instead, whatever `serves` says of them: a hand-off that
    /// old is no longer wanted. A held row isn't due, whichever instance
    /// holds it, as its holder keeps leasing it. A `limit` of 0 only leases
    /// the held rows again and drops the stale ones.
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
        serves: impl Fn(&ConvRef) -> bool,
    ) -> Result<DueHandOffs> {
        let leased = to_unix(now + lease);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE hand_offs SET due_at = ? WHERE id IN (SELECT value FROM json_each(?))")
            .bind(leased)
            .bind(sqlx::types::Json(held))
            .execute(&mut *tx)
            .await?;
        let stale = sqlx::query("DELETE FROM hand_offs WHERE created_at < ? AND due_at <= ?")
            .bind(to_unix(stale_before))
            .bind(to_unix(now))
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if limit == 0 {
            tx.commit().await?;
            return Ok(DueHandOffs {
                taken: Vec::new(),
                stale,
            });
        }
        let due: Vec<(i64, String, String, String)> = sqlx::query_as(
            "SELECT id, surface, team_id, conversation FROM hand_offs WHERE due_at <= ? \
             ORDER BY due_at, id",
        )
        .bind(to_unix(now))
        .fetch_all(&mut *tx)
        .await?;
        let mut ids = Vec::new();
        let mut left = limit;
        for (id, surface, team, conversation) in due {
            if left == 0 {
                break;
            }
            let Ok(surface) = surface.parse::<SurfaceKind>() else {
                continue;
            };
            let conv = ConvRef {
                surface,
                team: TeamId::new(team),
                conversation: ConversationId::new(conversation),
            };
            if serves(&conv) {
                ids.push(id);
                left -= 1;
            }
        }
        let rows: Vec<(i64, String, String, i64)> = sqlx::query_as(
            "UPDATE hand_offs SET due_at = ? WHERE id IN (SELECT value FROM json_each(?)) \
             RETURNING id, agent_id, event_json, created_at",
        )
        .bind(leased)
        .bind(sqlx::types::Json(&ids))
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

    fn conv(team: &str) -> ConvRef {
        ConvRef {
            surface: SurfaceKind::Slack,
            team: TeamId::new(team),
            conversation: ConversationId::new("C1"),
        }
    }

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
            .add_hand_off(agent, &conv("T1"), r#"{"first":1}"#, now, now)
            .await
            .unwrap();
        let later = store
            .add_hand_off(agent, &conv("T1"), "{}", now, soon)
            .await
            .unwrap();
        let lease = Duration::from_secs(300);
        let stale = now - Duration::from_secs(3600);
        let ids = |due: DueHandOffs| due.taken.iter().map(|h| h.id).collect::<Vec<_>>();

        let taken = store
            .take_due_hand_offs(now, lease, stale, 10, &[], |_| true)
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
            .take_due_hand_offs(now, lease, stale, 10, &[], |_| true)
            .await
            .unwrap();
        assert!(none.taken.is_empty(), "a taken row waits out its lease");
        let at_soon = store
            .take_due_hand_offs(soon, lease, stale, 10, &[], |_| true)
            .await
            .unwrap();
        assert_eq!(ids(at_soon), [later]);
        let after_lease = now + lease;
        let again = store
            .take_due_hand_offs(after_lease, lease, stale, 1, &[], |_| true)
            .await
            .unwrap();
        assert_eq!(ids(again), [first]);

        store.finish_hand_off(first).await.unwrap();
        store.finish_hand_off(first).await.unwrap();
        let rest = store
            .take_due_hand_offs(after_lease + lease, lease, stale, 10, &[], |_| true)
            .await
            .unwrap();
        assert_eq!(ids(rest), [later]);
        let fresh_cut = after_lease + lease + Duration::from_secs(1);
        let dropped = store
            .take_due_hand_offs(fresh_cut + lease, lease, fresh_cut, 10, &[], |_| true)
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
        let held = store
            .add_hand_off(agent, &conv("T1"), "{}", now, now)
            .await
            .unwrap();
        let free = store
            .add_hand_off(agent, &conv("T1"), "{}", now, now)
            .await
            .unwrap();
        let taken = store
            .take_due_hand_offs(now, lease, stale, 10, &[held], |_| true)
            .await
            .unwrap();
        assert_eq!(
            taken.taken.iter().map(|h| h.id).collect::<Vec<_>>(),
            [free],
            "the holder's own row is leased again, not taken"
        );
        let later = now + lease;
        let renewed = store
            .take_due_hand_offs(now + lease / 2, lease, stale, 10, &[held], |_| true)
            .await
            .unwrap();
        assert!(renewed.taken.is_empty());
        let others = store
            .take_due_hand_offs(later, lease, stale, 10, &[], |_| true)
            .await
            .unwrap();
        assert!(
            others.taken.iter().all(|h| h.id != held),
            "another taker sees a row its holder keeps leasing as not due"
        );
        let old = later + Duration::from_secs(3600);
        let past = store
            .take_due_hand_offs(old, lease, now + Duration::from_secs(1), 0, &[held], |_| {
                true
            })
            .await
            .unwrap();
        assert_eq!(
            past,
            DueHandOffs {
                taken: Vec::new(),
                stale: 1,
            },
            "a row its job still holds isn't dropped as stale, and a limit of 0 takes none"
        );
        let elsewhere = store
            .take_due_hand_offs(
                old + lease / 2,
                lease,
                now + Duration::from_secs(1),
                10,
                &[],
                |_| true,
            )
            .await
            .unwrap();
        assert_eq!(
            elsewhere,
            DueHandOffs {
                taken: Vec::new(),
                stale: 0,
            },
            "nor by another instance, while its holder keeps it leased"
        );
        store.release_hand_offs(&[held], later).await.unwrap();
        let released = store
            .take_due_hand_offs(later, lease, stale, 10, &[], |_| true)
            .await
            .unwrap();
        assert_eq!(
            released.taken.iter().map(|h| h.id).collect::<Vec<_>>(),
            [held],
            "a released row is due at once"
        );
    }

    #[tokio::test]
    async fn a_hand_off_is_taken_only_by_a_taker_serving_its_workspace() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("u1"), "Ada", at(1))
            .await
            .unwrap();
        let agent = agent(&store, owner, "helper").await;
        let now = datetime!(2026-10-01 12:00 UTC);
        let lease = Duration::from_secs(300);
        let stale = now - Duration::from_secs(3600);
        let mut elsewhere = Vec::new();
        for _ in 0..3 {
            elsewhere.push(
                store
                    .add_hand_off(agent, &conv("T2"), "{}", now, now)
                    .await
                    .unwrap(),
            );
        }
        let here = store
            .add_hand_off(agent, &conv("T1"), "{}", now, now)
            .await
            .unwrap();
        let ids = |due: DueHandOffs| due.taken.iter().map(|h| h.id).collect::<Vec<_>>();
        let in_team = |team: &'static str| move |conv: &ConvRef| conv.team.as_str() == team;

        let taken = store
            .take_due_hand_offs(now, lease, stale, 1, &[], in_team("T1"))
            .await
            .unwrap();
        assert_eq!(
            ids(taken),
            [here],
            "rows of a workspace the taker doesn't serve take no place, even due ahead"
        );
        let served = store
            .take_due_hand_offs(now, lease, stale, 10, &[], in_team("T2"))
            .await
            .unwrap();
        assert_eq!(
            ids(served),
            elsewhere,
            "they were left due, for a taker that serves them"
        );

        store
            .add_hand_off(agent, &conv("T2"), "{}", stale - lease, now)
            .await
            .unwrap();
        let dropped = store
            .take_due_hand_offs(now, lease, stale, 10, &[], in_team("T1"))
            .await
            .unwrap();
        assert_eq!(
            dropped,
            DueHandOffs {
                taken: Vec::new(),
                stale: 1,
            },
            "a stale row is dropped by any taker, so one no instance serves ages out"
        );
    }

    #[tokio::test]
    async fn a_hand_off_on_an_unknown_surface_is_left_to_age_out() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("u1"), "Ada", at(1))
            .await
            .unwrap();
        let agent = agent(&store, owner, "helper").await;
        let now = datetime!(2026-10-01 12:00 UTC);
        let lease = Duration::from_secs(300);
        let stale = now - Duration::from_secs(3600);
        let unknown = store
            .add_hand_off(agent, &conv("T1"), "{}", now, now)
            .await
            .unwrap();
        let known = store
            .add_hand_off(agent, &conv("T1"), "{}", now, now)
            .await
            .unwrap();
        let mut conn = store.pool.acquire().await.unwrap();
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("UPDATE hand_offs SET surface = 'newer' WHERE id = ?")
            .bind(unknown)
            .execute(&mut *conn)
            .await
            .unwrap();
        sqlx::query("PRAGMA ignore_check_constraints = OFF")
            .execute(&mut *conn)
            .await
            .unwrap();
        drop(conn);

        let taken = store
            .take_due_hand_offs(now, lease, stale, 10, &[], |_| true)
            .await
            .unwrap();
        assert_eq!(
            taken.taken.iter().map(|h| h.id).collect::<Vec<_>>(),
            [known],
            "a row this version can't read doesn't stop the take"
        );
        let dropped = store
            .take_due_hand_offs(
                now + lease,
                lease,
                now + Duration::from_secs(1),
                10,
                &[],
                |_| true,
            )
            .await
            .unwrap();
        assert_eq!(
            dropped,
            DueHandOffs {
                taken: Vec::new(),
                stale: 2,
            },
            "it was left due, so it ages out"
        );
    }

    #[tokio::test]
    async fn a_hand_offs_id_is_never_given_again() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("u1"), "Ada", at(1))
            .await
            .unwrap();
        let agent = agent(&store, owner, "helper").await;
        let now = datetime!(2026-10-01 12:00 UTC);
        let first = store
            .add_hand_off(agent, &conv("T1"), "{}", now, now)
            .await
            .unwrap();
        store.finish_hand_off(first).await.unwrap();
        let second = store
            .add_hand_off(agent, &conv("T1"), "{}", now, now)
            .await
            .unwrap();
        assert_ne!(
            first, second,
            "a job still holding the first id must not reach the second row"
        );
    }
}

//! `channel_id_changes`: the `channel_id_changed` events agents' Slack
//! apps received.
//!
//! Each row says a channel the binding's bot is in changed its id. It
//! waits until it is settled: Slack confirmed where the channel is now, and
//! the rules of the binding's agent moved there. A try is claimed by moving
//! `next_attempt_at` past it, so one that fails, or whose process dies, is
//! made again once that has passed, by this instance or another. A settled
//! row is kept a while, so a later change in a chain finds where the
//! channel went and a replay is known; one still waiting a while after it
//! arrived is given up.

use core_types::{AgentId, BindingId, ConversationId, TeamId};
use time::OffsetDateTime;

use crate::{Result, Store, from_unix, parse_column, to_unix};

const TABLE: &str = "channel_id_changes";

/// A channel id change a binding's app was told of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelIdChange {
    /// The binding whose app was told.
    pub binding: BindingId,
    /// The channel's id until then.
    pub old: ConversationId,
    /// The id the event says it has since.
    pub new: ConversationId,
    /// When agentd received the event.
    pub received_at: OffsetDateTime,
}

/// A recorded channel id change of one of an agent's bindings, from
/// [`Store::channel_id_changes_of_agent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownChannelIdChange {
    /// The change.
    pub change: ChannelIdChange,
    /// The binding's workspace.
    pub team: TeamId,
    /// Whether it still waits to be settled.
    pub waiting: bool,
}

/// What [`Store::record_channel_id_change`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelIdChangeRecord {
    /// The change is recorded, waiting, due at once.
    Recorded,
    /// The same change of the same binding is recorded already, waiting or
    /// settled.
    Known,
    /// The binding has as many changes waiting as it may, or as many kept
    /// as it may with none it can forget; nothing was recorded.
    Full,
}

/// How many channel id changes [`Store::record_channel_id_change`] lets
/// one binding have. `kept` should be well above `waiting`, so a binding
/// at `kept` has settled changes to forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelIdChangeLimits {
    /// The most waiting at once. Past it, a new change isn't recorded.
    pub waiting: u32,
    /// The most kept, waiting or settled. Past it, the earliest settled
    /// ones no waiting change's chain runs through are deleted to make
    /// room; a waiting one never is, and if none can go, a new change
    /// isn't recorded.
    pub kept: u32,
}

type Row = (String, String, String, i64);

fn change_of((binding, old, new, received_at): Row) -> Result<ChannelIdChange> {
    Ok(ChannelIdChange {
        binding: parse_column(&binding, TABLE, "binding_id")?,
        old: old.into(),
        new: new.into(),
        received_at: from_unix(received_at, TABLE, "received_at")?,
    })
}

impl Store {
    /// Records `change`, waiting and due at once, unless the same change of
    /// the same binding is recorded already, or the binding has
    /// `limits.waiting` changes waiting. To keep it at `limits.kept` rows,
    /// the binding's earliest settled changes are deleted, but never one
    /// that the chain of a waiting change runs through: one from the old
    /// id of a waiting change, or from an id such a chain reaches. When
    /// none can go, the change isn't recorded. Whether the change is
    /// known, or the binding has too many waiting, is read first without
    /// the write lock, so a replay or a flood of forged changes doesn't
    /// hold up the store's writers.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if a query
    /// fails, as when there is no such binding or an id isn't shaped like a
    /// channel's.
    pub async fn record_channel_id_change(
        &self,
        change: &ChannelIdChange,
        limits: ChannelIdChangeLimits,
    ) -> Result<ChannelIdChangeRecord> {
        const COUNTS: &str = "SELECT COUNT(CASE WHEN settled_at IS NULL THEN 1 END), \
             COUNT(CASE WHEN old_channel = ? AND new_channel = ? THEN 1 END) \
             FROM channel_id_changes WHERE binding_id = ?";
        let binding = change.binding.to_string();
        let refused = |(waiting, known): (i64, i64)| {
            if known > 0 {
                Some(ChannelIdChangeRecord::Known)
            } else if waiting >= i64::from(limits.waiting) {
                Some(ChannelIdChangeRecord::Full)
            } else {
                None
            }
        };
        let counts: (i64, i64) = sqlx::query_as(COUNTS)
            .bind(change.old.as_str())
            .bind(change.new.as_str())
            .bind(&binding)
            .fetch_one(&self.pool)
            .await?;
        if let Some(refused) = refused(counts) {
            return Ok(refused);
        }
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let counts: (i64, i64) = sqlx::query_as(COUNTS)
            .bind(change.old.as_str())
            .bind(change.new.as_str())
            .bind(&binding)
            .fetch_one(&mut *tx)
            .await?;
        if let Some(refused) = refused(counts) {
            return Ok(refused);
        }
        sqlx::query(
            "WITH RECURSIVE reached(channel) AS (\
             SELECT old_channel FROM channel_id_changes \
             WHERE binding_id = ? AND settled_at IS NULL \
             UNION SELECT c.new_channel FROM channel_id_changes AS c \
             JOIN reached AS r ON c.old_channel = r.channel WHERE c.binding_id = ?) \
             DELETE FROM channel_id_changes WHERE rowid IN (SELECT rowid FROM channel_id_changes \
             WHERE binding_id = ? AND settled_at IS NOT NULL \
             AND old_channel NOT IN (SELECT channel FROM reached) ORDER BY received_at, rowid \
             LIMIT max(0, (SELECT COUNT(*) FROM channel_id_changes WHERE binding_id = ?) - ? + 1))",
        )
        .bind(&binding)
        .bind(&binding)
        .bind(&binding)
        .bind(&binding)
        .bind(i64::from(limits.kept))
        .execute(&mut *tx)
        .await?;
        let (kept,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM channel_id_changes WHERE binding_id = ?")
                .bind(&binding)
                .fetch_one(&mut *tx)
                .await?;
        if kept >= i64::from(limits.kept) {
            return Ok(ChannelIdChangeRecord::Full);
        }
        let at = to_unix(change.received_at);
        sqlx::query(
            "INSERT INTO channel_id_changes \
             (binding_id, old_channel, new_channel, received_at, next_attempt_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&binding)
        .bind(change.old.as_str())
        .bind(change.new.as_str())
        .bind(at)
        .bind(at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ChannelIdChangeRecord::Recorded)
    }

    /// Up to `limit` waiting changes that may be claimed at `now`, the one
    /// due longest first, so changes tried and not settled go to the back.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails, [`StoreError::Corrupt`](crate::StoreError::Corrupt) if a row
    /// doesn't parse.
    pub async fn due_channel_id_changes(
        &self,
        now: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<ChannelIdChange>> {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT binding_id, old_channel, new_channel, received_at FROM channel_id_changes \
             WHERE settled_at IS NULL AND next_attempt_at <= ? \
             ORDER BY next_attempt_at, received_at LIMIT ?",
        )
        .bind(to_unix(now))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(change_of).collect()
    }

    /// Up to `limit` changes still waiting that were received before
    /// `received_before`, the earliest first, to be given up.
    ///
    /// # Errors
    ///
    /// As for [`due_channel_id_changes`](Self::due_channel_id_changes).
    pub async fn expired_channel_id_changes(
        &self,
        received_before: OffsetDateTime,
        limit: u32,
    ) -> Result<Vec<ChannelIdChange>> {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT binding_id, old_channel, new_channel, received_at FROM channel_id_changes \
             WHERE settled_at IS NULL AND received_at < ? ORDER BY received_at, rowid LIMIT ?",
        )
        .bind(to_unix(received_before))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(change_of).collect()
    }

    /// Claims a try at the waiting `change` at `now`, until
    /// `next_attempt_at`, when the next may be made. Returns true only for
    /// the one call that claims it: false when it is gone or settled, or a
    /// claim runs past `now`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn claim_channel_id_change(
        &self,
        change: &ChannelIdChange,
        now: OffsetDateTime,
        next_attempt_at: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE channel_id_changes SET next_attempt_at = ? \
             WHERE binding_id = ? AND old_channel = ? AND new_channel = ? \
             AND settled_at IS NULL AND next_attempt_at <= ?",
        )
        .bind(to_unix(next_attempt_at))
        .bind(change.binding.to_string())
        .bind(change.old.as_str())
        .bind(change.new.as_str())
        .bind(to_unix(now))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Marks the waiting `change` settled at `at`. Returns whether it was
    /// waiting.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn settle_channel_id_change(
        &self,
        change: &ChannelIdChange,
        at: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE channel_id_changes SET settled_at = ? \
             WHERE binding_id = ? AND old_channel = ? AND new_channel = ? \
             AND settled_at IS NULL",
        )
        .bind(to_unix(at))
        .bind(change.binding.to_string())
        .bind(change.old.as_str())
        .bind(change.new.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes `change`, waiting or settled. Returns whether it was there.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn delete_channel_id_change(&self, change: &ChannelIdChange) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM channel_id_changes \
             WHERE binding_id = ? AND old_channel = ? AND new_channel = ?",
        )
        .bind(change.binding.to_string())
        .bind(change.old.as_str())
        .bind(change.new.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes the settled changes received before `received_before` of
    /// bindings with no change waiting, and says how many there were. A
    /// binding with one waiting keeps them, so that change, given up, still
    /// follows its chain through them.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn purge_settled_channel_id_changes(
        &self,
        received_before: OffsetDateTime,
    ) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM channel_id_changes AS c WHERE settled_at IS NOT NULL AND received_at < ? \
             AND NOT EXISTS (SELECT 1 FROM channel_id_changes AS w \
             WHERE w.binding_id = c.binding_id AND w.settled_at IS NULL)",
        )
        .bind(to_unix(received_before))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// The changes recorded for `agent`'s bindings, waiting or settled,
    /// the earliest received first.
    ///
    /// # Errors
    ///
    /// As for [`due_channel_id_changes`](Self::due_channel_id_changes).
    pub async fn channel_id_changes_of_agent(
        &self,
        agent: AgentId,
    ) -> Result<Vec<KnownChannelIdChange>> {
        let rows: Vec<(String, String, String, i64, String, bool)> = sqlx::query_as(
            "SELECT c.binding_id, c.old_channel, c.new_channel, c.received_at, b.team_id, \
             c.settled_at IS NULL FROM channel_id_changes c \
             JOIN agent_bindings b ON b.id = c.binding_id WHERE b.agent_id = ? \
             ORDER BY c.received_at, c.rowid",
        )
        .bind(agent.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(binding, old, new, received_at, team, waiting)| {
                Ok(KnownChannelIdChange {
                    change: change_of((binding, old, new, received_at))?,
                    team: team.into(),
                    waiting,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;

//! `channel_id_changes`: the `channel_id_changed` events agents' Slack
//! apps received and agentd hasn't settled yet.
//!
//! Each row says a channel the binding's bot is in changed its id. It waits
//! until the change is settled: confirmed with Slack, and the rules of the
//! binding's agent moved to the new id, or found unconfirmed. A try is
//! claimed by moving `next_attempt_at` past it, so one that fails, or whose
//! process dies, is made again once that has passed, by this instance or
//! another.

use core_types::{BindingId, ConversationId};
use time::OffsetDateTime;

use crate::{Result, Store, from_unix, parse_column, to_unix};

const TABLE: &str = "channel_id_changes";

/// A channel id change a binding's app was told of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelIdChange {
    /// The binding whose app was told.
    pub binding: BindingId,
    /// The channel's id until now.
    pub old: ConversationId,
    /// The id the event says it has now.
    pub new: ConversationId,
    /// When agentd received the event.
    pub received_at: OffsetDateTime,
}

/// What [`Store::record_channel_id_change`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelIdChangeRecord {
    /// The change is recorded, due at once.
    Recorded,
    /// The same change of the same binding waits already.
    Known,
    /// The binding has as many changes waiting as it may; nothing was
    /// recorded.
    Full,
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
    /// Records `change`, due at once, unless the same change of the same
    /// binding waits already, or the binding has `max_waiting` changes
    /// waiting.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if a query
    /// fails, as when there is no such binding.
    pub async fn record_channel_id_change(
        &self,
        change: &ChannelIdChange,
        max_waiting: u32,
    ) -> Result<ChannelIdChangeRecord> {
        let binding = change.binding.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (waiting, known): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COUNT(CASE WHEN old_channel = ? AND new_channel = ? THEN 1 END) \
             FROM channel_id_changes WHERE binding_id = ?",
        )
        .bind(change.old.as_str())
        .bind(change.new.as_str())
        .bind(&binding)
        .fetch_one(&mut *tx)
        .await?;
        if known > 0 {
            return Ok(ChannelIdChangeRecord::Known);
        }
        if waiting >= i64::from(max_waiting) {
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

    /// Up to `limit` changes that may be claimed at `now`, the earliest
    /// received first.
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
             WHERE next_attempt_at <= ? ORDER BY received_at, rowid LIMIT ?",
        )
        .bind(to_unix(now))
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(change_of).collect()
    }

    /// Claims a try at `change` at `now`, until `next_attempt_at`, when the
    /// next may be made. Returns true only for the one call that claims it:
    /// false when it is gone, or a claim runs past `now`.
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
             AND next_attempt_at <= ?",
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

    /// Deletes `change`, once it is settled. Returns whether it was there.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn finish_channel_id_change(&self, change: &ChannelIdChange) -> Result<bool> {
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

    /// Deletes the changes received before `received_before`, which are
    /// given up, and returns them.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails, [`StoreError::Corrupt`](crate::StoreError::Corrupt) if a row
    /// doesn't parse.
    pub async fn drop_stale_channel_id_changes(
        &self,
        received_before: OffsetDateTime,
    ) -> Result<Vec<ChannelIdChange>> {
        let rows: Vec<Row> = sqlx::query_as(
            "DELETE FROM channel_id_changes WHERE received_at < ? \
             RETURNING binding_id, old_channel, new_channel, received_at",
        )
        .bind(to_unix(received_before))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(change_of).collect()
    }
}

#[cfg(test)]
mod tests;

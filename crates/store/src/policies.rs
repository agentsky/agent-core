//! `agent_policies`: an owner's limits and allow and deny rules for one
//! agent, and `bans`: the members a community admin banned.

use core_types::{AgentId, MemberId, MemberKey};
use time::OffsetDateTime;

use crate::{Result, Store, StoreError, from_unix, parse_column, to_unix};

const POLICIES: &str = "agent_policies";
const BANS: &str = "bans";

/// An empty rule list, as `allow_json` and `deny_json` hold it.
pub const NO_RULES: &str = "[]";

/// An owner's settings for one agent, from `agent_policies`. An agent
/// without a row has the [`Default`]: no limits and no rules.
///
/// The rules are JSON lists whose shape agentd owns; the store keeps them
/// as given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSettings {
    /// The most turns a day the agent takes for anyone but its owner, or
    /// `None` for no cap.
    pub turns_per_day: Option<u32>,
    /// The agent's own hop limit, or `None` to leave the global one.
    pub max_hops: Option<u8>,
    /// The allow rules.
    pub allow_json: String,
    /// The deny rules.
    pub deny_json: String,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            turns_per_day: None,
            max_hops: None,
            allow_json: NO_RULES.to_owned(),
            deny_json: NO_RULES.to_owned(),
        }
    }
}

/// A community admin's ban of a member, from `bans`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ban {
    /// The admin who banned them.
    pub banned_by: MemberKey,
    /// Why, as the admin wrote it.
    pub reason: Option<String>,
    /// When.
    pub created_at: OffsetDateTime,
}

impl Store {
    /// `agent`'s settings, or the [`Default`] if its owner set none.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails,
    /// [`StoreError::Corrupt`] if a limit is out of range.
    pub async fn agent_settings(&self, agent: AgentId) -> Result<AgentSettings> {
        settings_of(&self.pool, agent).await
    }

    /// Changes `agent`'s settings with `change`, which gets them as they
    /// are (the [`Default`] if its owner set none) and may change them, and
    /// returns what `change` returns. The read and the write are one
    /// transaction, so changes made at once are all kept.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if a query fails, for example because
    /// there is no such agent, [`StoreError::Corrupt`] if a limit is out of
    /// range.
    pub async fn update_agent_settings<T>(
        &self,
        agent: AgentId,
        change: impl FnOnce(&mut AgentSettings) -> T,
    ) -> Result<T> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let before = settings_of(&mut *tx, agent).await?;
        let mut settings = before.clone();
        let changed = change(&mut settings);
        if settings != before {
            sqlx::query(
                "INSERT INTO agent_policies (agent_id, turns_per_day, max_hops, allow_json, \
                 deny_json) VALUES (?, ?, ?, ?, ?) ON CONFLICT (agent_id) DO UPDATE SET \
                 turns_per_day = excluded.turns_per_day, max_hops = excluded.max_hops, \
                 allow_json = excluded.allow_json, deny_json = excluded.deny_json",
            )
            .bind(agent.to_string())
            .bind(settings.turns_per_day.map(i64::from))
            .bind(settings.max_hops.map(i64::from))
            .bind(&settings.allow_json)
            .bind(&settings.deny_json)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
        }
        Ok(changed)
    }

    /// Bans `member`, recording that `by` did at `now` and why. Returns
    /// false, and changes nothing, if they were banned already.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, for example because
    /// there is no such member.
    pub async fn ban_member(
        &self,
        member: MemberId,
        by: &MemberKey,
        reason: Option<&str>,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "INSERT INTO bans (member_id, banned_by, reason, created_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (member_id) DO NOTHING",
        )
        .bind(member.to_string())
        .bind(by.to_string())
        .bind(reason)
        .bind(to_unix(now))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Lifts `member`'s ban. Returns whether they were banned.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn unban_member(&self, member: MemberId) -> Result<bool> {
        let result = sqlx::query("DELETE FROM bans WHERE member_id = ?")
            .bind(member.to_string())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// `member`'s ban, or `None` if they aren't banned.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails,
    /// [`StoreError::Corrupt`] if the row doesn't parse.
    pub async fn ban(&self, member: MemberId) -> Result<Option<Ban>> {
        let row: Option<(String, Option<String>, i64)> =
            sqlx::query_as("SELECT banned_by, reason, created_at FROM bans WHERE member_id = ?")
                .bind(member.to_string())
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(by, reason, at)| {
            Ok(Ban {
                banned_by: parse_column(&by, BANS, "banned_by")?,
                reason,
                created_at: from_unix(at, BANS, "created_at")?,
            })
        })
        .transpose()
    }

    /// Whether `member` is banned, without reading the ban.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn is_banned(&self, member: MemberId) -> Result<bool> {
        let banned: Option<i64> = sqlx::query_scalar("SELECT 1 FROM bans WHERE member_id = ?")
            .bind(member.to_string())
            .fetch_optional(&self.pool)
            .await?;
        Ok(banned.is_some())
    }
}

/// `agent`'s settings, read through `executor`.
async fn settings_of<'c>(
    executor: impl sqlx::SqliteExecutor<'c>,
    agent: AgentId,
) -> Result<AgentSettings> {
    let row: Option<(Option<i64>, Option<i64>, String, String)> = sqlx::query_as(
        "SELECT turns_per_day, max_hops, allow_json, deny_json FROM agent_policies \
         WHERE agent_id = ?",
    )
    .bind(agent.to_string())
    .fetch_optional(executor)
    .await?;
    let Some((turns, hops, allow_json, deny_json)) = row else {
        return Ok(AgentSettings::default());
    };
    let corrupt = |column| StoreError::Corrupt {
        table: POLICIES,
        column,
    };
    Ok(AgentSettings {
        turns_per_day: turns
            .map(|turns| u32::try_from(turns).map_err(|_| corrupt("turns_per_day")))
            .transpose()?,
        max_hops: hops
            .map(|hops| u8::try_from(hops).map_err(|_| corrupt("max_hops")))
            .transpose()?,
        allow_json,
        deny_json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{agent, at, member_key, memory_store};

    #[tokio::test]
    async fn an_agent_without_settings_has_the_default() {
        let store = memory_store().await;
        let alice = store
            .ensure_member(&member_key("alice"), "alice", at(1))
            .await
            .unwrap();
        let helper = agent(&store, alice, "helper").await;
        assert_eq!(
            store.agent_settings(helper).await.unwrap(),
            AgentSettings::default()
        );
        assert_eq!(AgentSettings::default().allow_json, NO_RULES);
    }

    #[tokio::test]
    async fn settings_change_in_place_and_an_unchanged_agent_keeps_no_row() {
        let store = memory_store().await;
        let alice = store
            .ensure_member(&member_key("alice"), "alice", at(1))
            .await
            .unwrap();
        let helper = agent(&store, alice, "helper").await;
        let rows = async || -> i64 {
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_policies")
                .fetch_one(&store.pool)
                .await
                .unwrap()
        };
        let seen = store
            .update_agent_settings(helper, |settings| settings.clone())
            .await
            .unwrap();
        assert_eq!(seen, AgentSettings::default());
        assert_eq!(rows().await, 0, "nothing changed, so nothing is written");

        store
            .update_agent_settings(helper, |settings| {
                settings.turns_per_day = Some(50);
                settings.max_hops = Some(2);
                settings.allow_json = r#"["a"]"#.into();
            })
            .await
            .unwrap();
        store
            .update_agent_settings(helper, |settings| {
                settings.deny_json = r#"["d"]"#.into();
                settings.max_hops = None;
            })
            .await
            .unwrap();
        assert_eq!(
            store.agent_settings(helper).await.unwrap(),
            AgentSettings {
                turns_per_day: Some(50),
                max_hops: None,
                allow_json: r#"["a"]"#.into(),
                deny_json: r#"["d"]"#.into(),
            }
        );
        assert_eq!(rows().await, 1);
        assert!(
            store
                .update_agent_settings(AgentId::new_v4(), |settings| settings.max_hops = Some(1))
                .await
                .is_err(),
            "no such agent"
        );
    }

    #[tokio::test]
    async fn an_out_of_range_limit_is_corrupt() {
        let store = memory_store().await;
        let alice = store
            .ensure_member(&member_key("alice"), "alice", at(1))
            .await
            .unwrap();
        let helper = agent(&store, alice, "helper").await;
        sqlx::query("INSERT INTO agent_policies (agent_id, turns_per_day) VALUES (?, ?)")
            .bind(helper.to_string())
            .bind(i64::from(u32::MAX) + 1)
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store.agent_settings(helper).await,
            Err(StoreError::Corrupt {
                column: "turns_per_day",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn a_member_is_banned_once_until_unbanned() {
        let store = memory_store().await;
        let bob = store
            .ensure_member(&member_key("bob"), "bob", at(1))
            .await
            .unwrap();
        let admin = member_key("admin");
        assert!(!store.is_banned(bob).await.unwrap());
        assert_eq!(store.ban(bob).await.unwrap(), None);
        assert!(
            store
                .ban_member(bob, &admin, Some("spam"), at(10))
                .await
                .unwrap()
        );
        assert!(
            !store.ban_member(bob, &admin, None, at(20)).await.unwrap(),
            "banned already"
        );
        assert!(store.is_banned(bob).await.unwrap());
        assert_eq!(
            store.ban(bob).await.unwrap(),
            Some(Ban {
                banned_by: admin,
                reason: Some("spam".into()),
                created_at: at(10),
            })
        );
        assert!(store.unban_member(bob).await.unwrap());
        assert!(!store.unban_member(bob).await.unwrap());
        assert!(!store.is_banned(bob).await.unwrap());
    }
}

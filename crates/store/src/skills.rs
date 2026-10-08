//! `agent_skills`: the skills owners added to their agents, and the hosts
//! each lets the agent's sandboxes reach; and `skill_leases`, which keep
//! one writer at a time on each agent's skill name.

use std::time::Duration;

use core_types::{AgentId, LeaseId, MemberId, SessionId};
use time::OffsetDateTime;

use crate::{Result, Store, StoreError, from_unix, parse_column, to_unix, ttl_seconds};

const TABLE: &str = "agent_skills";

/// Whether a skill is in use or waiting for its owner to confirm its hosts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SkillState {
    /// Added, but not in use: its `SKILL.md` asks for hosts the owner
    /// hasn't confirmed yet.
    Pending,
    /// In use: its files are in the agent's skills directory, and its hosts
    /// extend the agent's egress allowlist.
    Active,
}

impl SkillState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "active" => Ok(Self::Active),
            _ => Err(StoreError::Corrupt {
                table: TABLE,
                column: "state",
            }),
        }
    }
}

/// An `agent_skills` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSkill {
    /// The agent the skill belongs to.
    pub agent: AgentId,
    /// The skill's name, from its `SKILL.md`.
    pub name: String,
    /// Pending or active.
    pub state: SkillState,
    /// Where it came from: a Git URL, or `upload:` and a file name.
    pub source: String,
    /// The host rules its sandboxes may reach, as written.
    pub hosts: Vec<String>,
    /// The member who added it.
    pub added_by: MemberId,
    /// When it was added.
    pub added_at: OffsetDateTime,
}

/// A skill to record, for [`Store::put_skill`].
#[derive(Debug, Clone, Copy)]
pub struct NewSkill<'a> {
    /// The agent the skill belongs to.
    pub agent: AgentId,
    /// The skill's name.
    pub name: &'a str,
    /// Where it came from.
    pub source: &'a str,
    /// The host rules its sandboxes may reach. None of them may hold a line
    /// break.
    pub hosts: &'a [String],
    /// The member adding it.
    pub added_by: MemberId,
}

#[derive(sqlx::FromRow)]
struct Row {
    agent_id: String,
    name: String,
    state: String,
    source: String,
    hosts: String,
    added_by: String,
    added_at: i64,
}

impl Row {
    fn into_skill(self) -> Result<AgentSkill> {
        Ok(AgentSkill {
            agent: parse_column(&self.agent_id, TABLE, "agent_id")?,
            name: self.name,
            state: SkillState::parse(&self.state)?,
            source: self.source,
            hosts: split_hosts(&self.hosts),
            added_by: parse_column(&self.added_by, TABLE, "added_by")?,
            added_at: from_unix(self.added_at, TABLE, "added_at")?,
        })
    }
}

fn split_hosts(hosts: &str) -> Vec<String> {
    hosts
        .lines()
        .filter(|host| !host.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The columns every query reads, in [`Row`]'s order.
macro_rules! columns {
    () => {
        "agent_id, name, state, source, hosts, added_by, added_at"
    };
}

impl Store {
    /// Records `skill` in `state`, replacing the agent's skill of that name
    /// in that state, unless the agent already has `max_skills` skills of
    /// other names, pending or active. Recording an active skill also drops
    /// a pending one of the same name, which it supersedes. Returns whether
    /// it recorded the skill.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the agent or member doesn't exist, a
    /// host holds a line break, or the query fails.
    pub async fn put_skill(
        &self,
        skill: &NewSkill<'_>,
        state: SkillState,
        max_skills: usize,
        now: OffsetDateTime,
    ) -> Result<bool> {
        if skill.hosts.iter().any(|host| host.contains(['\n', '\r'])) {
            return Err(StoreError::Database(sqlx::Error::Protocol(
                "a skill's host holds a line break".into(),
            )));
        }
        let agent = skill.agent.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let others: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT name) FROM agent_skills WHERE agent_id = ? AND name <> ?",
        )
        .bind(&agent)
        .bind(skill.name)
        .fetch_one(&mut *tx)
        .await?;
        if usize::try_from(others).unwrap_or(usize::MAX) >= max_skills {
            return Ok(false);
        }
        if state == SkillState::Active {
            sqlx::query("DELETE FROM agent_skills WHERE agent_id = ? AND name = ?")
                .bind(&agent)
                .bind(skill.name)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "INSERT INTO agent_skills (agent_id, name, state, source, hosts, added_by, added_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (agent_id, name, state) DO UPDATE SET source = excluded.source, \
             hosts = excluded.hosts, added_by = excluded.added_by, added_at = excluded.added_at",
        )
        .bind(&agent)
        .bind(skill.name)
        .bind(state.as_str())
        .bind(skill.source)
        .bind(skill.hosts.join("\n"))
        .bind(skill.added_by.to_string())
        .bind(to_unix(now))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Makes the pending skill `shown` active, replacing the active skill
    /// of that name, if its row still holds what `shown` read: the same
    /// hosts, added at the same time. Returns it, or `None` if that row is
    /// gone or was replaced.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn confirm_skill(&self, shown: &AgentSkill) -> Result<Option<AgentSkill>> {
        let agent = shown.agent.to_string();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let matched: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM agent_skills \
             WHERE agent_id = ? AND name = ? AND state = 'pending' AND hosts = ? AND added_at = ?",
        )
        .bind(&agent)
        .bind(&shown.name)
        .bind(shown.hosts.join("\n"))
        .bind(to_unix(shown.added_at))
        .fetch_optional(&mut *tx)
        .await?;
        if matched.is_none() {
            return Ok(None);
        }
        sqlx::query(
            "DELETE FROM agent_skills WHERE agent_id = ? AND name = ? AND state = 'active'",
        )
        .bind(&agent)
        .bind(&shown.name)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE agent_skills SET state = 'active' \
             WHERE agent_id = ? AND name = ? AND state = 'pending'",
        )
        .bind(&agent)
        .bind(&shown.name)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(AgentSkill {
            state: SkillState::Active,
            ..shown.clone()
        }))
    }

    /// Deletes the agent's skill `name` in `state`, or in both states
    /// with `None`, and returns the rows it deleted, by state.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn delete_skill(
        &self,
        agent: AgentId,
        name: &str,
        state: Option<SkillState>,
    ) -> Result<Vec<AgentSkill>> {
        let rows: Vec<Row> = sqlx::query_as(concat!(
            "DELETE FROM agent_skills WHERE agent_id = ? AND name = ? \
             AND (? IS NULL OR state = ?) RETURNING ",
            columns!()
        ))
        .bind(agent.to_string())
        .bind(name)
        .bind(state.map(SkillState::as_str))
        .bind(state.map(SkillState::as_str))
        .fetch_all(&self.pool)
        .await?;
        let mut deleted = rows
            .into_iter()
            .map(Row::into_skill)
            .collect::<Result<Vec<_>>>()?;
        deleted.sort_by_key(|row| row.state.as_str());
        Ok(deleted)
    }

    /// The agent's skills, active and pending, by name.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn agent_skills(&self, agent: AgentId) -> Result<Vec<AgentSkill>> {
        let rows: Vec<Row> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM agent_skills WHERE agent_id = ? ORDER BY name, state"
        ))
        .bind(agent.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Row::into_skill).collect()
    }

    /// The host rules of the active skills of `session`'s agent, as
    /// written: what the egress proxy adds to that session's allowlist.
    /// Empty for an unknown session or a deleted agent.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn skill_hosts_for_session(&self, session: SessionId) -> Result<Vec<String>> {
        let hosts: Vec<String> = sqlx::query_scalar(
            "SELECT k.hosts FROM agent_skills k \
             JOIN sessions s ON s.agent_id = k.agent_id \
             JOIN agents a ON a.id = k.agent_id \
             WHERE s.id = ? AND k.state = 'active' AND a.state <> 'deleted' AND k.hosts <> ''",
        )
        .bind(session.to_string())
        .fetch_all(&self.pool)
        .await?;
        let mut rules: Vec<String> = hosts.iter().flat_map(|h| split_hosts(h)).collect();
        rules.sort();
        rules.dedup();
        Ok(rules)
    }

    /// The agent and name of every pending skill added before `before`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn pending_skills_before(
        &self,
        before: OffsetDateTime,
    ) -> Result<Vec<(AgentId, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT agent_id, name FROM agent_skills WHERE state = 'pending' AND added_at < ? \
             ORDER BY agent_id, name",
        )
        .bind(to_unix(before))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(agent, name)| Ok((parse_column(&agent, TABLE, "agent_id")?, name)))
            .collect()
    }

    /// Deletes the agent's pending skill `name` if it was added before
    /// `before`, and returns whether it did.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn delete_pending_skill_before(
        &self,
        agent: AgentId,
        name: &str,
        before: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM agent_skills \
             WHERE agent_id = ? AND name = ? AND state = 'pending' AND added_at < ?",
        )
        .bind(agent.to_string())
        .bind(name)
        .bind(to_unix(before))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Takes the lease on the agent's skill `name` at `now`, until
    /// `now + ttl`, unless another lease on it runs past `now`. Returns the
    /// lease, to release, or `None` if it is held.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn acquire_skill_lease(
        &self,
        agent: AgentId,
        name: &str,
        now: OffsetDateTime,
        ttl: Duration,
    ) -> Result<Option<LeaseId>> {
        let lease = LeaseId::new_v4();
        let now = to_unix(now);
        let granted: Option<String> = sqlx::query_scalar(
            "INSERT INTO skill_leases (agent_id, name, lease_id, expires_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT (agent_id, name) DO UPDATE SET lease_id = excluded.lease_id, \
             expires_at = excluded.expires_at WHERE skill_leases.expires_at <= ? \
             RETURNING lease_id",
        )
        .bind(agent.to_string())
        .bind(name)
        .bind(lease.to_string())
        .bind(now.saturating_add(ttl_seconds(ttl)))
        .bind(now)
        .fetch_optional(&self.pool)
        .await?;
        Ok(granted.map(|_| lease))
    }

    /// Gives up `lease` on the agent's skill `name`. Returns false,
    /// changing nothing, if it isn't the lease held on it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn release_skill_lease(
        &self,
        agent: AgentId,
        name: &str,
        lease: LeaseId,
    ) -> Result<bool> {
        let result = sqlx::query(
            "DELETE FROM skill_leases WHERE agent_id = ? AND name = ? AND lease_id = ?",
        )
        .bind(agent.to_string())
        .bind(name)
        .bind(lease.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use core_types::{ScopeKey, SurfaceKind, TeamId, ThreadKey};

    use super::*;
    use crate::test_util::{at, member_key, memory_store};
    use crate::{AgentCreation, NewAgent, Visibility};

    async fn agent(store: &Store, owner: MemberId, name: &str) -> AgentId {
        let team = TeamId::new("T1");
        let new = NewAgent {
            owner,
            name,
            persona: "p",
            visibility: Visibility::Public,
            surface: SurfaceKind::RocketChat,
            team: &team,
        };
        match store.create_agent(&new, 10, at(1)).await.unwrap() {
            AgentCreation::Created(agent, _) => agent.id,
            other => panic!("{other:?}"),
        }
    }

    fn skill<'a>(agent: AgentId, name: &'a str, hosts: &'a [String], by: MemberId) -> NewSkill<'a> {
        NewSkill {
            agent,
            name,
            source: "https://git.example/s.git",
            hosts,
            added_by: by,
        }
    }

    #[tokio::test]
    async fn a_pending_skill_waits_next_to_the_active_one_until_confirmed_as_shown() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("o"), "o", at(1))
            .await
            .unwrap();
        let a = agent(&store, owner, "helper").await;
        let none: Vec<String> = Vec::new();
        let hosts = vec!["api.github.com".to_owned(), "*.example.org:8443".to_owned()];
        store
            .put_skill(
                &skill(a, "gh", &none, owner),
                SkillState::Active,
                32,
                at(10),
            )
            .await
            .unwrap();
        store
            .put_skill(
                &skill(a, "gh", &hosts, owner),
                SkillState::Pending,
                32,
                at(20),
            )
            .await
            .unwrap();
        let rows = store.agent_skills(a).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.state).collect::<Vec<_>>(),
            [SkillState::Active, SkillState::Pending]
        );
        assert_eq!(rows[1].hosts, hosts);
        assert_eq!(rows[1].added_at, at(20));

        let shown = rows[1].clone();
        for replaced in [
            AgentSkill {
                added_at: at(19),
                ..shown.clone()
            },
            AgentSkill {
                hosts: vec!["api.github.com".to_owned()],
                ..shown.clone()
            },
        ] {
            assert_eq!(store.confirm_skill(&replaced).await.unwrap(), None);
        }
        assert_eq!(store.agent_skills(a).await.unwrap(), rows);
        let confirmed = store.confirm_skill(&shown).await.unwrap().unwrap();
        assert_eq!(confirmed.state, SkillState::Active);
        assert_eq!(confirmed.hosts, hosts);
        let rows = store.agent_skills(a).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, SkillState::Active);
        assert_eq!(rows[0].hosts, hosts);
        assert_eq!(store.confirm_skill(&shown).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_active_skill_supersedes_a_pending_one() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("o"), "o", at(1))
            .await
            .unwrap();
        let a = agent(&store, owner, "helper").await;
        let hosts = vec!["api.github.com".to_owned()];
        store
            .put_skill(
                &skill(a, "gh", &hosts, owner),
                SkillState::Pending,
                32,
                at(10),
            )
            .await
            .unwrap();
        store
            .put_skill(&skill(a, "gh", &[], owner), SkillState::Active, 32, at(11))
            .await
            .unwrap();
        let rows = store.agent_skills(a).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].state, rows[0].hosts.len()),
            (SkillState::Active, 0)
        );
    }

    #[tokio::test]
    async fn only_active_hosts_of_the_sessions_agent_extend_its_allowlist() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("o"), "o", at(1))
            .await
            .unwrap();
        let a = agent(&store, owner, "helper").await;
        let b = agent(&store, owner, "other").await;
        let gh = vec!["api.github.com".to_owned(), "uploads.github.com".to_owned()];
        let pypi = vec!["pypi.org".to_owned(), "api.github.com".to_owned()];
        let secret = vec!["secret.example".to_owned()];
        store
            .put_skill(&skill(a, "gh", &gh, owner), SkillState::Active, 32, at(1))
            .await
            .unwrap();
        store
            .put_skill(&skill(a, "py", &pypi, owner), SkillState::Active, 32, at(1))
            .await
            .unwrap();
        store
            .put_skill(
                &skill(a, "wait", &secret, owner),
                SkillState::Pending,
                32,
                at(1),
            )
            .await
            .unwrap();
        store
            .put_skill(
                &skill(b, "b", &secret, owner),
                SkillState::Active,
                32,
                at(1),
            )
            .await
            .unwrap();
        let thread = ThreadKey {
            conv: core_types::ConvRef {
                surface: SurfaceKind::RocketChat,
                team: TeamId::new("T1"),
                conversation: "C1".into(),
            },
            root: None,
        };
        let session = store
            .session_for_thread(a, &thread, &ScopeKey::Private, at(2))
            .await
            .unwrap()
            .session
            .id;
        assert_eq!(
            store.skill_hosts_for_session(session).await.unwrap(),
            ["api.github.com", "pypi.org", "uploads.github.com"]
        );
        assert!(
            store
                .skill_hosts_for_session(SessionId::new_v4())
                .await
                .unwrap()
                .is_empty()
        );
        assert!(store.delete_agent(a, at(3)).await.unwrap());
        assert!(
            store
                .skill_hosts_for_session(session)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn delete_and_expiry() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("o"), "o", at(1))
            .await
            .unwrap();
        let a = agent(&store, owner, "helper").await;
        let hosts = vec!["api.github.com".to_owned()];
        store
            .put_skill(&skill(a, "gh", &[], owner), SkillState::Active, 32, at(1))
            .await
            .unwrap();
        store
            .put_skill(
                &skill(a, "gh", &hosts, owner),
                SkillState::Pending,
                32,
                at(5),
            )
            .await
            .unwrap();
        store
            .put_skill(
                &skill(a, "late", &hosts, owner),
                SkillState::Pending,
                32,
                at(50),
            )
            .await
            .unwrap();
        assert_eq!(
            store.pending_skills_before(at(10)).await.unwrap(),
            [(a, "gh".to_owned())]
        );
        assert!(
            !store
                .delete_pending_skill_before(a, "late", at(10))
                .await
                .unwrap()
        );
        assert!(
            store
                .delete_pending_skill_before(a, "gh", at(10))
                .await
                .unwrap()
        );
        assert!(
            store
                .delete_skill(a, "gh", Some(SkillState::Pending))
                .await
                .unwrap()
                .is_empty()
        );
        let deleted = store.delete_skill(a, "gh", None).await.unwrap();
        assert_eq!(
            deleted
                .iter()
                .map(|row| (row.state, row.hosts.len()))
                .collect::<Vec<_>>(),
            [(SkillState::Active, 0)]
        );
        assert!(store.delete_skill(a, "gh", None).await.unwrap().is_empty());
        let deleted = store
            .delete_skill(a, "late", Some(SkillState::Pending))
            .await
            .unwrap();
        assert_eq!(
            deleted.iter().map(|row| row.state).collect::<Vec<_>>(),
            [SkillState::Pending]
        );
        assert!(store.agent_skills(a).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_skill_lease_has_one_holder_until_released_or_expired() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("o"), "o", at(1))
            .await
            .unwrap();
        let a = agent(&store, owner, "helper").await;
        let b = agent(&store, owner, "other").await;
        let ttl = Duration::from_secs(60);
        let first = store
            .acquire_skill_lease(a, "gh", at(100), ttl)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .acquire_skill_lease(a, "gh", at(159), ttl)
                .await
                .unwrap(),
            None,
            "held"
        );
        for (agent, name) in [(a, "py"), (b, "gh")] {
            assert!(
                store
                    .acquire_skill_lease(agent, name, at(100), ttl)
                    .await
                    .unwrap()
                    .is_some(),
                "{name}: one lease per agent and name"
            );
        }
        assert!(
            !store
                .release_skill_lease(a, "gh", LeaseId::new_v4())
                .await
                .unwrap()
        );
        assert!(store.release_skill_lease(a, "gh", first).await.unwrap());
        let second = store
            .acquire_skill_lease(a, "gh", at(101), ttl)
            .await
            .unwrap()
            .unwrap();
        let third = store
            .acquire_skill_lease(a, "gh", at(161), ttl)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(second, third, "an expired lease is taken over");
        assert!(!store.release_skill_lease(a, "gh", second).await.unwrap());
        assert!(store.release_skill_lease(a, "gh", third).await.unwrap());
    }

    #[tokio::test]
    async fn an_agent_has_at_most_max_skills_by_name() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("o"), "o", at(1))
            .await
            .unwrap();
        let a = agent(&store, owner, "helper").await;
        let hosts = ["api.github.com".to_owned()];
        for (name, hosts, state) in [
            ("one", &[][..], SkillState::Active),
            ("two", &hosts[..], SkillState::Pending),
            ("one", &hosts[..], SkillState::Pending),
            ("two", &[][..], SkillState::Active),
        ] {
            assert!(
                store
                    .put_skill(&skill(a, name, hosts, owner), state, 2, at(1))
                    .await
                    .unwrap(),
                "{name} {state:?}"
            );
        }
        assert!(
            !store
                .put_skill(&skill(a, "three", &[], owner), SkillState::Active, 2, at(1))
                .await
                .unwrap()
        );
        assert_eq!(store.agent_skills(a).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn a_host_with_a_line_break_is_refused() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("o"), "o", at(1))
            .await
            .unwrap();
        let a = agent(&store, owner, "helper").await;
        let hosts = vec!["a.example\nb.example".to_owned()];
        assert!(
            store
                .put_skill(&skill(a, "x", &hosts, owner), SkillState::Active, 32, at(1))
                .await
                .is_err()
        );
        assert!(store.agent_skills(a).await.unwrap().is_empty());
    }
}

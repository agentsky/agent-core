//! `agents` and `agent_bindings`.
//!
//! An agent is created together with one binding in state
//! [`creating`](BindingState::Creating). The caller then notes the username
//! it asks for
//! ([`set_binding_bot_username`](Store::set_binding_bot_username)), creates
//! the bot user on the platform, [records it](Store::set_binding_bot_user),
//! obtains its token and [activates](Store::activate_binding) the binding. A
//! creation that never finishes, because the caller failed or died, is
//! [abandoned](Store::abandon_creation): the binding is disabled and the
//! agent deleted, which frees its name.
//!
//! A disabled binding whose bot user exists owes its retirement:
//! deactivating the bot user. So does one that only noted a username, since
//! a creation that died after `users.create` may have made a bot user under
//! it: the retirement looks it up first, and
//! [forgets the username](Store::forget_binding_bot_username) once it is
//! known to be no bot user of the binding. It follows the relink notices'
//! pattern. A caller [claims](Store::claim_retirement) it with a conditional
//! `UPDATE`, which counts an attempt and holds a lease, so one caller at a
//! time retires it across processes and restarts, then
//! [marks it retired](Store::mark_retired) or
//! [defers](Store::defer_retirement) the next attempt.

use std::fmt;

use core_types::{AgentId, BindingId, MemberId, MemberKey, SurfaceKind, TeamId, UserId};
use secrecy::SecretString;
use sqlx::SqliteConnection;
use time::OffsetDateTime;

use crate::seal::Aad;
use crate::{Result, Store, StoreError, from_unix, parse_column, to_unix};

const AGENTS: &str = "agents";
const BINDINGS: &str = "agent_bindings";
const TOKEN: &str = "bot_token_enc";

/// Whether an agent takes part in conversations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentState {
    /// It answers.
    Active,
    /// Its owner paused it: its identity stays, but it takes no turns.
    Paused,
    /// Deleted. The row stays because other records name the agent.
    Deleted,
}

impl AgentState {
    /// The column value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Deleted => "deleted",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "active" => Ok(Self::Active),
            "paused" => Ok(Self::Paused),
            "deleted" => Ok(Self::Deleted),
            _ => Err(corrupt(AGENTS, "state")),
        }
    }
}

/// Who sees an agent in the directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Visibility {
    /// Everyone.
    Public,
    /// Only its owner.
    Private,
}

impl Visibility {
    /// The column value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "public" => Ok(Self::Public),
            "private" => Ok(Self::Private),
            _ => Err(corrupt(AGENTS, "visibility")),
        }
    }
}

/// Where a binding is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingState {
    /// Its bot identity is being created.
    Creating,
    /// A Slack app waiting to be installed.
    PendingInstall,
    /// Its bot identity exists and agentd listens as it.
    Active,
    /// No longer used. Its bot user is retired, or owes retirement.
    Disabled,
}

impl BindingState {
    /// The column value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::PendingInstall => "pending_install",
            Self::Active => "active",
            Self::Disabled => "disabled",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "creating" => Ok(Self::Creating),
            "pending_install" => Ok(Self::PendingInstall),
            "active" => Ok(Self::Active),
            "disabled" => Ok(Self::Disabled),
            _ => Err(corrupt(BINDINGS, "state")),
        }
    }
}

/// An agent row. Its `Debug` shows the persona's length, not the persona.
#[derive(Clone, PartialEq, Eq)]
pub struct Agent {
    /// Its id.
    pub id: AgentId,
    /// The member who owns it.
    pub owner: MemberId,
    /// Its name, unique among its owner's agents that aren't deleted.
    pub name: String,
    /// Its persona: the system prompt its sessions append.
    pub persona: String,
    /// Who sees it in the directory.
    pub visibility: Visibility,
    /// Whether it takes part in conversations.
    pub state: AgentState,
    /// When it was created.
    pub created_at: OffsetDateTime,
}

/// What [`Store::create_agent`] needs. Its `Debug` shows the persona's
/// length, not the persona.
#[derive(Clone, Copy)]
pub struct NewAgent<'a> {
    /// The owner.
    pub owner: MemberId,
    /// The name.
    pub name: &'a str,
    /// The persona.
    pub persona: &'a str,
    /// Who sees it in the directory.
    pub visibility: Visibility,
    /// The surface of its first binding.
    pub surface: SurfaceKind,
    /// The team of its first binding.
    pub team: &'a TeamId,
}

impl fmt::Debug for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Agent")
            .field("id", &self.id)
            .field("owner", &self.owner)
            .field("name", &self.name)
            .field("persona_len", &self.persona.len())
            .field("visibility", &self.visibility)
            .field("state", &self.state)
            .field("created_at", &self.created_at)
            .finish()
    }
}

impl fmt::Debug for NewAgent<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewAgent")
            .field("owner", &self.owner)
            .field("name", &self.name)
            .field("persona_len", &self.persona.len())
            .field("visibility", &self.visibility)
            .field("surface", &self.surface)
            .field("team", &self.team)
            .finish()
    }
}

/// A binding row, without its secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentBinding {
    /// Its id.
    pub id: BindingId,
    /// The agent it exposes.
    pub agent: AgentId,
    /// The surface.
    pub surface: SurfaceKind,
    /// The team (Slack workspace or Rocket.Chat server).
    pub team: TeamId,
    /// The bot user's id, once the platform created it.
    pub bot_user: Option<UserId>,
    /// The bot user's username once the platform created it, and before
    /// that the username a creation last asked the platform for.
    pub bot_username: Option<String>,
    /// Where it is in its life.
    pub state: BindingState,
    /// When `state` last changed.
    pub state_changed_at: OffsetDateTime,
    /// When its bot user was deactivated, for a disabled binding.
    pub retired_at: Option<OffsetDateTime>,
}

/// An agent in the directory, from [`Store::directory`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryEntry {
    /// The agent.
    pub agent: Agent,
    /// Its owner's display name.
    pub owner_name: String,
    /// The username of its active bot on the surface and team asked for.
    pub bot_username: Option<String>,
    /// The user id of that bot.
    pub bot_user: Option<UserId>,
}

/// What [`Store::create_agent`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentCreation {
    /// It stored the agent and its `creating` binding.
    Created(Agent, BindingId),
    /// The owner already has an agent of that name that isn't deleted.
    NameTaken,
    /// The owner already has as many agents that aren't deleted as they may.
    LimitReached,
}

/// An active binding agentd listens as, with its token, from
/// [`Store::active_bots`].
#[derive(Debug, Clone)]
pub struct ActiveBot {
    /// The binding.
    pub binding: BindingId,
    /// Its agent.
    pub agent: AgentId,
    /// The bot user's identity.
    pub bot: MemberKey,
    /// The bot user's token.
    pub token: SecretString,
}

/// A disabled binding whose bot user owes retirement, from
/// [`Store::pending_retirements`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRetirement {
    /// The binding.
    pub binding: BindingId,
    /// The bot user to deactivate, or `None` while it is still to be found
    /// by the username the binding noted.
    pub bot_user: Option<UserId>,
}

/// The columns an [`AgentRow`] reads, from `agents a`.
macro_rules! agent_columns {
    () => {
        "a.id, a.owner_id, a.name, a.persona, a.visibility, a.state, a.created_at"
    };
}

#[derive(sqlx::FromRow)]
struct AgentRow {
    id: String,
    owner_id: String,
    name: String,
    persona: String,
    visibility: String,
    state: String,
    created_at: i64,
}

impl AgentRow {
    fn into_agent(self) -> Result<Agent> {
        Ok(Agent {
            id: parse_column(&self.id, AGENTS, "id")?,
            owner: parse_column(&self.owner_id, AGENTS, "owner_id")?,
            name: self.name,
            persona: self.persona,
            visibility: Visibility::parse(&self.visibility)?,
            state: AgentState::parse(&self.state)?,
            created_at: from_unix(self.created_at, AGENTS, "created_at")?,
        })
    }
}

/// The columns a [`BindingRow`] reads.
macro_rules! binding_columns {
    () => {
        "id, agent_id, surface, team_id, bot_user_id, bot_username, state, state_changed_at, \
         retired_at"
    };
}

#[derive(sqlx::FromRow)]
struct BindingRow {
    id: String,
    agent_id: String,
    surface: String,
    team_id: String,
    bot_user_id: Option<String>,
    bot_username: Option<String>,
    state: String,
    state_changed_at: i64,
    retired_at: Option<i64>,
}

impl BindingRow {
    fn into_binding(self) -> Result<AgentBinding> {
        Ok(AgentBinding {
            id: parse_column(&self.id, BINDINGS, "id")?,
            agent: parse_column(&self.agent_id, BINDINGS, "agent_id")?,
            surface: parse_column(&self.surface, BINDINGS, "surface")?,
            team: self.team_id.into(),
            bot_user: self.bot_user_id.map(UserId::from),
            bot_username: self.bot_username,
            state: BindingState::parse(&self.state)?,
            state_changed_at: from_unix(self.state_changed_at, BINDINGS, "state_changed_at")?,
            retired_at: self
                .retired_at
                .map(|at| from_unix(at, BINDINGS, "retired_at"))
                .transpose()?,
        })
    }
}

fn corrupt(table: &'static str, column: &'static str) -> StoreError {
    StoreError::Corrupt { table, column }
}

fn token_aad(binding: &str) -> Aad<'_> {
    Aad {
        table: BINDINGS,
        column: TOKEN,
        key: binding,
    }
}

/// The conditions under which a retirement may be claimed at `now` (bound
/// first) with fewer than `max_attempts` claims so far (bound second).
macro_rules! retirable {
    () => {
        "state = 'disabled' AND (bot_user_id IS NOT NULL OR bot_username IS NOT NULL) \
         AND retired_at IS NULL \
         AND (retire_next_attempt_at IS NULL OR retire_next_attempt_at <= ?) \
         AND retire_attempts < ?"
    };
}

impl Store {
    /// Creates an agent in state `active` at `now`, with one binding on
    /// `new.surface` and `new.team` in state `creating`, in one
    /// transaction, unless the owner already has `max_per_owner` agents that
    /// aren't deleted, or one of that name.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the owner doesn't exist or a query fails.
    pub async fn create_agent(
        &self,
        new: &NewAgent<'_>,
        max_per_owner: u32,
        now: OffsetDateTime,
    ) -> Result<AgentCreation> {
        let agent = Agent {
            id: AgentId::new_v4(),
            owner: new.owner,
            name: new.name.to_owned(),
            persona: new.persona.to_owned(),
            visibility: new.visibility,
            state: AgentState::Active,
            created_at: from_unix(to_unix(now), AGENTS, "created_at")?,
        };
        let binding = BindingId::new_v4();
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let owned: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM agents WHERE owner_id = ? AND state <> 'deleted'",
        )
        .bind(agent.owner.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if owned >= i64::from(max_per_owner) {
            return Ok(AgentCreation::LimitReached);
        }
        let inserted = sqlx::query(
            "INSERT INTO agents (id, owner_id, name, persona, visibility, state, created_at) \
             VALUES (?, ?, ?, ?, ?, 'active', ?)",
        )
        .bind(agent.id.to_string())
        .bind(agent.owner.to_string())
        .bind(&agent.name)
        .bind(&agent.persona)
        .bind(agent.visibility.as_str())
        .bind(to_unix(now))
        .execute(&mut *tx)
        .await;
        match inserted {
            Ok(_) => {}
            Err(sqlx::Error::Database(err)) if err.is_unique_violation() => {
                return Ok(AgentCreation::NameTaken);
            }
            Err(err) => return Err(err.into()),
        }
        sqlx::query(
            "INSERT INTO agent_bindings (id, agent_id, surface, team_id, state, state_changed_at) \
             VALUES (?, ?, ?, ?, 'creating', ?)",
        )
        .bind(binding.to_string())
        .bind(agent.id.to_string())
        .bind(new.surface.as_str())
        .bind(new.team.as_str())
        .bind(to_unix(now))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(AgentCreation::Created(agent, binding))
    }

    /// The agent `id`, deleted or not.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the row doesn't parse.
    pub async fn agent(&self, id: AgentId) -> Result<Option<Agent>> {
        let row: Option<AgentRow> = sqlx::query_as(concat!(
            "SELECT ",
            agent_columns!(),
            " FROM agents a WHERE a.id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(AgentRow::into_agent).transpose()
    }

    /// `owner`'s agent named `name`, unless it is deleted.
    ///
    /// # Errors
    ///
    /// As for [`agent`](Self::agent).
    pub async fn agent_by_name(&self, owner: MemberId, name: &str) -> Result<Option<Agent>> {
        let row: Option<AgentRow> = sqlx::query_as(concat!(
            "SELECT ",
            agent_columns!(),
            " FROM agents a \
             WHERE a.owner_id = ? AND a.name = ? AND a.state <> 'deleted'"
        ))
        .bind(owner.to_string())
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(AgentRow::into_agent).transpose()
    }

    /// Every agent that isn't deleted, or only `owner`'s, ordered by name
    /// and owner, each with its owner's display name and the username of
    /// its active bot on `surface` and `team`, if it has one.
    ///
    /// # Errors
    ///
    /// As for [`agent`](Self::agent).
    pub async fn directory(
        &self,
        surface: SurfaceKind,
        team: &TeamId,
        owner: Option<MemberId>,
    ) -> Result<Vec<DirectoryEntry>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            #[sqlx(flatten)]
            agent: AgentRow,
            owner_name: String,
            bot_username: Option<String>,
            bot_user_id: Option<String>,
        }
        let rows: Vec<Row> = sqlx::query_as(concat!(
            "SELECT ",
            agent_columns!(),
            ", m.display_name AS owner_name, b.bot_username, b.bot_user_id \
             FROM agents a JOIN members m ON m.id = a.owner_id \
             LEFT JOIN agent_bindings b ON b.agent_id = a.id AND b.surface = ? \
             AND b.team_id = ? AND b.state = 'active' \
             WHERE a.state <> 'deleted' AND (? IS NULL OR a.owner_id = ?) \
             ORDER BY a.name, m.display_name, a.id"
        ))
        .bind(surface.as_str())
        .bind(team.as_str())
        .bind(owner.map(|o| o.to_string()))
        .bind(owner.map(|o| o.to_string()))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(DirectoryEntry {
                    agent: row.agent.into_agent()?,
                    owner_name: row.owner_name,
                    bot_username: row.bot_username,
                    bot_user: row.bot_user_id.map(UserId::from),
                })
            })
            .collect()
    }

    /// Replaces `agent`'s persona. Returns false if the agent doesn't exist
    /// or is deleted.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn set_agent_persona(&self, agent: AgentId, persona: &str) -> Result<bool> {
        let result =
            sqlx::query("UPDATE agents SET persona = ? WHERE id = ? AND state <> 'deleted'")
                .bind(persona)
                .bind(agent.to_string())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Pauses an active `agent`, or resumes a paused one. Returns false if
    /// it wasn't in the state that changes: already paused (or active), or
    /// deleted.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn set_agent_paused(&self, agent: AgentId, paused: bool) -> Result<bool> {
        let (from, to) = if paused {
            (AgentState::Active, AgentState::Paused)
        } else {
            (AgentState::Paused, AgentState::Active)
        };
        let result = sqlx::query("UPDATE agents SET state = ? WHERE id = ? AND state = ?")
            .bind(to.as_str())
            .bind(agent.to_string())
            .bind(from.as_str())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes `agent` at `now`: its state becomes `deleted`, and each of
    /// its bindings that isn't disabled yet is disabled and forgets its bot
    /// token and its Slack app's client and signing secrets, in one
    /// transaction. The bindings then owe retirement. Returns
    /// false if the agent doesn't exist or was deleted already.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if a query fails.
    pub async fn delete_agent(&self, agent: AgentId, now: OffsetDateTime) -> Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let deleted = delete_agent(&mut tx, &agent.to_string(), now).await?;
        tx.commit().await?;
        Ok(deleted)
    }

    /// The binding `id`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the row doesn't parse.
    pub async fn binding(&self, id: BindingId) -> Result<Option<AgentBinding>> {
        let row: Option<BindingRow> = sqlx::query_as(concat!(
            "SELECT ",
            binding_columns!(),
            " FROM agent_bindings WHERE id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(BindingRow::into_binding).transpose()
    }

    /// Every binding of `agent`, oldest first.
    ///
    /// # Errors
    ///
    /// As for [`binding`](Self::binding).
    pub async fn bindings_of(&self, agent: AgentId) -> Result<Vec<AgentBinding>> {
        let rows: Vec<BindingRow> = sqlx::query_as(concat!(
            "SELECT ",
            binding_columns!(),
            " FROM agent_bindings WHERE agent_id = ? \
             ORDER BY rowid"
        ))
        .bind(agent.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(BindingRow::into_binding).collect()
    }

    /// Notes the username a creation is about to ask the platform for on
    /// the `creating` binding, which has no bot user yet, so that the bot
    /// user of a creation that dies before
    /// [recording it](Self::set_binding_bot_user) can be found. Returns
    /// false, noting nothing, if the binding isn't `creating` any more or
    /// has a bot user.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn set_binding_bot_username(
        &self,
        binding: BindingId,
        bot_username: &str,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE agent_bindings SET bot_username = ? \
             WHERE id = ? AND state = 'creating' AND bot_user_id IS NULL",
        )
        .bind(bot_username)
        .bind(binding.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Records the bot user the platform created for `binding`, which has
    /// none yet, whatever the binding's state: a creation abandoned
    /// meanwhile leaves a disabled binding whose bot user then owes
    /// retirement. Returns whether the binding is still `creating`, or
    /// `None`, recording nothing, if it doesn't exist or already has a bot
    /// user.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if another binding already has that bot
    /// user on the same surface and team, or the query fails.
    pub async fn set_binding_bot_user(
        &self,
        binding: BindingId,
        bot_user: &UserId,
        bot_username: &str,
    ) -> Result<Option<bool>> {
        Ok(sqlx::query_scalar(
            "UPDATE agent_bindings SET bot_user_id = ?, bot_username = ? \
             WHERE id = ? AND bot_user_id IS NULL RETURNING state = 'creating'",
        )
        .bind(bot_user.as_str())
        .bind(bot_username)
        .bind(binding.to_string())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Stores the bot token of a `creating` binding whose bot user is
    /// recorded and makes it `active` at `now`. Returns false, storing
    /// nothing, if the binding isn't `creating` any more or has no bot
    /// user.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Seal`] if
    /// the token can't be sealed.
    pub async fn activate_binding(
        &self,
        binding: BindingId,
        token: &SecretString,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let key = binding.to_string();
        let sealed = self.seal(token_aad(&key), token)?;
        let result = sqlx::query(
            "UPDATE agent_bindings SET bot_token_enc = ?, state = 'active', state_changed_at = ? \
             WHERE id = ? AND state = 'creating' AND bot_user_id IS NOT NULL",
        )
        .bind(sealed)
        .bind(to_unix(now))
        .bind(&key)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The `creating` bindings on `surface` and `team` whose creation
    /// started at or before `started_before`, oldest first.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if an id doesn't parse.
    pub async fn stale_creations(
        &self,
        surface: SurfaceKind,
        team: &TeamId,
        started_before: OffsetDateTime,
    ) -> Result<Vec<BindingId>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM agent_bindings WHERE surface = ? AND team_id = ? \
             AND state = 'creating' AND state_changed_at <= ? ORDER BY state_changed_at, rowid",
        )
        .bind(surface.as_str())
        .bind(team.as_str())
        .bind(to_unix(started_before))
        .fetch_all(&self.pool)
        .await?;
        ids.iter()
            .map(|id| parse_column(id, BINDINGS, "id"))
            .collect()
    }

    /// Abandons the creation of `binding`, if it is still `creating` and
    /// started at or before `started_before`: at `now` the binding is
    /// disabled and its agent deleted, in one transaction. A bot user
    /// already recorded then owes retirement. Returns false if nothing
    /// changed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if a query fails.
    pub async fn abandon_creation(
        &self,
        binding: BindingId,
        started_before: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let agent: Option<String> = sqlx::query_scalar(
            "SELECT agent_id FROM agent_bindings \
             WHERE id = ? AND state = 'creating' AND state_changed_at <= ?",
        )
        .bind(binding.to_string())
        .bind(to_unix(started_before))
        .fetch_optional(&mut *tx)
        .await?;
        let abandoned = match agent {
            Some(agent) => delete_agent(&mut tx, &agent, now).await?,
            None => false,
        };
        tx.commit().await?;
        Ok(abandoned)
    }

    /// The bot users of [`active_bots`](Self::active_bots), without reading
    /// their tokens.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn active_bot_users(
        &self,
        surface: SurfaceKind,
        team: &TeamId,
    ) -> Result<Vec<UserId>> {
        let users: Vec<String> = sqlx::query_scalar(
            "SELECT b.bot_user_id FROM agent_bindings b JOIN agents a ON a.id = b.agent_id \
             WHERE b.surface = ? AND b.team_id = ? AND b.state = 'active' \
             AND a.state IN ('active', 'paused') \
             AND b.bot_user_id IS NOT NULL AND b.bot_token_enc IS NOT NULL \
             ORDER BY b.state_changed_at, b.rowid",
        )
        .bind(surface.as_str())
        .bind(team.as_str())
        .fetch_all(&self.pool)
        .await?;
        Ok(users.into_iter().map(UserId::from).collect())
    }

    /// Every `active` binding on `surface` and `team` whose agent is active
    /// or paused, with its bot token, oldest first: the bots agentd listens
    /// as. A row whose ids don't parse or whose token doesn't decrypt is
    /// logged and left out, so one bad row doesn't silence every bot.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn active_bots(&self, surface: SurfaceKind, team: &TeamId) -> Result<Vec<ActiveBot>> {
        let rows: Vec<(String, String, String, Vec<u8>)> = sqlx::query_as(
            "SELECT b.id, b.agent_id, b.bot_user_id, b.bot_token_enc \
             FROM agent_bindings b JOIN agents a ON a.id = b.agent_id \
             WHERE b.surface = ? AND b.team_id = ? AND b.state = 'active' \
             AND a.state IN ('active', 'paused') \
             AND b.bot_user_id IS NOT NULL AND b.bot_token_enc IS NOT NULL \
             ORDER BY b.state_changed_at, b.rowid",
        )
        .bind(surface.as_str())
        .bind(team.as_str())
        .fetch_all(&self.pool)
        .await?;
        let read = |(binding, agent, user, token): (String, String, String, Vec<u8>)| {
            Ok::<_, StoreError>(ActiveBot {
                binding: parse_column(&binding, BINDINGS, "id")?,
                agent: parse_column(&agent, BINDINGS, "agent_id")?,
                bot: MemberKey {
                    surface,
                    team: team.clone(),
                    user: user.into(),
                },
                token: self.open_sealed(token_aad(&binding), &token)?,
            })
        };
        Ok(rows
            .into_iter()
            .filter_map(|row| {
                let binding = row.0.clone();
                read(row)
                    .inspect_err(|err| {
                        tracing::error!(%binding, error = %err, "skipping an active binding that doesn't read");
                    })
                    .ok()
            })
            .collect())
    }

    /// The agent whose `active` binding is the bot user `bot`, with the
    /// binding's id, whatever the agent's state.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn agent_for_bot(&self, bot: &MemberKey) -> Result<Option<(Agent, BindingId)>> {
        #[derive(sqlx::FromRow)]
        struct Row {
            #[sqlx(flatten)]
            agent: AgentRow,
            binding: String,
        }
        let row: Option<Row> = sqlx::query_as(concat!(
            "SELECT ",
            agent_columns!(),
            ", b.id AS binding \
             FROM agent_bindings b JOIN agents a ON a.id = b.agent_id \
             WHERE b.surface = ? AND b.team_id = ? AND b.bot_user_id = ? AND b.state = 'active'"
        ))
        .bind(bot.surface.as_str())
        .bind(bot.team.as_str())
        .bind(bot.user.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|row| {
            Ok((
                row.agent.into_agent()?,
                parse_column(&row.binding, BINDINGS, "id")?,
            ))
        })
        .transpose()
    }

    /// The agent whose binding, in any state, has the bot user `bot`: every
    /// bot user agentd ever created for an agent, so a paused or deleted
    /// agent's bot is never taken for a person.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the row doesn't parse.
    pub async fn agent_of_bot_user(&self, bot: &MemberKey) -> Result<Option<AgentId>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT agent_id FROM agent_bindings              WHERE surface = ? AND team_id = ? AND bot_user_id = ?              ORDER BY state = 'active' DESC, state_changed_at DESC LIMIT 1",
        )
        .bind(bot.surface.as_str())
        .bind(bot.team.as_str())
        .bind(bot.user.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(agent,)| parse_column(&agent, BINDINGS, "agent_id"))
            .transpose()
    }

    /// The agent of `binding`, if the binding is `active`.
    ///
    /// # Errors
    ///
    /// As for [`agent_for_bot`](Self::agent_for_bot).
    pub async fn agent_for_binding(&self, binding: BindingId) -> Result<Option<Agent>> {
        let row: Option<AgentRow> = sqlx::query_as(concat!(
            "SELECT ",
            agent_columns!(),
            " FROM agent_bindings b JOIN agents a ON a.id = b.agent_id \
             WHERE b.id = ? AND b.state = 'active'"
        ))
        .bind(binding.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(AgentRow::into_agent).transpose()
    }

    /// The bot token of `binding`, if the binding is `active`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Seal`] if
    /// the token doesn't decrypt.
    pub async fn bot_token(&self, binding: BindingId) -> Result<Option<SecretString>> {
        let key = binding.to_string();
        let sealed: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT bot_token_enc FROM agent_bindings \
             WHERE id = ? AND state = 'active' AND bot_token_enc IS NOT NULL",
        )
        .bind(&key)
        .fetch_optional(&self.pool)
        .await?;
        sealed
            .map(|sealed| self.open_sealed(token_aad(&key), &sealed))
            .transpose()
    }

    /// Every retirement on `surface` and `team` that may be claimed at
    /// `now`, oldest first: the binding is disabled, has a bot user, or a
    /// noted username, that isn't retired, no lease or backoff runs past
    /// `now`, and it was claimed fewer than `max_attempts` times.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if an id doesn't parse.
    pub async fn pending_retirements(
        &self,
        surface: SurfaceKind,
        team: &TeamId,
        now: OffsetDateTime,
        max_attempts: u32,
    ) -> Result<Vec<PendingRetirement>> {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(concat!(
            "SELECT id, bot_user_id FROM agent_bindings WHERE surface = ? AND team_id = ? AND ",
            retirable!(),
            " ORDER BY state_changed_at, rowid"
        ))
        .bind(surface.as_str())
        .bind(team.as_str())
        .bind(to_unix(now))
        .bind(i64::from(max_attempts))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(binding, user)| {
                Ok(PendingRetirement {
                    binding: parse_column(&binding, BINDINGS, "id")?,
                    bot_user: user.map(UserId::from),
                })
            })
            .collect()
    }

    /// Claims the retirement of `binding` at `now`, with a lease until
    /// `lease_until`, if it may be claimed (as for
    /// [`pending_retirements`](Self::pending_retirements)). Returns which
    /// attempt this is, counting from 1, only for the one call that claims
    /// it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the attempt count is negative.
    pub async fn claim_retirement(
        &self,
        binding: BindingId,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
        max_attempts: u32,
    ) -> Result<Option<u32>> {
        let attempt: Option<i64> = sqlx::query_scalar(concat!(
            "UPDATE agent_bindings \
             SET retire_attempts = retire_attempts + 1, retire_next_attempt_at = ? \
             WHERE id = ? AND ",
            retirable!(),
            " RETURNING retire_attempts"
        ))
        .bind(to_unix(lease_until))
        .bind(binding.to_string())
        .bind(to_unix(now))
        .bind(i64::from(max_attempts))
        .fetch_optional(&self.pool)
        .await?;
        attempt
            .map(|attempt| u32::try_from(attempt).map_err(|_| corrupt(BINDINGS, "retire_attempts")))
            .transpose()
    }

    /// Forgets the username the disabled `binding` noted without recording
    /// a bot user, once the user of that name is known not to be its bot
    /// user: it then owes no retirement. Returns false if nothing changed.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn forget_binding_bot_username(&self, binding: BindingId) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE agent_bindings SET bot_username = NULL \
             WHERE id = ? AND state = 'disabled' AND bot_user_id IS NULL",
        )
        .bind(binding.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Records that the bot user of the disabled `binding` was deactivated
    /// at `now`. Returns false if it was recorded already.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn mark_retired(&self, binding: BindingId, now: OffsetDateTime) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE agent_bindings SET retired_at = ?, retire_next_attempt_at = NULL \
             WHERE id = ? AND state = 'disabled' AND retired_at IS NULL",
        )
        .bind(to_unix(now))
        .bind(binding.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Ends a claim on the retirement of `binding` that failed: it may be
    /// claimed again from `retry_at`, if attempts are left. Returns false if
    /// the retirement isn't owed any more.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn defer_retirement(
        &self,
        binding: BindingId,
        retry_at: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE agent_bindings SET retire_next_attempt_at = ? \
             WHERE id = ? AND state = 'disabled' AND retired_at IS NULL",
        )
        .bind(to_unix(retry_at))
        .bind(binding.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

/// Deletes the agent `agent` and disables its bindings at `now`, inside
/// the caller's transaction. Returns false if it was deleted already.
async fn delete_agent(
    conn: &mut SqliteConnection,
    agent: &str,
    now: OffsetDateTime,
) -> Result<bool> {
    let result =
        sqlx::query("UPDATE agents SET state = 'deleted' WHERE id = ? AND state <> 'deleted'")
            .bind(agent)
            .execute(&mut *conn)
            .await?;
    if result.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "UPDATE agent_bindings SET state = 'disabled', state_changed_at = ?, bot_token_enc = NULL, \
         client_secret_enc = NULL, signing_secret_enc = NULL \
         WHERE agent_id = ? AND state <> 'disabled'",
    )
    .bind(to_unix(now))
    .bind(agent)
    .execute(&mut *conn)
    .await?;
    Ok(true)
}

#[cfg(test)]
mod tests;

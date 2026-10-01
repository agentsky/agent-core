//! `consents`: requests for private tasks on an agent owner's private
//! resources, their owners' decisions, their consent cards, and the work a
//! decided consent owes.
//!
//! A consent is created by `agentctl private`, `pending` unless the agent's
//! owner asked for it at hop 0, in which case it is `approved` at once
//! ([`Approval::Asked`]). Each agent and requester may have only so many
//! consents unfinished ([`OpenLimits`]). Only a pending consent can be
//! [decided](Store::decide_consent) or [expired](Store::expire_consents),
//! each in one conditional `UPDATE`, so a decision and an expiry, or two
//! decisions, never both land.
//!
//! The consent card and the work a decided consent owes are sent or done
//! at least once, the way the relink notices are: a claim is a conditional
//! `UPDATE` that counts an attempt and sets a lease, after which the card
//! or the work is claimable again if its claimer died. The card is retried
//! until the consent expires. The work's claimer
//! [renews](Store::renew_consent_work) the lease while a task runs, only
//! the claim of the latest attempt can [finish](Store::finish_consent) it,
//! and the claims that [failed](Store::fail_consent_work) are counted.

use core_types::{
    AgentId, ConsentId, ConvRef, ConversationId, Hop, MemberId, MemberKey, MessageId, MsgRef,
    Requester, SessionId, SurfaceKind, TeamId, ThreadKey,
};
use time::OffsetDateTime;

use crate::{Result, Store, StoreError, from_unix, parse_column, to_unix};

const TABLE: &str = "consents";

/// The columns every query reads, in [`Row`]'s order.
macro_rules! columns {
    () => {
        "id, agent_id, requester_member, requester_key, hop, task_text, attachments_json, \
         state, reply_surface, reply_team_id, reply_conversation, reply_thread_root, \
         origin_session_id, private_session_id, created_at, expires_at, decided_by, \
         decided_at, card_conversation, card_message, card_closed_at, work_attempts, \
         finished_at, approval, work_failures"
    };
}

/// The owner's decision on a consent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConsentState {
    /// Waiting for the owner to answer the consent card.
    Pending,
    /// The owner approved it, or asked for the task themselves.
    Approved,
    /// The owner declined it.
    Declined,
    /// Nobody answered the card before it expired.
    Expired,
}

impl ConsentState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Declined => "declined",
            Self::Expired => "expired",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "declined" => Ok(Self::Declined),
            "expired" => Ok(Self::Expired),
            _ => Err(corrupt("state")),
        }
    }
}

/// How an approved consent was approved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Approval {
    /// At once, because the agent's owner asked for it at hop 0.
    Asked,
    /// By the owner, on the consent card.
    Card,
}

impl Approval {
    fn as_str(self) -> &'static str {
        match self {
            Self::Asked => "asked",
            Self::Card => "card",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "asked" => Ok(Self::Asked),
            "card" => Ok(Self::Card),
            _ => Err(corrupt("approval")),
        }
    }
}

/// How many consents may be unfinished at once, for
/// [`Store::create_consent`]: asked for and not yet decided, or decided and
/// owing their work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenLimits {
    /// The most for one agent and one requester's identity.
    pub per_requester: u32,
    /// The most for one agent.
    pub per_agent: u32,
}

/// A consent to record, for [`Store::create_consent`].
#[derive(Debug, Clone, Copy)]
pub struct NewConsent<'a> {
    /// The consent's id.
    pub id: ConsentId,
    /// The agent whose owner's resources the task runs on.
    pub agent: AgentId,
    /// Who asked, through the turn that ran `agentctl private`.
    pub requester: &'a Requester,
    /// The hop of that turn.
    pub hop: Hop,
    /// The task text, exactly as the owner is shown it.
    pub task: &'a str,
    /// The staged attachments, as JSON agentd owns.
    pub attachments_json: &'a str,
    /// Where the result goes: the thread of the turn that asked.
    pub thread: &'a ThreadKey,
    /// The session of the turn that asked.
    pub origin_session: SessionId,
    /// When an unanswered card expires.
    pub expires_at: OffsetDateTime,
    /// `Some` with the requester's identity when the agent's owner asked
    /// for the task at hop 0: the consent is approved at once, by them
    /// ([`Approval::Asked`]). The store refuses it at any other hop.
    pub approved_by_owner: Option<&'a MemberKey>,
}

impl NewConsent<'_> {
    /// The consent [`Store::create_consent`] would record at `now`, as a
    /// card would show it before it is asked for.
    pub fn draft(&self, now: OffsetDateTime) -> Consent {
        let approved = self.approved_by_owner.is_some();
        Consent {
            id: self.id,
            agent: self.agent,
            requester: self.requester.clone(),
            hop: self.hop,
            task: self.task.to_owned(),
            attachments_json: self.attachments_json.to_owned(),
            state: if approved {
                ConsentState::Approved
            } else {
                ConsentState::Pending
            },
            approval: approved.then_some(Approval::Asked),
            thread: self.thread.clone(),
            origin_session: self.origin_session,
            private_session: None,
            created_at: now,
            expires_at: self.expires_at,
            decided_by: self.approved_by_owner.cloned(),
            decided_at: approved.then_some(now),
            card: None,
            card_closed_at: None,
            work_attempts: 0,
            work_failures: 0,
            finished_at: None,
        }
    }
}

/// A `consents` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consent {
    /// The consent's id.
    pub id: ConsentId,
    /// The agent whose owner's resources the task runs on.
    pub agent: AgentId,
    /// Who asked, and pays for what the task leads to.
    pub requester: Requester,
    /// The hop of the turn that asked.
    pub hop: Hop,
    /// The task text.
    pub task: String,
    /// The staged attachments, as JSON agentd owns.
    pub attachments_json: String,
    /// The owner's decision.
    pub state: ConsentState,
    /// How it was approved, once it is.
    pub approval: Option<Approval>,
    /// Where the result goes.
    pub thread: ThreadKey,
    /// The session of the turn that asked.
    pub origin_session: SessionId,
    /// The session the task last ran in.
    pub private_session: Option<SessionId>,
    /// When it was asked for.
    pub created_at: OffsetDateTime,
    /// When an unanswered card expires.
    pub expires_at: OffsetDateTime,
    /// Who decided, unless it expired or is pending.
    pub decided_by: Option<MemberKey>,
    /// When it was decided or expired.
    pub decided_at: Option<OffsetDateTime>,
    /// Where the consent card is, once posted.
    pub card: Option<MsgRef>,
    /// When the card was updated with the outcome.
    pub card_closed_at: Option<OffsetDateTime>,
    /// How many times its work was claimed.
    pub work_attempts: u32,
    /// How many of those claims failed.
    pub work_failures: u32,
    /// When its work was done.
    pub finished_at: Option<OffsetDateTime>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: String,
    agent_id: String,
    requester_member: Option<String>,
    requester_key: String,
    hop: i64,
    task_text: String,
    attachments_json: String,
    state: String,
    reply_surface: String,
    reply_team_id: String,
    reply_conversation: String,
    reply_thread_root: String,
    origin_session_id: String,
    private_session_id: Option<String>,
    created_at: i64,
    expires_at: i64,
    decided_by: Option<String>,
    decided_at: Option<i64>,
    card_conversation: Option<String>,
    card_message: Option<String>,
    card_closed_at: Option<i64>,
    work_attempts: i64,
    finished_at: Option<i64>,
    approval: Option<String>,
    work_failures: i64,
}

fn corrupt(column: &'static str) -> StoreError {
    StoreError::Corrupt {
        table: TABLE,
        column,
    }
}

fn optional_time(value: Option<i64>, column: &'static str) -> Result<Option<OffsetDateTime>> {
    value.map(|at| from_unix(at, TABLE, column)).transpose()
}

impl Row {
    fn into_consent(self) -> Result<Consent> {
        let conv = ConvRef {
            surface: parse_column::<SurfaceKind>(&self.reply_surface, TABLE, "reply_surface")?,
            team: TeamId::new(self.reply_team_id),
            conversation: ConversationId::new(self.reply_conversation),
        };
        let card = match (self.card_conversation, self.card_message) {
            (Some(conversation), Some(message)) => {
                let key = parse_column::<ConvRef>(&conversation, TABLE, "card_conversation")?;
                Some(MsgRef {
                    conv: key,
                    id: MessageId::new(message),
                })
            }
            _ => None,
        };
        Ok(Consent {
            id: parse_column(&self.id, TABLE, "id")?,
            agent: parse_column(&self.agent_id, TABLE, "agent_id")?,
            requester: Requester {
                member: self
                    .requester_member
                    .map(|member| parse_column::<MemberId>(&member, TABLE, "requester_member"))
                    .transpose()?,
                key: parse_column(&self.requester_key, TABLE, "requester_key")?,
            },
            hop: Hop(u8::try_from(self.hop).map_err(|_| corrupt("hop"))?),
            task: self.task_text,
            attachments_json: self.attachments_json,
            state: ConsentState::parse(&self.state)?,
            approval: self.approval.as_deref().map(Approval::parse).transpose()?,
            thread: ThreadKey {
                conv,
                root: (!self.reply_thread_root.is_empty())
                    .then(|| MessageId::new(self.reply_thread_root)),
            },
            origin_session: parse_column(&self.origin_session_id, TABLE, "origin_session_id")?,
            private_session: self
                .private_session_id
                .map(|session| parse_column(&session, TABLE, "private_session_id"))
                .transpose()?,
            created_at: from_unix(self.created_at, TABLE, "created_at")?,
            expires_at: from_unix(self.expires_at, TABLE, "expires_at")?,
            decided_by: self
                .decided_by
                .map(|key| parse_column(&key, TABLE, "decided_by"))
                .transpose()?,
            decided_at: optional_time(self.decided_at, "decided_at")?,
            card,
            card_closed_at: optional_time(self.card_closed_at, "card_closed_at")?,
            work_attempts: u32::try_from(self.work_attempts)
                .map_err(|_| corrupt("work_attempts"))?,
            work_failures: u32::try_from(self.work_failures)
                .map_err(|_| corrupt("work_failures"))?,
            finished_at: optional_time(self.finished_at, "finished_at")?,
        })
    }
}

fn consents(rows: Vec<Row>) -> Result<Vec<Consent>> {
    rows.into_iter().map(Row::into_consent).collect()
}

fn attempt_count(value: Option<i64>, column: &'static str) -> Result<Option<u32>> {
    value
        .map(|attempt| u32::try_from(attempt).map_err(|_| corrupt(column)))
        .transpose()
}

/// The conditions under which a card is owed and may be claimed at `now`
/// (bound once).
macro_rules! card_claimable {
    () => {
        "state = 'pending' AND finished_at IS NULL AND card_message IS NULL \
         AND (card_next_attempt_at IS NULL OR card_next_attempt_at <= ?)"
    };
}

/// The conditions under which a consent's work is owed and may be claimed
/// at `now` (bound once).
macro_rules! work_claimable {
    () => {
        "state <> 'pending' AND finished_at IS NULL \
         AND (work_next_attempt_at IS NULL OR work_next_attempt_at <= ?)"
    };
}

impl Store {
    /// Records `consent` at `now`: `pending`, or `approved` by the owner
    /// ([`Approval::Asked`]) when [`NewConsent::approved_by_owner`] says
    /// the owner asked at hop 0. Returns `None`, recording nothing, when
    /// the agent, or the agent and the requester's identity, already have
    /// as many unfinished consents as `limits` allow. The count and the
    /// insert are one `BEGIN IMMEDIATE` transaction, so concurrent requests
    /// never pass the limits together.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if a query fails, as for an id that is
    /// taken, an agent that doesn't exist, or an owner's approval at a hop
    /// other than 0.
    pub async fn create_consent(
        &self,
        consent: &NewConsent<'_>,
        limits: OpenLimits,
        now: OffsetDateTime,
    ) -> Result<Option<Consent>> {
        let (state, approval, decided_by, decided_at) = match consent.approved_by_owner {
            Some(owner) => (
                ConsentState::Approved,
                Some(Approval::Asked.as_str()),
                Some(owner.to_string()),
                Some(to_unix(now)),
            ),
            None => (ConsentState::Pending, None, None, None),
        };
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let (per_agent, per_requester): (i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(requester_key = ?), 0) FROM consents \
             WHERE agent_id = ? AND finished_at IS NULL",
        )
        .bind(consent.requester.key.to_string())
        .bind(consent.agent.to_string())
        .fetch_one(&mut *tx)
        .await?;
        if per_agent >= i64::from(limits.per_agent)
            || per_requester >= i64::from(limits.per_requester)
        {
            return Ok(None);
        }
        let row: Row = sqlx::query_as(concat!(
            "INSERT INTO consents (id, agent_id, requester_member, requester_key, hop, \
             task_text, attachments_json, state, approval, reply_surface, reply_team_id, \
             reply_conversation, reply_thread_root, origin_session_id, created_at, expires_at, \
             decided_by, decided_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING ",
            columns!()
        ))
        .bind(consent.id.to_string())
        .bind(consent.agent.to_string())
        .bind(consent.requester.member.map(|member| member.to_string()))
        .bind(consent.requester.key.to_string())
        .bind(i64::from(consent.hop.0))
        .bind(consent.task)
        .bind(consent.attachments_json)
        .bind(state.as_str())
        .bind(approval)
        .bind(consent.thread.conv.surface.as_str())
        .bind(consent.thread.conv.team.as_str())
        .bind(consent.thread.conv.conversation.as_str())
        .bind(consent.thread.root.as_ref().map_or("", MessageId::as_str))
        .bind(consent.origin_session.to_string())
        .bind(to_unix(now))
        .bind(to_unix(consent.expires_at))
        .bind(decided_by)
        .bind(decided_at)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        row.into_consent().map(Some)
    }

    /// The consent `id`, if there is one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the row doesn't parse.
    pub async fn consent(&self, id: ConsentId) -> Result<Option<Consent>> {
        let row: Option<Row> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM consents WHERE id = ?"
        ))
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        row.map(Row::into_consent).transpose()
    }

    /// Records `by`'s decision on the pending consent `id` at `now`:
    /// approved ([`Approval::Card`]), or declined. Returns the decided consent, or `None` when
    /// it isn't pending any more or expired before `now`: a decision never
    /// overrides another, or an expiry.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the row doesn't parse.
    pub async fn decide_consent(
        &self,
        id: ConsentId,
        approve: bool,
        by: &MemberKey,
        now: OffsetDateTime,
    ) -> Result<Option<Consent>> {
        let (state, approval) = if approve {
            (ConsentState::Approved, Some(Approval::Card.as_str()))
        } else {
            (ConsentState::Declined, None)
        };
        let row: Option<Row> = sqlx::query_as(concat!(
            "UPDATE consents SET state = ?, approval = ?, decided_by = ?, decided_at = ? \
             WHERE id = ? AND state = 'pending' AND expires_at > ? RETURNING ",
            columns!()
        ))
        .bind(state.as_str())
        .bind(approval)
        .bind(by.to_string())
        .bind(to_unix(now))
        .bind(id.to_string())
        .bind(to_unix(now))
        .fetch_optional(&self.pool)
        .await?;
        row.map(Row::into_consent).transpose()
    }

    /// Marks `expired` every pending consent whose card expired by `now`,
    /// or whose agent was deleted, and returns them.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn expire_consents(&self, now: OffsetDateTime) -> Result<Vec<Consent>> {
        let rows: Vec<Row> = sqlx::query_as(concat!(
            "UPDATE consents SET state = 'expired', decided_at = ? \
             WHERE state = 'pending' AND finished_at IS NULL AND (expires_at <= ? \
             OR agent_id IN (SELECT id FROM agents WHERE state = 'deleted')) RETURNING ",
            columns!()
        ))
        .bind(to_unix(now))
        .bind(to_unix(now))
        .fetch_all(&self.pool)
        .await?;
        consents(rows)
    }

    /// Marks the pending consent `id` expired at `now`, before its time,
    /// as when its card can't reach the owner. False if it isn't pending.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn expire_consent(&self, id: ConsentId, now: OffsetDateTime) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE consents SET state = 'expired', decided_at = ? \
             WHERE id = ? AND state = 'pending'",
        )
        .bind(to_unix(now))
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The pending consents whose card may be claimed at `now`, oldest
    /// first: not posted yet, and no lease or backoff running past `now`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn consent_cards_owed(&self, now: OffsetDateTime) -> Result<Vec<Consent>> {
        let rows: Vec<Row> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM consents WHERE ",
            card_claimable!(),
            " ORDER BY created_at, id"
        ))
        .bind(to_unix(now))
        .fetch_all(&self.pool)
        .await?;
        consents(rows)
    }

    /// Claims the card of consent `id` at `now`, with a lease until
    /// `lease_until`, if it may be claimed as for
    /// [`consent_cards_owed`](Self::consent_cards_owed). Returns which
    /// attempt this is, from 1, only for the one call that claims it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the count is negative.
    pub async fn claim_consent_card(
        &self,
        id: ConsentId,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
    ) -> Result<Option<u32>> {
        let claimed: Option<i64> = sqlx::query_scalar(concat!(
            "UPDATE consents SET card_attempts = card_attempts + 1, card_next_attempt_at = ? \
             WHERE id = ? AND ",
            card_claimable!(),
            " RETURNING card_attempts"
        ))
        .bind(to_unix(lease_until))
        .bind(id.to_string())
        .bind(to_unix(now))
        .fetch_optional(&self.pool)
        .await?;
        attempt_count(claimed, "card_attempts")
    }

    /// Records where consent `id`'s card was posted, which ends its claim.
    /// False if a card was recorded already.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn record_consent_card(&self, id: ConsentId, card: &MsgRef) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE consents SET card_conversation = ?, card_message = ?, \
             card_next_attempt_at = NULL WHERE id = ? AND card_message IS NULL",
        )
        .bind(card.conv.to_string())
        .bind(card.id.as_str())
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Ends claim `attempt` on consent `id`'s card, which couldn't be sent:
    /// it may be claimed again from `retry_at`. False if the card was
    /// posted, or a later claim took it over.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn defer_consent_card(
        &self,
        id: ConsentId,
        attempt: u32,
        retry_at: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE consents SET card_next_attempt_at = ? \
             WHERE id = ? AND card_attempts = ? AND card_message IS NULL",
        )
        .bind(to_unix(retry_at))
        .bind(id.to_string())
        .bind(i64::from(attempt))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The decided consents whose posted card doesn't show the outcome
    /// yet.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn consent_cards_to_close(&self) -> Result<Vec<Consent>> {
        let rows: Vec<Row> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM consents WHERE state <> 'pending' AND card_message IS NOT NULL \
             AND card_closed_at IS NULL ORDER BY decided_at, id"
        ))
        .fetch_all(&self.pool)
        .await?;
        consents(rows)
    }

    /// Claims updating consent `id`'s card with its outcome, at `now`.
    /// True only for the one call that claims it: the update is tried once.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn claim_consent_card_close(
        &self,
        id: ConsentId,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE consents SET card_closed_at = ? WHERE id = ? AND state <> 'pending' \
             AND card_message IS NOT NULL AND card_closed_at IS NULL",
        )
        .bind(to_unix(now))
        .bind(id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The soonest time after `now` at which something a consent owes
    /// falls due: a pending consent's expiry, a card's or a claim's retry
    /// or lease. `None` when nothing is due after `now`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the time doesn't parse.
    pub async fn next_consent_deadline(
        &self,
        now: OffsetDateTime,
    ) -> Result<Option<OffsetDateTime>> {
        let next: Option<i64> = sqlx::query_scalar(
            "SELECT MIN(due) FROM ( \
             SELECT expires_at AS due FROM consents WHERE state = 'pending' \
             AND finished_at IS NULL \
             UNION ALL SELECT card_next_attempt_at FROM consents WHERE state = 'pending' \
             AND finished_at IS NULL AND card_message IS NULL \
             UNION ALL SELECT work_next_attempt_at FROM consents WHERE state <> 'pending' \
             AND finished_at IS NULL) WHERE due > ?",
        )
        .bind(to_unix(now))
        .fetch_one(&self.pool)
        .await?;
        next.map(|at| from_unix(at, TABLE, "next deadline"))
            .transpose()
    }

    /// The decided consents whose work may be claimed at `now`, oldest
    /// decision first: not finished, and no lease or backoff running past
    /// `now`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn consent_work_owed(&self, now: OffsetDateTime) -> Result<Vec<Consent>> {
        let rows: Vec<Row> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM consents WHERE ",
            work_claimable!(),
            " ORDER BY decided_at, id"
        ))
        .bind(to_unix(now))
        .fetch_all(&self.pool)
        .await?;
        consents(rows)
    }

    /// Claims consent `id`'s work at `now`, with a lease until
    /// `lease_until`, if it may be claimed as for
    /// [`consent_work_owed`](Self::consent_work_owed). Returns which
    /// attempt this is, from 1, only for the one call that claims it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the count is negative.
    pub async fn claim_consent_work(
        &self,
        id: ConsentId,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
    ) -> Result<Option<u32>> {
        let claimed: Option<i64> = sqlx::query_scalar(concat!(
            "UPDATE consents SET work_attempts = work_attempts + 1, work_next_attempt_at = ? \
             WHERE id = ? AND ",
            work_claimable!(),
            " RETURNING work_attempts"
        ))
        .bind(to_unix(lease_until))
        .bind(id.to_string())
        .bind(to_unix(now))
        .fetch_optional(&self.pool)
        .await?;
        attempt_count(claimed, "work_attempts")
    }

    /// Extends the lease of claim `attempt` on consent `id`'s work to
    /// `lease_until`. False if the work finished, or a later claim took it
    /// over.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn renew_consent_work(
        &self,
        id: ConsentId,
        attempt: u32,
        lease_until: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE consents SET work_next_attempt_at = ? \
             WHERE id = ? AND work_attempts = ? AND finished_at IS NULL",
        )
        .bind(to_unix(lease_until))
        .bind(id.to_string())
        .bind(i64::from(attempt))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Ends claim `attempt` on consent `id`'s work, which failed: it may be
    /// claimed again from `retry_at`. Returns how many claims failed so
    /// far, or `None` if the work finished or a later claim took it over.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the count is negative.
    pub async fn fail_consent_work(
        &self,
        id: ConsentId,
        attempt: u32,
        retry_at: OffsetDateTime,
    ) -> Result<Option<u32>> {
        let failures: Option<i64> = sqlx::query_scalar(
            "UPDATE consents SET work_failures = work_failures + 1, work_next_attempt_at = ? \
             WHERE id = ? AND work_attempts = ? AND finished_at IS NULL RETURNING work_failures",
        )
        .bind(to_unix(retry_at))
        .bind(id.to_string())
        .bind(i64::from(attempt))
        .fetch_optional(&self.pool)
        .await?;
        attempt_count(failures, "work_failures")
    }

    /// Ends claim `attempt` on consent `id`'s work, which was cut short
    /// without failing, as by a shutdown: it may be claimed again from
    /// `now`, and counts as no failure. False if the work finished or a
    /// later claim took it over.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn release_consent_work(
        &self,
        id: ConsentId,
        attempt: u32,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE consents SET work_next_attempt_at = ? \
             WHERE id = ? AND work_attempts = ? AND finished_at IS NULL",
        )
        .bind(to_unix(now))
        .bind(id.to_string())
        .bind(i64::from(attempt))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Records that claim `attempt` on consent `id`'s work runs the task in
    /// `session`. False if a later claim took the work over, or it
    /// finished.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn set_consent_session(
        &self,
        id: ConsentId,
        attempt: u32,
        session: SessionId,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE consents SET private_session_id = ? \
             WHERE id = ? AND work_attempts = ? AND finished_at IS NULL",
        )
        .bind(session.to_string())
        .bind(id.to_string())
        .bind(i64::from(attempt))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Records at `now` that claim `attempt` on consent `id`'s work did it.
    /// False if a later claim took the work over, or it finished already.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn finish_consent(
        &self,
        id: ConsentId,
        attempt: u32,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE consents SET finished_at = ?, work_next_attempt_at = NULL \
             WHERE id = ? AND work_attempts = ? AND finished_at IS NULL",
        )
        .bind(to_unix(now))
        .bind(id.to_string())
        .bind(i64::from(attempt))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use core_types::UserId;

    use super::*;
    use crate::test_util::*;

    fn requester(member: Option<MemberId>) -> Requester {
        Requester {
            member,
            key: member_key("bob"),
        }
    }

    fn thread(root: Option<&str>) -> ThreadKey {
        ThreadKey {
            conv: ConvRef {
                surface: SurfaceKind::RocketChat,
                team: TeamId::new("chat.example.org"),
                conversation: ConversationId::new("GENERAL"),
            },
            root: root.map(MessageId::new),
        }
    }

    struct Fixture {
        store: Store,
        agent: AgentId,
        owner: MemberKey,
    }

    async fn fixture() -> Fixture {
        let store = memory_store().await;
        let owner = member_key("alice");
        let member = store.ensure_member(&owner, "alice", at(1)).await.unwrap();
        let agent = agent(&store, member, "helper").await;
        Fixture {
            store,
            agent,
            owner,
        }
    }

    const LIMITS: OpenLimits = OpenLimits {
        per_requester: 100,
        per_agent: 100,
    };

    impl Fixture {
        async fn create(&self, owner_asked: bool, root: Option<&str>, expires: i64) -> Consent {
            self.try_create(&requester(None), owner_asked, root, expires, LIMITS)
                .await
                .unwrap()
                .unwrap()
        }

        async fn try_create(
            &self,
            requester: &Requester,
            owner_asked: bool,
            root: Option<&str>,
            expires: i64,
            limits: OpenLimits,
        ) -> Result<Option<Consent>> {
            let thread = thread(root);
            self.store
                .create_consent(
                    &NewConsent {
                        id: ConsentId::new_v4(),
                        agent: self.agent,
                        requester,
                        hop: if owner_asked { Hop::ZERO } else { Hop(2) },
                        task: "Summarize the repo.",
                        attachments_json: "[]",
                        thread: &thread,
                        origin_session: SessionId::new_v4(),
                        expires_at: at(expires),
                        approved_by_owner: owner_asked.then_some(&self.owner),
                    },
                    limits,
                    at(100),
                )
                .await
        }
    }

    #[tokio::test]
    async fn a_consent_round_trips_and_an_owners_is_approved_at_once() {
        let fx = fixture().await;
        let pending = fx.create(false, Some("1.1"), 1_000).await;
        assert_eq!(pending.state, ConsentState::Pending);
        assert_eq!(pending.hop, Hop(2));
        assert_eq!(pending.task, "Summarize the repo.");
        assert_eq!(pending.thread, thread(Some("1.1")));
        assert_eq!(pending.requester, requester(None));
        assert_eq!(pending.decided_at, None);
        assert_eq!(pending.card, None);
        assert_eq!(pending.approval, None);
        assert_eq!(fx.store.consent(pending.id).await.unwrap(), Some(pending));

        let owners = fx.create(true, None, 1_000).await;
        let requester = requester(None);
        let thread = thread(None);
        let new = NewConsent {
            id: owners.id,
            agent: fx.agent,
            requester: &requester,
            hop: Hop::ZERO,
            task: "Summarize the repo.",
            attachments_json: "[]",
            thread: &thread,
            origin_session: owners.origin_session,
            expires_at: at(1_000),
            approved_by_owner: Some(&fx.owner),
        };
        assert_eq!(new.draft(at(100)), owners, "a draft is the row to be");
        assert_eq!(owners.state, ConsentState::Approved);
        assert_eq!(owners.approval, Some(Approval::Asked));
        assert_eq!(owners.decided_by, Some(fx.owner.clone()));
        assert_eq!(owners.decided_at, Some(at(100)));
        assert_eq!(owners.thread.root, None);
        assert_eq!(fx.store.consent(ConsentId::new_v4()).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_decision_lands_once_and_never_after_the_expiry() {
        let fx = fixture().await;
        let consent = fx.create(false, None, 1_000).await;
        let by = member_key("alice");
        let decided = fx
            .store
            .decide_consent(consent.id, false, &by, at(200))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(decided.state, ConsentState::Declined);
        assert_eq!(decided.approval, None);
        assert_eq!(decided.decided_by, Some(by.clone()));
        assert_eq!(
            fx.store
                .decide_consent(consent.id, true, &by, at(201))
                .await
                .unwrap(),
            None
        );

        let late = fx.create(false, None, 300).await;
        assert_eq!(
            fx.store
                .decide_consent(late.id, true, &by, at(300))
                .await
                .unwrap(),
            None,
            "a card past its expiry takes no decision"
        );
        let expired = fx.store.expire_consents(at(300)).await.unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].id, late.id);
        assert_eq!(expired[0].state, ConsentState::Expired);
        assert_eq!(expired[0].decided_by, None);
        assert!(fx.store.expire_consents(at(400)).await.unwrap().is_empty());

        let orphan = fx.create(false, None, 10_000).await;
        assert!(fx.store.delete_agent(fx.agent, at(500)).await.unwrap());
        let expired = fx.store.expire_consents(at(500)).await.unwrap();
        assert_eq!(
            expired.iter().map(|c| c.id).collect::<Vec<_>>(),
            [orphan.id],
            "a deleted agent's consents expire at once"
        );
    }

    #[tokio::test]
    async fn a_card_is_claimed_once_per_lease_and_recorded_once() {
        let fx = fixture().await;
        let consent = fx.create(false, None, 10_000).await;
        fx.create(true, None, 10_000).await;
        let owed = fx.store.consent_cards_owed(at(100)).await.unwrap();
        assert_eq!(owed.len(), 1, "an approved consent owes no card");
        assert_eq!(
            fx.store
                .claim_consent_card(consent.id, at(100), at(700))
                .await
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            fx.store
                .claim_consent_card(consent.id, at(101), at(701))
                .await
                .unwrap(),
            None,
            "the lease holds"
        );
        assert!(
            fx.store
                .defer_consent_card(consent.id, 1, at(150))
                .await
                .unwrap()
        );
        assert_eq!(
            fx.store
                .claim_consent_card(consent.id, at(150), at(700))
                .await
                .unwrap(),
            Some(2)
        );
        assert!(
            !fx.store
                .defer_consent_card(consent.id, 1, at(151))
                .await
                .unwrap(),
            "a stale claim can't shorten the latest one's lease"
        );
        assert_eq!(
            fx.store
                .claim_consent_card(consent.id, at(160), at(800))
                .await
                .unwrap(),
            None
        );
        let card = MsgRef {
            conv: ConvRef {
                surface: SurfaceKind::Slack,
                team: TeamId::new("T1"),
                conversation: ConversationId::new("D1"),
            },
            id: MessageId::new("1.5"),
        };
        assert!(
            fx.store
                .record_consent_card(consent.id, &card)
                .await
                .unwrap()
        );
        assert!(
            !fx.store
                .record_consent_card(consent.id, &card)
                .await
                .unwrap()
        );
        assert!(
            !fx.store
                .defer_consent_card(consent.id, 2, at(150))
                .await
                .unwrap()
        );
        assert!(
            fx.store
                .consent_cards_owed(at(9_000))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fx.store.consent(consent.id).await.unwrap().unwrap().card,
            Some(card)
        );

        let other = fx.create(false, None, 10_000).await;
        for n in 1..=20 {
            assert_eq!(
                fx.store
                    .claim_consent_card(other.id, at(100 * n), at(100 * n + 1))
                    .await
                    .unwrap(),
                Some(u32::try_from(n).unwrap()),
                "a card is tried until its consent expires"
            );
        }
        assert!(fx.store.expire_consent(other.id, at(2_500)).await.unwrap());
        assert!(!fx.store.expire_consent(other.id, at(2_501)).await.unwrap());
        let expired = fx.store.consent(other.id).await.unwrap().unwrap();
        assert_eq!(expired.state, ConsentState::Expired);
        assert_eq!(expired.decided_at, Some(at(2_500)));
        assert_eq!(
            fx.store
                .claim_consent_card(other.id, at(9_000), at(9_001))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn a_decided_card_is_closed_once() {
        let fx = fixture().await;
        let consent = fx.create(false, None, 10_000).await;
        let card = MsgRef {
            conv: ConvRef {
                surface: SurfaceKind::RocketChat,
                team: TeamId::new("chat.example.org"),
                conversation: ConversationId::new("dm"),
            },
            id: MessageId::new("m1"),
        };
        fx.store
            .record_consent_card(consent.id, &card)
            .await
            .unwrap();
        assert!(fx.store.consent_cards_to_close().await.unwrap().is_empty());
        assert!(
            !fx.store
                .claim_consent_card_close(consent.id, at(100))
                .await
                .unwrap(),
            "a pending card stays open"
        );
        let approved = fx
            .store
            .decide_consent(consent.id, true, &fx.owner, at(150))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(approved.approval, Some(Approval::Card));
        assert_eq!(fx.store.consent_cards_to_close().await.unwrap().len(), 1);
        assert!(
            fx.store
                .claim_consent_card_close(consent.id, at(160))
                .await
                .unwrap()
        );
        assert!(
            !fx.store
                .claim_consent_card_close(consent.id, at(161))
                .await
                .unwrap()
        );
        assert!(fx.store.consent_cards_to_close().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn work_is_leased_renewed_and_finished_by_the_latest_claim_only() {
        let fx = fixture().await;
        let pending = fx.create(false, None, 10_000).await;
        let approved = fx.create(true, None, 10_000).await;
        let owed = fx.store.consent_work_owed(at(100)).await.unwrap();
        assert_eq!(
            owed.iter().map(|c| c.id).collect::<Vec<_>>(),
            [approved.id],
            "a pending consent owes no work"
        );
        assert_eq!(
            fx.store
                .claim_consent_work(pending.id, at(100), at(200))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            fx.store
                .claim_consent_work(approved.id, at(100), at(200))
                .await
                .unwrap(),
            Some(1)
        );
        assert!(
            fx.store
                .consent_work_owed(at(150))
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            fx.store
                .renew_consent_work(approved.id, 1, at(300))
                .await
                .unwrap()
        );
        assert_eq!(
            fx.store
                .claim_consent_work(approved.id, at(250), at(400))
                .await
                .unwrap(),
            None,
            "the renewed lease holds"
        );
        assert_eq!(
            fx.store
                .claim_consent_work(approved.id, at(300), at(400))
                .await
                .unwrap(),
            Some(2),
            "an expired lease is taken over"
        );
        assert_eq!(
            fx.store
                .fail_consent_work(approved.id, 1, at(350))
                .await
                .unwrap(),
            None,
            "a stale claim can't fail the work"
        );
        assert!(
            !fx.store
                .release_consent_work(approved.id, 1, at(300))
                .await
                .unwrap()
        );
        assert_eq!(
            fx.store
                .fail_consent_work(approved.id, 2, at(350))
                .await
                .unwrap(),
            Some(1)
        );
        assert!(
            fx.store
                .consent_work_owed(at(349))
                .await
                .unwrap()
                .is_empty(),
            "a failed claim waits for its retry"
        );
        assert_eq!(
            fx.store
                .claim_consent_work(approved.id, at(350), at(900))
                .await
                .unwrap(),
            Some(3)
        );
        assert!(
            fx.store
                .release_consent_work(approved.id, 3, at(360))
                .await
                .unwrap()
        );
        assert_eq!(
            fx.store
                .claim_consent_work(approved.id, at(360), at(900))
                .await
                .unwrap(),
            Some(4),
            "a released claim is taken again at once"
        );
        let session = SessionId::new_v4();
        assert!(
            !fx.store
                .set_consent_session(approved.id, 1, session)
                .await
                .unwrap()
        );
        assert!(
            !fx.store
                .renew_consent_work(approved.id, 1, at(500))
                .await
                .unwrap()
        );
        assert!(
            !fx.store
                .finish_consent(approved.id, 1, at(310))
                .await
                .unwrap()
        );
        assert!(
            fx.store
                .set_consent_session(approved.id, 4, session)
                .await
                .unwrap()
        );
        assert!(
            fx.store
                .finish_consent(approved.id, 4, at(320))
                .await
                .unwrap()
        );
        assert!(
            !fx.store
                .finish_consent(approved.id, 4, at(330))
                .await
                .unwrap()
        );
        let done = fx.store.consent(approved.id).await.unwrap().unwrap();
        assert_eq!(done.private_session, Some(session));
        assert_eq!(done.finished_at, Some(at(320)));
        assert_eq!(done.work_attempts, 4);
        assert_eq!(done.work_failures, 1);
        assert!(
            fx.store
                .consent_work_owed(at(9_000))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_next_deadline_is_the_soonest_due_after_now() {
        let fx = fixture().await;
        assert_eq!(fx.store.next_consent_deadline(at(100)).await.unwrap(), None);
        let pending = fx.create(false, None, 5_000).await;
        let approved = fx.create(true, None, 1_000).await;
        assert_eq!(
            fx.store.next_consent_deadline(at(100)).await.unwrap(),
            Some(at(5_000)),
            "an approved consent's expiry is no deadline"
        );
        fx.store
            .claim_consent_card(pending.id, at(100), at(700))
            .await
            .unwrap();
        fx.store
            .claim_consent_work(approved.id, at(100), at(400))
            .await
            .unwrap();
        assert_eq!(
            fx.store.next_consent_deadline(at(100)).await.unwrap(),
            Some(at(400))
        );
        assert_eq!(
            fx.store.next_consent_deadline(at(400)).await.unwrap(),
            Some(at(700)),
            "only what falls due after now"
        );
    }

    #[tokio::test]
    async fn a_consent_needs_its_agent() {
        let fx = fixture().await;
        let thread = thread(None);
        let requester = Requester {
            member: None,
            key: MemberKey {
                surface: SurfaceKind::Slack,
                team: TeamId::new("T1"),
                user: UserId::new("U1"),
            },
        };
        let err = fx
            .store
            .create_consent(
                &NewConsent {
                    id: ConsentId::new_v4(),
                    agent: AgentId::new_v4(),
                    requester: &requester,
                    hop: Hop::ZERO,
                    task: "x",
                    attachments_json: "[]",
                    thread: &thread,
                    origin_session: SessionId::new_v4(),
                    expires_at: at(10),
                    approved_by_owner: None,
                },
                LIMITS,
                at(1),
            )
            .await;
        assert!(matches!(err, Err(StoreError::Database(_))), "{err:?}");
    }

    #[tokio::test]
    async fn an_owners_approval_at_once_is_refused_past_hop_zero() {
        let fx = fixture().await;
        let thread = thread(None);
        let requester = requester(None);
        let err = fx
            .store
            .create_consent(
                &NewConsent {
                    id: ConsentId::new_v4(),
                    agent: fx.agent,
                    requester: &requester,
                    hop: Hop(1),
                    task: "x",
                    attachments_json: "[]",
                    thread: &thread,
                    origin_session: SessionId::new_v4(),
                    expires_at: at(10),
                    approved_by_owner: Some(&fx.owner),
                },
                LIMITS,
                at(1),
            )
            .await;
        assert!(matches!(err, Err(StoreError::Database(_))), "{err:?}");
    }

    #[tokio::test]
    async fn unfinished_consents_are_limited_per_requester_and_per_agent() {
        let fx = fixture().await;
        let limits = OpenLimits {
            per_requester: 2,
            per_agent: 3,
        };
        let bob = requester(None);
        let carol = Requester {
            member: None,
            key: member_key("carol"),
        };
        let first = fx
            .try_create(&bob, false, None, 1_000, limits)
            .await
            .unwrap()
            .unwrap();
        fx.try_create(&bob, false, None, 1_000, limits)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fx.try_create(&bob, false, None, 1_000, limits)
                .await
                .unwrap(),
            None,
            "bob has as many as he may"
        );
        fx.try_create(&carol, false, None, 1_000, limits)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            fx.try_create(&carol, false, None, 1_000, limits)
                .await
                .unwrap(),
            None,
            "the agent has as many as it may"
        );
        fx.store
            .decide_consent(first.id, false, &fx.owner, at(150))
            .await
            .unwrap();
        assert_eq!(
            fx.try_create(&bob, false, None, 1_000, limits)
                .await
                .unwrap(),
            None,
            "a decided consent counts until its work is done"
        );
        let attempt = fx
            .store
            .claim_consent_work(first.id, at(160), at(200))
            .await
            .unwrap()
            .unwrap();
        fx.store
            .finish_consent(first.id, attempt, at(170))
            .await
            .unwrap();
        assert!(
            fx.try_create(&bob, false, None, 1_000, limits)
                .await
                .unwrap()
                .is_some()
        );
    }
}

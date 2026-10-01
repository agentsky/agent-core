//! Cloud hand-off: `cloud_routines`, the routines members registered, and
//! `cloud_handoffs`, the record of each `cloud run`.
//!
//! A routine's token is sealed with its table, column, member, row id,
//! routine id and URL origin as associated data, and a hand-off's task with
//! its table, column, member and row id, so neither opens once its row is
//! moved to another member. A member holds at most [`MAX_CLOUD_ROUTINES`]
//! routines, each label and each routine id once.
//!
//! A hand-off is written `sending` before its request and gets one outcome
//! ([`CloudOutcome`]) from `sending`. A row a pass marked `unknown` because
//! its record was held up still takes the answer while its notice hasn't
//! told the member: `fired` or `rejected` replace `unknown`, and a late
//! `unknown` only marks the notice done. Recording an outcome marks its
//! notice done, since the command's reply tells the member. A row the pass
//! marked `unknown` owes a
//! notice, sent at least once the way the relink notices are: a
//! [claim](Store::claim_cloud_handoff_notice) counts an attempt and takes a
//! [lease](CLOUD_NOTICE_LEASE), a failed send
//! [backs off](Store::defer_cloud_handoff_notice), and the notice is given
//! up [a day](CLOUD_NOTICE_GIVE_UP) after the row was answered.
//!
//! Hand-off ids are minted by agentd in
//! [`begin_cloud_handoff`](Store::begin_cloud_handoff) and never taken from
//! what a member types, so the methods that take one aren't scoped to a
//! member.

use std::fmt;
use std::time::Duration;

use core_types::{CloudHandoffId, CloudRoutineId, MemberId, MemberKey, RoutineId, RoutineToken};
use secrecy::SecretString;
use time::OffsetDateTime;

use crate::{Aad, Result, Store, StoreError, from_unix, parse_column, to_unix};

const ROUTINES: &str = "cloud_routines";
const HANDOFFS: &str = "cloud_handoffs";

/// The most routines one member may hold.
pub const MAX_CLOUD_ROUTINES: u32 = 20;

/// How long a claim on a hand-off's notice keeps others from claiming it.
pub const CLOUD_NOTICE_LEASE: Duration = Duration::from_secs(10 * 60);

/// How long after its row was answered a notice is still owed.
pub const CLOUD_NOTICE_GIVE_UP: Duration = Duration::from_secs(24 * 60 * 60);

/// The backoff after a notice's first failed send. Each later failure
/// doubles it, up to [`NOTICE_BACKOFF_MAX`].
const NOTICE_BACKOFF_FIRST: Duration = Duration::from_secs(60);
const NOTICE_BACKOFF_MAX: Duration = Duration::from_secs(60 * 60);

/// The columns of `cloud_handoffs` every query reads but the task, in
/// [`HandoffRow`]'s order.
macro_rules! handoff_columns {
    () => {
        "id, member_id, routine_label, routine_id, requested_by, origin, state, http_status, \
         error_type, retry_after_secs, session_id, session_url, created_at, answered_at, \
         notice_attempts, notified_at, unknown_reason"
    };
}

/// The conditions under which a hand-off's notice is owed and may be claimed
/// at `now` (bound first), for a row answered after `give_up` (bound
/// second), the time [`CLOUD_NOTICE_GIVE_UP`] before `now`.
macro_rules! notice_claimable {
    () => {
        "state = 'unknown' AND notified_at IS NULL \
         AND (notice_next_attempt_at IS NULL OR notice_next_attempt_at <= ?) \
         AND answered_at > ?"
    };
}

/// A routine a member registered, from [`Store::cloud_routines`]. It never
/// holds the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudRoutine {
    /// The row's id.
    pub id: CloudRoutineId,
    /// The member's label for it.
    pub label: String,
    /// The routine's id.
    pub routine_id: RoutineId,
    /// The origin of the fire URL it was registered with.
    pub url_origin: String,
    /// The identity that registered it, or last replaced its token.
    pub added_by: MemberKey,
    /// When.
    pub added_at: OffsetDateTime,
}

/// A routine with its opened token, from [`Store::cloud_routine`], for the
/// fire request. `Debug` redacts the token.
#[derive(Debug, Clone)]
pub struct CloudRoutineToken {
    /// The row's id.
    pub id: CloudRoutineId,
    /// The routine's id.
    pub routine_id: RoutineId,
    /// The origin of the fire URL it was registered with, which a fire
    /// compares with `[cloud] base_url`'s.
    pub url_origin: String,
    /// The routine's API trigger token.
    pub token: RoutineToken,
}

/// A routine to register, for [`Store::put_cloud_routine`]. `Debug`
/// redacts the token.
#[derive(Debug, Clone, Copy)]
pub struct NewCloudRoutine<'a> {
    /// The member registering it.
    pub member: MemberId,
    /// The member's label for it.
    pub label: &'a str,
    /// The routine's id.
    pub routine_id: &'a RoutineId,
    /// The origin of the fire URL it was registered with, as
    /// `url::Origin::ascii_serialization` writes it.
    pub url_origin: &'a str,
    /// The routine's API trigger token.
    pub token: &'a RoutineToken,
    /// The identity that typed the command.
    pub added_by: &'a MemberKey,
}

/// What [`Store::put_cloud_routine`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudRoutinePut {
    /// A new routine was registered under the label, as this row.
    Added(CloudRoutineId),
    /// The label was registered already; its row now holds the new routine
    /// id and token.
    Replaced(CloudRoutineId),
    /// Nothing was stored: the member registered the routine under another
    /// label, this one.
    RoutineTaken {
        /// The label the routine is registered under.
        label: String,
    },
    /// Nothing was stored: the label is new and the member holds
    /// [`MAX_CLOUD_ROUTINES`] routines already.
    Full,
}

/// What [`Store::delete_cloud_routines_of`] deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CloudDeleted {
    /// How many routines.
    pub routines: u64,
    /// How many hand-offs.
    pub handoffs: u64,
}

/// The private place a `cloud run` was typed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloudOrigin {
    /// A Slack slash command.
    SlackSlash,
    /// A direct message with the Slack manager app.
    SlackDm,
    /// A direct message with the Rocket.Chat manager bot.
    RocketChatDm,
}

impl CloudOrigin {
    /// The stored form: `slack_slash`, `slack_dm` or `rocketchat_dm`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SlackSlash => "slack_slash",
            Self::SlackDm => "slack_dm",
            Self::RocketChatDm => "rocketchat_dm",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "slack_slash" => Ok(Self::SlackSlash),
            "slack_dm" => Ok(Self::SlackDm),
            "rocketchat_dm" => Ok(Self::RocketChatDm),
            _ => Err(corrupt(HANDOFFS, "origin")),
        }
    }
}

/// Where a hand-off stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloudHandoffState {
    /// Written before the request; no outcome recorded yet.
    Sending,
    /// The routine started a session.
    Fired,
    /// The endpoint refused the request, or it was never sent.
    Rejected,
    /// Whether a session started isn't known.
    Unknown,
}

impl CloudHandoffState {
    /// The stored form: `sending`, `fired`, `rejected` or `unknown`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sending => "sending",
            Self::Fired => "fired",
            Self::Rejected => "rejected",
            Self::Unknown => "unknown",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "sending" => Ok(Self::Sending),
            "fired" => Ok(Self::Fired),
            "rejected" => Ok(Self::Rejected),
            "unknown" => Ok(Self::Unknown),
            _ => Err(corrupt(HANDOFFS, "state")),
        }
    }
}

/// A fire request's outcome, for [`Store::finish_cloud_handoff`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudOutcome {
    /// The routine started a session.
    Fired {
        /// The session's id.
        session_id: String,
        /// Its link, when the endpoint gave one agentd shows.
        session_url: Option<String>,
    },
    /// The endpoint refused the request, or it was never sent.
    Rejected {
        /// The HTTP status, if there was an answer.
        status: Option<u16>,
        /// The error envelope's `error.type`, if it had one.
        error_type: Option<String>,
        /// `Retry-After`, in seconds.
        retry_after_secs: Option<u32>,
    },
    /// Whether a session started isn't known.
    Unknown {
        /// The HTTP status, if there was an answer.
        status: Option<u16>,
        /// Why it isn't known.
        reason: CloudUnknownReason,
    },
}

/// Why a hand-off's outcome isn't known, kept with an `unknown` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloudUnknownReason {
    /// The endpoint answered with a server error (5xx).
    ServerError,
    /// The endpoint answered with a status the fire client doesn't expect.
    OtherStatus,
    /// The request timed out after it was sent.
    Timeout,
    /// The connection was lost after the request was sent.
    ConnectionLost,
    /// The endpoint answered with a redirect, which isn't followed.
    Redirect,
    /// The endpoint answered with success, but the answer can't be read.
    UnreadableAnswer,
    /// No answer was recorded: a pass gave up waiting for one.
    NoAnswer,
}

impl CloudUnknownReason {
    /// The stored form, such as `server_error` or `no_answer`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ServerError => "server_error",
            Self::OtherStatus => "other_status",
            Self::Timeout => "timeout",
            Self::ConnectionLost => "connection_lost",
            Self::Redirect => "redirect",
            Self::UnreadableAnswer => "unreadable_answer",
            Self::NoAnswer => "no_answer",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "server_error" => Ok(Self::ServerError),
            "other_status" => Ok(Self::OtherStatus),
            "timeout" => Ok(Self::Timeout),
            "connection_lost" => Ok(Self::ConnectionLost),
            "redirect" => Ok(Self::Redirect),
            "unreadable_answer" => Ok(Self::UnreadableAnswer),
            "no_answer" => Ok(Self::NoAnswer),
            _ => Err(corrupt(HANDOFFS, "unknown_reason")),
        }
    }
}

impl CloudOutcome {
    /// The state recording this outcome leaves the hand-off in.
    pub fn state(&self) -> CloudHandoffState {
        match self {
            Self::Fired { .. } => CloudHandoffState::Fired,
            Self::Rejected { .. } => CloudHandoffState::Rejected,
            Self::Unknown { .. } => CloudHandoffState::Unknown,
        }
    }
}

/// A hand-off to record, for [`Store::begin_cloud_handoff`]. `Debug` leaves
/// out the task.
#[derive(Clone, Copy)]
pub struct NewCloudHandoff<'a> {
    /// The member whose routine it fires.
    pub member: MemberId,
    /// The routine's label.
    pub routine_label: &'a str,
    /// The routine's id.
    pub routine_id: &'a RoutineId,
    /// The identity that typed the command.
    pub requested_by: &'a MemberKey,
    /// Where it was typed.
    pub origin: CloudOrigin,
    /// The task text, sealed before it is stored. Never logged.
    pub task: &'a str,
}

impl fmt::Debug for NewCloudHandoff<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewCloudHandoff")
            .field("member", &self.member)
            .field("routine_label", &self.routine_label)
            .field("routine_id", self.routine_id)
            .field("requested_by", self.requested_by)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

/// A `cloud_handoffs` row, without its task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudHandoff {
    /// The hand-off's id.
    pub id: CloudHandoffId,
    /// The member whose routine it fired.
    pub member: MemberId,
    /// The routine's label when it was asked.
    pub routine_label: String,
    /// The routine's id.
    pub routine_id: RoutineId,
    /// The identity that asked.
    pub requested_by: MemberKey,
    /// Where it was asked.
    pub origin: CloudOrigin,
    /// Where it stands.
    pub state: CloudHandoffState,
    /// The answer's HTTP status, if any.
    pub http_status: Option<u16>,
    /// The answer's error type, if any.
    pub error_type: Option<String>,
    /// The answer's `Retry-After`, in seconds, if any.
    pub retry_after_secs: Option<u32>,
    /// The session's id, once fired.
    pub session_id: Option<String>,
    /// The session's link, once fired, if one was kept.
    pub session_url: Option<String>,
    /// When it was asked.
    pub created_at: OffsetDateTime,
    /// When its outcome was recorded, or when a pass marked it `unknown`.
    pub answered_at: Option<OffsetDateTime>,
    /// How many times its notice was claimed.
    pub notice_attempts: u32,
    /// When the member was told the outcome, by the reply or the notice.
    pub notified_at: Option<OffsetDateTime>,
    /// Why the outcome isn't known, while the row is `unknown`.
    pub unknown_reason: Option<CloudUnknownReason>,
}

/// A hand-off with its task opened, from [`Store::recent_cloud_handoffs`].
/// `Debug` redacts the task.
#[derive(Debug, Clone)]
pub struct RecentCloudHandoff {
    /// The hand-off.
    pub handoff: CloudHandoff,
    /// Its task text.
    pub task: SecretString,
}

#[derive(sqlx::FromRow)]
struct HandoffRow {
    id: String,
    member_id: String,
    routine_label: String,
    routine_id: String,
    requested_by: String,
    origin: String,
    state: String,
    http_status: Option<i64>,
    error_type: Option<String>,
    retry_after_secs: Option<i64>,
    session_id: Option<String>,
    session_url: Option<String>,
    created_at: i64,
    answered_at: Option<i64>,
    notice_attempts: i64,
    notified_at: Option<i64>,
    unknown_reason: Option<String>,
}

#[derive(sqlx::FromRow)]
struct RecentRow {
    task_enc: Vec<u8>,
    #[sqlx(flatten)]
    row: HandoffRow,
}

impl HandoffRow {
    fn into_handoff(self) -> Result<CloudHandoff> {
        Ok(CloudHandoff {
            id: parse_column(&self.id, HANDOFFS, "id")?,
            member: parse_column(&self.member_id, HANDOFFS, "member_id")?,
            routine_label: self.routine_label,
            routine_id: parse_column(&self.routine_id, HANDOFFS, "routine_id")?,
            requested_by: parse_column(&self.requested_by, HANDOFFS, "requested_by")?,
            origin: CloudOrigin::parse(&self.origin)?,
            state: CloudHandoffState::parse(&self.state)?,
            http_status: narrow(self.http_status, "http_status")?,
            error_type: self.error_type,
            retry_after_secs: narrow(self.retry_after_secs, "retry_after_secs")?,
            session_id: self.session_id,
            session_url: self.session_url,
            created_at: from_unix(self.created_at, HANDOFFS, "created_at")?,
            answered_at: self
                .answered_at
                .map(|at| from_unix(at, HANDOFFS, "answered_at"))
                .transpose()?,
            notice_attempts: u32::try_from(self.notice_attempts)
                .map_err(|_| corrupt(HANDOFFS, "notice_attempts"))?,
            notified_at: self
                .notified_at
                .map(|at| from_unix(at, HANDOFFS, "notified_at"))
                .transpose()?,
            unknown_reason: self
                .unknown_reason
                .as_deref()
                .map(CloudUnknownReason::parse)
                .transpose()?,
        })
    }
}

fn handoffs(rows: Vec<HandoffRow>) -> Result<Vec<CloudHandoff>> {
    rows.into_iter().map(HandoffRow::into_handoff).collect()
}

fn narrow<T: TryFrom<i64>>(value: Option<i64>, column: &'static str) -> Result<Option<T>> {
    value
        .map(|value| T::try_from(value).map_err(|_| corrupt(HANDOFFS, column)))
        .transpose()
}

fn corrupt(table: &'static str, column: &'static str) -> StoreError {
    StoreError::Corrupt { table, column }
}

/// The associated data's key for a routine's token: its member, row id,
/// routine id and URL origin. Only the origin may hold `:`, and it comes
/// last.
fn token_key(member: MemberId, id: &str, routine_id: &str, url_origin: &str) -> String {
    format!("{member}:{id}:{routine_id}:{url_origin}")
}

fn token_aad(key: &str) -> Aad<'_> {
    Aad {
        table: ROUTINES,
        column: "token_enc",
        key,
    }
}

/// The associated data's key for a hand-off's task: its member and row id.
fn task_key(member: MemberId, id: &str) -> String {
    format!("{member}:{id}")
}

fn task_aad(key: &str) -> Aad<'_> {
    Aad {
        table: HANDOFFS,
        column: "task_enc",
        key,
    }
}

fn later(at: OffsetDateTime, by: Duration) -> OffsetDateTime {
    at.saturating_add(time::Duration::try_from(by).unwrap_or(time::Duration::MAX))
}

fn earlier(at: OffsetDateTime, by: Duration) -> OffsetDateTime {
    at.saturating_sub(time::Duration::try_from(by).unwrap_or(time::Duration::MAX))
}

/// The backoff after claim `claim` of a notice failed: a minute after the
/// first, doubling with each claim, up to an hour.
fn notice_backoff(claim: u32) -> Duration {
    let doublings = claim.saturating_sub(1).min(16);
    NOTICE_BACKOFF_FIRST
        .saturating_mul(1 << doublings)
        .min(NOTICE_BACKOFF_MAX)
}

impl Store {
    /// Registers `routine` at `now`.
    ///
    /// A label the member registered already is replaced in place: its row
    /// keeps its id and takes the new routine id, token, identity and time,
    /// which is how a member registers a new token. A routine id the member
    /// registered under another label is refused, and so is a new label once
    /// the member holds [`MAX_CLOUD_ROUTINES`]. The checks and the write are
    /// one `BEGIN IMMEDIATE` transaction, so concurrent calls never pass
    /// them together.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if a query fails, as when the member doesn't
    /// exist, [`StoreError::Seal`] if the token can't be sealed.
    pub async fn put_cloud_routine(
        &self,
        routine: &NewCloudRoutine<'_>,
        now: OffsetDateTime,
    ) -> Result<CloudRoutinePut> {
        let NewCloudRoutine {
            member,
            label,
            routine_id,
            url_origin,
            token,
            added_by,
        } = *routine;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let taken: Option<String> = sqlx::query_scalar(
            "SELECT label FROM cloud_routines WHERE member_id = ? AND routine_id = ? \
             AND label <> ?",
        )
        .bind(member.to_string())
        .bind(routine_id.as_str())
        .bind(label)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(label) = taken {
            return Ok(CloudRoutinePut::RoutineTaken { label });
        }
        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM cloud_routines WHERE member_id = ? AND label = ?")
                .bind(member.to_string())
                .bind(label)
                .fetch_optional(&mut *tx)
                .await?;
        let put = if let Some(id) = existing {
            let key = token_key(member, &id, routine_id.as_str(), url_origin);
            let sealed = self.seal(token_aad(&key), token.as_secret())?;
            sqlx::query(
                "UPDATE cloud_routines SET routine_id = ?, url_origin = ?, token_enc = ?, \
                 added_by = ?, added_at = ? WHERE id = ?",
            )
            .bind(routine_id.as_str())
            .bind(url_origin)
            .bind(sealed)
            .bind(added_by.to_string())
            .bind(to_unix(now))
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            CloudRoutinePut::Replaced(parse_column(&id, ROUTINES, "id")?)
        } else {
            let held: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM cloud_routines WHERE member_id = ?")
                    .bind(member.to_string())
                    .fetch_one(&mut *tx)
                    .await?;
            if held >= i64::from(MAX_CLOUD_ROUTINES) {
                return Ok(CloudRoutinePut::Full);
            }
            let id = CloudRoutineId::new_v4();
            let key = token_key(member, &id.to_string(), routine_id.as_str(), url_origin);
            let sealed = self.seal(token_aad(&key), token.as_secret())?;
            sqlx::query(
                "INSERT INTO cloud_routines (id, member_id, label, routine_id, url_origin, \
                 token_enc, added_by, added_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(id.to_string())
            .bind(member.to_string())
            .bind(label)
            .bind(routine_id.as_str())
            .bind(url_origin)
            .bind(sealed)
            .bind(added_by.to_string())
            .bind(to_unix(now))
            .execute(&mut *tx)
            .await?;
            CloudRoutinePut::Added(id)
        };
        tx.commit().await?;
        Ok(put)
    }

    /// `member`'s routine registered under `label`, with its token opened,
    /// if there is one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Seal`] if
    /// the token doesn't open (sealed for another row, or under another
    /// key), [`StoreError::Corrupt`] if the row doesn't parse.
    pub async fn cloud_routine(
        &self,
        member: MemberId,
        label: &str,
    ) -> Result<Option<CloudRoutineToken>> {
        let row: Option<(String, String, String, Vec<u8>)> = sqlx::query_as(
            "SELECT id, routine_id, url_origin, token_enc FROM cloud_routines \
             WHERE member_id = ? AND label = ?",
        )
        .bind(member.to_string())
        .bind(label)
        .fetch_optional(&self.pool)
        .await?;
        let Some((id, routine_id, url_origin, sealed)) = row else {
            return Ok(None);
        };
        let key = token_key(member, &id, &routine_id, &url_origin);
        Ok(Some(CloudRoutineToken {
            token: RoutineToken::parse(self.open_sealed(token_aad(&key), &sealed)?)
                .map_err(|_| corrupt(ROUTINES, "token_enc"))?,
            id: parse_column(&id, ROUTINES, "id")?,
            routine_id: parse_column(&routine_id, ROUTINES, "routine_id")?,
            url_origin,
        }))
    }

    /// `member`'s routines, by label, without their tokens.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn cloud_routines(&self, member: MemberId) -> Result<Vec<CloudRoutine>> {
        let rows: Vec<(String, String, String, String, String, i64)> = sqlx::query_as(
            "SELECT id, label, routine_id, url_origin, added_by, added_at FROM cloud_routines \
             WHERE member_id = ? ORDER BY label",
        )
        .bind(member.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, label, routine_id, url_origin, added_by, added_at)| {
                Ok(CloudRoutine {
                    id: parse_column(&id, ROUTINES, "id")?,
                    label,
                    routine_id: parse_column(&routine_id, ROUTINES, "routine_id")?,
                    url_origin,
                    added_by: parse_column(&added_by, ROUTINES, "added_by")?,
                    added_at: from_unix(added_at, ROUTINES, "added_at")?,
                })
            })
            .collect()
    }

    /// Deletes `member`'s routine registered under `label`, with its token.
    /// False if there was none. Its hand-offs are kept.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn delete_cloud_routine(&self, member: MemberId, label: &str) -> Result<bool> {
        let result = sqlx::query("DELETE FROM cloud_routines WHERE member_id = ? AND label = ?")
            .bind(member.to_string())
            .bind(label)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes every routine and hand-off of `member`, whichever identity
    /// added them, in one transaction: for `logout`, and for a member Slack
    /// reports deleted.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if a query fails.
    pub async fn delete_cloud_routines_of(&self, member: MemberId) -> Result<CloudDeleted> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let routines = sqlx::query("DELETE FROM cloud_routines WHERE member_id = ?")
            .bind(member.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        let handoffs = sqlx::query("DELETE FROM cloud_handoffs WHERE member_id = ?")
            .bind(member.to_string())
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok(CloudDeleted { routines, handoffs })
    }

    /// Records `handoff`, asked at `now`, as `sending`, with its task sealed
    /// to its row, and returns its id. Write it before the request is sent.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, as when the member
    /// doesn't exist, [`StoreError::Seal`] if the task can't be sealed.
    pub async fn begin_cloud_handoff(
        &self,
        handoff: &NewCloudHandoff<'_>,
        now: OffsetDateTime,
    ) -> Result<CloudHandoffId> {
        let id = CloudHandoffId::new_v4();
        let key = task_key(handoff.member, &id.to_string());
        let task = self.seal(task_aad(&key), &SecretString::from(handoff.task))?;
        sqlx::query(
            "INSERT INTO cloud_handoffs (id, member_id, routine_label, routine_id, \
             requested_by, origin, task_enc, state, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'sending', ?)",
        )
        .bind(id.to_string())
        .bind(handoff.member.to_string())
        .bind(handoff.routine_label)
        .bind(handoff.routine_id.as_str())
        .bind(handoff.requested_by.to_string())
        .bind(handoff.origin.as_str())
        .bind(task)
        .bind(to_unix(now))
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Records `outcome` for hand-off `id` at `now`, and marks its notice
    /// done, since the command's reply tells the member the outcome. False,
    /// changing nothing, unless the hand-off is `sending`, or a pass marked
    /// it `unknown` (its record was held up) and its notice hasn't told the
    /// member yet. Such a late `fired` or `rejected` replaces `unknown`; a
    /// late `unknown` keeps the row's time and only fills in a status it
    /// lacked. A claim of the notice still sending then finds it done
    /// already. Nothing retries a record that fails.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn finish_cloud_handoff(
        &self,
        id: CloudHandoffId,
        outcome: &CloudOutcome,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let (status, error_type, retry_after, session_id, session_url) = match outcome {
            CloudOutcome::Fired {
                session_id,
                session_url,
            } => (
                None,
                None,
                None,
                Some(session_id.as_str()),
                session_url.as_deref(),
            ),
            CloudOutcome::Rejected {
                status,
                error_type,
                retry_after_secs,
            } => (
                *status,
                error_type.as_deref(),
                *retry_after_secs,
                None,
                None,
            ),
            CloudOutcome::Unknown { status, .. } => (*status, None, None, None, None),
        };
        let unknown_reason = match outcome {
            CloudOutcome::Unknown { reason, .. } => Some(reason.as_str()),
            CloudOutcome::Fired { .. } | CloudOutcome::Rejected { .. } => None,
        };
        let now = to_unix(now);
        let result = sqlx::query(
            "UPDATE cloud_handoffs SET state = ?1, \
             http_status = CASE WHEN state = 'unknown' AND ?1 = 'unknown' \
             THEN COALESCE(http_status, ?2) ELSE ?2 END, \
             error_type = ?3, retry_after_secs = ?4, session_id = ?5, session_url = ?6, \
             answered_at = CASE WHEN state = 'unknown' AND ?1 = 'unknown' \
             THEN answered_at ELSE ?7 END, \
             notified_at = ?7, unknown_reason = ?9 \
             WHERE id = ?8 AND (state = 'sending' OR (state = 'unknown' AND notified_at IS NULL))",
        )
        .bind(outcome.state().as_str())
        .bind(status.map(i64::from))
        .bind(error_type)
        .bind(retry_after.map(i64::from))
        .bind(session_id)
        .bind(session_url)
        .bind(now)
        .bind(id.to_string())
        .bind(unknown_reason)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// `member`'s `limit` most recent hand-offs, newest first, each with its
    /// task opened.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Seal`] if
    /// a task doesn't open, [`StoreError::Corrupt`] if a row doesn't parse.
    pub async fn recent_cloud_handoffs(
        &self,
        member: MemberId,
        limit: u32,
    ) -> Result<Vec<RecentCloudHandoff>> {
        let rows: Vec<RecentRow> = sqlx::query_as(concat!(
            "SELECT task_enc, ",
            handoff_columns!(),
            " FROM cloud_handoffs WHERE member_id = ? \
             ORDER BY created_at DESC, rowid DESC LIMIT ?"
        ))
        .bind(member.to_string())
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|RecentRow { task_enc, row }| {
                let key = task_key(member, &row.id);
                let task = self.open_sealed(task_aad(&key), &task_enc)?;
                Ok(RecentCloudHandoff {
                    handoff: row.into_handoff()?,
                    task,
                })
            })
            .collect()
    }

    /// Marks every `sending` hand-off asked before `before` `unknown`,
    /// answered at `now`, and returns them: their requests' outcomes were
    /// never recorded, so each owes its member a notice. One statement, so
    /// concurrent passes return each row once.
    ///
    /// `before` is the caller's: every instance's pass marks every
    /// instance's rows, so during a blue-green deploy that changes
    /// `[cloud] timeout_secs`, an instance with the shorter timeout can mark
    /// a row whose request the other instance still waits on. The late
    /// answer is then still recorded, unless the notice went out first.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn stale_cloud_handoffs(
        &self,
        before: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<Vec<CloudHandoff>> {
        let rows: Vec<HandoffRow> = sqlx::query_as(concat!(
            "UPDATE cloud_handoffs SET state = 'unknown', answered_at = ?, \
             unknown_reason = 'no_answer' \
             WHERE state = 'sending' AND created_at < ? RETURNING ",
            handoff_columns!()
        ))
        .bind(to_unix(now))
        .bind(to_unix(before))
        .fetch_all(&self.pool)
        .await?;
        let mut stale = handoffs(rows)?;
        stale.sort_by_key(|handoff| (handoff.created_at, handoff.id));
        Ok(stale)
    }

    /// The hand-offs whose notice may be claimed at `now`, oldest answer
    /// first: `unknown`, the member not told, no lease or backoff running
    /// past `now`, and answered less than [`CLOUD_NOTICE_GIVE_UP`] ago.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn due_cloud_handoff_notices(
        &self,
        now: OffsetDateTime,
    ) -> Result<Vec<CloudHandoff>> {
        let rows: Vec<HandoffRow> = sqlx::query_as(concat!(
            "SELECT ",
            handoff_columns!(),
            " FROM cloud_handoffs WHERE ",
            notice_claimable!(),
            " ORDER BY answered_at, created_at, id"
        ))
        .bind(to_unix(now))
        .bind(to_unix(earlier(now, CLOUD_NOTICE_GIVE_UP)))
        .fetch_all(&self.pool)
        .await?;
        handoffs(rows)
    }

    /// Claims hand-off `id`'s notice at `now`, with a
    /// [lease](CLOUD_NOTICE_LEASE), if it may be claimed as for
    /// [`due_cloud_handoff_notices`](Self::due_cloud_handoff_notices).
    /// Returns the claim, which attempt this is from 1, only for the one
    /// call that claims it; `None` otherwise.
    ///
    /// Claim before sending, then
    /// [mark it sent](Self::mark_cloud_handoff_notified) or
    /// [defer](Self::defer_cloud_handoff_notice) it. A claim neither
    /// follows, because its sender died, is due again once its lease ends.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the count is negative.
    pub async fn claim_cloud_handoff_notice(
        &self,
        id: CloudHandoffId,
        now: OffsetDateTime,
    ) -> Result<Option<u32>> {
        let lease = to_unix(later(now, CLOUD_NOTICE_LEASE));
        let claim: Option<i64> = sqlx::query_scalar(concat!(
            "UPDATE cloud_handoffs SET notice_attempts = notice_attempts + 1, \
             notice_next_attempt_at = ? WHERE id = ? AND ",
            notice_claimable!(),
            " RETURNING notice_attempts"
        ))
        .bind(lease)
        .bind(id.to_string())
        .bind(to_unix(now))
        .bind(to_unix(earlier(now, CLOUD_NOTICE_GIVE_UP)))
        .fetch_optional(&self.pool)
        .await?;
        narrow(claim, "notice_attempts")
    }

    /// Ends claim `claim` on hand-off `id`'s notice, which couldn't be sent
    /// at `now`: it may be claimed again after a backoff of a minute after
    /// the first claim, doubling with each claim up to an hour. False if
    /// the notice isn't owed any more, or a later claim took it over.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn defer_cloud_handoff_notice(
        &self,
        id: CloudHandoffId,
        claim: u32,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let retry_at = later(now, notice_backoff(claim));
        let result = sqlx::query(
            "UPDATE cloud_handoffs SET notice_next_attempt_at = ? \
             WHERE id = ? AND notice_attempts = ? AND state = 'unknown' AND notified_at IS NULL",
        )
        .bind(to_unix(retry_at))
        .bind(id.to_string())
        .bind(i64::from(claim))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Records at `now` that claim `claim` sent hand-off `id`'s notice, so
    /// it is no longer owed. A claim a later one took over may still mark
    /// it, since its notice reached the member. False if the member was
    /// told already, or the notice was never claimed that many times.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn mark_cloud_handoff_notified(
        &self,
        id: CloudHandoffId,
        claim: u32,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE cloud_handoffs SET notified_at = ?, notice_next_attempt_at = NULL \
             WHERE id = ? AND notice_attempts >= ? AND ? > 0 \
             AND state <> 'sending' AND notified_at IS NULL",
        )
        .bind(to_unix(now))
        .bind(id.to_string())
        .bind(i64::from(claim))
        .bind(i64::from(claim))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Deletes the hand-offs asked before `before`, and returns how many.
    /// A row whose notice is still owed at `now` is kept until it is sent
    /// or given up, however short the retention.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn purge_cloud_handoffs(
        &self,
        before: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<u64> {
        let result = sqlx::query(
            "DELETE FROM cloud_handoffs WHERE created_at < ? AND NOT \
             (state = 'unknown' AND notified_at IS NULL AND answered_at > ?)",
        )
        .bind(to_unix(before))
        .bind(to_unix(earlier(now, CLOUD_NOTICE_GIVE_UP)))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests;

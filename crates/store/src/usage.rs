//! `usage`, `thread_usage` and `limit_notices`: the meter that bills each
//! turn to its requester, the per-thread counts the limits read, and the
//! notices that a limit was reached.
//!
//! Days and hours are UTC.

use core_types::{AgentId, MemberId, MessageId, ThreadKey};
use time::{Duration, OffsetDateTime};

use crate::{Result, Store, to_unix};

/// How long `thread_usage` and `limit_notices` rows are kept once their
/// day is over: the longest window a limit counts is a day.
pub const THREAD_USAGE_RETENTION: Duration = Duration::days(2);

const SECONDS_PER_DAY: i64 = 86_400;
const SECONDS_PER_HOUR: i64 = 3_600;

/// The most tokens one turn is billed: far more than any turn uses, and
/// small enough that no sum can overflow a column.
const MAX_TOKENS_PER_TURN: u64 = u32::MAX as u64;

/// What one turn used, as the meter bills it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TurnUsage {
    /// Input the model read fresh: uncached input and cache writes.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// The turn's cost in US dollars, as the CLI reckons it, or why it
    /// isn't known: the turn is then billed no cost, and recorded as
    /// unbilled for this reason.
    pub cost: Result<f64, CostUnknown>,
}

/// Why a turn's cost isn't known. The meter bills such a turn no cost and
/// records the reason with its turns and tokens, so what went unbilled can
/// be counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostUnknown {
    /// The turn crashed or timed out before the CLI's result.
    NoResult,
    /// The CLI's result has no plausible running total, or the previous
    /// result of its process had none to take off.
    NoTotal,
    /// The running total fell, or rose by more than a turn can cost.
    TotalOutOfRange,
    /// The first turn of a resumed process started in a container an
    /// earlier process ran in, where a process it left could have changed
    /// the transcript the restored total is read from.
    ReusedContainer,
    /// The first turn of a resumed process whose transcript is past the
    /// size the runner reads a restored total from.
    TranscriptTooLarge,
    /// The first turn of a resumed process whose transcript is missing or
    /// couldn't be opened or read.
    TranscriptUnreadable,
    /// The first turn of a resumed process whose transcript the runner
    /// isn't sure the CLI reads the way it does.
    TranscriptUnrecognized,
}

impl CostUnknown {
    /// The reason as the `usage` table's `cost_unknown` holds it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoResult => "no_result",
            Self::NoTotal => "no_total",
            Self::TotalOutOfRange => "total_out_of_range",
            Self::ReusedContainer => "reused_container",
            Self::TranscriptTooLarge => "transcript_too_large",
            Self::TranscriptUnreadable => "transcript_unreadable",
            Self::TranscriptUnrecognized => "transcript_unrecognized",
        }
    }
}

impl std::fmt::Display for CostUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What was billed to a member over some days: turns, and what they used.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    /// Turns billed to them.
    pub turns: u64,
    /// Input their turns' models read fresh.
    pub input_tokens: u64,
    /// Their turns' output tokens.
    pub output_tokens: u64,
    /// Their turns' known costs in US dollars, added up.
    pub cost_usd: f64,
}

impl UsageTotals {
    /// Input and output tokens together.
    pub fn tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// What was billed to a member today and this month, from
/// [`member_usage`](Store::member_usage).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct MemberUsage {
    /// Today's, UTC.
    pub today: UsageTotals,
    /// This month's, UTC, today's included.
    pub month: UsageTotals,
}

/// What agents spent in one thread, from
/// [`thread_spend`](Store::thread_spend).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadSpend {
    /// Turns agents took in the thread in the hour, every agent counted.
    pub turns_this_hour: u32,
    /// Tokens their turns used there in the day.
    pub tokens_today: u64,
}

/// The window a limit counts turns or tokens in, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitWindow {
    /// The day.
    Day,
    /// The hour.
    Hour,
}

impl LimitWindow {
    /// When the window `at` falls in starts, in Unix seconds.
    fn start(self, at: OffsetDateTime) -> i64 {
        let day = day_of(at) * SECONDS_PER_DAY;
        match self {
            Self::Day => day,
            Self::Hour => day + hour_of(at) * SECONDS_PER_HOUR,
        }
    }
}

/// The day `at` falls on, in days since 1970-01-01, UTC.
fn day_of(at: OffsetDateTime) -> i64 {
    at.unix_timestamp().div_euclid(SECONDS_PER_DAY)
}

/// The hour of its day `at` falls in, 0 to 23, UTC.
fn hour_of(at: OffsetDateTime) -> i64 {
    at.unix_timestamp().rem_euclid(SECONDS_PER_DAY) / SECONDS_PER_HOUR
}

fn tokens(value: u64) -> i64 {
    i64::try_from(value.min(MAX_TOKENS_PER_TURN)).unwrap_or(i64::MAX)
}

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

/// A thread's columns, as `message_refs` names a thread: `thread_root` is
/// empty for a conversation without threads.
fn thread_columns(thread: &ThreadKey) -> [&str; 4] {
    [
        thread.conv.surface.as_str(),
        thread.conv.team.as_str(),
        thread.conv.conversation.as_str(),
        thread.root.as_ref().map_or("", MessageId::as_str),
    ]
}

impl Store {
    /// Bills one turn of `agent` in `thread` to `member` at `at`: adds a
    /// turn and `usage` to the member's day, and a turn and its tokens to
    /// the thread's hour for the agent, in one transaction. A turn
    /// `for_owner`, requested by the agent's owner, isn't one its daily cap
    /// counts ([`capped_turns_on`](Self::capped_turns_on)).
    ///
    /// A turn whose [cost is unknown](TurnUsage::cost) is billed
    /// no cost in the member's day row for that reason, apart from the
    /// turns whose cost is known. A cost that isn't a finite number of at
    /// least 0 counts as 0, and a turn's tokens are capped far above what
    /// any turn uses.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if a query
    /// fails, for example because the member or the agent doesn't exist.
    pub async fn record_turn_usage(
        &self,
        member: MemberId,
        agent: AgentId,
        thread: &ThreadKey,
        usage: TurnUsage,
        for_owner: bool,
        at: OffsetDateTime,
    ) -> Result<()> {
        let cost = match usage.cost {
            Ok(cost) if cost.is_finite() && cost > 0.0 => cost,
            _ => 0.0,
        };
        let unknown = usage.cost.err().map_or("", CostUnknown::as_str);
        let (input, output) = (tokens(usage.input_tokens), tokens(usage.output_tokens));
        let [surface, team, conversation, root] = thread_columns(thread);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO usage (member_id, day, turns, input_tokens, output_tokens, cost_usd, \
             cost_unknown) VALUES (?, ?, 1, ?, ?, ?, ?) \
             ON CONFLICT (member_id, day, cost_unknown) DO UPDATE SET turns = turns + 1, \
             input_tokens = input_tokens + excluded.input_tokens, \
             output_tokens = output_tokens + excluded.output_tokens, \
             cost_usd = cost_usd + excluded.cost_usd",
        )
        .bind(member.to_string())
        .bind(day_of(at))
        .bind(input)
        .bind(output)
        .bind(cost)
        .bind(unknown)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO thread_usage (surface, team_id, conversation, thread_root, day, hour, \
             agent_id, agent_turns, others_turns, tokens) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?) \
             ON CONFLICT (surface, team_id, conversation, thread_root, day, hour, agent_id) \
             DO UPDATE SET agent_turns = agent_turns + 1, \
             others_turns = others_turns + excluded.others_turns, \
             tokens = tokens + excluded.tokens",
        )
        .bind(surface)
        .bind(team)
        .bind(conversation)
        .bind(root)
        .bind(day_of(at))
        .bind(hour_of(at))
        .bind(agent.to_string())
        .bind(i64::from(!for_owner))
        .bind(input.saturating_add(output))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// What was billed to `member` on the day `now` falls on, and in its
    /// month.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn member_usage(&self, member: MemberId, now: OffsetDateTime) -> Result<MemberUsage> {
        let today = day_of(now);
        let month = day_of(now.replace_day(1).unwrap_or(now));
        let row: (i64, i64, i64, f64, i64, i64, i64, f64) = sqlx::query_as(
            "SELECT COALESCE(SUM(CASE WHEN day = ?1 THEN turns END), 0), \
             COALESCE(SUM(CASE WHEN day = ?1 THEN input_tokens END), 0), \
             COALESCE(SUM(CASE WHEN day = ?1 THEN output_tokens END), 0), \
             COALESCE(SUM(CASE WHEN day = ?1 THEN cost_usd END), 0.0), \
             COALESCE(SUM(turns), 0), COALESCE(SUM(input_tokens), 0), \
             COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cost_usd), 0.0) \
             FROM usage WHERE member_id = ?2 AND day BETWEEN ?3 AND ?1",
        )
        .bind(today)
        .bind(member.to_string())
        .bind(month)
        .fetch_one(&self.pool)
        .await?;
        let totals = |turns, input, output, cost_usd| UsageTotals {
            turns: unsigned(turns),
            input_tokens: unsigned(input),
            output_tokens: unsigned(output),
            cost_usd,
        };
        let (turns, input, output, cost, month_turns, month_input, month_output, month_cost) = row;
        Ok(MemberUsage {
            today: totals(turns, input, output, cost),
            month: totals(month_turns, month_input, month_output, month_cost),
        })
    }

    /// The turns `agent` took on the day `at` falls on, in every thread,
    /// for anyone but its owner: the turns its daily cap counts.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn capped_turns_on(&self, agent: AgentId, at: OffsetDateTime) -> Result<u32> {
        let turns: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(others_turns), 0) FROM thread_usage \
             WHERE agent_id = ? AND day = ?",
        )
        .bind(agent.to_string())
        .bind(day_of(at))
        .fetch_one(&self.pool)
        .await?;
        Ok(u32::try_from(turns).unwrap_or(u32::MAX))
    }

    /// What every agent spent in `thread`: its turns in the hour `at` falls
    /// in, and its tokens on that day.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn thread_spend(
        &self,
        thread: &ThreadKey,
        at: OffsetDateTime,
    ) -> Result<ThreadSpend> {
        let [surface, team, conversation, root] = thread_columns(thread);
        let (turns, tokens): (i64, i64) = sqlx::query_as(
            "SELECT COALESCE(SUM(CASE WHEN hour = ? THEN agent_turns ELSE 0 END), 0), \
             COALESCE(SUM(tokens), 0) FROM thread_usage WHERE surface = ? AND team_id = ? \
             AND conversation = ? AND thread_root = ? AND day = ?",
        )
        .bind(hour_of(at))
        .bind(surface)
        .bind(team)
        .bind(conversation)
        .bind(root)
        .bind(day_of(at))
        .fetch_one(&self.pool)
        .await?;
        Ok(ThreadSpend {
            turns_this_hour: u32::try_from(turns).unwrap_or(u32::MAX),
            tokens_today: unsigned(tokens),
        })
    }

    /// Claims the one notice `agent` posts in `thread` that it reached the
    /// limit `kind`, which counts over `window`, in the window `at` falls
    /// in: true the first time, false after. One caller gets it.
    ///
    /// Claim before posting, and [release](Self::release_limit_notice) the
    /// claim if the post fails.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn claim_limit_notice(
        &self,
        agent: AgentId,
        thread: &ThreadKey,
        kind: &str,
        window: LimitWindow,
        at: OffsetDateTime,
    ) -> Result<bool> {
        let [surface, team, conversation, root] = thread_columns(thread);
        let result = sqlx::query(
            "INSERT INTO limit_notices (agent_id, surface, team_id, conversation, thread_root, \
             kind, window_start) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
        )
        .bind(agent.to_string())
        .bind(surface)
        .bind(team)
        .bind(conversation)
        .bind(root)
        .bind(kind)
        .bind(window.start(at))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Gives up a claim [`claim_limit_notice`](Self::claim_limit_notice)
    /// made, so the next message past the limit tries to post again.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn release_limit_notice(
        &self,
        agent: AgentId,
        thread: &ThreadKey,
        kind: &str,
        window: LimitWindow,
        at: OffsetDateTime,
    ) -> Result<()> {
        let [surface, team, conversation, root] = thread_columns(thread);
        sqlx::query(
            "DELETE FROM limit_notices WHERE agent_id = ? AND surface = ? AND team_id = ? \
             AND conversation = ? AND thread_root = ? AND kind = ? AND window_start = ?",
        )
        .bind(agent.to_string())
        .bind(surface)
        .bind(team)
        .bind(conversation)
        .bind(root)
        .bind(kind)
        .bind(window.start(at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Deletes `thread_usage` rows of days and `limit_notices` of windows
    /// that ended more than [`THREAD_USAGE_RETENTION`] before `now`.
    /// Returns how many rows of each it deleted.
    pub(crate) async fn sweep_thread_usage(&self, now: OffsetDateTime) -> Result<(u64, u64)> {
        let cutoff = now - THREAD_USAGE_RETENTION;
        let usage = sqlx::query("DELETE FROM thread_usage WHERE day < ?")
            .bind(day_of(cutoff))
            .execute(&self.pool)
            .await?
            .rows_affected();
        let notices = sqlx::query("DELETE FROM limit_notices WHERE window_start < ?")
            .bind(to_unix(cutoff))
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok((usage, notices))
    }
}

#[cfg(test)]
mod tests {
    use core_types::{ConvRef, SurfaceKind};

    use super::*;
    use crate::test_util::{agent, at, member_key, memory_store};

    const DAY: i64 = 86_400;
    const HOUR: i64 = 3_600;

    fn thread(root: Option<&str>) -> ThreadKey {
        ThreadKey {
            conv: ConvRef {
                surface: SurfaceKind::Slack,
                team: "T1".into(),
                conversation: "C1".into(),
            },
            root: root.map(Into::into),
        }
    }

    fn usage(input: u64, output: u64, cost: f64) -> TurnUsage {
        TurnUsage {
            input_tokens: input,
            output_tokens: output,
            cost: Ok(cost),
        }
    }

    async fn member(store: &Store, user: &str) -> MemberId {
        store
            .ensure_member(&member_key(user), user, at(1))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_member_is_billed_per_day_and_totals_count_from_a_day() {
        let store = memory_store().await;
        let (alice, bob) = (member(&store, "alice").await, member(&store, "bob").await);
        let helper = agent(&store, alice, "helper").await;
        let t = thread(Some("1.0"));
        let day1 = 10 * DAY + 5;
        store
            .record_turn_usage(alice, helper, &t, usage(100, 10, 0.5), false, at(day1))
            .await
            .unwrap();
        store
            .record_turn_usage(
                alice,
                helper,
                &t,
                usage(200, 20, 0.25),
                false,
                at(day1 + HOUR),
            )
            .await
            .unwrap();
        store
            .record_turn_usage(alice, helper, &t, usage(1, 2, 1.0), false, at(day1 + DAY))
            .await
            .unwrap();
        store
            .record_turn_usage(bob, helper, &t, usage(7, 7, 7.0), false, at(day1))
            .await
            .unwrap();

        let billed = store.member_usage(alice, at(11 * DAY)).await.unwrap();
        assert_eq!(
            billed.today,
            UsageTotals {
                turns: 1,
                input_tokens: 1,
                output_tokens: 2,
                cost_usd: 1.0,
            }
        );
        let month = billed.month;
        assert_eq!(
            (month.turns, month.tokens(), month.cost_usd),
            (3, 333, 1.75)
        );
        assert_eq!(
            store.member_usage(alice, at(12 * DAY)).await.unwrap().today,
            UsageTotals::default(),
            "nothing yet on a later day"
        );
        assert_eq!(
            store.member_usage(alice, at(31 * DAY)).await.unwrap(),
            MemberUsage::default(),
            "nor in a later month: 1970-02-01"
        );
        assert_eq!(
            store
                .member_usage(alice, at(10 * DAY))
                .await
                .unwrap()
                .month
                .turns,
            2,
            "a month counts up to the day asked about"
        );
    }

    #[tokio::test]
    async fn a_bad_cost_counts_as_zero_and_huge_token_counts_are_capped() {
        let store = memory_store().await;
        let alice = member(&store, "alice").await;
        let helper = agent(&store, alice, "helper").await;
        let t = thread(None);
        for cost in [f64::NAN, f64::INFINITY, -1.0] {
            store
                .record_turn_usage(
                    alice,
                    helper,
                    &t,
                    usage(u64::MAX, u64::MAX, cost),
                    false,
                    at(5),
                )
                .await
                .unwrap();
        }
        let totals = store.member_usage(alice, at(5)).await.unwrap().today;
        assert_eq!(totals.turns, 3);
        assert_eq!(totals.cost_usd, 0.0);
        assert_eq!(totals.input_tokens, 3 * MAX_TOKENS_PER_TURN);
        assert_eq!(
            store.thread_spend(&t, at(5)).await.unwrap().tokens_today,
            6 * MAX_TOKENS_PER_TURN
        );
    }

    #[tokio::test]
    async fn turns_of_unknown_cost_are_billed_nothing_and_counted_by_reason() {
        let store = memory_store().await;
        let alice = member(&store, "alice").await;
        let helper = agent(&store, alice, "helper").await;
        let t = thread(None);
        let turns = [
            Ok(0.5),
            Err(CostUnknown::TranscriptTooLarge),
            Err(CostUnknown::TranscriptTooLarge),
            Err(CostUnknown::ReusedContainer),
            Ok(0.25),
        ];
        for cost in turns {
            let used = TurnUsage {
                cost,
                ..usage(100, 10, 0.0)
            };
            store
                .record_turn_usage(alice, helper, &t, used, false, at(5))
                .await
                .unwrap();
        }
        let totals = store.member_usage(alice, at(5)).await.unwrap().today;
        assert_eq!(totals.turns, 5);
        assert_eq!(totals.cost_usd, 0.75);
        assert_eq!(totals.input_tokens, 500);
        let unbilled: Vec<(String, i64, i64, f64)> = sqlx::query_as(
            "SELECT cost_unknown, turns, input_tokens + output_tokens, cost_usd FROM usage \
             WHERE cost_unknown != '' ORDER BY cost_unknown",
        )
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert_eq!(
            unbilled,
            [
                ("reused_container".to_owned(), 1, 110, 0.0),
                ("transcript_too_large".to_owned(), 2, 220, 0.0),
            ]
        );
    }

    #[tokio::test]
    async fn every_reason_a_cost_is_unknown_is_one_the_table_takes() {
        use CostUnknown::*;
        let store = memory_store().await;
        let alice = member(&store, "alice").await;
        let helper = agent(&store, alice, "helper").await;
        let reasons = [
            NoResult,
            NoTotal,
            TotalOutOfRange,
            ReusedContainer,
            TranscriptTooLarge,
            TranscriptUnreadable,
            TranscriptUnrecognized,
        ];
        for reason in reasons {
            let used = TurnUsage {
                cost: Err(reason),
                ..usage(1, 1, 0.0)
            };
            store
                .record_turn_usage(alice, helper, &thread(None), used, false, at(5))
                .await
                .unwrap_or_else(|error| panic!("{reason}: {error}"));
        }
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM usage WHERE cost_unknown != ''")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(rows, 7);
        let other = sqlx::query(
            "INSERT INTO usage (member_id, day, turns, input_tokens, output_tokens, cost_usd, \
             cost_unknown) VALUES (?, 0, 1, 0, 0, 0, 'other')",
        )
        .bind(alice.to_string())
        .execute(&store.pool)
        .await;
        assert!(other.is_err(), "the table takes only the reasons it names");
    }

    #[tokio::test]
    async fn agents_turns_count_per_day_and_threads_per_hour_and_day() {
        let store = memory_store().await;
        let alice = member(&store, "alice").await;
        let (a, b) = (
            agent(&store, alice, "a-helper").await,
            agent(&store, alice, "b-helper").await,
        );
        let (one, other) = (thread(Some("1.0")), thread(Some("2.0")));
        let noon = 20 * DAY + 12 * HOUR;
        for (agent, thread, for_owner, seconds) in [
            (a, &one, false, noon),
            (a, &one, false, noon + 60),
            (b, &one, false, noon + 120),
            (a, &other, false, noon),
            (a, &one, false, noon - HOUR),
            (a, &one, true, noon + 180),
            (a, &one, false, noon - DAY),
        ] {
            store
                .record_turn_usage(
                    alice,
                    agent,
                    thread,
                    usage(10, 1, 0.0),
                    for_owner,
                    at(seconds),
                )
                .await
                .unwrap();
        }
        assert_eq!(
            store.capped_turns_on(a, at(noon)).await.unwrap(),
            4,
            "the owner's own turn isn't counted"
        );
        assert_eq!(store.capped_turns_on(b, at(noon)).await.unwrap(), 1);
        assert_eq!(store.capped_turns_on(a, at(noon - DAY)).await.unwrap(), 1);
        assert_eq!(
            store.thread_spend(&one, at(noon + 600)).await.unwrap(),
            ThreadSpend {
                turns_this_hour: 4,
                tokens_today: 55
            },
            "both agents' turns this hour, the owner's too, and the whole day's tokens"
        );
        assert_eq!(
            store.thread_spend(&other, at(noon)).await.unwrap(),
            ThreadSpend {
                turns_this_hour: 1,
                tokens_today: 11
            }
        );
        assert_eq!(
            store.thread_spend(&thread(None), at(noon)).await.unwrap(),
            ThreadSpend::default()
        );
    }

    #[tokio::test]
    async fn a_limit_notice_is_claimed_once_per_window_and_can_be_released() {
        let store = memory_store().await;
        let alice = member(&store, "alice").await;
        let helper = agent(&store, alice, "helper").await;
        let t = thread(Some("1.0"));
        assert!(
            store
                .claim_limit_notice(helper, &t, "daily", LimitWindow::Day, at(DAY))
                .await
                .unwrap()
        );
        assert!(
            !store
                .claim_limit_notice(helper, &t, "daily", LimitWindow::Day, at(DAY))
                .await
                .unwrap()
        );
        assert!(
            store
                .claim_limit_notice(helper, &t, "daily", LimitWindow::Day, at(2 * DAY))
                .await
                .unwrap(),
            "a new window"
        );
        assert!(
            store
                .claim_limit_notice(helper, &t, "thread_turns", LimitWindow::Day, at(DAY))
                .await
                .unwrap(),
            "another limit"
        );
        assert!(
            store
                .claim_limit_notice(helper, &thread(None), "daily", LimitWindow::Day, at(DAY))
                .await
                .unwrap(),
            "another thread"
        );
        store
            .release_limit_notice(helper, &t, "daily", LimitWindow::Day, at(DAY))
            .await
            .unwrap();
        assert!(
            store
                .claim_limit_notice(helper, &t, "daily", LimitWindow::Day, at(DAY))
                .await
                .unwrap()
        );
        let hourly = |seconds| {
            store.claim_limit_notice(helper, &t, "hourly", LimitWindow::Hour, at(seconds))
        };
        assert!(hourly(DAY + 60).await.unwrap());
        assert!(!hourly(DAY + 3_000).await.unwrap(), "the same hour");
        assert!(hourly(DAY + HOUR).await.unwrap(), "the next hour");
        assert!(
            !store
                .claim_limit_notice(helper, &t, "daily", LimitWindow::Day, at(DAY + 20 * HOUR))
                .await
                .unwrap(),
            "the same day"
        );
    }

    #[tokio::test]
    async fn old_thread_usage_and_notices_are_swept() {
        let store = memory_store().await;
        let alice = member(&store, "alice").await;
        let helper = agent(&store, alice, "helper").await;
        let t = thread(Some("1.0"));
        let now = 100 * DAY;
        for day in [96, 97, 98, 100] {
            store
                .record_turn_usage(alice, helper, &t, usage(1, 1, 0.0), false, at(day * DAY))
                .await
                .unwrap();
            store
                .claim_limit_notice(helper, &t, "daily", LimitWindow::Day, at(day * DAY))
                .await
                .unwrap();
        }
        let swept = store.sweep_expired(at(now)).await.unwrap();
        assert_eq!((swept.thread_usage, swept.limit_notices), (2, 2));
        assert_eq!(
            store.capped_turns_on(helper, at(98 * DAY)).await.unwrap(),
            1
        );
        assert_eq!(
            store
                .member_usage(alice, at(100 * DAY))
                .await
                .unwrap()
                .month
                .turns,
            4,
            "the meter itself is kept"
        );
    }
}

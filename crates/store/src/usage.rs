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
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TurnUsage {
    /// Input the model read fresh: uncached input and cache writes.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// The turn's cost in US dollars, as the CLI reckons it.
    pub cost_usd: f64,
}

impl TurnUsage {
    /// Input and output tokens together.
    pub fn tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
}

/// A member's usage over some days, from
/// [`member_usage_since`](Store::member_usage_since).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    /// Turns billed to them.
    pub turns: u64,
    /// Input tokens, as [`TurnUsage::input_tokens`] counts them.
    pub input_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// Cost in US dollars.
    pub cost_usd: f64,
}

impl UsageTotals {
    /// Input and output tokens together.
    pub fn tokens(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }
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
    /// the thread's hour for the agent, in one transaction.
    ///
    /// A cost that isn't a finite number of at least 0 counts as 0, and a
    /// turn's tokens are capped far above what any turn uses.
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
        at: OffsetDateTime,
    ) -> Result<()> {
        let cost = if usage.cost_usd.is_finite() && usage.cost_usd > 0.0 {
            usage.cost_usd
        } else {
            0.0
        };
        let (input, output) = (tokens(usage.input_tokens), tokens(usage.output_tokens));
        let [surface, team, conversation, root] = thread_columns(thread);
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "INSERT INTO usage (member_id, day, turns, input_tokens, output_tokens, cost_usd) \
             VALUES (?, ?, 1, ?, ?, ?) \
             ON CONFLICT (member_id, day) DO UPDATE SET turns = turns + 1, \
             input_tokens = input_tokens + excluded.input_tokens, \
             output_tokens = output_tokens + excluded.output_tokens, \
             cost_usd = cost_usd + excluded.cost_usd",
        )
        .bind(member.to_string())
        .bind(day_of(at))
        .bind(input)
        .bind(output)
        .bind(cost)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO thread_usage (surface, team_id, conversation, thread_root, day, hour, \
             agent_id, agent_turns, tokens) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?) \
             ON CONFLICT (surface, team_id, conversation, thread_root, day, hour, agent_id) \
             DO UPDATE SET agent_turns = agent_turns + 1, tokens = tokens + excluded.tokens",
        )
        .bind(surface)
        .bind(team)
        .bind(conversation)
        .bind(root)
        .bind(day_of(at))
        .bind(hour_of(at))
        .bind(agent.to_string())
        .bind(input.saturating_add(output))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// What was billed to `member` from the day `since` falls on through
    /// today: pass the start of today, or of this month.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn member_usage_since(
        &self,
        member: MemberId,
        since: OffsetDateTime,
    ) -> Result<UsageTotals> {
        let (turns, input, output, cost): (i64, i64, i64, f64) = sqlx::query_as(
            "SELECT COALESCE(SUM(turns), 0), COALESCE(SUM(input_tokens), 0), \
             COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cost_usd), 0.0) \
             FROM usage WHERE member_id = ? AND day >= ?",
        )
        .bind(member.to_string())
        .bind(day_of(since))
        .fetch_one(&self.pool)
        .await?;
        Ok(UsageTotals {
            turns: unsigned(turns),
            input_tokens: unsigned(input),
            output_tokens: unsigned(output),
            cost_usd: cost,
        })
    }

    /// The turns `agent` took on the day `at` falls on, in every thread and
    /// for every requester.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn agent_turns_on(&self, agent: AgentId, at: OffsetDateTime) -> Result<u32> {
        let turns: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(agent_turns), 0) FROM thread_usage \
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
    /// limit `kind` in the window starting at `window_start`: true the
    /// first time, false after. One caller gets it.
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
        window_start: OffsetDateTime,
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
        .bind(to_unix(window_start))
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
        window_start: OffsetDateTime,
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
        .bind(to_unix(window_start))
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
            cost_usd: cost,
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
            .record_turn_usage(alice, helper, &t, usage(100, 10, 0.5), at(day1))
            .await
            .unwrap();
        store
            .record_turn_usage(alice, helper, &t, usage(200, 20, 0.25), at(day1 + HOUR))
            .await
            .unwrap();
        store
            .record_turn_usage(alice, helper, &t, usage(1, 2, 1.0), at(day1 + DAY))
            .await
            .unwrap();
        store
            .record_turn_usage(bob, helper, &t, usage(7, 7, 7.0), at(day1))
            .await
            .unwrap();

        let today = store.member_usage_since(alice, at(11 * DAY)).await.unwrap();
        assert_eq!(
            today,
            UsageTotals {
                turns: 1,
                input_tokens: 1,
                output_tokens: 2,
                cost_usd: 1.0
            }
        );
        let both = store.member_usage_since(alice, at(10 * DAY)).await.unwrap();
        assert_eq!((both.turns, both.tokens(), both.cost_usd), (3, 333, 1.75));
        assert_eq!(
            store.member_usage_since(alice, at(12 * DAY)).await.unwrap(),
            UsageTotals::default(),
            "nothing yet on a later day"
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
                .record_turn_usage(alice, helper, &t, usage(u64::MAX, u64::MAX, cost), at(5))
                .await
                .unwrap();
        }
        let totals = store.member_usage_since(alice, at(0)).await.unwrap();
        assert_eq!(totals.turns, 3);
        assert_eq!(totals.cost_usd, 0.0);
        assert_eq!(totals.input_tokens, 3 * MAX_TOKENS_PER_TURN);
        assert_eq!(
            store.thread_spend(&t, at(5)).await.unwrap().tokens_today,
            6 * MAX_TOKENS_PER_TURN
        );
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
        for (agent, thread, seconds) in [
            (a, &one, noon),
            (a, &one, noon + 60),
            (b, &one, noon + 120),
            (a, &other, noon),
            (a, &one, noon - HOUR),
            (a, &one, noon - DAY),
        ] {
            store
                .record_turn_usage(alice, agent, thread, usage(10, 1, 0.0), at(seconds))
                .await
                .unwrap();
        }
        assert_eq!(store.agent_turns_on(a, at(noon)).await.unwrap(), 4);
        assert_eq!(store.agent_turns_on(b, at(noon)).await.unwrap(), 1);
        assert_eq!(store.agent_turns_on(a, at(noon - DAY)).await.unwrap(), 1);
        assert_eq!(
            store.thread_spend(&one, at(noon + 600)).await.unwrap(),
            ThreadSpend {
                turns_this_hour: 3,
                tokens_today: 44
            },
            "both agents' turns this hour, and the whole day's tokens"
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
                .claim_limit_notice(helper, &t, "daily", at(DAY))
                .await
                .unwrap()
        );
        assert!(
            !store
                .claim_limit_notice(helper, &t, "daily", at(DAY))
                .await
                .unwrap()
        );
        assert!(
            store
                .claim_limit_notice(helper, &t, "daily", at(2 * DAY))
                .await
                .unwrap(),
            "a new window"
        );
        assert!(
            store
                .claim_limit_notice(helper, &t, "thread_turns", at(DAY))
                .await
                .unwrap(),
            "another limit"
        );
        assert!(
            store
                .claim_limit_notice(helper, &thread(None), "daily", at(DAY))
                .await
                .unwrap(),
            "another thread"
        );
        store
            .release_limit_notice(helper, &t, "daily", at(DAY))
            .await
            .unwrap();
        assert!(
            store
                .claim_limit_notice(helper, &t, "daily", at(DAY))
                .await
                .unwrap()
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
                .record_turn_usage(alice, helper, &t, usage(1, 1, 0.0), at(day * DAY))
                .await
                .unwrap();
            store
                .claim_limit_notice(helper, &t, "daily", at(day * DAY))
                .await
                .unwrap();
        }
        let swept = store.sweep_expired(at(now)).await.unwrap();
        assert_eq!((swept.thread_usage, swept.limit_notices), (2, 2));
        assert_eq!(store.agent_turns_on(helper, at(98 * DAY)).await.unwrap(), 1);
        assert_eq!(
            store.member_usage_since(alice, at(0)).await.unwrap().turns,
            4,
            "the meter itself is kept"
        );
    }
}

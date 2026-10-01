//! `message_refs`: the messages agentd posted as agents, and the inbound
//! messages shown to each session's model, with their short ids.

use core_types::{
    AgentId, ConsentId, ConvRef, ConversationId, Hop, MemberId, MemberKey, MessageId, MsgRef,
    Requester, SessionId, SurfaceKind, TeamId, ThreadKey, TurnId,
};
use sqlx::SqliteConnection;
use time::OffsetDateTime;

use crate::{NewHandOff, Result, Store, StoreError, from_unix, parse_column, to_unix};

const TABLE: &str = "message_refs";

/// The columns every query reads, in [`Row`]'s order.
macro_rules! columns {
    () => {
        "session_id, short_id, surface, team_id, conversation, thread_root, platform_ref, \
         agent_id, turn_id, requester_member, requester_key, hop, posted_at, consent_id, \
         hands_off"
    };
}

/// A message to record in a session, for [`Store::record_message_ref`].
#[derive(Debug, Clone, Copy)]
pub struct NewMessageRef<'a> {
    /// The session the message is shown to, or that posted it.
    pub session: SessionId,
    /// The message.
    pub msg: &'a MsgRef,
    /// The root of the thread it is in, or `None` at a conversation's top
    /// level.
    pub thread_root: Option<&'a MessageId>,
    /// The agent agentd posted it as, or `None` for an inbound message.
    pub agent: Option<AgentId>,
    /// The turn that posted it, if a turn did.
    pub turn: Option<TurnId>,
    /// Who pays for it: the requester of the turn that posted it, or an
    /// inbound message's sender.
    pub requester: &'a Requester,
    /// The posting turn's hop, or 0 for an inbound message.
    pub hop: Hop,
    /// The consent whose private task's result or outcome the message
    /// reports, if it does: a mention in it starts no agent's turn.
    pub consent: Option<ConsentId>,
    /// Whether a mention in it hands off to the agent mentioned: a turn's
    /// post in the turn's own thread, never a private task's.
    pub hands_off: bool,
}

/// A `message_refs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRef {
    /// The session the row belongs to.
    pub session: SessionId,
    /// The message's short id in that session, counting from 1.
    pub short_id: u32,
    /// The message.
    pub msg: MsgRef,
    /// The root of the thread it is in, or `None` at the top level.
    pub thread_root: Option<MessageId>,
    /// The agent agentd posted it as, or `None` for an inbound message.
    pub agent: Option<AgentId>,
    /// The turn that posted it, if a turn did.
    pub turn: Option<TurnId>,
    /// Who pays for it: the requester of the turn that posted it, or an
    /// inbound message's sender.
    pub requester: Requester,
    /// The posting turn's hop, or 0 for an inbound message.
    pub hop: Hop,
    /// When the row was recorded.
    pub posted_at: OffsetDateTime,
    /// The consent whose private task's result or outcome the message
    /// reports, if it does.
    pub consent: Option<ConsentId>,
    /// Whether a mention in it hands off to the agent mentioned.
    pub hands_off: bool,
}

#[derive(sqlx::FromRow)]
struct Row {
    session_id: String,
    short_id: i64,
    surface: String,
    team_id: String,
    conversation: String,
    thread_root: String,
    platform_ref: String,
    agent_id: Option<String>,
    turn_id: Option<String>,
    requester_member: Option<String>,
    requester_key: String,
    hop: i64,
    posted_at: i64,
    consent_id: Option<String>,
    hands_off: i64,
}

fn corrupt(column: &'static str) -> StoreError {
    StoreError::Corrupt {
        table: TABLE,
        column,
    }
}

impl Row {
    fn into_ref(self) -> Result<MessageRef> {
        let conv = ConvRef {
            surface: parse_column::<SurfaceKind>(&self.surface, TABLE, "surface")?,
            team: TeamId::new(self.team_id),
            conversation: ConversationId::new(self.conversation),
        };
        Ok(MessageRef {
            session: parse_column(&self.session_id, TABLE, "session_id")?,
            short_id: u32::try_from(self.short_id).map_err(|_| corrupt("short_id"))?,
            msg: MsgRef {
                conv,
                id: MessageId::new(self.platform_ref),
            },
            thread_root: (!self.thread_root.is_empty()).then(|| MessageId::new(self.thread_root)),
            agent: self
                .agent_id
                .map(|agent| parse_column(&agent, TABLE, "agent_id"))
                .transpose()?,
            turn: self
                .turn_id
                .map(|turn| parse_column(&turn, TABLE, "turn_id"))
                .transpose()?,
            requester: Requester {
                member: self
                    .requester_member
                    .map(|member| parse_column::<MemberId>(&member, TABLE, "requester_member"))
                    .transpose()?,
                key: parse_column::<MemberKey>(&self.requester_key, TABLE, "requester_key")?,
                outside: None,
            },
            hop: Hop(u8::try_from(self.hop).map_err(|_| corrupt("hop"))?),
            posted_at: from_unix(self.posted_at, TABLE, "posted_at")?,
            consent: self
                .consent_id
                .map(|consent| parse_column(&consent, TABLE, "consent_id"))
                .transpose()?,
            hands_off: self.hands_off != 0,
        })
    }
}

/// The row `session` has for `msg`, read inside a transaction.
async fn in_session(
    conn: &mut SqliteConnection,
    session: SessionId,
    msg: &MsgRef,
) -> Result<Option<MessageRef>> {
    let row: Option<Row> = sqlx::query_as(concat!(
        "SELECT ",
        columns!(),
        " FROM message_refs WHERE session_id = ? AND surface = ? AND team_id = ? \
         AND conversation = ? AND platform_ref = ?"
    ))
    .bind(session.to_string())
    .bind(msg.conv.surface.as_str())
    .bind(msg.conv.team.as_str())
    .bind(msg.conv.conversation.as_str())
    .bind(msg.id.as_str())
    .fetch_optional(conn)
    .await?;
    row.map(Row::into_ref).transpose()
}

/// Gives the session's inbound row for `new.msg` the post's attribution
/// from `new`, inside a transaction. The partial unique index still refuses
/// it if another session's row attributes the message.
async fn attribute(conn: &mut SqliteConnection, new: &NewMessageRef<'_>) -> Result<MessageRef> {
    let row: Row = sqlx::query_as(concat!(
        "UPDATE message_refs SET agent_id = ?, turn_id = ?, requester_member = ?, \
         requester_key = ?, hop = ?, consent_id = ?, hands_off = ? WHERE session_id = ? \
         AND surface = ? \
         AND team_id = ? AND conversation = ? AND platform_ref = ? RETURNING ",
        columns!()
    ))
    .bind(new.agent.map(|agent| agent.to_string()))
    .bind(new.turn.map(|turn| turn.to_string()))
    .bind(new.requester.member.map(|member| member.to_string()))
    .bind(new.requester.key.to_string())
    .bind(i64::from(new.hop.0))
    .bind(new.consent.map(|consent| consent.to_string()))
    .bind(i64::from(new.hands_off))
    .bind(new.session.to_string())
    .bind(new.msg.conv.surface.as_str())
    .bind(new.msg.conv.team.as_str())
    .bind(new.msg.conv.conversation.as_str())
    .bind(new.msg.id.as_str())
    .fetch_one(conn)
    .await?;
    row.into_ref()
}

impl Store {
    /// Records `new` in its session with the session's next short id, and
    /// returns the row. A message the session already has keeps its row and
    /// short id, returned unchanged, except that an inbound row (no agent)
    /// takes `new`'s agent, turn, requester and hop when `new` names an
    /// agent: the session was shown the message before it recorded posting
    /// it, and the post's attribution must not be lost.
    ///
    /// It runs in one `BEGIN IMMEDIATE` transaction, so concurrent calls
    /// for one session never hand out one short id twice.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if a query fails, including when `new`
    /// names an agent and the message is already recorded as posted in
    /// another session; [`StoreError::Corrupt`] if a row doesn't parse.
    pub async fn record_message_ref(
        &self,
        new: &NewMessageRef<'_>,
        now: OffsetDateTime,
    ) -> Result<MessageRef> {
        Ok(self.record_post(new, now, &[]).await?.0)
    }

    /// Records `new` as [`record_message_ref`](Self::record_message_ref)
    /// does, and in the same transaction a `hand_offs` row for each of
    /// `hand_offs`, so a post that hands off is never recorded without its
    /// hand-offs. Returns the row, and the hand-offs' ids in order.
    ///
    /// # Errors
    ///
    /// As [`record_message_ref`](Self::record_message_ref), and
    /// [`StoreError::Database`] if a hand-off can't be inserted, as when
    /// its agent doesn't exist; then nothing is recorded.
    pub async fn record_post(
        &self,
        new: &NewMessageRef<'_>,
        now: OffsetDateTime,
        hand_offs: &[NewHandOff<'_>],
    ) -> Result<(MessageRef, Vec<i64>)> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = match in_session(&mut tx, new.session, new.msg).await? {
            Some(found) => match (found.agent, new.agent) {
                (None, Some(_)) => attribute(&mut tx, new).await?,
                _ => found,
            },
            None => insert(&mut tx, new, now).await?,
        };
        let mut ids = Vec::with_capacity(hand_offs.len());
        for hand_off in hand_offs {
            ids.push(crate::hand_offs::insert(&mut *tx, hand_off).await?);
        }
        tx.commit().await?;
        Ok((row, ids))
    }
}

/// Inserts `new`, posted at `now`, as the next short id of its session.
async fn insert(
    tx: &mut SqliteConnection,
    new: &NewMessageRef<'_>,
    now: OffsetDateTime,
) -> Result<MessageRef> {
    let row: Row = sqlx::query_as(concat!(
        "INSERT INTO message_refs (session_id, short_id, surface, team_id, conversation, \
             thread_root, platform_ref, agent_id, turn_id, requester_member, requester_key, hop, \
             posted_at, consent_id, hands_off) \
             SELECT ?1, COALESCE(MAX(short_id), 0) + 1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, \
             ?12, ?13, ?14 FROM message_refs WHERE session_id = ?1 RETURNING ",
        columns!()
    ))
    .bind(new.session.to_string())
    .bind(new.msg.conv.surface.as_str())
    .bind(new.msg.conv.team.as_str())
    .bind(new.msg.conv.conversation.as_str())
    .bind(new.thread_root.map_or("", MessageId::as_str))
    .bind(new.msg.id.as_str())
    .bind(new.agent.map(|agent| agent.to_string()))
    .bind(new.turn.map(|turn| turn.to_string()))
    .bind(new.requester.member.map(|member| member.to_string()))
    .bind(new.requester.key.to_string())
    .bind(i64::from(new.hop.0))
    .bind(to_unix(now))
    .bind(new.consent.map(|consent| consent.to_string()))
    .bind(i64::from(new.hands_off))
    .fetch_one(&mut *tx)
    .await?;
    row.into_ref()
}

impl Store {
    /// Whether agentd posted anything for consent `consent`: a private
    /// task's result, or its outcome, each of which is its last word.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn consent_posted(&self, consent: ConsentId) -> Result<bool> {
        let found: Option<i64> =
            sqlx::query_scalar("SELECT 1 FROM message_refs WHERE consent_id = ? LIMIT 1")
                .bind(consent.to_string())
                .fetch_optional(&self.pool)
                .await?;
        Ok(found.is_some())
    }

    /// The row of `msg` if agentd posted it as an agent: the one that
    /// attributes it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the row doesn't parse.
    pub async fn posted_message_ref(&self, msg: &MsgRef) -> Result<Option<MessageRef>> {
        let row: Option<Row> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM message_refs WHERE surface = ? AND team_id = ? AND conversation = ? \
             AND platform_ref = ? AND agent_id IS NOT NULL"
        ))
        .bind(msg.conv.surface.as_str())
        .bind(msg.conv.team.as_str())
        .bind(msg.conv.conversation.as_str())
        .bind(msg.id.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(Row::into_ref).transpose()
    }

    /// The row `session` has for `msg`, if it recorded one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the row doesn't parse.
    pub async fn session_message_ref(
        &self,
        session: SessionId,
        msg: &MsgRef,
    ) -> Result<Option<MessageRef>> {
        let mut conn = self.pool.acquire().await?;
        in_session(&mut conn, session, msg).await
    }

    /// The message with short id `short_id` in `session`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if the row doesn't parse.
    pub async fn message_ref_by_short_id(
        &self,
        session: SessionId,
        short_id: u32,
    ) -> Result<Option<MessageRef>> {
        let row: Option<Row> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM message_refs WHERE session_id = ? AND short_id = ?"
        ))
        .bind(session.to_string())
        .bind(i64::from(short_id))
        .fetch_optional(&self.pool)
        .await?;
        row.map(Row::into_ref).transpose()
    }

    /// Deletes the inbound rows of `session` with these short ids, and
    /// returns how many went: messages recorded for a turn that never
    /// reached the model, which the session's next turn must show again.
    /// A row with `agent_id` set is kept.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if a query fails; nothing is deleted then.
    pub async fn forget_message_refs(&self, session: SessionId, short_ids: &[u32]) -> Result<u64> {
        let mut tx = self.pool.begin().await?;
        let mut deleted = 0;
        for short_id in short_ids {
            deleted += sqlx::query(
                "DELETE FROM message_refs WHERE session_id = ? AND short_id = ? \
                 AND agent_id IS NULL",
            )
            .bind(session.to_string())
            .bind(i64::from(*short_id))
            .execute(&mut *tx)
            .await?
            .rows_affected();
        }
        tx.commit().await?;
        Ok(deleted)
    }

    /// The messages agentd posted as `agent` in `thread` from sessions
    /// other than `session`, which `session` hasn't recorded yet, oldest
    /// first. Other threads of the conversation are left out.
    ///
    /// These are what the session's transcript lacks of the agent's own
    /// posts: a private task's result and its declined or expired outcomes
    /// are posted to the thread from other sessions.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails, [`StoreError::Corrupt`]
    /// if a row doesn't parse.
    pub async fn posted_elsewhere(
        &self,
        agent: AgentId,
        thread: &ThreadKey,
        session: SessionId,
    ) -> Result<Vec<MessageRef>> {
        let rows: Vec<Row> = sqlx::query_as(concat!(
            "SELECT ",
            columns!(),
            " FROM message_refs AS posted WHERE agent_id = ?1 AND surface = ?2 AND team_id = ?3 \
             AND conversation = ?4 AND thread_root = ?5 AND session_id != ?6 \
             AND NOT EXISTS (SELECT 1 FROM message_refs AS shown WHERE shown.session_id = ?6 \
             AND shown.surface = posted.surface AND shown.team_id = posted.team_id \
             AND shown.conversation = posted.conversation \
             AND shown.platform_ref = posted.platform_ref) \
             ORDER BY posted_at, rowid"
        ))
        .bind(agent.to_string())
        .bind(thread.conv.surface.as_str())
        .bind(thread.conv.team.as_str())
        .bind(thread.conv.conversation.as_str())
        .bind(thread.root.as_ref().map_or("", MessageId::as_str))
        .bind(session.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(Row::into_ref).collect()
    }
}

#[cfg(test)]
mod tests {
    use core_types::UserId;

    use super::*;
    use crate::test_util::*;

    fn conv(id: &str) -> ConvRef {
        ConvRef {
            surface: SurfaceKind::Slack,
            team: TeamId::new("T1"),
            conversation: ConversationId::new(id),
        }
    }

    fn msg(conv_id: &str, id: &str) -> MsgRef {
        MsgRef {
            conv: conv(conv_id),
            id: MessageId::new(id),
        }
    }

    fn requester(user: &str, member: Option<MemberId>) -> Requester {
        Requester {
            member,
            key: MemberKey {
                surface: SurfaceKind::Slack,
                team: TeamId::new("T1"),
                user: UserId::new(user),
            },
            outside: None,
        }
    }

    fn inbound<'a>(
        session: SessionId,
        msg: &'a MsgRef,
        root: Option<&'a MessageId>,
        requester: &'a Requester,
    ) -> NewMessageRef<'a> {
        NewMessageRef {
            session,
            msg,
            thread_root: root,
            agent: None,
            turn: None,
            requester,
            hop: Hop::ZERO,
            consent: None,
            hands_off: false,
        }
    }

    #[tokio::test]
    async fn short_ids_count_per_session_and_a_message_keeps_its_own() {
        let store = memory_store().await;
        let (one, two) = (SessionId::new_v4(), SessionId::new_v4());
        let root = MessageId::new("1.1");
        let person = requester("U1", None);
        let (a, b) = (msg("C1", "1.1"), msg("C1", "1.2"));
        let first = store
            .record_message_ref(&inbound(one, &a, Some(&root), &person), at(10))
            .await
            .unwrap();
        assert_eq!(first.short_id, 1);
        assert_eq!(first.msg, a);
        assert_eq!(first.thread_root, Some(root.clone()));
        assert_eq!(first.requester, person);
        assert_eq!(first.hop, Hop::ZERO);
        assert_eq!(first.posted_at, at(10));
        let second = store
            .record_message_ref(&inbound(one, &b, Some(&root), &person), at(11))
            .await
            .unwrap();
        assert_eq!(second.short_id, 2);
        let again = store
            .record_message_ref(&inbound(one, &a, Some(&root), &person), at(12))
            .await
            .unwrap();
        assert_eq!(again, first);
        let elsewhere = store
            .record_message_ref(&inbound(two, &b, Some(&root), &person), at(13))
            .await
            .unwrap();
        assert_eq!(elsewhere.short_id, 1);
        assert_eq!(
            store.message_ref_by_short_id(one, 2).await.unwrap(),
            Some(second.clone())
        );
        assert_eq!(store.message_ref_by_short_id(one, 3).await.unwrap(), None);
        assert_eq!(
            store.session_message_ref(two, &b).await.unwrap(),
            Some(elsewhere)
        );
        assert_eq!(store.session_message_ref(two, &a).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_slack_ts_is_unique_only_within_its_channel() {
        let store = memory_store().await;
        let session = SessionId::new_v4();
        let person = requester("U1", None);
        let (here, there) = (msg("C1", "1.1"), msg("C2", "1.1"));
        let a = store
            .record_message_ref(&inbound(session, &here, None, &person), at(1))
            .await
            .unwrap();
        let b = store
            .record_message_ref(&inbound(session, &there, None, &person), at(1))
            .await
            .unwrap();
        assert_ne!(a.short_id, b.short_id);
        assert_eq!(a.thread_root, None);
    }

    #[tokio::test]
    async fn a_post_and_its_hand_offs_are_recorded_together_or_not_at_all() {
        let store = memory_store().await;
        let owner = store
            .ensure_member(&member_key("u1"), "Ada", at(1))
            .await
            .unwrap();
        let writer = agent(&store, owner, "writer").await;
        let payer = requester("U1", Some(owner));
        let (first, second) = (msg("C1", "1.1"), msg("C1", "2.1"));
        let post = |msg| NewMessageRef {
            session: SessionId::new_v4(),
            msg,
            thread_root: None,
            agent: Some(writer),
            turn: Some(TurnId::new_v4()),
            requester: &payer,
            hop: Hop::ZERO,
            consent: None,
            hands_off: true,
        };
        let hand_off = |agent| NewHandOff {
            agent,
            event_json: "{}",
            created_at: at(2),
            due_at: at(3),
        };
        let (row, ids) = store
            .record_post(&post(&first), at(2), &[hand_off(writer)])
            .await
            .unwrap();
        assert!(row.hands_off);
        let due = store
            .take_due_hand_offs(at(3), std::time::Duration::from_secs(60), at(1), 10, &[])
            .await
            .unwrap();
        assert_eq!(due.taken.iter().map(|h| h.id).collect::<Vec<_>>(), ids);

        assert!(
            store
                .record_post(&post(&second), at(2), &[hand_off(AgentId::new_v4())])
                .await
                .is_err()
        );
        assert_eq!(
            store.posted_message_ref(&second).await.unwrap(),
            None,
            "a hand-off that can't be recorded takes its post's record with it"
        );
    }

    #[tokio::test]
    async fn a_posted_message_is_attributed_once_and_found_by_its_ref() {
        let store = memory_store().await;
        let (session, other) = (SessionId::new_v4(), SessionId::new_v4());
        let agent = AgentId::new_v4();
        let turn = TurnId::new_v4();
        let member = MemberId::new_v4();
        let payer = requester("U1", Some(member));
        let reply = msg("C1", "2.1");
        let root = MessageId::new("1.1");
        let posted = store
            .record_message_ref(
                &NewMessageRef {
                    session,
                    msg: &reply,
                    thread_root: Some(&root),
                    agent: Some(agent),
                    turn: Some(turn),
                    requester: &payer,
                    hop: Hop(2),
                    consent: None,
                    hands_off: true,
                },
                at(5),
            )
            .await
            .unwrap();
        assert_eq!(posted.agent, Some(agent));
        assert_eq!(posted.turn, Some(turn));
        assert_eq!(posted.requester, payer);
        assert_eq!(posted.hop, Hop(2));
        assert!(posted.hands_off);
        assert_eq!(
            store.posted_message_ref(&reply).await.unwrap(),
            Some(posted.clone())
        );

        let sender = requester("UBOT", None);
        let shown = store
            .record_message_ref(&inbound(other, &reply, Some(&root), &sender), at(6))
            .await
            .unwrap();
        assert_eq!(shown.agent, None);
        assert_eq!(
            store.posted_message_ref(&reply).await.unwrap(),
            Some(posted)
        );

        let twice = store
            .record_message_ref(
                &NewMessageRef {
                    session: other,
                    msg: &msg("C1", "3.1"),
                    thread_root: None,
                    agent: Some(agent),
                    turn: None,
                    requester: &payer,
                    hop: Hop::ZERO,
                    consent: None,
                    hands_off: false,
                },
                at(7),
            )
            .await
            .unwrap();
        let clash = store
            .record_message_ref(
                &NewMessageRef {
                    session: SessionId::new_v4(),
                    msg: &twice.msg,
                    thread_root: None,
                    agent: Some(agent),
                    turn: None,
                    requester: &payer,
                    hop: Hop::ZERO,
                    consent: None,
                    hands_off: false,
                },
                at(8),
            )
            .await;
        assert!(matches!(clash, Err(StoreError::Database(_))), "{clash:?}");
        assert_eq!(
            store.posted_message_ref(&msg("C1", "9.9")).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn forgotten_rows_are_shown_again_and_posts_are_kept() {
        let store = memory_store().await;
        let (session, other) = (SessionId::new_v4(), SessionId::new_v4());
        let person = requester("U1", None);
        let (a, b, c) = (msg("C1", "1.1"), msg("C1", "1.2"), msg("C1", "1.3"));
        let kept = store
            .record_message_ref(&inbound(session, &a, None, &person), at(1))
            .await
            .unwrap();
        let posted = store
            .record_message_ref(
                &NewMessageRef {
                    agent: Some(AgentId::new_v4()),
                    ..inbound(session, &b, None, &person)
                },
                at(2),
            )
            .await
            .unwrap();
        let shown = store
            .record_message_ref(&inbound(session, &c, None, &person), at(3))
            .await
            .unwrap();
        let elsewhere = store
            .record_message_ref(&inbound(other, &c, None, &person), at(3))
            .await
            .unwrap();
        let forgotten = store
            .forget_message_refs(session, &[posted.short_id, shown.short_id])
            .await
            .unwrap();
        assert_eq!(forgotten, 1);
        assert_eq!(store.session_message_ref(session, &c).await.unwrap(), None);
        assert_eq!(
            store.session_message_ref(session, &a).await.unwrap(),
            Some(kept)
        );
        assert_eq!(
            store.posted_message_ref(&b).await.unwrap(),
            Some(posted.clone())
        );
        assert_eq!(
            store.session_message_ref(other, &c).await.unwrap(),
            Some(elsewhere)
        );
        let again = store
            .record_message_ref(&inbound(session, &c, None, &person), at(4))
            .await
            .unwrap();
        assert_eq!(again.short_id, shown.short_id);
    }

    #[tokio::test]
    async fn recording_a_shown_message_as_posted_attributes_its_row() {
        let store = memory_store().await;
        let (session, other) = (SessionId::new_v4(), SessionId::new_v4());
        let agent = AgentId::new_v4();
        let turn = TurnId::new_v4();
        let payer = requester("U1", Some(MemberId::new_v4()));
        let sender = requester("UBOT", None);
        let root = MessageId::new("1.1");
        let reply = msg("C1", "2.1");
        let posted = |session| NewMessageRef {
            session,
            msg: &reply,
            thread_root: Some(&root),
            agent: Some(agent),
            turn: Some(turn),
            requester: &payer,
            hop: Hop(3),
            consent: None,
            hands_off: true,
        };
        store
            .record_message_ref(&inbound(session, &msg("C1", "1.1"), None, &sender), at(1))
            .await
            .unwrap();
        let shown = store
            .record_message_ref(&inbound(session, &reply, Some(&root), &sender), at(2))
            .await
            .unwrap();
        assert_eq!(shown.short_id, 2);
        assert_eq!(store.posted_message_ref(&reply).await.unwrap(), None);

        let attributed = store
            .record_message_ref(&posted(session), at(3))
            .await
            .unwrap();
        assert_eq!(attributed.short_id, shown.short_id);
        assert_eq!(attributed.posted_at, shown.posted_at);
        assert_eq!(attributed.thread_root, Some(root.clone()));
        assert_eq!(attributed.agent, Some(agent));
        assert_eq!(attributed.turn, Some(turn));
        assert_eq!(attributed.requester, payer);
        assert_eq!(attributed.hop, Hop(3));
        assert!(attributed.hands_off, "the post's own row hands off");
        assert!(!shown.hands_off);
        assert_eq!(
            store.posted_message_ref(&reply).await.unwrap(),
            Some(attributed.clone())
        );

        let again = store
            .record_message_ref(&inbound(session, &reply, Some(&root), &sender), at(4))
            .await
            .unwrap();
        assert_eq!(again, attributed);

        store
            .record_message_ref(&inbound(other, &reply, Some(&root), &sender), at(5))
            .await
            .unwrap();
        let clash = store.record_message_ref(&posted(other), at(6)).await;
        assert!(matches!(clash, Err(StoreError::Database(_))), "{clash:?}");
        assert_eq!(
            store
                .session_message_ref(other, &reply)
                .await
                .unwrap()
                .unwrap()
                .agent,
            None
        );
        assert_eq!(
            store.posted_message_ref(&reply).await.unwrap(),
            Some(attributed)
        );
    }

    #[tokio::test]
    async fn posted_elsewhere_finds_the_agents_posts_in_this_thread_the_session_lacks() {
        let store = memory_store().await;
        let (channel, private, other_thread) = (
            SessionId::new_v4(),
            SessionId::new_v4(),
            SessionId::new_v4(),
        );
        let (agent, someone_else) = (AgentId::new_v4(), AgentId::new_v4());
        let payer = requester("U1", None);
        let root = MessageId::new("1.1");
        let thread = ThreadKey {
            conv: conv("C1"),
            root: Some(root.clone()),
        };
        let post = |session, id: &'static str, agent, root: Option<&'static str>| {
            let store = store.clone();
            let payer = payer.clone();
            async move {
                let msg = msg("C1", id);
                let root = root.map(MessageId::new);
                store
                    .record_message_ref(
                        &NewMessageRef {
                            session,
                            msg: &msg,
                            thread_root: root.as_ref(),
                            agent: Some(agent),
                            turn: None,
                            requester: &payer,
                            hop: Hop::ZERO,
                            consent: None,
                            hands_off: false,
                        },
                        at(20),
                    )
                    .await
                    .unwrap()
            }
        };
        post(channel, "2.1", agent, Some("1.1")).await;
        let result = post(private, "3.1", agent, Some("1.1")).await;
        let outcome = post(private, "3.2", agent, Some("1.1")).await;
        post(other_thread, "4.1", agent, Some("9.9")).await;
        post(other_thread, "4.2", agent, None).await;
        post(private, "5.1", someone_else, Some("1.1")).await;

        let found = store
            .posted_elsewhere(agent, &thread, channel)
            .await
            .unwrap();
        assert_eq!(found, vec![result.clone(), outcome.clone()]);

        store
            .record_message_ref(&inbound(channel, &result.msg, Some(&root), &payer), at(30))
            .await
            .unwrap();
        let found = store
            .posted_elsewhere(agent, &thread, channel)
            .await
            .unwrap();
        assert_eq!(found, vec![outcome]);

        let top = ThreadKey {
            conv: conv("C1"),
            root: None,
        };
        let found = store.posted_elsewhere(agent, &top, channel).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].msg.id, MessageId::new("4.2"));
    }

    #[tokio::test]
    async fn concurrent_records_in_one_session_get_distinct_short_ids() {
        let dir = TempDir::new();
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let session = SessionId::new_v4();
        let tasks: Vec<_> = (0..16)
            .map(|i| {
                let store = store.clone();
                tokio::spawn(async move {
                    let msg = msg("C1", &format!("1.{i}"));
                    let person = requester("U1", None);
                    store
                        .record_message_ref(&inbound(session, &msg, None, &person), at(1))
                        .await
                        .unwrap()
                        .short_id
                })
            })
            .collect();
        let mut ids = Vec::new();
        for task in tasks {
            ids.push(task.await.unwrap());
        }
        ids.sort_unstable();
        assert_eq!(ids, (1..=16).collect::<Vec<u32>>());
    }

    #[tokio::test]
    async fn a_row_that_does_not_parse_is_corrupt() {
        let store = memory_store().await;
        let session = SessionId::new_v4();
        let person = requester("U1", None);
        let a = msg("C1", "1.1");
        store
            .record_message_ref(&inbound(session, &a, None, &person), at(1))
            .await
            .unwrap();
        sqlx::query("UPDATE message_refs SET requester_key = 'nope'")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            store.session_message_ref(session, &a).await,
            Err(StoreError::Corrupt {
                column: "requester_key",
                ..
            })
        ));
    }
}

//! The session commands: `sessions <name>` and `reset <name> [here]`, for
//! an agent's owner.
//!
//! Both act on the agent's sessions in use ([`Store::sessions_in_use`]):
//! the live ones (not reset) that have had a turn, have one running, or
//! have a warm container. A live session that never had a turn, such as
//! the one a reset put in place of another, has nothing to show or to
//! reset.
//!
//! - `sessions` lists them, most recently active first, with where each
//!   one is (a link where one can be built and the owner can open it),
//!   when its last turn ended, and whether its container is warm on this
//!   instance.
//! - `reset` resets every one of them, or with `here` those of the
//!   conversation the command was sent in: every thread of a channel, or a
//!   DM's one session. `here` needs a conversation an agent answers in, so
//!   it is refused in the direct message with the manager bot. Every reset
//!   joins its session's queue at once, behind the turns queued before it
//!   ([`SessionManager::reset`]), so a message sent after the command
//!   starts the new conversation. It stops the warm process and container
//!   first, and gives the conversation a new session id, which its next
//!   turn starts with `--session-id`. Only [`store::RESETS_AT_ONCE`] resets
//!   write to the store at once, so a big reset doesn't take the whole
//!   store pool. The reply comes as soon as the resets are queued, and
//!   waiting for them to end is a [`FollowUp`], which holds up none of the
//!   owner's later commands. A session whose container can't be stopped
//!   isn't reset, and the owner is told in a direct message from the
//!   manager bot.
//!
//! The runner reaches the commands through [`SessionControl`], which
//! [`Turns`](crate::pipeline::Turns) hands over when it starts. Without a
//! runner (no `[sandbox]`, or once it has stopped) a reset only marks the
//! session reset in the store, and no container is warm. Another agentd
//! instance's warm containers aren't seen: its next turn on a reset session
//! finds it reset and moves to the replacement, and the idle reaper stops
//! the old container.
//!
//! [`Store::sessions_in_use`]: store::Store::sessions_in_use

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::Poll;

use async_trait::async_trait;
use core_types::{ConvRef, ConversationId, MemberKey, ScopeKey, SessionId, SurfaceKind};
use futures::future::{self, Future};
use runner::{RunnerError, SessionManager, TurnHooks};
use store::{Session, SessionKind};
use surface_rocketchat::rest::RoomType;
use time::OffsetDateTime;
use time::macros::format_description;

use super::agents::no_such_agent;
use super::{Commands, Failure, FollowUp, Origin};

/// The most sessions `sessions` lists.
pub const MAX_LISTED: usize = 20;

/// What the session commands need from the runner.
#[async_trait]
pub trait SessionControl: Send + Sync {
    /// Resets `session` once the turns queued before it have run, stopping
    /// its warm process and container first. Returns its replacement, or
    /// `None` for a private task's session and one already reset. The reset
    /// joins the session's queue when the future is first polled.
    ///
    /// # Errors
    ///
    /// [`RunnerError::Sandbox`] if the warm container couldn't be stopped,
    /// and the session isn't reset; [`RunnerError::Store`].
    async fn reset(&self, session: SessionId) -> Result<Option<Session>, RunnerError>;

    /// The sessions that have a warm container, or a turn running.
    fn warm_sessions(&self) -> Vec<SessionId>;
}

#[async_trait]
impl<H: TurnHooks> SessionControl for SessionManager<H> {
    async fn reset(&self, session: SessionId) -> Result<Option<Session>, RunnerError> {
        SessionManager::reset(self, session).await
    }

    fn warm_sessions(&self) -> Vec<SessionId> {
        SessionManager::warm_sessions(self)
    }
}

/// What `reset` did.
#[derive(Debug, PartialEq, Eq)]
struct Resets {
    done: usize,
    failed: usize,
}

fn sessions_word(count: usize) -> &'static str {
    if count == 1 { "session" } else { "sessions" }
}

/// What the owner is told when some of the resets `asked` for `name`'s
/// sessions failed; `None` when none did.
fn failed_resets(resets: &Resets, name: &str, asked: &str) -> Option<String> {
    let what = match *resets {
        Resets { failed: 0, .. } => return None,
        Resets { done: 0, failed: 1 } => format!("`{name}`'s session"),
        Resets { done: 0, failed } => format!("any of `{name}`'s {failed} sessions"),
        Resets { done, failed } => format!(
            "{failed} of `{name}`'s {} sessions; the other {done} {} reset",
            done + failed,
            if done == 1 { "is" } else { "are" }
        ),
    };
    Some(format!(
        "{asked} couldn't reset {what}. Please send it again in a minute."
    ))
}

impl Commands {
    /// Uses `sessions` for `sessions` and `reset` from now on, for as long
    /// as it lives. Holding it weakly leaves the runner's lifetime to its
    /// owner.
    pub fn use_sessions(&self, sessions: Weak<dyn SessionControl>) {
        *self
            .inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sessions);
    }

    fn session_control(&self) -> Option<Arc<dyn SessionControl>> {
        self.inner
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(Weak::upgrade)
    }

    /// The sessions warm on this instance's runner, if there is one.
    fn warm_sessions(control: Option<&dyn SessionControl>) -> Vec<SessionId> {
        control.map_or_else(Vec::new, SessionControl::warm_sessions)
    }

    pub(super) async fn sessions(
        &self,
        key: &MemberKey,
        name: &str,
        origin: &Origin,
    ) -> Result<String, Failure> {
        let Some(agent) = self.own_agent(key, name).await? else {
            return Ok(no_such_agent(name));
        };
        let warm = Self::warm_sessions(self.session_control().as_deref());
        let store = &self.inner.store;
        let sessions = store
            .sessions_in_use(agent.id, &warm, Some(MAX_LISTED))
            .await?;
        if sessions.is_empty() {
            return Ok(format!("`{name}` has no sessions yet."));
        }
        let total = if sessions.len() < MAX_LISTED {
            sessions.len()
        } else {
            store
                .count_sessions_in_use(agent.id, &warm)
                .await?
                .max(sessions.len())
        };
        let mut reply = if total > sessions.len() {
            format!(
                "`{name}`'s {} most recent sessions of {total}:",
                sessions.len()
            )
        } else {
            format!(
                "`{name}`'s {} {}, most recent first:",
                total,
                sessions_word(total)
            )
        };
        let warm: HashSet<SessionId> = warm.into_iter().collect();
        let mut rooms = HashMap::new();
        for session in &sessions {
            let last = session.last_turn_at.map_or_else(
                || "no turn finished yet".to_owned(),
                |at| format!("last turn {}", when(at)),
            );
            let state = if warm.contains(&session.id) {
                "warm"
            } else {
                "cold"
            };
            reply.push_str(&format!("\n- {}, {last}, {state}", place(session)));
            if let Some(link) = self.link(session, &mut rooms).await {
                reply.push_str(": ");
                reply.push_str(&link);
            }
        }
        reply.push_str(&format!(
            "\n\nStart them all over with {}, or one conversation's with {} there.",
            origin.command(&format!("reset {name}")),
            reset_here(key.surface, name),
        ));
        Ok(reply)
    }

    pub(super) async fn reset(
        &self,
        key: &MemberKey,
        name: &str,
        here: bool,
        origin: &Origin,
    ) -> Result<(String, FollowUp), Failure> {
        let done = |reply: String| Ok((reply, FollowUp::default()));
        let Some(agent) = self.own_agent(key, name).await? else {
            return done(no_such_agent(name));
        };
        let conv = match (here, origin.conversation(key)) {
            (false, _) => None,
            (true, Some(conv)) => Some(conv),
            (true, None) => return done(here_elsewhere(key.surface, name, origin)),
        };
        let control = self.session_control();
        let warm = Self::warm_sessions(control.as_deref());
        let ids: Vec<SessionId> = self
            .inner
            .store
            .sessions_in_use(agent.id, &warm, None)
            .await?
            .into_iter()
            .filter(|session| {
                conv.as_ref()
                    .is_none_or(|conv| in_conversation(session, conv))
            })
            .map(|session| session.id)
            .collect();
        if ids.is_empty() {
            return done(match &conv {
                Some(_) if self.is_manager_dm(key, origin).await => {
                    here_elsewhere(key.surface, name, origin)
                }
                Some(_) => format!("`{name}` has no session here to reset."),
                None => format!("`{name}` has no sessions to reset."),
            });
        }
        let count = ids.len();
        let mut resets = Box::pin(self.clone().reset_all(control, ids));
        let queued = futures::poll!(resets.as_mut());
        let place = if here { " here" } else { "" };
        let reply = if count == 1 {
            format!(
                "Resetting `{name}`'s session{place}: the next message in it starts a new \
                 conversation. If it is running a turn, it resets once that turn ends. If it \
                 can't be reset, I'll tell you in a direct message."
            )
        } else {
            format!(
                "Resetting `{name}`'s {count} sessions{place}: the next message in each starts \
                 a new conversation. A session running a turn resets once that turn ends. If \
                 any can't be reset, I'll tell you in a direct message."
            )
        };
        let asked = if here {
            reset_here(key.surface, name)
        } else {
            origin.command(&format!("reset {name}"))
        };
        let commands = self.clone();
        let owner = key.clone();
        let name = name.to_owned();
        let agent = agent.id;
        let follow_up = FollowUp::new(async move {
            let resets = match queued {
                Poll::Ready(resets) => resets,
                Poll::Pending => resets.await,
            };
            tracing::info!(
                %agent,
                here,
                reset = resets.done,
                failed = resets.failed,
                "reset an agent's sessions"
            );
            if let Some(notice) = failed_resets(&resets, &name, &asked)
                && let Err(error) = commands.inner.replies.dm(&owner, &notice).await
            {
                tracing::warn!(member = %owner, %error, "couldn't tell an owner a reset failed");
            }
        });
        Ok((reply, follow_up))
    }

    /// Whether `origin` is a Slack slash command sent in the manager app's
    /// direct message with `key`, where no agent answers. Elsewhere the
    /// manager bot's DM is never a command's conversation: a message there
    /// is [`Origin::SlackDm`] or [`Origin::RocketChatDm`].
    async fn is_manager_dm(&self, key: &MemberKey, origin: &Origin) -> bool {
        let Origin::SlackSlash { conv, .. } = origin else {
            return false;
        };
        if conv.surface != key.surface || conv.team != key.team {
            return false;
        }
        match self.inner.replies.dm_room(key).await {
            Ok(room) => room == conv.conversation,
            Err(error) => {
                tracing::debug!(member = %key, %error, "couldn't find the manager bot's DM");
                false
            }
        }
    }

    /// Resets the sessions `ids`, all at once. The first poll polls every
    /// reset once, outside tokio's cooperative budget, so each joins its
    /// session's queue before that poll returns.
    async fn reset_all(
        self,
        control: Option<Arc<dyn SessionControl>>,
        ids: Vec<SessionId>,
    ) -> Resets {
        let mut resets: Vec<_> = ids
            .into_iter()
            .map(|id| future::maybe_done(Box::pin(self.reset_one(control.as_deref(), id))))
            .collect();
        tokio::task::unconstrained(future::poll_fn(|cx| {
            for reset in &mut resets {
                _ = Pin::new(reset).poll(cx);
            }
            Poll::Ready(())
        }))
        .await;
        future::join_all(resets.iter_mut()).await;
        let done = resets
            .iter_mut()
            .filter_map(|reset| Pin::new(reset).take_output())
            .filter(|done| *done)
            .count();
        Resets {
            done,
            failed: resets.len() - done,
        }
    }

    /// Resets `session` through the runner if there is one, or else in the
    /// store, and returns whether it worked.
    async fn reset_one(&self, control: Option<&dyn SessionControl>, session: SessionId) -> bool {
        let reset = match control {
            Some(control) => control.reset(session).await.map(drop),
            None => self
                .inner
                .store
                .reset_session(session, OffsetDateTime::now_utc())
                .await
                .map(drop)
                .map_err(RunnerError::from),
        };
        if let Err(error) = &reset {
            tracing::warn!(%session, %error, "couldn't reset a session");
        }
        reset.is_ok()
    }

    /// A link to `session`'s thread, where one can be built and the owner
    /// can open it: on Slack always, on Rocket.Chat from the room's type,
    /// and for a channel its name, which `rooms` caches per room. No link
    /// to another member's DM with the agent or a private task's thread,
    /// which may be in one, nor to a Rocket.Chat private group, whose name
    /// the owner may not be allowed to see.
    async fn link(
        &self,
        session: &Session,
        rooms: &mut HashMap<ConversationId, Option<(RoomType, Option<String>)>>,
    ) -> Option<String> {
        let thread = &session.thread;
        let direct = match (&session.kind, &session.scope) {
            (SessionKind::Private(_), _) | (SessionKind::Normal, ScopeKey::Dm(_)) => return None,
            (SessionKind::Normal, ScopeKey::Private | ScopeKey::GroupDm(_)) => true,
            (SessionKind::Normal, ScopeKey::Channel(_)) => false,
        };
        match thread.conv.surface {
            SurfaceKind::Slack => surface_slack::surface::thread_link(thread),
            SurfaceKind::RocketChat => {
                let agents = self
                    .inner
                    .rocketchat
                    .as_ref()
                    .filter(|agents| *agents.team() == thread.conv.team)?;
                let room = &thread.conv.conversation;
                let (room_type, name) = if direct {
                    (RoomType::Direct, None)
                } else {
                    if !rooms.contains_key(room) {
                        let info = match agents.rest().room_info(room).await {
                            Ok(info) => Some((info.room_type, info.name)),
                            Err(error) => {
                                tracing::debug!(%room, %error, "no link to a room the manager can't read");
                                None
                            }
                        };
                        rooms.insert(room.clone(), info);
                    }
                    rooms.get(room).cloned().flatten()?
                };
                if room_type == RoomType::Group {
                    return None;
                }
                agents
                    .rest()
                    .room_link(&room_type, room, name.as_deref(), thread.root.as_ref())
            }
        }
    }
}

/// Where `session` is, as its owner reads it.
fn place(session: &Session) -> &'static str {
    match (&session.kind, &session.scope, &session.thread.root) {
        (SessionKind::Private(_), _, _) => "A private task",
        (SessionKind::Normal, ScopeKey::Private, _) => "Your DM with it",
        (SessionKind::Normal, ScopeKey::Dm(_), _) => "Another member's DM with it",
        (SessionKind::Normal, ScopeKey::GroupDm(_), _) => "A group DM",
        (SessionKind::Normal, ScopeKey::Channel(_), Some(_)) => "A thread in a channel",
        (SessionKind::Normal, ScopeKey::Channel(_), None) => "A channel",
    }
}

/// Whether `session` is a normal session of `conv`.
fn in_conversation(session: &Session, conv: &ConvRef) -> bool {
    session.kind == SessionKind::Normal && session.thread.conv == *conv
}

/// How a member on `surface` resets the sessions of `name` in one
/// conversation, typed there.
fn reset_here(surface: SurfaceKind, name: &str) -> String {
    match surface {
        SurfaceKind::RocketChat => format!("`!agent reset {name} here`"),
        SurfaceKind::Slack => format!("`/agent reset {name} here`"),
    }
}

/// The reply to `reset <name> here` from `origin`, which names no
/// conversation to reset: the direct message with the manager bot.
fn here_elsewhere(surface: SurfaceKind, name: &str, origin: &Origin) -> String {
    format!(
        "`reset {name} here` resets the conversation it is sent in, and no agent answers in \
         this one. Send {} in the conversation to reset, or {} here to reset them all.",
        reset_here(surface, name),
        origin.command(&format!("reset {name}")),
    )
}

/// `at` as replies show it, in UTC to the minute.
pub(super) fn when(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(format_description!(
            "[year]-[month]-[day] [hour]:[minute] UTC"
        ))
        .unwrap_or_else(|_| at.unix_timestamp().to_string())
}

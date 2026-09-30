//! The session commands: `sessions <name>` and `reset <name> [here]`, for
//! an agent's owner.
//!
//! Both act on the agent's sessions in use: the live ones (not reset) that
//! have had a turn, have one running, or have a warm container. A live
//! session that never had a turn, such as the one a reset put in place of
//! another, has nothing to show or to reset.
//!
//! - `sessions` lists them, most recently active first, with where each
//!   one is (a link where one can be built), when its last turn ended, and
//!   whether its container is warm on this instance.
//! - `reset` resets every one of them, or with `here` those of the
//!   conversation the command was sent in: every thread of a channel, or a
//!   DM's one session. `here` needs a conversation, so it is refused in the
//!   direct message with the manager bot. A reset runs after the turns
//!   queued before it on the session, stops its warm process and container
//!   first ([`SessionManager::reset`]), and gives the conversation a new
//!   session id, which its next turn starts with `--session-id`. A session
//!   whose container can't be stopped isn't reset, and the reply says so.
//!
//! The runner reaches the commands through [`SessionControl`], which
//! [`Turns`](crate::pipeline::Turns) hands over when it starts. Without a
//! runner (no `[sandbox]`, or once it has stopped) a reset only marks the
//! session reset in the store, and no container is warm. Another agentd
//! instance's warm containers aren't seen: its next turn on a reset session
//! finds it reset and moves to the replacement, and the idle reaper stops
//! the old container.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use core_types::{ConvRef, ConversationId, MemberKey, ScopeKey, SessionId, SurfaceKind};
use futures::StreamExt as _;
use runner::{RunnerError, SessionManager, TurnHooks};
use store::{Session, SessionKind};
use surface_rocketchat::rest::RoomType;
use time::OffsetDateTime;
use time::macros::format_description;

use super::agents::no_such_agent;
use super::{Commands, Failure, Origin};

/// The most sessions `sessions` lists.
pub const MAX_LISTED: usize = 20;

/// How many sessions `reset` resets at once.
const RESETS_AT_ONCE: usize = 8;

/// What the session commands need from the runner.
#[async_trait]
pub trait SessionControl: Send + Sync {
    /// Resets `session` once the turns queued before it have run, stopping
    /// its warm process and container first. Returns its replacement, or
    /// `None` for a private task's session and one already reset.
    ///
    /// # Errors
    ///
    /// [`RunnerError::Sandbox`] if the warm container couldn't be stopped,
    /// and the session isn't reset; [`RunnerError::Store`].
    async fn reset(&self, session: SessionId) -> Result<Option<Session>, RunnerError>;

    /// Whether `session` has a warm container, or a turn running.
    fn is_warm(&self, session: SessionId) -> bool;
}

#[async_trait]
impl<H: TurnHooks> SessionControl for SessionManager<H> {
    async fn reset(&self, session: SessionId) -> Result<Option<Session>, RunnerError> {
        SessionManager::reset(self, session).await
    }

    fn is_warm(&self, session: SessionId) -> bool {
        SessionManager::is_warm(self, session)
    }
}

/// Whether `session` has had a turn, or has one going to its CLI.
fn has_run(session: &Session) -> bool {
    session.started || session.maybe_started || session.last_turn_at.is_some()
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

    /// `agent`'s sessions in use, most recently active first, each with
    /// whether it is warm.
    async fn sessions_in_use(
        &self,
        agent: core_types::AgentId,
        control: Option<&dyn SessionControl>,
    ) -> Result<Vec<(Session, bool)>, Failure> {
        Ok(self
            .inner
            .store
            .live_sessions(agent)
            .await?
            .into_iter()
            .map(|session| {
                let warm = control.is_some_and(|control| control.is_warm(session.id));
                (session, warm)
            })
            .filter(|(session, warm)| *warm || has_run(session))
            .collect())
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
        let control = self.session_control();
        let sessions = self.sessions_in_use(agent.id, control.as_deref()).await?;
        if sessions.is_empty() {
            return Ok(format!("`{name}` has no sessions yet."));
        }
        let total = sessions.len();
        let mut reply = if total > MAX_LISTED {
            format!("`{name}`'s {MAX_LISTED} most recent sessions of {total}:")
        } else {
            format!(
                "`{name}`'s {} {}, most recent first:",
                total,
                sessions_word(total)
            )
        };
        let mut rooms = HashMap::new();
        for (session, warm) in sessions.iter().take(MAX_LISTED) {
            let last = session.last_turn_at.map_or_else(
                || "no turn finished yet".to_owned(),
                |at| format!("last turn {}", when(at)),
            );
            let warm = if *warm { "warm" } else { "cold" };
            reply.push_str(&format!("\n- {}, {last}, {warm}", place(session)));
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
    ) -> Result<String, Failure> {
        let Some(agent) = self.own_agent(key, name).await? else {
            return Ok(no_such_agent(name));
        };
        let conv = match (here, origin.conversation(key)) {
            (false, _) => None,
            (true, Some(conv)) => Some(conv),
            (true, None) => return Ok(here_elsewhere(key.surface, name, origin)),
        };
        let control = self.session_control();
        let sessions: Vec<Session> = self
            .sessions_in_use(agent.id, control.as_deref())
            .await?
            .into_iter()
            .map(|(session, _)| session)
            .filter(|session| {
                conv.as_ref()
                    .is_none_or(|conv| in_conversation(session, conv))
            })
            .collect();
        if sessions.is_empty() {
            return Ok(if here {
                format!("`{name}` has no session here to reset.")
            } else {
                format!("`{name}` has no sessions to reset.")
            });
        }
        let resets = self.reset_all(control.as_deref(), &sessions).await;
        tracing::info!(
            agent = %agent.id,
            here,
            reset = resets.done,
            failed = resets.failed,
            "reset an agent's sessions"
        );
        let count = sessions.len();
        let place = if here { " here" } else { "" };
        let what = if count == 1 {
            format!("`{name}`'s session{place}")
        } else {
            format!("`{name}`'s {count} sessions{place}")
        };
        Ok(match resets {
            Resets { failed: 0, .. } if count == 1 => {
                format!("Reset {what}: its next message starts a new conversation.")
            }
            Resets { failed: 0, .. } => {
                format!("Reset {what}: the next message in each starts a new conversation.")
            }
            Resets { done: 0, .. } => {
                format!("I couldn't reset {what}. Please try again in a minute.")
            }
            Resets { done, failed } => format!(
                "Reset {done} of {what}; {failed} couldn't be reset. Please try again in a minute."
            ),
        })
    }

    /// Resets `sessions`, a few at once.
    async fn reset_all(
        &self,
        control: Option<&dyn SessionControl>,
        sessions: &[Session],
    ) -> Resets {
        let ids: Vec<SessionId> = sessions.iter().map(|session| session.id).collect();
        let results: Vec<bool> = futures::stream::iter(ids)
            .map(|id| self.reset_one(control, id))
            .buffer_unordered(RESETS_AT_ONCE)
            .collect()
            .await;
        let done = results.iter().filter(|done| **done).count();
        Resets {
            done,
            failed: results.len() - done,
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

    /// A link to `session`'s thread, where one can be built: on Slack
    /// always, on Rocket.Chat from the room's type, and for a channel its
    /// name, which `rooms` caches per room. No link to another member's DM
    /// with the agent, which the owner can't open.
    async fn link(
        &self,
        session: &Session,
        rooms: &mut HashMap<ConversationId, Option<(RoomType, Option<String>)>>,
    ) -> Option<String> {
        let thread = &session.thread;
        let direct = match (&session.kind, &session.scope) {
            (_, ScopeKey::Dm(_)) => return None,
            (SessionKind::Normal, ScopeKey::Private | ScopeKey::GroupDm(_)) => true,
            _ => false,
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
fn when(at: OffsetDateTime) -> String {
    at.to_offset(time::UtcOffset::UTC)
        .format(format_description!(
            "[year]-[month]-[day] [hour]:[minute] UTC"
        ))
        .unwrap_or_else(|_| at.unix_timestamp().to_string())
}

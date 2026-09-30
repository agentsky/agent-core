//! [`Pipeline`]: from an inbound message to the agents' replies.

use std::path::PathBuf;
use std::sync::Arc;

use core_types::{
    AgentId, Caps, ConvKind, CredentialRef, Hop, InboundEvent, MemberKey, MsgRef, ReplyTarget,
    Requester, ScopeKey, ScopeKind, SendError, Sender, Side, Sink, Surface, ThreadKey, TurnId,
    TurnKind,
};
use render::directives::{self, Directive};
use router::{Decision, ModelPolicy, RefuseReason};
use runner::{ErrorKind, RunnerError, Session, TurnOutcome, TurnReport, TurnRequest};
use store::{Agent, NewMessageRef, Store, StoreError};
use time::OffsetDateTime;

use super::Turns;
use super::message;
use super::view::StoreView;
use crate::commands::Replies;
use crate::ctl::{Outbox, SurfaceLookup};

/// The emoji an agent's bot reacts with to the message a turn answers,
/// while the turn runs, unless `[runner] working_emoji` says otherwise.
pub const DEFAULT_WORKING_EMOJI: &str = "hourglass_flowing_sand";

/// What a failed turn tells the thread when the requester's usage limit is
/// reached.
pub const USAGE_LIMIT_TEXT: &str =
    "Sorry, I can't answer that: your Claude usage limit is reached. Try again when it resets.";
/// What a failed turn tells the thread when the requester's login expired.
pub const LOGIN_EXPIRED_TEXT: &str = "Sorry, I can't answer that: your Claude login expired. \
     Run `/agent login` (`!agent login` on Rocket.Chat) and ask again.";
/// What any other failed turn tells the thread.
pub const FAILED_TEXT: &str = "Sorry, that turn failed. Try again in a moment.";
/// What a turn that ran out of time tells the thread.
pub const TIMED_OUT_TEXT: &str = "Sorry, that took too long, and the turn was stopped.";

/// How the pipeline runs, besides the store and the runner.
#[derive(Debug, Clone)]
pub struct PipelineSettings {
    /// agentd's data directory, which holds the agents' persona files.
    pub data_dir: PathBuf,
    /// The manager bots' identities, whose posts start no turn.
    pub managers: Vec<MemberKey>,
    /// The emoji a bot reacts with while its turn runs.
    pub working_emoji: String,
    /// Which model a requester's plan gets, or `None` for the CLI's
    /// default.
    pub models: Option<ModelPolicy>,
}

/// Takes every surface's messages that aren't commands, decides which
/// agents answer, runs their turns and delivers their replies.
///
/// For each message:
///
/// 1. **Candidates.** On a surface with [`Caps::per_binding_delivery`],
///    the agent whose binding received it. Elsewhere, every managed agent
///    it mentions, the agent whose DM it is, and the agent that posted the
///    message it replies to (a thread's root). Never the agent that posted
///    it.
/// 2. **Routing.** [`router::route`] for each candidate, with a
///    view of the store loaded for it. Each candidate goes on in a task of its
///    own, so a turn never holds up the connection that delivered the
///    message, or another agent.
/// 3. **The turn.** On [`Decision::Run`], only when the agent's bot may
///    post in the conversation without joining it
///    ([`Surface::can_post`]): the persona file is written from the store,
///    the thread's session looked up (a DM has one for the conversation, a
///    channel one per thread, rooted at the message when it starts one),
///    the turn message built with what the session's transcript lacks, and the turn run, with
///    the working emoji on the message while it runs. A turn refused with
///    [`RunnerError::SessionReset`] runs once more, on the session looked
///    up again.
/// 4. **Delivery**, as the agent's bot, in the thread: the directives are
///    taken out of the reply, the turn's staged attachments uploaded, the
///    reply rendered, split and posted, and a `message_refs` row recorded
///    for every chunk with the turn's requester and hop; then the
///    directives' reactions, and the reactions and posts the turn queued
///    with agentctl. A failed turn posts a short message that says why
///    when the runner could tell: a usage limit, or a login that expired.
/// 5. [`Decision::LinkPrompt`] sends the requester a DM from the manager
///    bot saying how to link an account, [`Decision::Refuse`] posts one
///    line in the thread, and [`Decision::Ignore`] does nothing.
///
/// Cloning is cheap; clones share everything.
#[derive(Clone)]
pub struct Pipeline {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipeline")
            .field("settings", &self.inner.settings)
            .finish_non_exhaustive()
    }
}

struct Inner {
    store: Store,
    turns: Turns,
    surfaces: Arc<dyn SurfaceLookup>,
    replies: Replies,
    settings: PipelineSettings,
}

impl Pipeline {
    /// A pipeline over `store`, running turns on `turns`, posting through
    /// `surfaces`, and prompting for links through `replies`.
    pub fn new(
        store: Store,
        turns: Turns,
        surfaces: Arc<dyn SurfaceLookup>,
        replies: Replies,
        settings: PipelineSettings,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                turns,
                surfaces,
                replies,
                settings,
            }),
        }
    }

    /// Where a surface with `caps` delivers its messages: each message
    /// sent is handled in the background, so sending never waits for a
    /// turn.
    pub fn sink(&self, caps: Caps) -> Sender<InboundEvent> {
        Sender::new(PipelineSink {
            pipeline: self.clone(),
            caps,
        })
    }

    /// Handles `event`, from a surface with `caps`: every candidate's
    /// decision, and its turn, run to the end.
    pub async fn handle(&self, event: InboundEvent, caps: Caps) {
        let candidates = match self.candidates(&event, caps).await {
            Ok(candidates) => candidates,
            Err(err) => {
                tracing::warn!(message = %event.message.id, error = %err, "couldn't look up the agents a message addresses");
                return;
            }
        };
        let event = Arc::new(event);
        let mut running = tokio::task::JoinSet::new();
        for agent in candidates {
            let pipeline = self.clone();
            let event = Arc::clone(&event);
            running.spawn(async move { pipeline.candidate(&event, agent, caps).await });
        }
        while let Some(done) = running.join_next().await {
            if let Err(err) = done {
                tracing::error!(error = %err, "a candidate's turn panicked");
            }
        }
    }

    /// The agents that may answer `event`, each once.
    async fn candidates(
        &self,
        event: &InboundEvent,
        caps: Caps,
    ) -> Result<Vec<AgentId>, StoreError> {
        let store = &self.inner.store;
        let mut candidates = Vec::new();
        if caps.per_binding_delivery {
            if let Some(agent) = store.agent_for_binding(event.binding).await? {
                candidates.push(agent.id);
            }
        } else {
            for user in &event.mentions {
                let bot = MemberKey {
                    surface: event.conv.surface,
                    team: event.conv.team.clone(),
                    user: user.clone(),
                };
                if let Some((agent, _)) = store.agent_for_bot(&bot).await? {
                    candidates.push(agent.id);
                }
            }
            if event.conv_kind == ConvKind::Dm
                && let Some(agent) = store.agent_for_binding(event.binding).await?
            {
                candidates.push(agent.id);
            }
            if let Some(reply_to) = &event.reply_to
                && let Some(agent) = store
                    .posted_message_ref(reply_to)
                    .await?
                    .and_then(|posted| posted.agent)
            {
                candidates.push(agent);
            }
        }
        let sender = store.agent_of_bot_user(&event.sender).await?;
        let mut seen = Vec::new();
        candidates.retain(|agent| {
            let keep = Some(*agent) != sender && !seen.contains(agent);
            seen.push(*agent);
            keep
        });
        Ok(candidates)
    }

    /// Routes `event` for `agent` and acts on the decision.
    async fn candidate(&self, event: &InboundEvent, agent: AgentId, caps: Caps) {
        let store = &self.inner.store;
        let view = match StoreView::load(store, event, agent, &self.inner.settings.managers).await {
            Ok(view) => view,
            Err(err) => {
                tracing::warn!(%agent, message = %event.message.id, error = %err, "couldn't load what routing needs");
                return;
            }
        };
        let decision = router::route(event, agent, &view);
        let result = match decision {
            Decision::Ignore(reason) => {
                tracing::debug!(%agent, message = %event.message.id, %reason, "ignored a message");
                Ok(())
            }
            Decision::LinkPrompt { requester } => self.link_prompt(agent, &requester).await,
            Decision::Refuse(reason) => self.refuse(event, agent, reason).await,
            Decision::Run {
                requester,
                hop,
                credential,
                scope,
                side,
            } => {
                let turn = Run {
                    requester,
                    hop,
                    credential,
                    scope,
                    side,
                };
                self.run(event, agent, caps, turn).await
            }
        };
        if let Err(err) = result {
            tracing::warn!(%agent, message = %event.message.id, error = %err, "handling a message failed");
        }
    }

    /// Tells `requester` privately how to link an account.
    async fn link_prompt(
        &self,
        agent: AgentId,
        requester: &Requester,
    ) -> Result<(), PipelineError> {
        let name = self.agent_name(agent).await?;
        let text = format!(
            "{name} runs on the Claude account of whoever asks it. Link yours to use it: send \
             `login` to me here."
        );
        if let Err(err) = self.inner.replies.dm(&requester.key, &text).await {
            tracing::warn!(%agent, requester = %requester.key, error = %err, "couldn't send a link prompt");
        }
        Ok(())
    }

    /// Posts one line in `event`'s thread saying why `agent` won't answer.
    async fn refuse(
        &self,
        event: &InboundEvent,
        agent: AgentId,
        reason: RefuseReason,
    ) -> Result<(), PipelineError> {
        let Some(surface) = self.inner.surfaces.surface(agent, &event.conv).await else {
            return Ok(());
        };
        if !surface.can_post(&event.conv).await? {
            return Ok(());
        }
        let name = self.agent_name(agent).await?;
        let text = match reason {
            RefuseReason::Paused => format!("{name} is paused by its owner."),
            RefuseReason::Banned => format!("{name} can't take requests from you."),
            RefuseReason::Denied => format!("{name}'s owner hasn't allowed you to use it here."),
            RefuseReason::HopCap { max } => format!(
                "{name} won't answer: this chain of agents has reached its limit of {max} hops."
            ),
            RefuseReason::PolicyUnavailable => {
                format!("{name} can't check who may use it right now. Try again later.")
            }
        };
        let target = reply_target(event, surface.caps());
        for chunk in surface.render(&text) {
            surface.post(&target, &chunk).await?;
        }
        tracing::info!(%agent, message = %event.message.id, %reason, "refused a message");
        Ok(())
    }

    async fn agent_name(&self, agent: AgentId) -> Result<String, StoreError> {
        Ok(self
            .inner
            .store
            .agent(agent)
            .await?
            .map_or_else(|| "This agent".to_owned(), |agent| agent.name))
    }

    /// Runs `agent`'s turn on `event` and delivers what it made.
    async fn run(
        &self,
        event: &InboundEvent,
        agent: AgentId,
        caps: Caps,
        turn: Run,
    ) -> Result<(), PipelineError> {
        let store = &self.inner.store;
        let Some(surface) = self.inner.surfaces.surface(agent, &event.conv).await else {
            tracing::warn!(%agent, conv = %event.conv, "the agent has no surface in this conversation");
            return Ok(());
        };
        if !surface.can_post(&event.conv).await? {
            tracing::info!(%agent, conv = %event.conv, "not answering: the agent's bot isn't in this conversation");
            return Ok(());
        }
        let Some(row) = store.agent(agent).await? else {
            return Ok(());
        };
        let Some(bot) = self.bot_of(&row, event).await? else {
            return Ok(());
        };
        runner::write_persona(&self.inner.settings.data_dir, agent, &row.persona).await?;
        let thread = thread_of(event, caps);
        let scope = match turn.scope {
            ScopeKind::Private => ScopeKey::Private,
            _ => ScopeKey::for_conversation(event.conv_kind, event.conv.clone()),
        };
        let model = self.model_for(turn.credential).await?;
        let emoji = &self.inner.settings.working_emoji;
        if let Err(err) = surface.react(&event.message, emoji).await {
            tracing::debug!(%agent, error = %err, "couldn't show that the turn is running");
        }
        let sessions = self.inner.turns.sessions();
        let mut attempt = 0;
        let looped = async {
            loop {
                attempt += 1;
                let session = sessions.lookup_or_create(agent, &thread, &scope).await?;
                let text = message::build(
                    store,
                    surface.as_ref(),
                    &session,
                    &bot,
                    event,
                    &turn.requester,
                )
                .await?;
                let request = TurnRequest {
                    turn: TurnId::new_v4(),
                    message: text,
                    credential: turn.credential,
                    model: model.clone(),
                    requester: turn.requester.clone(),
                    hop: turn.hop,
                    side: turn.side,
                    kind: TurnKind::Normal,
                    trigger: Some(event.message.id.clone()),
                };
                let turn_id = request.turn;
                match sessions.run_turn(session.id, request).await {
                    Err(RunnerError::SessionReset) if attempt == 1 => {
                        tracing::info!(session = %session.id, "the session was reset before the turn ran; running it on the new one");
                    }
                    ran => {
                        return Ok::<_, PipelineError>((
                            session,
                            ran.map(|report| (turn_id, report)),
                        ));
                    }
                }
            }
        };
        let looped = looped.await;
        if let Err(err) = surface.unreact(&event.message, emoji).await {
            tracing::debug!(%agent, error = %err, "couldn't clear the working reaction");
        }
        let (session, ran) = looped?;
        let delivery = Delivery {
            store,
            surface: surface.as_ref(),
            session: &session,
            agent,
            requester: &turn.requester,
            hop: turn.hop,
            target: ReplyTarget {
                conv: event.conv.clone(),
                thread_root: thread.root.clone(),
            },
            answered: &event.message,
        };
        match ran {
            Ok((turn_id, report)) => delivery.report(turn_id, report).await,
            Err(err) => {
                tracing::warn!(%agent, session = %session.id, error = %err, "a turn failed to run");
                if matches!(
                    err,
                    RunnerError::Hook {
                        hook: "turn_starting",
                        ..
                    }
                ) {
                    sessions.stop(session.id).await;
                }
                delivery.post(None, FAILED_TEXT).await
            }
        }
    }

    /// The identity of `agent`'s bot on `event`'s surface and team.
    async fn bot_of(
        &self,
        agent: &Agent,
        event: &InboundEvent,
    ) -> Result<Option<MemberKey>, StoreError> {
        Ok(self
            .inner
            .store
            .bindings_of(agent.id)
            .await?
            .into_iter()
            .find(|binding| {
                binding.surface == event.conv.surface
                    && binding.team == event.conv.team
                    && binding.state == store::BindingState::Active
            })
            .and_then(|binding| binding.bot_user)
            .map(|user| MemberKey {
                surface: event.conv.surface,
                team: event.conv.team.clone(),
                user,
            }))
    }

    /// The model a turn on `credential` runs on: the one the member's plan
    /// maps to, or the default.
    async fn model_for(&self, credential: CredentialRef) -> Result<Option<String>, StoreError> {
        let Some(models) = &self.inner.settings.models else {
            return Ok(None);
        };
        let plan = match credential {
            CredentialRef::Member(member) => self
                .inner
                .store
                .claude_link_status(member)
                .await?
                .and_then(|status| status.plan),
            CredentialRef::Community => None,
        };
        Ok(Some(models.model_for(plan.as_deref()).to_owned()))
    }
}

/// A turn the router decided to run.
struct Run {
    requester: Requester,
    hop: Hop,
    credential: CredentialRef,
    scope: ScopeKind,
    side: Side,
}

/// The thread a turn on `event` runs and replies in: a DM's conversation,
/// or the thread the message is in, rooted at the message when it starts
/// one. Without threads, the conversation.
fn thread_of(event: &InboundEvent, caps: Caps) -> ThreadKey {
    let root = match event.conv_kind {
        ConvKind::Dm => None,
        ConvKind::Channel | ConvKind::GroupDm if caps.supports_threads => Some(
            event
                .thread_root
                .clone()
                .unwrap_or_else(|| event.message.id.clone()),
        ),
        ConvKind::Channel | ConvKind::GroupDm => None,
    };
    ThreadKey {
        conv: event.conv.clone(),
        root,
    }
}

fn reply_target(event: &InboundEvent, caps: Caps) -> ReplyTarget {
    ReplyTarget::from(thread_of(event, caps))
}

/// Delivers what one turn made, as the agent's bot.
struct Delivery<'a> {
    store: &'a Store,
    surface: &'a dyn Surface,
    session: &'a Session,
    agent: AgentId,
    requester: &'a Requester,
    hop: Hop,
    target: ReplyTarget,
    answered: &'a MsgRef,
}

impl Delivery<'_> {
    /// Delivers `report`: the attachments, then the reply or the failure's
    /// message, then the reactions, then the queued posts.
    async fn report(
        &self,
        turn: TurnId,
        report: TurnReport<Option<Outbox>>,
    ) -> Result<(), PipelineError> {
        let outbox = match report.finished {
            Ok(outbox) => outbox,
            Err(err) => {
                tracing::warn!(session = %self.session.id, error = %err, "the turn's outbox was lost");
                None
            }
        };
        let (reply, reactions) = match &report.outcome {
            TurnOutcome::Finished(result) if !result.is_error => {
                let (text, found) = directives::extract(result.result.as_deref().unwrap_or(""));
                let reactions = found
                    .into_iter()
                    .filter_map(|directive| match directive {
                        Directive::React { emoji } => Some(emoji),
                        _ => None,
                    })
                    .collect();
                (text, reactions)
            }
            TurnOutcome::Finished(result) => {
                let text = match result.error_kind {
                    Some(ErrorKind::UsageLimit) => USAGE_LIMIT_TEXT,
                    Some(ErrorKind::Auth) => LOGIN_EXPIRED_TEXT,
                    _ => FAILED_TEXT,
                };
                (text.to_owned(), Vec::new())
            }
            TurnOutcome::Crashed { .. } => (FAILED_TEXT.to_owned(), Vec::new()),
            TurnOutcome::TimedOut { .. } => (TIMED_OUT_TEXT.to_owned(), Vec::new()),
        };
        tracing::info!(
            agent = %self.agent,
            session = %self.session.id,
            %turn,
            success = report.outcome.is_success(),
            reply_len = reply.len(),
            "a turn ended"
        );
        if let Some(outbox) = &outbox
            && !outbox.attachments().is_empty()
            && let Err(err) = self
                .surface
                .upload(&self.target, outbox.attachments())
                .await
        {
            tracing::warn!(session = %self.session.id, error = %err, "uploading the turn's attachments failed");
        }
        self.post(Some(turn), &reply).await?;
        for emoji in reactions {
            self.react(self.answered, &emoji).await;
        }
        if let Some(outbox) = &outbox {
            for reaction in outbox.reactions() {
                self.react(&reaction.msg, &reaction.emoji).await;
            }
            for queued in outbox.posts() {
                let target = Delivery {
                    target: queued.to.clone(),
                    ..*self
                };
                if let Err(err) = target.post(Some(turn), &queued.text).await {
                    tracing::warn!(session = %self.session.id, error = %err, "delivering a queued post failed");
                }
            }
        }
        Ok(())
    }

    /// Renders and posts Markdown `text` to the target, recording a
    /// `message_refs` row for each chunk. Empty text posts nothing.
    async fn post(&self, turn: Option<TurnId>, text: &str) -> Result<(), PipelineError> {
        if text.trim().is_empty() {
            return Ok(());
        }
        for chunk in self.surface.render(text) {
            let posted = self.surface.post(&self.target, &chunk).await?;
            self.store
                .record_message_ref(
                    &NewMessageRef {
                        session: self.session.id,
                        msg: &posted,
                        thread_root: self.target.thread_root.as_ref(),
                        agent: Some(self.agent),
                        turn,
                        requester: self.requester,
                        hop: self.hop,
                    },
                    OffsetDateTime::now_utc(),
                )
                .await?;
        }
        Ok(())
    }

    async fn react(&self, msg: &MsgRef, emoji: &str) {
        if let Err(err) = self.surface.react(msg, emoji).await {
            tracing::warn!(session = %self.session.id, error = %err, "adding a reaction failed");
        }
    }
}

/// Why handling a message failed.
#[derive(Debug, thiserror::Error)]
enum PipelineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error(transparent)]
    Surface(#[from] core_types::SurfaceError),
}

/// The sink behind [`Pipeline::sink`].
struct PipelineSink {
    pipeline: Pipeline,
    caps: Caps,
}

#[async_trait::async_trait]
impl Sink<InboundEvent> for PipelineSink {
    async fn send(&self, event: InboundEvent) -> Result<(), SendError> {
        let pipeline = self.pipeline.clone();
        let caps = self.caps;
        tokio::spawn(async move { pipeline.handle(event, caps).await });
        Ok(())
    }
}

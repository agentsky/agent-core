//! [`Pipeline`]: from an inbound message to the agents' replies.

use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use core_types::{
    AgentId, Caps, ConvKind, CredentialRef, Hop, InboundEvent, MemberKey, MsgRef, ReplyTarget,
    Requester, ScopeKey, ScopeKind, SendError, Sender, Side, Sink, Surface, SurfaceError,
    ThreadKey, TurnId, TurnKind,
};
use futures::FutureExt as _;
use render::directives::{self, Directive};
use router::{Decision, ModelPolicy, RefuseReason};
use runner::{ErrorKind, RunnerError, Session, TurnOutcome, TurnReport, TurnRequest};
use store::{Agent, NewMessageRef, Store, StoreError};
use time::OffsetDateTime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tokio::task::JoinSet;

use super::Turns;
use super::keyed::KeyedLocks;
use super::message;
use super::view::StoreView;
use crate::commands::Replies;
use crate::ctl::{MAX_POST_BYTES, Outbox, SurfaceLookup};

/// The emoji an agent's bot reacts with to the message a turn answers,
/// while the turn runs, unless `[runner] working_emoji` says otherwise.
pub const DEFAULT_WORKING_EMOJI: &str = "hourglass_flowing_sand";

/// What a failed turn tells the thread when the usage limit of the account
/// it ran on is reached: the requester's, or the community key's.
pub const USAGE_LIMIT_TEXT: &str = "Sorry, I can't answer that: the Claude account this request \
     runs on has reached its usage limit. Try again when it resets.";
/// What a failed turn tells the thread when the login it ran on expired.
pub const LOGIN_EXPIRED_TEXT: &str = "Sorry, I can't answer that: the Claude login this request \
     runs on has expired. If it's yours, run `/agent login` (`!agent login` on Rocket.Chat) and \
     ask again.";
/// What any other failed turn tells the thread, including one that failed
/// before it reached the model.
pub const FAILED_TEXT: &str = "Sorry, that turn failed. Try again in a moment.";
/// What a turn that ran out of time after the CLI read its message tells
/// the thread. One that ran out of time before tells [`FAILED_TEXT`], as a
/// turn that never reached the model does.
pub const TIMED_OUT_TEXT: &str = "Sorry, that took too long, and the turn was stopped.";
/// What the thread is told when part of a turn's reply, its files or its
/// queued posts couldn't be posted.
pub const DELIVERY_FAILED_TEXT: &str = "Sorry, part of this reply couldn't be delivered.";
/// What the thread is told when agentd stopped before the turn answering
/// it finished.
pub const RESTARTING_TEXT: &str =
    "Sorry, I'm restarting and couldn't finish this. Please ask again in a minute.";
/// Appended to a reply cut at [`MAX_POST_BYTES`].
pub const TRUNCATED_NOTE: &str = "\n\n*(The reply was cut here: it was too long to post.)*";

/// How many messages may wait for one agent in one thread, besides the
/// one being answered, unless the settings say otherwise.
pub const DEFAULT_QUEUE_PER_THREAD: usize = 8;
/// How many messages may wait or be answered at once across every agent
/// and thread, unless the settings say otherwise.
pub const DEFAULT_MAX_PENDING: usize = 64;

/// How long the notices of turns cut short by a shutdown may take.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest a chunk's post waits to be tried again after the platform
/// asked to slow down.
const RETRY_WAIT_CAP: Duration = Duration::from_secs(5);

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
    /// How many messages may wait for one agent in one thread.
    pub queue_per_thread: usize,
    /// How many messages may wait or be answered at once in all.
    pub max_pending: usize,
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
/// 2. **Queues.** Each candidate's messages in a thread are answered one
///    at a time, in the order they arrived, in a task of the pipeline's
///    own, so a turn never holds up the connection that delivered the
///    message, or another agent or thread. A thread's messages are looked
///    up and queued one at a time, in the order their sending started,
///    even when different connections deliver them at once. At most
///    [`queue_per_thread`](PipelineSettings::queue_per_thread) messages
///    wait for one agent in one thread, and
///    [`max_pending`](PipelineSettings::max_pending) wait or run in all; a
///    person's message past either gets one line saying the agent is busy,
///    and a bot's gets nothing, so two bots can't answer each other's busy
///    lines.
/// 3. **Routing.** [`router::route`] for the candidate, with a view of the
///    store loaded for it.
/// 4. **The turn.** On [`Decision::Run`], only when the agent's bot may
///    post in the conversation without joining it
///    ([`Surface::can_post`]): the persona file is written from the store,
///    the thread's session looked up (a DM has one for the conversation, a
///    channel one per thread, rooted at the message when it starts one),
///    the turn message built with what the session's transcript lacks, and
///    the turn run, with the working emoji on the message until its reply
///    is delivered. A turn refused with [`RunnerError::SessionReset`] runs
///    once more, on the session looked up again. What the turn message
///    recorded as shown is forgotten when the turn never reached the model,
///    so the next turn shows it again.
/// 5. **Delivery**, as the agent's bot, in the thread: the directives are
///    taken out of the reply, the turn's staged attachments uploaded, the
///    reply cut at [`MAX_POST_BYTES`] (closing a code block the cut left
///    open), rendered, split and posted, and a `message_refs` row recorded
///    for every chunk with the turn's requester and hop; then the
///    directives' reactions, and the reactions and posts the turn queued
///    with agentctl. Each goes out even when another failed, and then the
///    thread is told part of the reply was lost (a refused reaction is
///    only logged); so it is when a chunk was posted but its row couldn't
///    be recorded, or the turn's outbox was lost because `turn_finished`
///    failed. A failed turn posts a
///    short message that says why when the runner could tell: a usage
///    limit, or a login that expired. A turn that ran out of time after
///    the CLI read its message posts [`TIMED_OUT_TEXT`]; any other
///    failure, including a crash or a timeout before the CLI read the
///    message, posts [`FAILED_TEXT`].
/// 6. [`Decision::LinkPrompt`] sends the requester a DM from the manager
///    bot saying how to link an account, when the agent's bot may post in
///    the conversation; [`Decision::Refuse`] posts one line in the thread,
///    and [`Decision::Ignore`] does nothing.
///
/// Notices the pipeline posts on its own, such as refusals and failures
/// before a turn reached the model, have no `message_refs` row.
///
/// On shutdown, [`close`](Self::close) stops taking messages,
/// [`drain`](Self::drain) waits for what was taken, and
/// [`cut_short`](Self::cut_short) drops what is left, telling the threads
/// whose turns were running.
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
    lanes: Mutex<HashMap<LaneKey, VecDeque<Job>>>,
    dispatching: KeyedLocks<ThreadKey>,
    pending: Arc<Semaphore>,
    tasks: Mutex<JoinSet<()>>,
    closed: AtomicBool,
    working: Mutex<Working>,
}

/// One agent in one thread: its messages are answered in arrival order.
type LaneKey = (AgentId, ThreadKey);

/// A message waiting for one candidate agent. Dropping it releases its
/// place under [`PipelineSettings::max_pending`] and tells whoever waits
/// for it in [`Pipeline::handle`].
struct Job {
    event: Arc<InboundEvent>,
    caps: Caps,
    _pending: OwnedSemaphorePermit,
    _done: oneshot::Sender<()>,
}

/// The working emoji of the turns running, and of those a shutdown cut
/// short.
#[derive(Default)]
struct Working {
    next: u64,
    running: HashMap<u64, Indicator>,
    cut: Vec<Indicator>,
}

/// The working emoji on the message a turn answers.
struct Indicator {
    surface: Arc<dyn Surface>,
    msg: MsgRef,
    target: ReplyTarget,
    emoji: String,
}

impl Indicator {
    async fn clear(&self) {
        if let Err(err) = self.surface.unreact(&self.msg, &self.emoji).await {
            tracing::debug!(msg = %self.msg.id, error = %err, "couldn't clear the working reaction");
        }
    }
}

/// Holds a turn's [`Indicator`] while the turn runs and its reply is
/// delivered. [`finish`](Self::finish) takes the emoji off. Dropped without
/// it, as when the turn panics, the emoji is taken off in the background;
/// during a shutdown the turn is left for [`Pipeline::cut_short`] or
/// [`Pipeline::drain`] to announce.
struct WorkingGuard {
    pipeline: Pipeline,
    id: u64,
}

impl WorkingGuard {
    async fn finish(self) {
        let indicator = self.pipeline.working().running.remove(&self.id);
        if let Some(indicator) = indicator {
            indicator.clear().await;
        }
    }
}

impl Drop for WorkingGuard {
    fn drop(&mut self) {
        let mut working = self.pipeline.working();
        let Some(indicator) = working.running.remove(&self.id) else {
            return;
        };
        if self.pipeline.is_closed() {
            working.cut.push(indicator);
            return;
        }
        drop(working);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { indicator.clear().await });
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
        let pending = Arc::new(Semaphore::new(settings.max_pending));
        Self {
            inner: Arc::new(Inner {
                store,
                turns,
                surfaces,
                replies,
                settings,
                lanes: Mutex::new(HashMap::new()),
                dispatching: KeyedLocks::default(),
                pending,
                tasks: Mutex::new(JoinSet::new()),
                closed: AtomicBool::new(false),
                working: Mutex::new(Working::default()),
            }),
        }
    }

    /// Where a surface with `caps` delivers its messages. Sending looks up
    /// the message's candidates and queues it for each, or tells the
    /// thread an agent is busy. It may wait for the lookup of a message
    /// sent before in the same thread, never for a turn.
    pub fn sink(&self, caps: Caps) -> Sender<InboundEvent> {
        Sender::new(PipelineSink {
            pipeline: self.clone(),
            caps,
        })
    }

    /// Handles `event`, from a surface with `caps`, as the
    /// [`sink`](Self::sink) does, and waits until every candidate's
    /// decision, and its turn, has run to the end.
    pub async fn handle(&self, event: InboundEvent, caps: Caps) {
        for done in self.dispatch(event, caps).await {
            let _ = done.await;
        }
    }

    /// Stops taking messages: those sent from now on are dropped. It sets
    /// the flag under the lanes' lock, where queueing checks it, so a
    /// message is either queued before the close, and answered by the
    /// drain, or dropped.
    pub fn close(&self) {
        let _lanes = lock(&self.inner.lanes);
        self.inner.closed.store(true, Ordering::SeqCst);
    }

    fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Waits until every message taken is answered. Call it after
    /// [`close`](Self::close), or it may never end.
    ///
    /// Cancelling it leaves what is still running for
    /// [`cut_short`](Self::cut_short).
    pub async fn drain(&self) {
        while let Some(joined) =
            std::future::poll_fn(|cx| lock(&self.inner.tasks).poll_join_next(cx)).await
        {
            if let Err(err) = joined {
                tracing::error!(error = %err, "a pipeline task failed");
            }
        }
        self.tell_cut().await;
    }

    /// Closes the pipeline and drops every message still waiting or being
    /// answered. Each turn that was running, or delivering its reply, has
    /// its working emoji taken off and its thread told to ask again
    /// ([`RESTARTING_TEXT`]), within a few seconds. Messages still waiting
    /// are dropped without a word: no decision was made about them yet.
    pub async fn cut_short(&self) {
        self.close();
        let mut tasks = std::mem::take(&mut *lock(&self.inner.tasks));
        tasks.shutdown().await;
        lock(&self.inner.lanes).clear();
        {
            let mut working = self.working();
            let running: Vec<_> = working.running.drain().map(|(_, cut)| cut).collect();
            working.cut.extend(running);
        }
        self.tell_cut().await;
    }

    /// Stops every warm session's process and container, as
    /// [`SessionManager::stop_all`](runner::SessionManager::stop_all) does.
    /// A graceful shutdown calls it once the pipeline has drained.
    pub async fn stop_sessions(&self) {
        self.inner.turns.sessions().stop_all().await;
    }

    /// Takes the working emoji off the turns a shutdown cut short, and
    /// tells their threads.
    async fn tell_cut(&self) {
        let cut = std::mem::take(&mut self.working().cut);
        if cut.is_empty() {
            return;
        }
        tracing::warn!(
            turns = cut.len(),
            "telling the threads of the turns cut short to ask again"
        );
        let told = tokio::time::timeout(NOTICE_TIMEOUT, async {
            for indicator in &cut {
                indicator.clear().await;
                if let Err(err) =
                    say(indicator.surface.as_ref(), &indicator.target, RESTARTING_TEXT).await
                {
                    tracing::warn!(msg = %indicator.msg.id, error = %err, "couldn't tell a thread its turn was cut short");
                }
            }
        })
        .await;
        if told.is_err() {
            tracing::warn!("telling the threads of the turns cut short took too long");
        }
    }

    fn working(&self) -> MutexGuard<'_, Working> {
        lock(&self.inner.working)
    }

    /// Queues `event` for each of its candidates, and returns what
    /// completes when each is done with.
    ///
    /// The dispatches of one thread look up and queue one at a time, in
    /// the order they started, under the thread's lock: each Rocket.Chat
    /// connection in a room may deliver a message, and two of a thread's
    /// messages delivered by different connections at once would
    /// otherwise reach the lanes in the order their lookups ended. The
    /// busy lines are posted after the lock is released.
    async fn dispatch(&self, event: InboundEvent, caps: Caps) -> Vec<oneshot::Receiver<()>> {
        if self.is_closed() {
            tracing::info!(message = %event.message.id, "shutting down: not handling a message");
            return Vec::new();
        }
        let thread = thread_of(&event, caps);
        let in_order = self.inner.dispatching.lock(thread.clone()).await;
        let (candidates, from_bot) = match self.candidates(&event, caps).await {
            Ok(found) => found,
            Err(err) => {
                tracing::warn!(message = %event.message.id, error = %err, "couldn't look up the agents a message addresses");
                return Vec::new();
            }
        };
        let event = Arc::new(event);
        let mut waiting = Vec::new();
        let mut busy = Vec::new();
        for agent in candidates {
            let (done, finished) = oneshot::channel();
            if self.enqueue((agent, thread.clone()), Arc::clone(&event), caps, done) {
                waiting.push(finished);
            } else if from_bot {
                tracing::warn!(%agent, message = %event.message.id, "too many messages waiting; dropping a bot's message");
            } else {
                busy.push(agent);
            }
        }
        drop(in_order);
        for agent in busy {
            self.busy(&event, agent, caps).await;
        }
        waiting
    }

    /// Queues `event` for the agent of `key`, starting the lane's task if
    /// it has none. False when the lane or the pipeline is full. Once the
    /// pipeline is [closed](Self::close) the message is dropped instead,
    /// and true returned. It never waits, so a sender cancelled
    /// mid-dispatch can't leave a lane without its task.
    ///
    /// The open check, the queueing and the start of a lane's task happen
    /// under the lanes' lock, so every message queued before the close is
    /// in a lane whose task is in the set [`drain`](Self::drain) waits for.
    fn enqueue(
        &self,
        key: LaneKey,
        event: Arc<InboundEvent>,
        caps: Caps,
        done: oneshot::Sender<()>,
    ) -> bool {
        let mut lanes = lock(&self.inner.lanes);
        if self.is_closed() {
            tracing::info!(agent = %key.0, message = %event.message.id, "shutting down: not handling a message");
            return true;
        }
        let Ok(pending) = Arc::clone(&self.inner.pending).try_acquire_owned() else {
            return false;
        };
        let job = Job {
            event,
            caps,
            _pending: pending,
            _done: done,
        };
        match lanes.get_mut(&key) {
            Some(queue) if queue.len() >= self.inner.settings.queue_per_thread => return false,
            Some(queue) => {
                queue.push_back(job);
                return true;
            }
            None => {
                lanes.insert(key.clone(), VecDeque::new());
            }
        }
        let mut tasks = lock(&self.inner.tasks);
        while tasks.try_join_next().is_some() {}
        tasks.spawn(self.clone().lane(key, job));
        true
    }

    /// Answers the lane's messages one at a time, until none waits.
    async fn lane(self, key: LaneKey, mut job: Job) {
        let agent = key.0;
        loop {
            let answered = AssertUnwindSafe(self.candidate(&job.event, agent, job.caps))
                .catch_unwind()
                .await;
            if answered.is_err() {
                tracing::error!(%agent, message = %job.event.message.id, "handling a message panicked");
            }
            drop(job);
            let next = {
                let mut lanes = lock(&self.inner.lanes);
                match lanes.get_mut(&key).and_then(VecDeque::pop_front) {
                    Some(next) => next,
                    None => {
                        lanes.remove(&key);
                        return;
                    }
                }
            };
            job = next;
        }
    }

    /// Tells `event`'s thread that `agent` has too many messages to take
    /// this one.
    async fn busy(&self, event: &InboundEvent, agent: AgentId, caps: Caps) {
        tracing::warn!(%agent, message = %event.message.id, "too many messages waiting; not taking this one");
        let told = async {
            let Some(surface) = self.inner.surfaces.surface(agent, &event.conv).await else {
                return Ok(());
            };
            if !surface.can_post(&event.conv).await? {
                return Ok(());
            }
            let name = self.agent_name(agent).await?;
            let text = format!("{name} is busy with other requests. Ask again in a few minutes.");
            say(surface.as_ref(), &reply_target(event, caps), &text).await?;
            Ok::<_, PipelineError>(())
        };
        if let Err(err) = told.await {
            tracing::warn!(%agent, message = %event.message.id, error = %err, "couldn't say an agent is busy");
        }
    }

    /// The agents that may answer `event`, each once, and whether a bot
    /// sent it: one the surface flags, an agent's or a manager bot.
    async fn candidates(
        &self,
        event: &InboundEvent,
        caps: Caps,
    ) -> Result<(Vec<AgentId>, bool), StoreError> {
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
        let from_bot = event.sender_is_bot
            || event.sender_bot_user.is_some()
            || sender.is_some()
            || self.inner.settings.managers.contains(&event.sender);
        Ok((candidates, from_bot))
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
            Decision::LinkPrompt { requester } => self.link_prompt(event, agent, &requester).await,
            Decision::Refuse(reason) => self.refuse(event, agent, caps, reason).await,
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

    /// Tells `requester` privately how to link an account, if `agent`'s bot
    /// may post in `event`'s conversation: an agent that couldn't answer
    /// there doesn't prompt either.
    async fn link_prompt(
        &self,
        event: &InboundEvent,
        agent: AgentId,
        requester: &Requester,
    ) -> Result<(), PipelineError> {
        let Some(surface) = self.inner.surfaces.surface(agent, &event.conv).await else {
            return Ok(());
        };
        if !surface.can_post(&event.conv).await? {
            return Ok(());
        }
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
        caps: Caps,
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
        say(surface.as_ref(), &reply_target(event, caps), &text).await?;
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

    /// Runs `agent`'s turn on `event` and delivers what it made. Once the
    /// agent's bot is known to be able to post, a failure before the turn
    /// reached the model posts [`FAILED_TEXT`].
    async fn run(
        &self,
        event: &InboundEvent,
        agent: AgentId,
        caps: Caps,
        turn: Run,
    ) -> Result<(), PipelineError> {
        let Some(surface) = self.inner.surfaces.surface(agent, &event.conv).await else {
            tracing::warn!(%agent, conv = %event.conv, "the agent has no surface in this conversation");
            return Ok(());
        };
        if !surface.can_post(&event.conv).await? {
            tracing::info!(%agent, conv = %event.conv, "not answering: the agent's bot isn't in this conversation");
            return Ok(());
        }
        let target = reply_target(event, caps);
        let (working, ran) = match self.prepare(agent, event, turn.credential).await {
            Ok(None) => return Ok(()),
            Ok(Some(prepared)) => {
                let working = self.show_working(&surface, event, &target).await;
                let ran = self
                    .turn(event, agent, caps, &turn, surface.as_ref(), prepared)
                    .await;
                (Some(working), ran)
            }
            Err(err) => (None, Err(err)),
        };
        let delivered = match ran {
            Ok((session, turn_id, report)) => {
                let delivery = Delivery {
                    store: &self.inner.store,
                    surface: surface.as_ref(),
                    session: &session,
                    agent,
                    requester: &turn.requester,
                    hop: turn.hop,
                    target,
                    answered: &event.message,
                };
                delivery.report(turn_id, report).await;
                Ok(())
            }
            Err(err) => {
                tracing::warn!(%agent, message = %event.message.id, error = %err, "a turn failed before it reached the model");
                say(surface.as_ref(), &target, FAILED_TEXT)
                    .await
                    .map_err(PipelineError::from)
            }
        };
        if let Some(working) = working {
            working.finish().await;
        }
        delivered
    }

    /// What a turn of `agent` needs before its session: its bot's identity
    /// and model, with the persona file written. `None` when the agent or
    /// its bot is gone.
    async fn prepare(
        &self,
        agent: AgentId,
        event: &InboundEvent,
        credential: CredentialRef,
    ) -> Result<Option<Prepared>, PipelineError> {
        let Some(row) = self.inner.store.agent(agent).await? else {
            return Ok(None);
        };
        let Some(bot) = self.bot_of(&row, event).await? else {
            return Ok(None);
        };
        runner::write_persona(&self.inner.settings.data_dir, agent, &row.persona).await?;
        let model = self.model_for(credential).await?;
        Ok(Some(Prepared { bot, model }))
    }

    /// Puts the working emoji on `event`'s message until the returned
    /// guard is finished or dropped. A shutdown tells the thread of a turn
    /// whose guard is still held.
    async fn show_working(
        &self,
        surface: &Arc<dyn Surface>,
        event: &InboundEvent,
        target: &ReplyTarget,
    ) -> WorkingGuard {
        let emoji = &self.inner.settings.working_emoji;
        if let Err(err) = surface.react(&event.message, emoji).await {
            tracing::debug!(msg = %event.message.id, error = %err, "couldn't show that the turn is running");
        }
        let mut working = self.working();
        working.next += 1;
        let id = working.next;
        working.running.insert(
            id,
            Indicator {
                surface: Arc::clone(surface),
                msg: event.message.clone(),
                target: target.clone(),
                emoji: emoji.clone(),
            },
        );
        WorkingGuard {
            pipeline: self.clone(),
            id,
        }
    }

    /// Looks the session up, builds the turn message and runs the turn,
    /// once more on a session reset in between. What the turn message
    /// recorded is forgotten when the turn didn't run, which includes a
    /// process that crashed or timed out before the CLI read the message
    /// (no `init` line): that is [`PipelineError::Unread`].
    async fn turn(
        &self,
        event: &InboundEvent,
        agent: AgentId,
        caps: Caps,
        turn: &Run,
        surface: &dyn Surface,
        prepared: Prepared,
    ) -> Result<(Session, TurnId, TurnReport<Option<Outbox>>), PipelineError> {
        let store = &self.inner.store;
        let thread = thread_of(event, caps);
        let scope = match turn.scope {
            ScopeKind::Private => ScopeKey::Private,
            _ => ScopeKey::for_conversation(event.conv_kind, event.conv.clone()),
        };
        let sessions = self.inner.turns.sessions();
        let mut attempt = 0;
        loop {
            attempt += 1;
            let session = sessions.lookup_or_create(agent, &thread, &scope).await?;
            let built = message::build(
                store,
                surface,
                &session,
                &prepared.bot,
                event,
                &turn.requester,
            )
            .await?;
            let request = TurnRequest {
                turn: TurnId::new_v4(),
                message: built.text.clone(),
                credential: turn.credential,
                model: prepared.model.clone(),
                requester: turn.requester.clone(),
                hop: turn.hop,
                side: turn.side,
                kind: TurnKind::Normal,
                trigger: Some(event.message.id.clone()),
            };
            let turn_id = request.turn;
            match sessions.run_turn(session.id, request).await {
                Ok(report) if unread(&report.outcome) => {
                    built.forget(store, session.id).await;
                    return Err(PipelineError::Unread);
                }
                Ok(report) => return Ok((session, turn_id, report)),
                Err(err) => {
                    built.forget(store, session.id).await;
                    if matches!(
                        err,
                        RunnerError::Hook {
                            hook: "turn_starting",
                            ..
                        }
                    ) {
                        sessions.stop(session.id).await;
                    }
                    if matches!(err, RunnerError::SessionReset) && attempt == 1 {
                        tracing::info!(session = %session.id, "the session was reset before the turn ran; running it on the new one");
                        continue;
                    }
                    return Err(err.into());
                }
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

/// What [`Pipeline::prepare`] found.
struct Prepared {
    bot: MemberKey,
    model: Option<String>,
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

/// Renders and posts Markdown `text` to `target` without a `message_refs`
/// row: a notice of agentd's own, not a turn's reply.
async fn say(surface: &dyn Surface, target: &ReplyTarget, text: &str) -> Result<(), SurfaceError> {
    for chunk in surface.render(text) {
        surface.post(target, &chunk).await?;
    }
    Ok(())
}

/// Posts one chunk, and once more after the wait the platform asks for
/// when it asks to slow down, up to a few seconds.
async fn post_chunk(
    surface: &dyn Surface,
    target: &ReplyTarget,
    chunk: &str,
) -> Result<MsgRef, SurfaceError> {
    match surface.post(target, chunk).await {
        Err(SurfaceError::RateLimited { retry_after }) => {
            tokio::time::sleep(retry_after.min(RETRY_WAIT_CAP)).await;
            surface.post(target, chunk).await
        }
        posted => posted,
    }
}

/// `text`, cut at [`MAX_POST_BYTES`] on a character boundary with
/// [`TRUNCATED_NOTE`] after it when it is longer. A code block the cut
/// leaves open is closed first, so the note isn't shown as code.
fn capped(text: String) -> String {
    if text.len() <= MAX_POST_BYTES {
        return text;
    }
    let mut end = MAX_POST_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let kept = &text[..end];
    let close = match open_fence(kept) {
        Some(fence) if kept.ends_with('\n') => fence.to_owned(),
        Some(fence) => format!("\n{fence}"),
        None => String::new(),
    };
    format!("{kept}{close}{TRUNCATED_NOTE}")
}

/// The fence of the code block Markdown `text` leaves open at its end, if
/// any: the run of three or more backticks or tildes, indented by at most
/// three spaces, that opened it. A block closes at a line holding only a
/// run of the same character at least as long.
fn open_fence(text: &str) -> Option<&str> {
    let mut open: Option<&str> = None;
    for line in text.lines() {
        let trimmed = line.trim_start_matches(' ');
        if line.len() - trimmed.len() > 3 {
            continue;
        }
        let Some(marker) = trimmed.chars().next().filter(|c| matches!(c, '`' | '~')) else {
            continue;
        };
        let rest = trimmed.trim_start_matches(marker);
        let run = &trimmed[..trimmed.len() - rest.len()];
        if run.len() < 3 {
            continue;
        }
        match open {
            None if marker == '`' && rest.contains('`') => {}
            None => open = Some(run),
            Some(fence)
                if fence.starts_with(marker)
                    && run.len() >= fence.len()
                    && rest.trim().is_empty() =>
            {
                open = None;
            }
            Some(_) => {}
        }
    }
    open
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
    /// message, then the reactions, then the queued posts. Each part goes
    /// out whatever happened to the others; if any but a reaction couldn't,
    /// or the turn's outbox was lost because `turn_finished` failed, the
    /// thread is told with [`DELIVERY_FAILED_TEXT`].
    async fn report(&self, turn: TurnId, report: TurnReport<Option<Outbox>>) {
        let (outbox, mut complete) = match report.finished {
            Ok(outbox) => (outbox, true),
            Err(err) => {
                tracing::warn!(session = %self.session.id, error = %err, "the turn's outbox was lost");
                (None, false)
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
                (capped(text), reactions)
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
            complete = false;
        }
        complete &= self.post(turn, &reply).await;
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
                complete &= target.post(turn, &queued.text).await;
            }
        }
        if !complete && let Err(err) = say(self.surface, &self.target, DELIVERY_FAILED_TEXT).await {
            tracing::warn!(session = %self.session.id, error = %err, "couldn't say part of a reply was lost");
        }
    }

    /// Renders and posts Markdown `text` to the target, recording a
    /// `message_refs` row for each chunk. A chunk that can't be posted is
    /// skipped and the rest still go. Empty text posts nothing. False if a
    /// chunk was lost, or posted without its row: unattributed, a mention
    /// in it starts no hop, a reply to it reaches no agent, and the next
    /// turn shows it again as history.
    async fn post(&self, turn: TurnId, text: &str) -> bool {
        if text.trim().is_empty() {
            return true;
        }
        let mut complete = true;
        for chunk in self.surface.render(text) {
            let posted = match post_chunk(self.surface, &self.target, &chunk).await {
                Ok(posted) => posted,
                Err(err) => {
                    tracing::warn!(session = %self.session.id, conv = %self.target.conv, error = %err, "posting part of a reply failed");
                    complete = false;
                    continue;
                }
            };
            let recorded = self
                .store
                .record_message_ref(
                    &NewMessageRef {
                        session: self.session.id,
                        msg: &posted,
                        thread_root: self.target.thread_root.as_ref(),
                        agent: Some(self.agent),
                        turn: Some(turn),
                        requester: self.requester,
                        hop: self.hop,
                    },
                    OffsetDateTime::now_utc(),
                )
                .await;
            if let Err(err) = recorded {
                tracing::warn!(session = %self.session.id, msg = %posted.id, error = %err, "recording a posted message failed");
                complete = false;
            }
        }
        complete
    }

    async fn react(&self, msg: &MsgRef, emoji: &str) {
        if let Err(err) = self.surface.react(msg, emoji).await {
            tracing::warn!(session = %self.session.id, error = %err, "adding a reaction failed");
        }
    }
}

/// Whether a turn that ended with `outcome` ended before the CLI read its
/// message: it crashed or timed out without printing `init`.
fn unread(outcome: &TurnOutcome) -> bool {
    match outcome {
        TurnOutcome::Crashed { stats, .. } | TurnOutcome::TimedOut { stats } => !stats.init_seen,
        TurnOutcome::Finished(_) => false,
    }
}

/// Why handling a message failed.
#[derive(Debug, thiserror::Error)]
enum PipelineError {
    #[error("the CLI ended before it read the turn's message")]
    Unread,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Runner(#[from] RunnerError),
    #[error(transparent)]
    Surface(#[from] SurfaceError),
}

/// The sink behind [`Pipeline::sink`].
struct PipelineSink {
    pipeline: Pipeline,
    caps: Caps,
}

#[async_trait::async_trait]
impl Sink<InboundEvent> for PipelineSink {
    async fn send(&self, event: InboundEvent) -> Result<(), SendError> {
        self.pipeline.dispatch(event, self.caps).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::App;
    use crate::config::Config;
    use crate::config::tests::{MINIMAL, env};
    use crate::pipeline::TurnSettings;
    use core_types::{BindingId, ConvRef, SessionId, SurfaceKind};
    use runner::{PoolConfig, ProcessConfig};
    use sandbox::ProcessSandbox;
    use testkit::TempDir;
    use testkit::surface::MockSurface;
    use time::macros::datetime;

    async fn pipeline(dir: &TempDir) -> Pipeline {
        let config = Config::parse(MINIMAL, env()).unwrap();
        let store = Store::open_in_memory(config.sealer().unwrap())
            .await
            .unwrap();
        let app = App::new(config, store.clone(), None).unwrap();
        let sandbox = ProcessSandbox::new(store, dir.path()).unwrap();
        let settings = TurnSettings {
            process: ProcessConfig::default(),
            pool: PoolConfig::default(),
            image: "unused".to_owned(),
            data_dir: dir.path().to_owned(),
            agentctl_url: "http://127.0.0.1:1".to_owned(),
            env: std::collections::BTreeMap::new(),
        };
        let turns = Turns::start(&app, Arc::new(sandbox), settings).unwrap();
        Pipeline::for_app(&app, turns)
    }

    fn lane_key() -> LaneKey {
        let conv = ConvRef {
            surface: SurfaceKind::RocketChat,
            team: "chat.example.org".into(),
            conversation: "GENERAL".into(),
        };
        (
            AgentId::new_v4(),
            ThreadKey {
                conv,
                root: Some("q1".into()),
            },
        )
    }

    fn event(key: &LaneKey) -> Arc<InboundEvent> {
        let conv = key.1.conv.clone();
        Arc::new(InboundEvent {
            event_id: "Ev-q2".to_owned(),
            binding: BindingId::new_v4(),
            sender: MemberKey {
                surface: conv.surface,
                team: conv.team.clone(),
                user: "alice".into(),
            },
            sender_is_bot: false,
            sender_bot_user: None,
            conv: conv.clone(),
            conv_kind: ConvKind::Channel,
            thread_root: key.1.root.clone(),
            message: MsgRef {
                conv,
                id: "q2".into(),
            },
            text: "and then?".to_owned(),
            mentions: vec![],
            reply_to: None,
            files: vec![],
            received_at: datetime!(2026-10-07 00:00 UTC),
        })
    }

    #[tokio::test]
    async fn after_close_a_message_for_a_running_lane_is_dropped_not_queued() {
        let dir = TempDir::new("pipeline-lanes");
        let pipeline = pipeline(&dir).await;
        let key = lane_key();
        lock(&pipeline.inner.lanes).insert(key.clone(), VecDeque::new());
        let permits = pipeline.inner.pending.available_permits();

        let (done, before) = oneshot::channel();
        let caps = MockSurface::DEFAULT_CAPS;
        assert!(pipeline.enqueue(key.clone(), event(&key), caps, done));
        assert_eq!(
            lock(&pipeline.inner.lanes)[&key].len(),
            1,
            "queued while open"
        );

        pipeline.close();
        let (done, after) = oneshot::channel();
        assert!(pipeline.enqueue(key.clone(), event(&key), caps, done));
        assert_eq!(
            lock(&pipeline.inner.lanes)[&key].len(),
            1,
            "not queued once closed"
        );
        assert!(after.await.is_err(), "the dropped message is done with");
        assert_eq!(pipeline.inner.pending.available_permits(), permits - 1);
        drop(before);
    }

    async fn store() -> Store {
        let config = Config::parse(MINIMAL, env()).unwrap();
        Store::open_in_memory(config.sealer().unwrap())
            .await
            .unwrap()
    }

    /// The report of a turn that replied `text`, with what `turn_finished`
    /// returned.
    fn replied(
        text: &str,
        finished: Result<Option<Outbox>, runner::HookError>,
    ) -> TurnReport<Option<Outbox>> {
        TurnReport {
            outcome: TurnOutcome::Finished(runner::TurnResult {
                is_error: false,
                error_kind: None,
                subtype: Some("success".to_owned()),
                result: Some(text.to_owned()),
                terminal_reason: None,
                api_error_status: None,
                usage: None,
                cost_usd: None,
                process_total_cost_usd: None,
                session_id: None,
                stats: runner::TurnStats::default(),
            }),
            finished,
            process_start: None,
            reran: false,
        }
    }

    /// Delivers `report` in a thread through `store`, and returns the texts
    /// posted.
    async fn delivered(store: &Store, report: TurnReport<Option<Outbox>>) -> Vec<String> {
        let surface = MockSurface::new();
        let key = lane_key();
        let event = event(&key);
        let session = Session {
            id: SessionId::new_v4(),
            agent: key.0,
            thread: key.1.clone(),
            scope: ScopeKey::for_conversation(event.conv_kind, event.conv.clone()),
            kind: runner::SessionKind::Normal,
            started: true,
            maybe_started: false,
            created_at: event.received_at,
            last_turn_at: None,
            reset_at: None,
        };
        let requester = Requester {
            member: None,
            key: event.sender.clone(),
        };
        let delivery = Delivery {
            store,
            surface: &surface,
            session: &session,
            agent: key.0,
            requester: &requester,
            hop: Hop::ZERO,
            target: ReplyTarget::from(key.1),
            answered: &event.message,
        };
        delivery.report(TurnId::new_v4(), report).await;
        surface.posts().into_iter().map(|(_, text)| text).collect()
    }

    #[tokio::test]
    async fn a_lost_outbox_is_reported_as_an_incomplete_delivery() {
        let lost = replied("Here.", Err("the store is down".into()));
        assert_eq!(
            delivered(&store().await, lost).await,
            ["Here.", DELIVERY_FAILED_TEXT]
        );
        let kept = replied("Here.", Ok(None));
        assert_eq!(delivered(&store().await, kept).await, ["Here."]);
    }

    #[tokio::test]
    async fn a_reply_posted_without_its_attribution_is_reported_as_incomplete() {
        let store = store().await;
        store.close().await;
        assert_eq!(
            delivered(&store, replied("Here.", Ok(None))).await,
            ["Here.", DELIVERY_FAILED_TEXT]
        );
    }

    #[test]
    fn only_a_crash_or_timeout_before_init_leaves_the_message_unread() {
        let before = runner::TurnStats::default();
        let after = runner::TurnStats {
            init_seen: true,
            ..runner::TurnStats::default()
        };
        assert!(unread(&TurnOutcome::Crashed {
            exit_code: Some(70),
            stats: before.clone(),
        }));
        assert!(unread(&TurnOutcome::TimedOut {
            stats: before.clone()
        }));
        assert!(!unread(&TurnOutcome::Crashed {
            exit_code: Some(70),
            stats: after.clone(),
        }));
        assert!(!unread(&TurnOutcome::TimedOut { stats: after }));
    }

    #[test]
    fn a_long_reply_is_cut_on_a_character_boundary_with_a_note() {
        let short = "a".repeat(MAX_POST_BYTES);
        assert_eq!(capped(short.clone()), short);
        let long = format!("{}é tail", "a".repeat(MAX_POST_BYTES - 1));
        let cut = capped(long);
        assert!(cut.ends_with(TRUNCATED_NOTE));
        let kept = cut.strip_suffix(TRUNCATED_NOTE).unwrap();
        assert_eq!(kept, "a".repeat(MAX_POST_BYTES - 1));
    }

    #[test]
    fn a_reply_cut_inside_a_code_block_closes_it_before_the_note() {
        let long = format!("Here:\n```rust\n{}", "x".repeat(MAX_POST_BYTES));
        let cut = capped(long);
        let kept = cut.strip_suffix(TRUNCATED_NOTE).unwrap();
        assert!(kept.ends_with("xx\n```"), "{}", &kept[kept.len() - 10..]);
        assert_eq!(open_fence(kept), None);

        let at_a_line = format!("~~~~\n{}\n", "x".repeat(MAX_POST_BYTES - 6));
        let cut = capped(format!("{at_a_line}more"));
        assert_eq!(cut, format!("{at_a_line}~~~~{TRUNCATED_NOTE}"));

        let closed = format!("```\ncode\n```\n{}", "y".repeat(MAX_POST_BYTES));
        let cut = capped(closed);
        assert!(cut.strip_suffix(TRUNCATED_NOTE).unwrap().ends_with('y'));
    }

    #[test]
    fn open_fence_finds_the_block_left_open() {
        assert_eq!(open_fence("a\n````md\n```\nstill code"), Some("````"));
        assert_eq!(open_fence("~~~\n```\n"), Some("~~~"));
        assert_eq!(open_fence("```\n``` not a close\n"), Some("```"));
        assert_eq!(open_fence("  ```sh\nmake"), Some("```"));
        assert_eq!(open_fence("```\ncode\n```"), None);
        assert_eq!(open_fence("``\nx"), None);
        assert_eq!(open_fence("    ```\nindented code"), None);
        assert_eq!(open_fence("``` a`b\ninline"), None);
    }
}

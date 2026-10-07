//! [`Pipeline`]: from an inbound message to the agents' replies.

use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use core_types::{
    AgentId, Caps, ConvKind, CredentialRef, Hop, InboundEvent, MemberId, MemberKey, MsgRef,
    ReplyTarget, Requester, ScopeKey, ScopeKind, SendError, Sender, Side, Sink, Surface,
    SurfaceError, ThreadKey, Throttle, TurnId, TurnKind,
};
use futures::FutureExt as _;
use render::directives::{self, Directive};
use router::{Decision, ModelPolicy, RefuseReason};
use runner::{RunnerError, Session, TurnOutcome, TurnReport, TurnRequest};
use store::{Agent, NewMessageRef, Store, StoreError};
use time::OffsetDateTime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};
use tokio::task::JoinSet;

use super::Turns;
use super::billing::{CredentialFailure, FAILURE_DM_INTERVAL};
use super::message;
use super::view::StoreView;
use crate::commands::Replies;
use crate::ctl::{MAX_POST_BYTES, Outbox, SurfaceLookup};

/// The emoji an agent's bot reacts with to the message a turn answers,
/// while the turn runs, unless `[runner] working_emoji` says otherwise.
pub const DEFAULT_WORKING_EMOJI: &str = "hourglass_flowing_sand";

/// What any other failed turn tells the thread, including one that failed
/// before it reached the model.
pub const FAILED_TEXT: &str = "Sorry, that turn failed. Try again in a moment.";
/// What a turn that ran out of time tells the thread.
pub const TIMED_OUT_TEXT: &str = "Sorry, that took too long, and the turn was stopped.";
/// What the thread is told when part of a turn's reply, its files or its
/// queued posts couldn't be posted.
pub const DELIVERY_FAILED_TEXT: &str = "Sorry, part of this reply couldn't be delivered.";
/// What the thread is told when agentd stopped before the turn answering
/// it finished.
pub const RESTARTING_TEXT: &str =
    "Sorry, I'm restarting and couldn't finish this. Please ask again in a minute.";
/// What the thread is told when a message couldn't be checked with the
/// platform: the platform failed, or asked to slow down, or the client
/// had no quota left for the lookup.
pub const UNCONFIRMED_TEXT: &str = "Sorry, I couldn't check this message. Try again in a moment.";
/// Appended to a reply cut at [`MAX_POST_BYTES`].
pub const TRUNCATED_NOTE: &str = "\n\n*(The reply was cut here: it was too long to post.)*";

/// How many messages may wait for one agent in one thread, besides the
/// one being answered, unless the settings say otherwise.
pub const DEFAULT_QUEUE_PER_THREAD: usize = 8;
/// How many messages may wait or be answered at once across every agent
/// and thread, unless the settings say otherwise.
pub const DEFAULT_MAX_PENDING: usize = 64;
/// How many messages may wait or be answered at once for the agents of one
/// owner, across their threads, unless the settings say otherwise.
pub const DEFAULT_MAX_PENDING_PER_OWNER: usize = 16;

/// How many notices the agents of one owner may be posting at once: busy
/// lines and asks to try again. One more while that many are is dropped.
const MAX_NOTICES: usize = 8;

/// How often a warning about one agent's flood (a message not taken, a
/// notice not posted) is logged, at most; the next says how many went
/// quiet.
const FLOOD_WARNING_INTERVAL: Duration = Duration::from_secs(60);

/// How long the notices of turns cut short by a shutdown may take.
const NOTICE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest a chunk's post waits to be tried again after the platform
/// asked to slow down.
const RETRY_WAIT_CAP: Duration = Duration::from_secs(5);

/// How the pipeline runs, besides the store and the runner.
#[derive(Debug, Clone)]
pub struct PipelineSettings {
    /// agentd's data directory, which holds the agents' persona files and
    /// skills.
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
    /// How many messages may wait or be answered at once for the agents of
    /// one owner.
    pub max_pending_per_owner: usize,
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
///    message, or another agent or thread. At most
///    [`queue_per_thread`](PipelineSettings::queue_per_thread) messages
///    wait for one agent in one thread,
///    [`max_pending_per_owner`](PipelineSettings::max_pending_per_owner)
///    wait or run for the agents of one owner, and
///    [`max_pending`](PipelineSettings::max_pending) in all, so one owner's
///    agents can't take every agent's place, however many they are and
///    whatever their owner signs for them; a person's message past any of
///    them gets one line saying the agent is busy, and a bot's gets
///    nothing, so two bots can't answer each other's busy lines.
/// 3. **Routing.** [`router::route`] for the candidate, with a view of the
///    store loaded for it.
/// 4. **Confirming.** Unless the decision is to ignore, the platform's
///    copy of the message ([`Surface::confirm`]) replaces the event, and is
///    routed again. The decision stands only if the copy's is the same, in
///    the same conversation, message and thread; anything else drops the
///    message silently. A confirmation that fails, the platform being down
///    or asking to slow down, tells the thread to try again
///    ([`UNCONFIRMED_TEXT`]). So what an event claims decides nothing,
///    whoever it names as the sender.
///
///    The busy line and the ask to try again are notices, posted in a task
///    of their own, which holds none of the message's places: at most eight
///    at once for the agents of one owner, and one more is dropped.
/// 5. **The turn.** On [`Decision::Run`], only when the agent's bot may
///    post in the conversation without joining it
///    ([`Surface::can_post`]): the persona file is written from the store,
///    and the bundled `agentctl` skill into the agent's skills,
///    the thread's session looked up (a DM has one for the conversation, a
///    channel one per thread, rooted at the message when it starts one),
///    the turn message built with what the session's transcript lacks, and
///    the turn run, with the working emoji on the message until its reply
///    is delivered. Only the owner's own turn on the owner's side runs in
///    the agent's private session; a decision that would put anyone else's
///    there fails the turn before it starts. A turn refused with
///    [`RunnerError::SessionReset`] runs once more, on the session looked
///    up again. What the turn message recorded as shown is forgotten when
///    the turn never reached the model, so the next turn shows it again.
/// 6. **Delivery**, as the agent's bot, in the thread: the directives are
///    taken out of the reply, the turn's staged attachments uploaded, the
///    reply cut at [`MAX_POST_BYTES`] (closing a code block the cut left
///    open), rendered, split and posted, and a `message_refs` row recorded
///    for every chunk with the turn's requester and hop; then the
///    directives' reactions, and the reactions and posts the turn queued
///    with agentctl. Each goes out even when another failed, and then the
///    thread is told part of the reply was lost. A failed turn posts a
///    short message that says why when the runner could tell. A usage
///    limit or a refused login names whose account it was, the
///    requester's or the community key's, and the requester alone is also
///    told privately by the manager bot, unless the thread is their own DM
///    with the agent or they were told about the same kind of failure
///    within [`FAILURE_DM_INTERVAL`]; the agent's owner never is, unless
///    they asked.
/// 7. [`Decision::LinkPrompt`] sends the requester a DM from the manager
///    bot saying how to link an account, and [`Decision::RelinkPrompt`]
///    one saying their link stopped working and how to link it again, when
///    the agent's bot may post in the conversation; nothing runs for them.
///    [`Decision::Refuse`] posts one line in the thread, and
///    [`Decision::Ignore`] does nothing.
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
    pending: Arc<Semaphore>,
    shares: Mutex<HashMap<MemberId, Share>>,
    tasks: Mutex<JoinSet<()>>,
    closed: AtomicBool,
    working: Mutex<Working>,
    floods: Throttle<(AgentId, Flood)>,
}

/// What an agent's flood makes the pipeline refuse or drop, each warned
/// about apart: a message not taken, a notice not posted or failed, and a
/// message that confirming dropped or couldn't check, which is what a
/// forged event reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Flood {
    BotMessage,
    Message,
    Notice,
    Unconfirmed,
}

/// One agent in one thread: its messages are answered in arrival order.
type LaneKey = (AgentId, ThreadKey);

/// A message waiting for one candidate agent, whose owner is `owner`.
/// Dropping it releases its places under [`PipelineSettings::max_pending`]
/// and [`PipelineSettings::max_pending_per_owner`]; once it and the notices
/// it started are gone, whoever waits for it in [`Pipeline::handle`] is
/// told.
struct Job {
    event: Arc<InboundEvent>,
    caps: Caps,
    owner: MemberId,
    _pending: (OwnedSemaphorePermit, OwnedSemaphorePermit),
    done: Done,
}

/// Dropped, with every clone, once a message is done with.
type Done = Arc<oneshot::Sender<()>>;

/// The places of one owner's agents: their messages waiting or being
/// answered, and their notices being posted.
#[derive(Clone)]
struct Share {
    pending: Arc<Semaphore>,
    notices: Arc<Semaphore>,
}

/// A line the pipeline posts on its own, in a task of its own.
#[derive(Debug, Clone, Copy)]
enum Notice {
    /// The agent has too many messages to take this one.
    Busy,
    /// The message couldn't be confirmed: [`UNCONFIRMED_TEXT`].
    Unconfirmed,
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

/// Takes the pipeline's finished tasks off `tasks`, logging any that
/// panicked, as [`Pipeline::drain`] does.
fn reap(tasks: &mut JoinSet<()>) {
    while let Some(joined) = tasks.try_join_next() {
        if let Err(err) = joined {
            tracing::error!(error = %err, "a pipeline task failed");
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
                pending,
                shares: Mutex::new(HashMap::new()),
                tasks: Mutex::new(JoinSet::new()),
                closed: AtomicBool::new(false),
                working: Mutex::new(Working::default()),
                floods: Throttle::new(FLOOD_WARNING_INTERVAL),
            }),
        }
    }

    /// Where a surface with `caps` delivers its messages. Sending looks up
    /// the message's candidates and queues it for each, or tells the
    /// thread an agent is busy, and never waits for a turn.
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

    /// Stops taking messages: those sent from now on are dropped.
    pub fn close(&self) {
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
    async fn dispatch(&self, event: InboundEvent, caps: Caps) -> Vec<oneshot::Receiver<()>> {
        if self.is_closed() {
            tracing::info!(message = %event.message.id, "shutting down: not handling a message");
            return Vec::new();
        }
        let (candidates, from_bot) = match self.candidates(&event, caps).await {
            Ok(found) => found,
            Err(err) => {
                tracing::warn!(message = %event.message.id, error = %err, "couldn't look up the agents a message addresses");
                return Vec::new();
            }
        };
        let event = Arc::new(event);
        let thread = thread_of(&event, caps);
        let mut waiting = Vec::new();
        for (agent, owner) in candidates {
            let (done, finished) = oneshot::channel();
            let done: Done = Arc::new(done);
            let taken = self.places(owner).is_some_and(|places| {
                let job = Job {
                    event: Arc::clone(&event),
                    caps,
                    owner,
                    _pending: places,
                    done: Arc::clone(&done),
                };
                self.enqueue((agent, thread.clone()), job)
            });
            if taken {
                waiting.push(finished);
            } else if from_bot {
                if let Some(quiet) = self.flooded(agent, Flood::BotMessage) {
                    tracing::warn!(%agent, message = %event.message.id, dropped_since_last_warning = quiet, "too many messages waiting; dropping a bot's message");
                }
            } else {
                if let Some(quiet) = self.flooded(agent, Flood::Message) {
                    tracing::warn!(%agent, message = %event.message.id, refused_since_last_warning = quiet, "too many messages waiting; not taking this one");
                }
                self.notice(&event, agent, owner, caps, Notice::Busy, done);
                waiting.push(finished);
            }
        }
        waiting
    }

    /// Whether a warning about `agent`'s `flood` is due: `Some` of how many
    /// went quiet since the last, at most once per
    /// [`FLOOD_WARNING_INTERVAL`] for each agent and kind, since whoever
    /// signs an agent's events can make as many as they like.
    fn flooded(&self, agent: AgentId, flood: Flood) -> Option<u64> {
        self.inner.floods.record((agent, flood), Instant::now())
    }

    /// A place in the pipeline and one among `owner`'s agents', or `None`
    /// when either is full.
    fn places(&self, owner: MemberId) -> Option<(OwnedSemaphorePermit, OwnedSemaphorePermit)> {
        let pending = Arc::clone(&self.inner.pending).try_acquire_owned().ok()?;
        let owners = self.share(owner).pending.try_acquire_owned().ok()?;
        Some((pending, owners))
    }

    /// Queues `job` for the agent of `key`, starting the lane's task if it
    /// has none. False when the lane is full. It never waits, so a sender
    /// cancelled mid-dispatch can't leave a lane without its task.
    fn enqueue(&self, key: LaneKey, job: Job) -> bool {
        let first = {
            let mut lanes = lock(&self.inner.lanes);
            match lanes.get_mut(&key) {
                Some(queue) if queue.len() >= self.inner.settings.queue_per_thread => return false,
                Some(queue) => {
                    queue.push_back(job);
                    return true;
                }
                None => {
                    lanes.insert(key.clone(), VecDeque::new());
                    job
                }
            }
        };
        let mut tasks = lock(&self.inner.tasks);
        if self.is_closed() {
            lock(&self.inner.lanes).remove(&key);
            return true;
        }
        reap(&mut tasks);
        tasks.spawn(self.clone().lane(key, first));
        true
    }

    /// The places of `owner`'s agents, made on their first message.
    fn share(&self, owner: MemberId) -> Share {
        let settings = &self.inner.settings;
        lock(&self.inner.shares)
            .entry(owner)
            .or_insert_with(|| Share {
                pending: Arc::new(Semaphore::new(settings.max_pending_per_owner)),
                notices: Arc::new(Semaphore::new(MAX_NOTICES)),
            })
            .clone()
    }

    /// Answers the lane's messages one at a time, until none waits. Once
    /// the pipeline is closed it starts none of those still waiting: they
    /// are dropped, as a message sent after closing is.
    async fn lane(self, key: LaneKey, mut job: Job) {
        let agent = key.0;
        loop {
            let answered = AssertUnwindSafe(self.candidate(&job, agent))
                .catch_unwind()
                .await;
            if answered.is_err() {
                tracing::error!(%agent, message = %job.event.message.id, "handling a message panicked");
            }
            drop(job);
            let next = {
                let mut lanes = lock(&self.inner.lanes);
                if self.is_closed() {
                    let dropped = lanes.remove(&key).map_or(0, |queue| queue.len());
                    if dropped > 0 {
                        tracing::info!(%agent, messages = dropped, "shutting down: not handling the messages still waiting");
                    }
                    return;
                }
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

    /// Posts `notice` in `event`'s thread as `agent`, whose owner is
    /// `owner`, in a task of its own, unless [`MAX_NOTICES`] of the owner's
    /// agents' are being posted already or the pipeline is closed. `done`
    /// is dropped once the line is posted or given up on.
    fn notice(
        &self,
        event: &Arc<InboundEvent>,
        agent: AgentId,
        owner: MemberId,
        caps: Caps,
        notice: Notice,
        done: Done,
    ) {
        let Ok(permit) = self.share(owner).notices.try_acquire_owned() else {
            if let Some(quiet) = self.flooded(agent, Flood::Notice) {
                tracing::warn!(%agent, message = %event.message.id, ?notice, not_posted_since_last_warning = quiet, "too many notices being posted; not posting another");
            }
            return;
        };
        let mut tasks = lock(&self.inner.tasks);
        if self.is_closed() {
            return;
        }
        reap(&mut tasks);
        let pipeline = self.clone();
        let event = Arc::clone(event);
        tasks.spawn(async move {
            let _permit = permit;
            let _done = done;
            pipeline.post_notice(&event, agent, caps, notice).await;
        });
    }

    async fn post_notice(&self, event: &InboundEvent, agent: AgentId, caps: Caps, notice: Notice) {
        let told = async {
            let Some(surface) = self.inner.surfaces.surface(agent, &event.conv).await else {
                return Ok(());
            };
            if !surface.can_post(&event.conv).await? {
                return Ok(());
            }
            let text = match notice {
                Notice::Busy => format!(
                    "{} is busy with other requests. Ask again in a few minutes.",
                    self.agent_name(agent).await?
                ),
                Notice::Unconfirmed => UNCONFIRMED_TEXT.to_owned(),
            };
            say(surface.as_ref(), &reply_target(event, caps), &text).await?;
            Ok::<_, PipelineError>(())
        };
        if let Err(err) = told.await
            && let Some(quiet) = self.flooded(agent, Flood::Notice)
        {
            tracing::warn!(%agent, message = %event.message.id, ?notice, error = %err, failed_since_last_warning = quiet, "couldn't post a notice");
        }
    }

    /// The agents that may answer `event`, each once with its owner, and
    /// whether a bot sent it: one the surface flags, an agent's or a
    /// manager bot.
    async fn candidates(
        &self,
        event: &InboundEvent,
        caps: Caps,
    ) -> Result<(Vec<(AgentId, MemberId)>, bool), StoreError> {
        let store = &self.inner.store;
        let mut candidates = Vec::new();
        if caps.per_binding_delivery {
            if let Some(agent) = store.agent_for_binding(event.binding).await? {
                candidates.push((agent.id, agent.owner));
            }
        } else {
            for user in &event.mentions {
                let bot = MemberKey {
                    surface: event.conv.surface,
                    team: event.conv.team.clone(),
                    user: user.clone(),
                };
                if let Some((agent, _)) = store.agent_for_bot(&bot).await? {
                    candidates.push((agent.id, agent.owner));
                }
            }
            if event.conv_kind == ConvKind::Dm
                && let Some(agent) = store.agent_for_binding(event.binding).await?
            {
                candidates.push((agent.id, agent.owner));
            }
            if let Some(reply_to) = &event.reply_to
                && let Some(agent) = store
                    .posted_message_ref(reply_to)
                    .await?
                    .and_then(|posted| posted.agent)
                && let Some(agent) = store.agent(agent).await?
            {
                candidates.push((agent.id, agent.owner));
            }
        }
        let sender = store.agent_of_bot_user(&event.sender).await?;
        let mut seen = Vec::new();
        candidates.retain(|(agent, _)| {
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

    /// Routes `job`'s message for `agent`, and unless the decision is to
    /// ignore it, routes the platform's copy again and acts on the copy.
    async fn candidate(&self, job: &Job, agent: AgentId) {
        let (event, caps) = (job.event.as_ref(), job.caps);
        let Some(decision) = self.decide(event, agent).await else {
            return;
        };
        if let Decision::Ignore(reason) = decision {
            tracing::debug!(%agent, message = %event.message.id, %reason, "ignored a message");
            return;
        }
        let Some(copy) = self.confirmed(job, agent).await else {
            return;
        };
        if copy == *event {
            self.act(event, agent, caps, decision).await;
            return;
        }
        let Some(confirmed) = self.decide(&copy, agent).await else {
            return;
        };
        if confirmed != decision {
            if let Some(quiet) = self.flooded(agent, Flood::Unconfirmed) {
                tracing::warn!(%agent, message = %event.message.id, unconfirmed_since_last_warning = quiet, "the platform's copy of a message routes differently from its event; dropped it");
            }
            return;
        }
        self.act(&copy, agent, caps, confirmed).await;
    }

    /// The router's decision on `event` for `agent`. `None` when the store
    /// can't be read.
    async fn decide(&self, event: &InboundEvent, agent: AgentId) -> Option<Decision> {
        let store = &self.inner.store;
        let view = match StoreView::load(store, event, agent, &self.inner.settings.managers).await {
            Ok(view) => view,
            Err(err) => {
                tracing::warn!(%agent, message = %event.message.id, error = %err, "couldn't load what routing needs");
                return None;
            }
        };
        Some(router::route(event, agent, &view))
    }

    /// The platform's copy of `job`'s message, if it is the same message in
    /// the same thread. On a failure that says nothing about the message
    /// (the platform unreachable, or asking to slow down past the client's
    /// retries), the thread is told to try again with a
    /// [`notice`](Self::notice), if `agent`'s bot may post there; a copy
    /// the platform doesn't have, or has elsewhere, is dropped without a
    /// word.
    async fn confirmed(&self, job: &Job, agent: AgentId) -> Option<InboundEvent> {
        let (event, caps) = (job.event.as_ref(), job.caps);
        let surface = self.inner.surfaces.surface(agent, &event.conv).await?;
        match surface.confirm(event).await {
            Ok(Some(copy))
                if copy.message == event.message
                    && thread_of(&copy, caps) == thread_of(event, caps) =>
            {
                Some(copy)
            }
            Ok(_) => {
                if let Some(quiet) = self.flooded(agent, Flood::Unconfirmed) {
                    tracing::warn!(%agent, message = %event.message.id, unconfirmed_since_last_warning = quiet, "the platform doesn't have this message as it arrived; dropped it");
                }
                None
            }
            Err(err @ (SurfaceError::RateLimited { .. } | SurfaceError::Transport(_))) => {
                if let Some(quiet) = self.flooded(agent, Flood::Unconfirmed) {
                    tracing::warn!(%agent, message = %event.message.id, error = %err, unconfirmed_since_last_warning = quiet, "couldn't confirm a message with the platform; asking to try again");
                }
                self.notice(
                    &job.event,
                    agent,
                    job.owner,
                    caps,
                    Notice::Unconfirmed,
                    Arc::clone(&job.done),
                );
                None
            }
            Err(err) => {
                if let Some(quiet) = self.flooded(agent, Flood::Unconfirmed) {
                    tracing::warn!(%agent, message = %event.message.id, error = %err, unconfirmed_since_last_warning = quiet, "the platform refused to confirm a message; dropped it");
                }
                None
            }
        }
    }

    /// Acts on `decision` for `agent` on `event`.
    async fn act(&self, event: &InboundEvent, agent: AgentId, caps: Caps, decision: Decision) {
        let result = match decision {
            Decision::Ignore(_) => Ok(()),
            Decision::LinkPrompt { requester } => {
                self.link_prompt(event, agent, &requester, link_text).await
            }
            Decision::RelinkPrompt { requester } => {
                self.link_prompt(event, agent, &requester, relink_text)
                    .await
            }
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

    /// Tells `requester` privately how to link an account, with the text
    /// `text` makes from the agent's name, if `agent`'s bot may post in
    /// `event`'s conversation: an agent that couldn't answer there doesn't
    /// prompt either.
    async fn link_prompt(
        &self,
        event: &InboundEvent,
        agent: AgentId,
        requester: &Requester,
        text: fn(&str) -> String,
    ) -> Result<(), PipelineError> {
        let Some(surface) = self.inner.surfaces.surface(agent, &event.conv).await else {
            return Ok(());
        };
        if !surface.can_post(&event.conv).await? {
            return Ok(());
        }
        let text = text(&self.agent_name(agent).await?);
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
                let failure = CredentialFailure::of(&report.outcome);
                let delivery = Delivery {
                    store: &self.inner.store,
                    surface: surface.as_ref(),
                    session: &session,
                    agent,
                    requester: &turn.requester,
                    hop: turn.hop,
                    credential: turn.credential,
                    target,
                    answered: &event.message,
                };
                delivery.report(turn_id, report).await;
                let their_own_dm = event.is_dm() && event.sender == turn.requester.key;
                if let Some(failure) = failure
                    && !their_own_dm
                {
                    self.tell_requester(agent, &turn, failure).await;
                }
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
    /// and model, with the persona file and the bundled skill written.
    /// `None` when the agent or its bot is gone.
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
        crate::skills::write_bundled(&self.inner.settings.data_dir, agent).await?;
        let model = self.model_for(credential).await?;
        Ok(Some(Prepared {
            bot,
            model,
            owner: row.owner,
        }))
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
        let scope = turn_scope(turn, prepared.owner, event).ok_or_else(|| {
            tracing::error!(%agent, message = %event.message.id, scope = ?turn.scope, side = ?turn.side, "refused to run a turn on the agent's private side for someone other than its owner");
            PipelineError::NotTheOwnersTurn
        })?;
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

    /// Tells `turn`'s requester privately, from the manager bot, that
    /// `agent`'s turn failed on their account or on the community key.
    /// A refused login whose link is already marked broken gets no message
    /// here: the relink notice tells the member once. Nor does a failure of
    /// a kind the requester was told about within [`FAILURE_DM_INTERVAL`].
    /// A message that fails to send releases its claim, so the next failure
    /// of that kind tries again.
    async fn tell_requester(&self, agent: AgentId, turn: &Run, failure: CredentialFailure) {
        let store = &self.inner.store;
        if failure == CredentialFailure::Refused
            && let CredentialRef::Member(member) = turn.credential
        {
            match store.claude_link_status(member).await {
                Ok(Some(status)) if status.broken_at.is_some() => return,
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(%agent, %member, error = %err, "couldn't read a link's state; telling the requester anyway");
                }
            }
        }
        let kind = failure.notice_kind(turn.credential);
        let now = OffsetDateTime::now_utc();
        let claimed = match store
            .claim_failure_notice(&turn.requester.key, kind, now, FAILURE_DM_INTERVAL)
            .await
        {
            Ok(true) => true,
            Ok(false) => {
                tracing::debug!(%agent, requester = %turn.requester.key, kind, "the requester was told about this failure recently");
                return;
            }
            Err(err) => {
                tracing::warn!(%agent, requester = %turn.requester.key, error = %err, "couldn't check when the requester was last told; telling them anyway");
                false
            }
        };
        let name = self.agent_name(agent).await.unwrap_or_else(|err| {
            tracing::warn!(%agent, error = %err, "couldn't read the agent's name");
            "This agent".to_owned()
        });
        let text = failure.requester_text(turn.credential, &name);
        if let Err(err) = self.inner.replies.dm(&turn.requester.key, &text).await {
            tracing::warn!(%agent, requester = %turn.requester.key, error = %err, "couldn't tell the requester why their turn failed");
            if claimed
                && let Err(err) = store
                    .release_failure_notice(&turn.requester.key, kind, now)
                    .await
            {
                tracing::warn!(%agent, requester = %turn.requester.key, error = %err, "couldn't release the claim on telling the requester");
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

/// The link prompt for an agent named `name`.
fn link_text(name: &str) -> String {
    format!(
        "{name} runs on the Claude account of whoever asks it. Link yours to use it: send \
         `login` to me here."
    )
}

/// The prompt for an agent named `name` to a member whose link broke.
fn relink_text(name: &str) -> String {
    format!(
        "{name} runs on the Claude account of whoever asks it, and yours stopped working: \
         Anthropic refused to renew the link. Send `login` to me here to link it again."
    )
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
    owner: MemberId,
}

/// The scope `turn` runs in, for an agent owned by `owner`: the agent's
/// private one only for the owner's own turn, on the owner's side and
/// their own credential, answering the owner's own message in a
/// one-to-one DM; otherwise `event`'s conversation's own channel, group DM
/// or DM scope. `None` for a decision that would put anyone else's turn, a
/// turn on any other event (a channel message, another agent's hop), or a
/// public-side turn on the private side, or an owner-side turn anywhere
/// else: such a turn must not run.
fn turn_scope(turn: &Run, owner: MemberId, event: &InboundEvent) -> Option<ScopeKey> {
    let owners_own = turn.requester.member == Some(owner)
        && turn.credential == CredentialRef::Member(owner)
        && turn.side == Side::Owner
        && event.is_dm()
        && event.sender == turn.requester.key
        && !event.sender_is_bot
        && event.sender_bot_user.is_none();
    match turn.scope {
        ScopeKind::Private => owners_own.then_some(ScopeKey::Private),
        ScopeKind::Channel | ScopeKind::GroupDm | ScopeKind::Dm => (turn.side == Side::Public)
            .then(|| ScopeKey::for_conversation(event.conv_kind, event.conv.clone())),
    }
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
    credential: CredentialRef,
    target: ReplyTarget,
    answered: &'a MsgRef,
}

impl Delivery<'_> {
    /// Delivers `report`: the attachments, then the reply or the failure's
    /// message, then the reactions, then the queued posts. Each part goes
    /// out whatever happened to the others; if any couldn't, the thread is
    /// told with [`DELIVERY_FAILED_TEXT`].
    async fn report(&self, turn: TurnId, report: TurnReport<Option<Outbox>>) {
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
                (capped(text), reactions)
            }
            TurnOutcome::Finished(_) => {
                let text = CredentialFailure::of(&report.outcome)
                    .map_or(FAILED_TEXT, |failure| failure.thread_text(self.credential));
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
        let mut complete = true;
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
    /// chunk was lost.
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
    #[error("the turn isn't the owner's own, so it can't run on the agent's private side")]
    NotTheOwnersTurn,
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

    fn is_closed(&self) -> bool {
        self.pipeline.is_closed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever a decision says, only the owner's own turn, on the owner's
    /// side and credential, answering the owner's own message in a
    /// one-to-one DM, resolves to the agent's private scope, and an
    /// owner-side turn resolves nowhere else.
    #[test]
    fn a_non_owners_turn_never_resolves_to_the_private_scope() {
        let owner = MemberId::new_v4();
        let other = MemberId::new_v4();
        let conv = core_types::ConvRef {
            surface: core_types::SurfaceKind::Slack,
            team: "T1".into(),
            conversation: "C1".into(),
        };
        let requester_key: MemberKey = "slack:T1:U1".parse().unwrap();
        let someone_else: MemberKey = "slack:T1:U2".parse().unwrap();
        let mut checked = 0;
        let mut private = 0;
        for requester in [Some(owner), Some(other), None] {
            for credential in [
                CredentialRef::Member(owner),
                CredentialRef::Member(other),
                CredentialRef::Community,
            ] {
                for scope in [
                    ScopeKind::Private,
                    ScopeKind::Channel,
                    ScopeKind::GroupDm,
                    ScopeKind::Dm,
                ] {
                    for side in [Side::Owner, Side::Public] {
                        for conv_kind in [ConvKind::Dm, ConvKind::GroupDm, ConvKind::Channel] {
                            for sender in [&requester_key, &someone_else] {
                                for (sender_is_bot, bot_user) in
                                    [(false, false), (true, false), (true, true), (false, true)]
                                {
                                    let turn = Run {
                                        requester: Requester {
                                            member: requester,
                                            key: requester_key.clone(),
                                        },
                                        hop: Hop::ZERO,
                                        credential,
                                        scope,
                                        side,
                                    };
                                    let event = InboundEvent {
                                        event_id: "e".into(),
                                        binding: core_types::BindingId::new_v4(),
                                        sender: sender.clone(),
                                        sender_is_bot,
                                        sender_bot_user: bot_user.then(|| sender.user.clone()),
                                        conv: conv.clone(),
                                        conv_kind,
                                        thread_root: None,
                                        message: MsgRef {
                                            conv: conv.clone(),
                                            id: "1.0".into(),
                                        },
                                        text: String::new(),
                                        mentions: Vec::new(),
                                        reply_to: None,
                                        files: Vec::new(),
                                        received_at: OffsetDateTime::UNIX_EPOCH,
                                    };
                                    let resolved = turn_scope(&turn, owner, &event);
                                    let owners_own = requester == Some(owner)
                                        && credential == CredentialRef::Member(owner)
                                        && side == Side::Owner
                                        && scope == ScopeKind::Private
                                        && conv_kind == ConvKind::Dm
                                        && *sender == requester_key
                                        && !sender_is_bot
                                        && !bot_user;
                                    let case = format!(
                                        "{requester:?} {credential:?} {scope:?} {side:?} \
                                         {conv_kind:?} {sender} bot={sender_is_bot}/{bot_user}"
                                    );
                                    assert_eq!(
                                        resolved == Some(ScopeKey::Private),
                                        owners_own,
                                        "{case}"
                                    );
                                    if side == Side::Owner {
                                        assert!(resolved.is_none() || owners_own, "{case}");
                                    }
                                    if let Some(resolved) = resolved
                                        && !owners_own
                                    {
                                        assert_eq!(
                                            resolved,
                                            ScopeKey::for_conversation(conv_kind, conv.clone()),
                                            "a public turn runs in the conversation's own scope: {case}"
                                        );
                                    }
                                    private += usize::from(owners_own);
                                    checked += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 3 * 3 * 4 * 2 * 3 * 2 * 4);
        assert_eq!(private, 1, "exactly one combination is the owner's own DM");
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

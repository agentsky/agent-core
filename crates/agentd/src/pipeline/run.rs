//! [`Pipeline`]: from an inbound message to the agents' replies, and
//! private tasks' work ([`private`]).

mod private;

pub use private::{WORK_LEASE, WORK_MAX_ATTEMPTS, WORK_RETRY};

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use core_types::{
    AgentId, BindingId, Caps, ConsentId, ConvKind, ConvRef, CredentialRef, Hop, InboundEvent,
    MemberId, MemberKey, MsgRef, Posted, ReplyTarget, Requester, ScopeKey, ScopeKind, SendError,
    Sender, SessionId, Side, Sink, Surface, SurfaceError, ThreadKey, Throttle, TurnId, TurnKind,
};
use futures::FutureExt as _;
use render::directives::{self, Directive};
use router::{Decision, ModelPolicy, RefuseReason};
use runner::{RunnerError, Session, TurnOutcome, TurnReport, TurnRequest};
use store::{
    Agent, CostUnknown, HandOff, LimitWindow, NewHandOff, NewMessageRef, PROCESSED_EVENT_RETENTION,
    Store, StoreError, TurnUsage,
};
use time::OffsetDateTime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot, watch};
use tokio::task::JoinSet;

use super::Turns;
use super::billing::{CredentialFailure, FAILURE_DM_INTERVAL};
use super::message;
use super::view::{StoreView, ViewContext};
use crate::commands::Replies;
use crate::ctl::{MAX_POST_BYTES, Outbox, SurfaceLookup};
use crate::policy::Limits;

/// How long after the manager bot told a requester that an agent refused
/// them (its rules deny them, or they are banned) it may tell them again.
pub const REFUSAL_DM_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

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
    /// The community admins' identities, whom a ban never holds back, as
    /// [`Commands`](crate::commands::Commands) never holds them back.
    pub admins: Vec<MemberKey>,
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
    /// The community's caps on threads and hops, from `[limits]`.
    pub limits: Limits,
    /// The clock the limits, the meter, the notices and the hand-offs read:
    /// which day and hour a turn counts in, which window a limit's notice
    /// is for, when a requester was last told of a failure or a refusal,
    /// and when a hand-off was recorded and is due.
    pub now: fn() -> OffsetDateTime,
    /// How long the attribution of a message an agent's bot sent is waited
    /// for: agentd records it just after posting, and the platform may
    /// deliver the message sooner. [`ATTRIBUTION_WAIT`] by default.
    pub attribution_wait: Duration,
    /// How often the hand-off worker looks at the hand-offs due
    /// ([`Pipeline::run_hand_offs`]). [`HAND_OFF_SWEEP_INTERVAL`] by
    /// default.
    pub hand_off_sweep: Duration,
}

/// The default [`PipelineSettings::attribution_wait`].
pub const ATTRIBUTION_WAIT: Duration = Duration::from_secs(2);

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
/// 7. **Hand-off.** Each post of the turn's own in the thread it answered,
///    its reply's chunks and the posts it queued (`agentctl post` and
///    `ask-agent`), outside a one-to-one DM and a private task, hands off
///    to the other managed agents the platform reads it as mentioning, each
///    agent once for the turn, from the first post that mentions it: as the
///    post is recorded, a `hand_offs` row keeps it, and once the delivery
///    is done it is queued for that agent as the posting bot's message,
///    without a read-back, as the platform would deliver it. The router
///    gives such a hop the requester and the next hop of the post's
///    `message_refs` row, so the hop caps, the thread's budget and the
///    agent's rules hold. One hop runs for each agent and posting turn: the
///    first copy to act claims it in the store, and any other, agentd's or
///    the platform's, is dropped. A row is deleted once its job settled the
///    hand-off, and is otherwise taken again by
///    [`replay_hand_offs`](Self::replay_hand_offs), here or on another
///    instance.
/// 8. [`Decision::LinkPrompt`] sends the requester a DM from the manager
///    bot saying how to link an account, and [`Decision::RelinkPrompt`]
///    one saying their link stopped working and how to link it again, when
///    the agent's bot may post in the conversation; nothing runs for them.
///    [`Decision::Refuse`] posts one line in the thread, or, for a refusal
///    of the requester themselves (banned, or denied by the agent's rules),
///    sends it to them from the manager bot at most once per
///    [`REFUSAL_DM_INTERVAL`] for each agent (for a ban, for all agents
///    together). [`Decision::Ignore`] does nothing.
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
    private: Mutex<HashMap<ConsentId, private::Claim>>,
    kills: Mutex<JoinSet<()>>,
    working: Mutex<Working>,
    floods: Throttle<(AgentId, Flood)>,
    holder: Holder,
}

/// The `hand_offs` rows held on this instance, from the moment a delivery
/// records one or a replay takes one until its job is dropped: a cache in
/// front of the rows, which [`Pipeline::replay_hand_offs`] keeps leasing
/// rather than queueing again, draining included. It also holds whether
/// the pipeline is closed: a row let go after that is set aside until
/// [`Pipeline::release_cut_hand_offs`] makes it due at once for the next
/// instance, at the end of [`Pipeline::drain`] or [`Pipeline::cut_short`],
/// and from the server once the hand-off worker is done.
#[derive(Clone, Default)]
struct Holder {
    ids: Arc<Mutex<HeldIds>>,
    closed: Arc<AtomicBool>,
}

/// The ids a [`Holder`] holds, and those let go since the pipeline closed.
#[derive(Default)]
struct HeldIds {
    live: HashSet<i64>,
    cut: Vec<i64>,
}

impl Holder {
    /// Holds the row `id`; `None` when it is held already, so no row has
    /// two jobs here.
    fn hold(&self, id: i64) -> Option<Holding> {
        lock(&self.ids).live.insert(id).then(|| Holding {
            id,
            holder: self.clone(),
        })
    }

    /// The rows held now.
    fn held(&self) -> Vec<i64> {
        lock(&self.ids).live.iter().copied().collect()
    }

    /// The rows let go since the pipeline closed and not released yet.
    /// They stay set aside until [`released`](Self::released), so a release
    /// that fails, or is cancelled, is made again by the next.
    fn cut(&self) -> Vec<i64> {
        lock(&self.ids).cut.clone()
    }

    /// Forgets the rows `ids` set aside, now released: another instance
    /// may hold them next, and a later release would take them from it.
    fn released(&self, ids: &[i64]) {
        lock(&self.ids).cut.retain(|id| !ids.contains(id));
    }
}

/// A hold on a `hand_offs` row, let go when dropped.
struct Holding {
    id: i64,
    holder: Holder,
}

impl Drop for Holding {
    fn drop(&mut self) {
        let mut ids = lock(&self.holder.ids);
        ids.live.remove(&self.id);
        if self.holder.closed.load(Ordering::SeqCst) {
            ids.cut.push(self.id);
        }
    }
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

/// A message waiting for one candidate agent, whose owner is `owner`. A
/// hand-off agentd built itself holds its `hand_offs` row in `hand_off`:
/// it skips confirmation, since agentd posted the message, and the row is
/// deleted once the job has settled the hand-off
/// ([`Pipeline::candidate`]).
/// Dropping it releases its places under [`PipelineSettings::max_pending`]
/// and [`PipelineSettings::max_pending_per_owner`]; once it and the notices
/// it started are gone, whoever waits for it in [`Pipeline::handle`] is
/// told.
struct Job {
    event: Arc<InboundEvent>,
    caps: Caps,
    owner: MemberId,
    hand_off: Option<Holding>,
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
                private: Mutex::new(HashMap::new()),
                kills: Mutex::new(JoinSet::new()),
                working: Mutex::new(Working::default()),
                floods: Throttle::new(FLOOD_WARNING_INTERVAL),
                holder: Holder::default(),
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

    /// The manager bots the pipeline prompts and notifies through.
    pub(crate) fn replies(&self) -> &Replies {
        &self.inner.replies
    }

    /// Stops taking messages: those sent from now on are dropped.
    pub fn close(&self) {
        self.inner.holder.closed.store(true, Ordering::SeqCst);
    }

    fn is_closed(&self) -> bool {
        self.inner.holder.closed.load(Ordering::SeqCst)
    }

    /// Waits until every message taken is answered, and the kills of
    /// private tasks' turns cut short meanwhile have ended
    /// ([`wait_for_kills`](Self::wait_for_kills)). Call it after
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
        self.release_cut_hand_offs().await;
        self.tell_cut().await;
        self.wait_for_kills().await;
    }

    /// Makes the `hand_offs` rows let go since the pipeline closed due at
    /// once, for the next instance to take rather than wait out a lease,
    /// each once. [`drain`](Self::drain) and [`cut_short`](Self::cut_short)
    /// call it as they end; call it again once nothing can let go of a row
    /// any more, as the server does after its hand-off worker
    /// ([`run_hand_offs`](Self::run_hand_offs)), whose last pass may.
    pub async fn release_cut_hand_offs(&self) {
        let cut = self.inner.holder.cut();
        if cut.is_empty() {
            return;
        }
        match self
            .inner
            .store
            .release_hand_offs(&cut, (self.inner.settings.now)())
            .await
        {
            Ok(()) => self.inner.holder.released(&cut),
            Err(err) => {
                tracing::warn!(error = %err, hand_offs = cut.len(), "couldn't make the hand-offs let go at shutdown due at once; they are taken after their lease");
            }
        }
    }

    /// Closes the pipeline and drops every message still waiting or being
    /// answered. Each turn that was running, or delivering its reply, has
    /// its working emoji taken off and its thread told to ask again
    /// ([`RESTARTING_TEXT`]), within a few seconds. Messages still waiting
    /// are dropped without a word: no decision was made about them yet.
    /// The hand-offs among them are made due at once, for the next
    /// instance to take.
    /// The private tasks it drops are released for another instance, but
    /// those whose turn had started are left to their kills, which
    /// [`wait_for_kills`](Self::wait_for_kills) waits for: they release
    /// the claim once the turn is billed.
    pub async fn cut_short(&self) {
        self.close();
        let mut tasks = std::mem::take(&mut *lock(&self.inner.tasks));
        tasks.shutdown().await;
        self.release_cut_tasks().await;
        lock(&self.inner.lanes).clear();
        self.release_cut_hand_offs().await;
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
        let (mut candidates, from_bot) = match self.candidates(&event, caps).await {
            Ok(found) => found,
            Err(err) => {
                tracing::warn!(message = %event.message.id, error = %err, "couldn't look up the agents a message addresses");
                return Vec::new();
            }
        };
        if from_bot {
            match self.posting_turn(&event.message).await {
                Ok(Some(turn)) => {
                    let mut fresh = Vec::with_capacity(candidates.len());
                    for (agent, owner) in candidates {
                        if self.hopped(&hop_key(agent, turn)).await {
                            tracing::debug!(%agent, message = %event.message.id, "the hop from this post's turn ran already; dropped this copy");
                        } else {
                            fresh.push((agent, owner));
                        }
                    }
                    candidates = fresh;
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(message = %event.message.id, error = %err, "couldn't read which turn posted a message");
                }
            }
        }
        let candidates = candidates
            .into_iter()
            .map(|(agent, owner)| (agent, owner, None))
            .collect();
        let quietly = from_bot || event.outside.is_some();
        self.queue(&Arc::new(event), caps, candidates, quietly)
    }

    /// The turn that posted `msg` where a mention in it hands off, if it
    /// is one ([`StoreView`] attributes only those).
    async fn posting_turn(&self, msg: &MsgRef) -> Result<Option<TurnId>, StoreError> {
        Ok(self
            .inner
            .store
            .posted_message_ref(msg)
            .await?
            .filter(|posted| posted.hands_off)
            .and_then(|posted| posted.turn))
    }

    /// Whether the hop `key` was claimed already
    /// ([`candidate`](Self::candidate) claims it): only a hint, which saves
    /// a copy that comes second its places and its read-back. A failed
    /// read says no; the claim still decides.
    async fn hopped(&self, key: &str) -> bool {
        self.inner
            .store
            .event_processed(HOP_SOURCE, key)
            .await
            .unwrap_or_else(|err| {
                tracing::warn!(error = %err, "couldn't read whether a hop ran");
                false
            })
    }

    /// Queues `event` for each of `candidates`, each with its owner and
    /// the `hand_offs` row it delivers, if any, which the job holds, and
    /// returns what completes when each is taken and done with. A candidate
    /// whose places are full is told nothing when `quietly`, as for a bot's
    /// message or one whose sender's fields say is from outside, whom
    /// nothing is posted for, and otherwise gets a busy line; a hand-off it
    /// couldn't take keeps its row, to be taken again.
    fn queue(
        &self,
        event: &Arc<InboundEvent>,
        caps: Caps,
        candidates: Vec<(AgentId, MemberId, Option<Holding>)>,
        quietly: bool,
    ) -> Vec<oneshot::Receiver<()>> {
        let thread = thread_of(event, caps);
        let mut waiting = Vec::new();
        for (agent, owner, hand_off) in candidates {
            let (done, finished) = oneshot::channel();
            let done: Done = Arc::new(done);
            let taken = self.places(owner).is_some_and(|places| {
                let job = Job {
                    event: Arc::clone(event),
                    caps,
                    owner,
                    hand_off,
                    _pending: places,
                    done: Arc::clone(&done),
                };
                self.enqueue((agent, thread.clone()), job)
            });
            if taken {
                waiting.push(finished);
            } else if quietly {
                if let Some(quiet) = self.flooded(agent, Flood::BotMessage) {
                    tracing::warn!(%agent, message = %event.message.id, dropped_since_last_warning = quiet, "too many messages waiting; dropping a bot's or an outside sender's message");
                }
            } else {
                if let Some(quiet) = self.flooded(agent, Flood::Message) {
                    tracing::warn!(%agent, message = %event.message.id, refused_since_last_warning = quiet, "too many messages waiting; not taking this one");
                }
                self.notice(event, agent, owner, caps, Notice::Busy, done);
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
            let settled = AssertUnwindSafe(self.candidate(&job, agent))
                .catch_unwind()
                .await
                .unwrap_or_else(|_| {
                    tracing::error!(%agent, message = %job.event.message.id, "handling a message panicked");
                    false
                });
            if settled
                && let Some(holding) = &job.hand_off
                && let Err(err) = self.inner.store.finish_hand_off(holding.id).await
            {
                tracing::warn!(%agent, error = %err, "couldn't record a hand-off as done; it will be delivered again");
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
            let Some(surface) = self.inner.surfaces.surface(agent, &event.conv).await? else {
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
    /// ignore it, routes the platform's copy again and acts on the copy
    /// if its decision may stand ([`copy_stands`]). A copy the platform
    /// gave without [`outside`](InboundEvent::outside) takes the event's,
    /// so an event saying its sender is from outside is never made home by
    /// its copy. A hand-off agentd built
    /// itself is acted on as it is, once the agent's bot is found, asking
    /// the platform now, to be able to post in the conversation.
    ///
    /// Returns whether the message is settled: true unless what deciding
    /// needed couldn't be read, or, on a hop, its claim couldn't be
    /// recorded, the agent's bot couldn't be checked or has no surface
    /// there, or the agent's rules couldn't be read. A hand-off's row is deleted only once it is
    /// settled, and is otherwise taken again.
    async fn candidate(&self, job: &Job, agent: AgentId) -> bool {
        let (event, caps) = (job.event.as_ref(), job.caps);
        let Some(decision) = self.decide(event, agent, caps).await else {
            return false;
        };
        if let Decision::Ignore(reason) = decision {
            tracing::debug!(%agent, message = %event.message.id, %reason, "ignored a message");
            return true;
        }
        let hop = if on_hop(event, &decision) {
            match self.posting_turn(&event.message).await {
                Ok(Some(turn)) => Some(hop_key(agent, turn)),
                Ok(None) => {
                    tracing::warn!(%agent, message = %event.message.id, "a hop's post has no turn on record; leaving it");
                    return false;
                }
                Err(err) => {
                    tracing::warn!(%agent, message = %event.message.id, error = %err, "couldn't read which turn posted a hop's message; leaving it");
                    return false;
                }
            }
        } else {
            None
        };
        if let Some(key) = &hop
            && self.hopped(key).await
        {
            tracing::debug!(%agent, message = %event.message.id, "the hop from this post's turn ran already; dropped this copy");
            return true;
        }
        let copy = if job.hand_off.is_some() {
            let surface = match self.inner.surfaces.surface(agent, &event.conv).await {
                Ok(Some(surface)) => surface,
                Ok(None) => {
                    tracing::debug!(%agent, conv = %event.conv, "the agent of a hand-off has no active binding there; leaving it");
                    return false;
                }
                Err(err) => {
                    tracing::warn!(%agent, conv = %event.conv, error = %err, "couldn't look up the surface of a hand-off; leaving it");
                    return false;
                }
            };
            match surface.can_post_now(&event.conv).await {
                Ok(true) => None,
                Ok(false) => {
                    tracing::info!(%agent, conv = %event.conv, "not taking a hand-off: the agent's bot isn't in this conversation");
                    return true;
                }
                Err(err) => {
                    tracing::warn!(%agent, conv = %event.conv, error = %err, "couldn't check the agent's bot can post; leaving the hand-off");
                    return false;
                }
            }
        } else {
            let Some(copy) = self.confirmed(job, agent).await else {
                return true;
            };
            Some(outside_kept(copy, event)).filter(|copy| copy != event)
        };
        let (event, decision) = if let Some(copy) = &copy {
            let Some(confirmed) = self.decide(copy, agent, caps).await else {
                return false;
            };
            if !copy_stands(&decision, &confirmed) {
                if let Some(quiet) = self.flooded(agent, Flood::Unconfirmed) {
                    tracing::warn!(%agent, message = %event.message.id, unconfirmed_since_last_warning = quiet, "the platform's copy of a message routes differently from its event; dropped it");
                }
                return true;
            }
            (copy, confirmed)
        } else {
            (event, decision)
        };
        if let Some(key) = &hop {
            let now = (self.inner.settings.now)();
            match self
                .inner
                .store
                .mark_event_processed(HOP_SOURCE, key, now, PROCESSED_EVENT_RETENTION)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(%agent, message = %event.message.id, "a hop's other copy was acted on already; dropped this one");
                    return true;
                }
                Err(err) => {
                    tracing::warn!(%agent, message = %event.message.id, error = %err, "couldn't claim a hop; leaving it");
                    return false;
                }
            }
        }
        let settled = !matches!(
            decision,
            Decision::Refuse {
                reason: RefuseReason::PolicyUnavailable,
                ..
            }
        );
        self.act(event, agent, caps, decision, job.hand_off.is_some())
            .await;
        settled
    }

    /// The router's decision on `event` for `agent`. `None` when the store
    /// can't be read.
    async fn decide(&self, event: &InboundEvent, agent: AgentId, caps: Caps) -> Option<Decision> {
        let store = &self.inner.store;
        let settings = &self.inner.settings;
        let thread = thread_of(event, caps);
        let context = ViewContext {
            managers: &settings.managers,
            admins: &settings.admins,
            thread: &thread,
            limits: &settings.limits,
            now: (settings.now)(),
            attribution_wait: settings.attribution_wait,
        };
        let view = match StoreView::load(store, event, agent, context).await {
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
    /// [`notice`](Self::notice), if `agent`'s bot may post there and a
    /// person sent the message, who can try again; a copy
    /// the platform doesn't have, or has elsewhere, is dropped without a
    /// word.
    async fn confirmed(&self, job: &Job, agent: AgentId) -> Option<InboundEvent> {
        let (event, caps) = (job.event.as_ref(), job.caps);
        let surface = match self.inner.surfaces.surface(agent, &event.conv).await {
            Ok(surface) => surface?,
            Err(err) => {
                tracing::warn!(%agent, error = %err, "looking up an agent's surface failed");
                return None;
            }
        };
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
            Err(err @ (SurfaceError::RateLimited { .. } | SurfaceError::Transport(_)))
                if !event.sender_is_bot && event.sender_bot_user.is_none() =>
            {
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

    /// Acts on `decision` for `agent` on `event`; `checked` when the
    /// platform was just asked whether the agent's bot can post there.
    async fn act(
        &self,
        event: &InboundEvent,
        agent: AgentId,
        caps: Caps,
        decision: Decision,
        checked: bool,
    ) {
        let result = match decision {
            Decision::Ignore(_) => Ok(()),
            Decision::LinkPrompt { requester } => {
                self.link_prompt(event, agent, &requester, link_text).await
            }
            Decision::RelinkPrompt { requester } => {
                self.link_prompt(event, agent, &requester, relink_text)
                    .await
            }
            Decision::Refuse { reason, requester } => {
                self.refuse(event, agent, caps, reason, &requester).await
            }
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
                self.run(event, agent, caps, turn, checked).await
            }
        };
        if let Err(err) = result {
            tracing::warn!(%agent, message = %event.message.id, error = %err, "handling a message failed");
        }
    }

    /// Tells `requester` privately how to link an account, with the text
    /// `text` makes from the agent's name, if `agent`'s bot may post in
    /// `event`'s conversation: an agent that couldn't answer there doesn't
    /// prompt either. On a hop, which the requester didn't ask for
    /// themselves, they are prompted at most once per
    /// [`FAILURE_DM_INTERVAL`], however many hops of theirs a chain of
    /// agents fans out to.
    async fn link_prompt(
        &self,
        event: &InboundEvent,
        agent: AgentId,
        requester: &Requester,
        text: fn(&str) -> String,
    ) -> Result<(), PipelineError> {
        let Some(surface) = self.inner.surfaces.surface(agent, &event.conv).await? else {
            return Ok(());
        };
        if !surface.can_post(&event.conv).await? {
            return Ok(());
        }
        let text = text(&self.agent_name(agent).await?);
        let now = (self.inner.settings.now)();
        let claimed = requester.key != event.sender;
        if claimed {
            match self
                .inner
                .store
                .claim_failure_notice(&requester.key, HOP_LINK_PROMPT, now, FAILURE_DM_INTERVAL)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::debug!(%agent, requester = %requester.key, "a hop's requester was prompted to link recently");
                    return Ok(());
                }
                Err(err) => {
                    tracing::warn!(%agent, requester = %requester.key, error = %err, "couldn't check when the requester was last prompted; prompting anyway");
                }
            }
        }
        if let Err(err) = self.inner.replies.dm(&requester.key, &text).await {
            tracing::warn!(%agent, requester = %requester.key, error = %err, "couldn't send a link prompt");
            if claimed
                && let Err(err) = self
                    .inner
                    .store
                    .release_failure_notice(&requester.key, HOP_LINK_PROMPT, now)
                    .await
            {
                tracing::warn!(%agent, requester = %requester.key, error = %err, "couldn't release the claim on prompting the requester");
            }
        }
        Ok(())
    }

    /// Says why `agent` won't answer `event` for `requester`: privately to
    /// them for a refusal of them ([`RefuseReason::is_personal`]), or in
    /// one line in the thread. A limit that counts turns or tokens over a
    /// day or an hour says so once per thread in that window, so a capped
    /// agent doesn't answer every message with the same line, and so do the
    /// hop cap and, on a hop, rules that can't be read, which every post
    /// and copy of a chain meets again. That last one is only posted once
    /// its claim is recorded: the store failing is what it reports, and
    /// each post of a chain would otherwise say it again.
    ///
    /// A refusal of them on a hop, where another agent's post named
    /// `agent` for them, is only logged: they never addressed `agent`, so
    /// a message about it would puzzle them, and an agent naming many
    /// agents that refuse its requester would have the manager send them
    /// one each; the thread isn't told either, since that would say who is
    /// banned or denied.
    async fn refuse(
        &self,
        event: &InboundEvent,
        agent: AgentId,
        caps: Caps,
        reason: RefuseReason,
        requester: &Requester,
    ) -> Result<(), PipelineError> {
        if reason.is_personal() && requester.key != event.sender {
            tracing::info!(%agent, message = %event.message.id, %reason, "refused a hop for its requester; told no one");
            return Ok(());
        }
        let Some(surface) = self.inner.surfaces.surface(agent, &event.conv).await? else {
            return Ok(());
        };
        if !surface.can_post(&event.conv).await? {
            return Ok(());
        }
        let text = refusal_text(&self.agent_name(agent).await?, reason);
        if reason.is_personal() {
            self.tell_refused(agent, requester, reason, &text).await;
            return Ok(());
        }
        let target = reply_target(event, caps);
        let on_hop = requester.key != event.sender;
        let Some((kind, window)) = limit_window(reason).or_else(|| {
            (on_hop && reason == RefuseReason::PolicyUnavailable)
                .then_some((POLICY_UNAVAILABLE_NOTICE, LimitWindow::Hour))
        }) else {
            say(surface.as_ref(), &target, &text).await?;
            tracing::info!(%agent, message = %event.message.id, %reason, "refused a message");
            return Ok(());
        };
        let store = &self.inner.store;
        let thread = thread_of(event, caps);
        let now = (self.inner.settings.now)();
        match store
            .claim_limit_notice(agent, &thread, kind, window, now)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(%agent, message = %event.message.id, %reason, "refused a message; the thread was told already");
                return Ok(());
            }
            Err(err) if kind == POLICY_UNAVAILABLE_NOTICE => {
                tracing::warn!(%agent, error = %err, "couldn't claim the notice that a hop's rules can't be read; not posting it");
                return Ok(());
            }
            Err(err) => {
                tracing::warn!(%agent, error = %err, "couldn't claim a limit notice; posting it anyway");
            }
        }
        if let Err(err) = say(surface.as_ref(), &target, &text).await {
            if let Err(err) = store
                .release_limit_notice(agent, &thread, kind, window, now)
                .await
            {
                tracing::warn!(%agent, error = %err, "couldn't release a limit notice's claim");
            }
            return Err(err.into());
        }
        tracing::info!(%agent, message = %event.message.id, %reason, "refused a message");
        Ok(())
    }

    /// Tells `requester` privately, in `text`, that `agent` refused them
    /// for `reason`, unless they were told within [`REFUSAL_DM_INTERVAL`]:
    /// about this agent's rules, or about their ban by any agent. A message
    /// that fails to send releases its claim.
    async fn tell_refused(
        &self,
        agent: AgentId,
        requester: &Requester,
        reason: RefuseReason,
        text: &str,
    ) {
        let store = &self.inner.store;
        let kind = match reason {
            RefuseReason::Banned => "refusal/banned".to_owned(),
            _ => format!("refusal/denied/{agent}"),
        };
        let now = (self.inner.settings.now)();
        let claimed = match store
            .claim_failure_notice(&requester.key, &kind, now, REFUSAL_DM_INTERVAL)
            .await
        {
            Ok(true) => true,
            Ok(false) => {
                tracing::debug!(%agent, requester = %requester.key, %reason, "refused a message; the requester was told recently");
                return;
            }
            Err(err) => {
                tracing::warn!(%agent, requester = %requester.key, error = %err, "couldn't check when the requester was last told; telling them anyway");
                false
            }
        };
        if let Err(err) = self.inner.replies.dm(&requester.key, text).await {
            tracing::warn!(%agent, requester = %requester.key, error = %err, "couldn't tell the requester why they were refused");
            if claimed
                && let Err(err) = store
                    .release_failure_notice(&requester.key, &kind, now)
                    .await
            {
                tracing::warn!(%agent, requester = %requester.key, error = %err, "couldn't release the claim on telling the requester");
            }
            return;
        }
        tracing::info!(%agent, %reason, "refused a message, and told the requester privately");
    }

    /// Bills `turn`, whose outcome is `outcome`, to its requester, and
    /// counts it for `agent` in `thread`, toward the agent's daily cap
    /// unless it is `for_owner`. A requester the store has no member for
    /// yet gets one. A failure is logged: the turn has run.
    async fn meter(
        &self,
        agent: AgentId,
        turn: &Run,
        thread: &ThreadKey,
        for_owner: bool,
        outcome: &TurnOutcome,
    ) {
        let key = &turn.requester.key;
        let member = match turn.requester.member {
            Some(member) => member,
            None => match self
                .inner
                .store
                .ensure_member(key, key.user.as_str(), (self.inner.settings.now)())
                .await
            {
                Ok(member) => member,
                Err(err) => {
                    tracing::warn!(%agent, requester = %key, error = %err, "couldn't find the member to bill a turn to");
                    return;
                }
            },
        };
        self.bill(member, agent, thread, for_owner, outcome).await;
    }

    /// Bills a turn of `agent` in `thread`, which ended as `outcome`, to
    /// `member`, as [`meter`](Self::meter) does.
    async fn bill(
        &self,
        member: MemberId,
        agent: AgentId,
        thread: &ThreadKey,
        for_owner: bool,
        outcome: &TurnOutcome,
    ) {
        let now = (self.inner.settings.now)();
        let usage = turn_usage(outcome);
        if let Err(err) = self
            .inner
            .store
            .record_turn_usage(member, agent, thread, usage, for_owner, now)
            .await
        {
            tracing::warn!(%agent, %member, error = %err, "couldn't meter a turn");
        }
    }

    async fn agent_name(&self, agent: AgentId) -> Result<String, StoreError> {
        Ok(self
            .inner
            .store
            .agent(agent)
            .await?
            .map_or_else(|| "This agent".to_owned(), |agent| agent.name))
    }

    /// Runs `agent`'s turn on `event` and delivers what it made, unless the
    /// agent's bot can't post in the conversation, which is asked unless
    /// `checked` says it was just now, as for a hand-off. A check the
    /// platform couldn't answer, unreachable, asking to slow down or
    /// failing on its side, answers anyway, as the message was read back
    /// with the bot's own access ([`confirmed`](Self::confirmed)); a
    /// refusal (not found, forbidden, unauthorized) doesn't.
    /// Once the turn may run, a failure before it reached the model posts
    /// [`FAILED_TEXT`].
    async fn run(
        &self,
        event: &InboundEvent,
        agent: AgentId,
        caps: Caps,
        turn: Run,
        checked: bool,
    ) -> Result<(), PipelineError> {
        let Some(surface) = self.inner.surfaces.surface(agent, &event.conv).await? else {
            tracing::warn!(%agent, conv = %event.conv, "the agent has no surface in this conversation");
            return Ok(());
        };
        if !checked {
            match surface.can_post(&event.conv).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::info!(%agent, conv = %event.conv, "not answering: the agent's bot isn't in this conversation");
                    return Ok(());
                }
                Err(
                    err @ (SurfaceError::RateLimited { .. }
                    | SurfaceError::Transport(_)
                    | SurfaceError::Api(_)),
                ) => {
                    tracing::warn!(%agent, conv = %event.conv, error = %err, "couldn't check the agent's bot can post; answering anyway");
                }
                Err(err) => return Err(err.into()),
            }
        }
        let target = reply_target(event, caps);
        let (working, ran) = match self.prepare(agent, &event.conv, turn.credential).await {
            Ok(None) => return Ok(()),
            Ok(Some(prepared)) => {
                let owner = prepared.owner;
                let bot = prepared.bot.clone();
                let working = self.show_working(&surface, event, &target).await;
                let ran = self
                    .turn(event, agent, caps, &turn, surface.as_ref(), prepared)
                    .await;
                (Some(working), ran.map(|ran| (owner, bot, ran)))
            }
            Err(err) => (None, Err(err)),
        };
        let delivered = match ran {
            Ok((owner, bot, (session, turn_id, report))) => {
                let for_owner = turn.requester.member == Some(owner);
                self.meter(
                    agent,
                    &turn,
                    &thread_of(event, caps),
                    for_owner,
                    &report.outcome,
                )
                .await;
                let failure = CredentialFailure::of(&report.outcome);
                let delivery = Delivery {
                    store: &self.inner.store,
                    surface: surface.as_ref(),
                    session: session.id,
                    agent,
                    requester: &turn.requester,
                    hop: turn.hop,
                    credential: turn.credential,
                    target,
                    answering: Answering::Message(&event.message),
                    hand_offs: self.hand_offs(event, agent, bot).await,
                };
                let handed = delivery.report(turn_id, report).await;
                let their_own_dm = event.is_dm() && event.sender == turn.requester.key;
                if let Some(failure) = failure
                    && !their_own_dm
                {
                    self.tell_requester(agent, &turn, failure).await;
                }
                self.hand_off(caps, handed).await;
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

    /// How the posts of `agent`'s turn on `event`, made by its bot `bot`,
    /// hand off ([`HandOffs`]): `None` in a one-to-one DM, where no other
    /// agent answers, or when the bot's binding can't be found as
    /// `agent`'s, so nothing it posts is attributed.
    async fn hand_offs(
        &self,
        event: &InboundEvent,
        agent: AgentId,
        bot: MemberKey,
    ) -> Option<HandOffs> {
        if event.is_dm() {
            return None;
        }
        match self.inner.store.agent_for_bot(&bot).await {
            Ok(Some((poster, binding))) if poster.id == agent => Some(HandOffs {
                conv_kind: event.conv_kind,
                bot,
                binding,
                now: (self.inner.settings.now)(),
                holder: self.inner.holder.clone(),
            }),
            Ok(_) => None,
            Err(err) => {
                tracing::warn!(%agent, error = %err, "couldn't look up the bot of an agent's posts; not handing them off");
                None
            }
        }
    }

    /// Queues each hand-off of a turn's posts ([`HandedOff`], recorded as
    /// it was posted) for the agent it names, as its bot's message: it is
    /// routed and run as any message is, so the router inherits the
    /// requester and the next hop from the post's `message_refs` row, and
    /// the caps and the agents' rules hold. One whose hop ran already, from
    /// the platform's copy, is done with. A hand-off whose job never
    /// settles it, as at a shutdown, or that found no place, keeps its row
    /// for [`replay_hand_offs`](Self::replay_hand_offs), here or on another
    /// instance; one handed while the pipeline closes is let go, which
    /// makes it due once the pipeline drains or is cut short.
    async fn hand_off(&self, caps: Caps, handed: Vec<HandedOff>) {
        if self.is_closed() {
            return;
        }
        for handed in handed {
            if self.hopped(&handed.hop).await {
                if let Err(err) = self.inner.store.finish_hand_off(handed.holding.id).await {
                    tracing::warn!(agent = %handed.agent, error = %err, "couldn't record a hand-off as done");
                }
                continue;
            }
            tracing::debug!(agent = %handed.agent, msg = %handed.event.message.id, "handing a post off");
            self.queue(
                &handed.event,
                caps,
                vec![(handed.agent, handed.owner, Some(handed.holding))],
                true,
            );
        }
    }

    /// Queues the hand-offs due in `hand_offs`, recorded by this instance
    /// or another, that no job holds: a shutdown or a crash cut their
    /// jobs, or they found no place. The rows this instance holds are
    /// leased again instead, and once the pipeline is closed that is all it
    /// does, until the drain ends, so a drain longer than the lease keeps
    /// its rows from other instances. Each is taken for [`HAND_OFF_LEASE`] and
    /// queued again as it was, unless its hop ran already, when it is
    /// done with. A row that can't be read now, whose agent is gone, or
    /// whose agent has no active binding there is left, and a row
    /// recorded more than an hour ago is dropped. Returns how many jobs
    /// were queued. A hand-off delivered twice runs once, as the hop's
    /// claim allows one run.
    ///
    /// # Errors
    ///
    /// If the store can't be read.
    pub async fn replay_hand_offs(&self) -> Result<usize, StoreError> {
        let closed = self.is_closed();
        let now = (self.inner.settings.now)();
        let held = self.inner.holder.held();
        let due = self
            .inner
            .store
            .take_due_hand_offs(
                now,
                HAND_OFF_LEASE,
                now - HAND_OFF_MAX_AGE,
                if closed { 0 } else { MAX_REPLAYED },
                &held,
            )
            .await?;
        if due.stale > 0 {
            tracing::warn!(
                dropped = due.stale,
                "dropped hand-offs recorded over an hour ago that never ran"
            );
        }
        let mut queued = 0;
        for hand_off in due.taken {
            queued += self.replay_hand_off(hand_off).await;
        }
        Ok(queued)
    }

    /// Queues `hand_off` again, as [`replay_hand_offs`](Self::replay_hand_offs)
    /// says; returns how many jobs it queued.
    async fn replay_hand_off(&self, hand_off: HandOff) -> usize {
        let agent = hand_off.agent;
        let Some(holding) = self.inner.holder.hold(hand_off.id) else {
            return 0;
        };
        let event: InboundEvent = match serde_json::from_str(&hand_off.event_json) {
            Ok(event) => event,
            Err(err) => {
                tracing::warn!(%agent, error = %err, "a recorded hand-off doesn't parse; leaving it");
                return 0;
            }
        };
        let store = &self.inner.store;
        let owner = match store.agent(agent).await {
            Ok(Some(row)) => row.owner,
            Ok(None) => return 0,
            Err(err) => {
                tracing::warn!(%agent, error = %err, "couldn't look up the agent of a hand-off; trying it later");
                return 0;
            }
        };
        let surface = match self.inner.surfaces.surface(agent, &event.conv).await {
            Ok(Some(surface)) => surface,
            Ok(None) => {
                tracing::debug!(%agent, "the agent of a hand-off has no active binding there; leaving it");
                return 0;
            }
            Err(err) => {
                tracing::warn!(%agent, error = %err, "couldn't look up the surface of a hand-off; trying it later");
                return 0;
            }
        };
        match self.posting_turn(&event.message).await {
            Ok(Some(turn)) if self.hopped(&hop_key(agent, turn)).await => {
                if let Err(err) = store.finish_hand_off(holding.id).await {
                    tracing::warn!(%agent, error = %err, "couldn't record a hand-off as done");
                }
                return 0;
            }
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(%agent, error = %err, "couldn't read which turn posted a hand-off; trying it later");
                return 0;
            }
        }
        tracing::info!(%agent, msg = %event.message.id, "delivering a hand-off again");
        self.queue(
            &Arc::new(event),
            surface.caps(),
            vec![(agent, owner, Some(holding))],
            true,
        )
        .len()
    }

    /// Calls [`replay_hand_offs`](Self::replay_hand_offs) now and then
    /// every [`PipelineSettings::hand_off_sweep`], until `stopping` becomes
    /// true or its sender is dropped.
    pub async fn run_hand_offs(self, mut stopping: watch::Receiver<bool>) {
        loop {
            if let Err(err) = self.replay_hand_offs().await {
                tracing::warn!(error = %err, "looking at the hand-offs due failed");
            }
            tokio::select! {
                biased;
                _ = stopping.wait_for(|stop| *stop) => break,
                () = tokio::time::sleep(self.inner.settings.hand_off_sweep) => {}
            }
        }
    }

    /// What a turn of `agent` in `conv` needs before its session: its
    /// bot's identity and model, with the persona file and the bundled
    /// skill written. `None` when the agent or its bot is gone.
    async fn prepare(
        &self,
        agent: AgentId,
        conv: &ConvRef,
        credential: CredentialRef,
    ) -> Result<Option<Prepared>, PipelineError> {
        let Some(row) = self.inner.store.agent(agent).await? else {
            return Ok(None);
        };
        let Some(bot) = self.bot_of(&row, conv).await? else {
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
        let now = (self.inner.settings.now)();
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

    /// The identity of `agent`'s bot on `conv`'s surface and team.
    async fn bot_of(&self, agent: &Agent, conv: &ConvRef) -> Result<Option<MemberKey>, StoreError> {
        Ok(self
            .inner
            .store
            .bindings_of(agent.id)
            .await?
            .into_iter()
            .find(|binding| {
                binding.surface == conv.surface
                    && binding.team == conv.team
                    && binding.state == store::BindingState::Active
            })
            .and_then(|binding| binding.bot_user)
            .map(|user| MemberKey {
                surface: conv.surface,
                team: conv.team.clone(),
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

/// The line a refusal of `reason` posts, for an agent named `name`.
fn refusal_text(name: &str, reason: RefuseReason) -> String {
    match reason {
        RefuseReason::Paused => format!("{name} is paused by its owner."),
        RefuseReason::Banned => {
            "A community admin banned you, so agents won't take your requests. Send `me` to \
             me to see why."
                .to_owned()
        }
        RefuseReason::Denied => {
            format!("{name}'s owner hasn't allowed you to use it where you asked it.")
        }
        RefuseReason::HopCap { max: Hop(1) } => {
            format!("{name} won't answer: it takes part in chains of at most 1 hand-off.")
        }
        RefuseReason::HopCap { max } => {
            format!("{name} won't answer: it takes part in chains of at most {max} hand-offs.")
        }
        RefuseReason::DailyCap { max: 0 } => {
            format!("{name} takes requests from its owner only.")
        }
        RefuseReason::DailyCap { max } => format!(
            "{name} has reached the daily limit its owner set on requests from others \
             ({max}). Try again after midnight UTC."
        ),
        RefuseReason::ThreadTurns { max } => format!(
            "{name} won't answer here for now: agents have reached this thread's hourly turn \
             limit ({max}). Try again next hour."
        ),
        RefuseReason::ThreadTokens { max } => format!(
            "{name} won't answer here for now: agents have used this thread's daily token \
             budget ({max}). Try again after midnight UTC, or in a new thread."
        ),
        RefuseReason::PolicyUnavailable => {
            format!("{name} can't check who may use it right now. Try again later.")
        }
    }
}

/// Whether `decision` is a refusal by a limit that counts turns or tokens
/// over a day or an hour, which the same message can meet or not from one
/// moment to the next.
fn limited(decision: &Decision) -> bool {
    matches!(
        decision,
        Decision::Refuse {
            reason: RefuseReason::DailyCap { .. }
                | RefuseReason::ThreadTurns { .. }
                | RefuseReason::ThreadTokens { .. },
            ..
        }
    )
}

/// The platform's `copy` of `event`, taking the event's
/// [`outside`](InboundEvent::outside) when the copy has none: an event that
/// says its sender is from outside is never made home by its copy.
fn outside_kept(mut copy: InboundEvent, event: &InboundEvent) -> InboundEvent {
    if copy.outside.is_none() {
        copy.outside.clone_from(&event.outside);
    }
    copy
}

/// Whether the decision on the platform's copy of a message, `confirmed`,
/// may be acted on when the event's was `decision`: when they are the
/// same, or, for the same requester's identity and the same
/// [`outside`](core_types::Requester::outside), when either is a limit's
/// refusal ([`limited`]). The counts a limit reads can change between the
/// two, as a turn ends or an hour or a day turns, and so can the member an
/// identity belongs to, as one is made for it (T27), so the requester's
/// `member` isn't compared; who asked, and whether they are from outside,
/// can't change.
fn copy_stands(decision: &Decision, confirmed: &Decision) -> bool {
    let asker = |decision: &Decision| {
        decision
            .requester()
            .map(|requester| (requester.key.clone(), requester.outside.clone()))
    };
    confirmed == decision
        || ((limited(decision) || limited(confirmed)) && asker(decision) == asker(confirmed))
}

/// For a refusal a limit over a day or an hour gives, the kind of notice
/// and the window it counts in. The hop cap counts nothing over time, but
/// a chain of agents that mention each other meets it many times at once,
/// so its line too is said once an hour in a thread.
fn limit_window(reason: RefuseReason) -> Option<(&'static str, LimitWindow)> {
    match reason {
        RefuseReason::DailyCap { .. } => Some(("daily_cap", LimitWindow::Day)),
        RefuseReason::ThreadTurns { .. } => Some(("thread_turns", LimitWindow::Hour)),
        RefuseReason::ThreadTokens { .. } => Some(("thread_tokens", LimitWindow::Day)),
        RefuseReason::HopCap { .. } => Some(("hop_cap", LimitWindow::Hour)),
        RefuseReason::Paused
        | RefuseReason::Banned
        | RefuseReason::Denied
        | RefuseReason::PolicyUnavailable => None,
    }
}

/// What the meter bills for a turn that ended as `outcome`: the input the
/// model read fresh (uncached input and cache writes), the output and the
/// cost, from [`TurnOutcome::usage`], so a turn that crashed or timed out
/// is billed what its messages used. Cache reads, which every call of a
/// turn repeats, aren't billed. A turn whose cost isn't known, one that
/// crashed or timed out among them, is billed none and recorded with why.
fn turn_usage(outcome: &TurnOutcome) -> TurnUsage {
    let usage = outcome.usage();
    let cost = match outcome {
        TurnOutcome::Finished(result) => result.cost_usd,
        TurnOutcome::Crashed { .. } | TurnOutcome::TimedOut { .. } => Err(CostUnknown::NoResult),
    };
    TurnUsage {
        input_tokens: usage
            .input_tokens
            .saturating_add(usage.cache_creation_input_tokens),
        output_tokens: usage.output_tokens,
        cost,
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
) -> Result<Posted, SurfaceError> {
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

/// Where [`Pipeline::candidate`] records hops' claims among the processed
/// events.
const HOP_SOURCE: &str = "hop";

/// The kind of failure notice that limits a hop's link prompts.
const HOP_LINK_PROMPT: &str = "link_prompt/hop";

/// The kind of limit notice that says, on a hop, that an agent's rules
/// can't be read.
const POLICY_UNAVAILABLE_NOTICE: &str = "policy_unavailable";

/// How long a hand-off recorded or taken from `hand_offs` is left to its
/// job before it is taken again; the instance whose job holds it leases it
/// again on each look.
pub const HAND_OFF_LEASE: Duration = Duration::from_secs(5 * 60);

/// The default [`PipelineSettings::hand_off_sweep`].
pub const HAND_OFF_SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// How old a hand-off may grow before it is dropped undelivered.
const HAND_OFF_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// The most hand-offs one look queues.
const MAX_REPLAYED: u32 = 64;

/// The claim key of a hop to `agent` from a post of the turn `turn`.
fn hop_key(agent: AgentId, turn: TurnId) -> String {
    format!("{agent}/{turn}")
}

/// Whether `decision` on `event` is on a hop, whose claim
/// [`Pipeline::candidate`] takes: a requester other than the sender, which
/// only an attributed agent's post gives. One hop runs for each agent and
/// posting turn, however many of the turn's posts mention the agent and
/// however many copies of them arrive. Not a refusal because the agent's
/// rules couldn't be read, which another copy may then get past.
fn on_hop(event: &InboundEvent, decision: &Decision) -> bool {
    decision
        .requester()
        .is_some_and(|requester| requester.key != event.sender)
        && !matches!(
            decision,
            Decision::Refuse {
                reason: RefuseReason::PolicyUnavailable,
                ..
            }
        )
}

/// How a turn's posts in its own thread hand off: as messages of the
/// agent's bot `bot`, of the binding `binding`, in a conversation of the
/// kind `conv_kind`, recorded in `hand_offs` at `now` and held by
/// `holder`.
struct HandOffs {
    conv_kind: ConvKind,
    bot: MemberKey,
    binding: BindingId,
    now: OffsetDateTime,
    holder: Holder,
}

impl HandOffs {
    /// The event of `posted`, the chunk `text` posted to `target`: as the
    /// platform would deliver it, sent by the bot, with the mentions the
    /// platform reads in it.
    fn event(&self, target: &ReplyTarget, posted: &Posted, text: &str) -> InboundEvent {
        let thread_root = target.thread_root.clone();
        InboundEvent {
            event_id: format!("{HOP_SOURCE}/{}", posted.msg.id),
            binding: self.binding,
            sender: self.bot.clone(),
            sender_is_bot: true,
            sender_bot_user: Some(self.bot.user.clone()),
            conv: target.conv.clone(),
            conv_kind: self.conv_kind,
            reply_to: thread_root.clone().map(|id| MsgRef {
                conv: target.conv.clone(),
                id,
            }),
            thread_root,
            message: posted.msg.clone(),
            text: text.to_owned(),
            mentions: posted.mentions.clone(),
            files: Vec::new(),
            received_at: self.now,
            outside: None,
        }
    }
}

/// A hand-off of a turn's post to the agent `agent`, owned by `owner`,
/// recorded as the `hand_offs` row `holding` holds, with its hop's claim
/// key `hop`: what [`Pipeline::hand_off`] queues.
struct HandedOff {
    holding: Holding,
    agent: AgentId,
    owner: MemberId,
    hop: String,
    event: Arc<InboundEvent>,
}

/// Delivers what one turn made, as the agent's bot.
struct Delivery<'a> {
    store: &'a Store,
    surface: &'a dyn Surface,
    session: SessionId,
    agent: AgentId,
    requester: &'a Requester,
    hop: Hop,
    credential: CredentialRef,
    target: ReplyTarget,
    answering: Answering<'a>,
    hand_offs: Option<HandOffs>,
}

/// What a delivered turn answers.
#[derive(Clone, Copy)]
enum Answering<'a> {
    /// A message, which the reply's reactions go on.
    Message(&'a MsgRef),
    /// A private task's consent: its reply is headed with the consent's
    /// id, and a failure on its credential names the owner's account.
    PrivateTask(ConsentId),
}

impl Delivery<'_> {
    /// Delivers `report`: the attachments, then the reply or the failure's
    /// message, then the reactions, then the queued posts. Each part goes
    /// out whatever happened to the others; if any couldn't, the thread is
    /// told with [`DELIVERY_FAILED_TEXT`]. Returns the hand-offs its posts
    /// recorded ([`HandedOff`]).
    async fn report(&self, turn: TurnId, report: TurnReport<Option<Outbox>>) -> Vec<HandedOff> {
        let outbox = match report.finished {
            Ok(outbox) => outbox,
            Err(err) => {
                tracing::warn!(session = %self.session, error = %err, "the turn's outbox was lost");
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
                let text =
                    CredentialFailure::of(&report.outcome).map_or(
                        FAILED_TEXT,
                        |failure| match self.answering {
                            Answering::Message(_) => failure.thread_text(self.credential),
                            Answering::PrivateTask(_) => failure.private_task_text(),
                        },
                    );
                (text.to_owned(), Vec::new())
            }
            TurnOutcome::Crashed { .. } => (FAILED_TEXT.to_owned(), Vec::new()),
            TurnOutcome::TimedOut { .. } => (TIMED_OUT_TEXT.to_owned(), Vec::new()),
        };
        let reply = match self.answering {
            Answering::PrivateTask(consent) => {
                format!(
                    "{}\n\n{reply}",
                    crate::consents::card::result_heading(consent)
                )
            }
            Answering::Message(_) => reply,
        };
        tracing::info!(
            agent = %self.agent,
            session = %self.session,
            %turn,
            success = report.outcome.is_success(),
            reply_len = reply.len(),
            "a turn ended"
        );
        let mut uploaded = true;
        if let Some(outbox) = &outbox
            && !outbox.attachments().is_empty()
            && let Err(err) = self
                .surface
                .upload(&self.target, outbox.attachments())
                .await
        {
            tracing::warn!(session = %self.session, error = %err, "uploading the turn's attachments failed");
            uploaded = false;
        }
        let mut handed = Vec::new();
        let mut complete = uploaded && self.post(Some(turn), &reply, &mut handed).await;
        if let Answering::Message(answered) = self.answering {
            for emoji in reactions {
                self.react(answered, &emoji).await;
            }
        }
        if let Some(outbox) = &outbox {
            for reaction in outbox.reactions() {
                self.react(&reaction.msg, &reaction.emoji).await;
            }
            for queued in outbox.posts() {
                complete &= self
                    .post_to(Some(turn), &queued.to, &queued.text, &mut handed)
                    .await;
            }
        }
        if !complete && let Err(err) = say(self.surface, &self.target, DELIVERY_FAILED_TEXT).await {
            tracing::warn!(session = %self.session, error = %err, "couldn't say part of a reply was lost");
        }
        handed
    }

    /// [`post_to`](Self::post_to) the target: the turn's own thread.
    async fn post(&self, turn: Option<TurnId>, text: &str, handed: &mut Vec<HandedOff>) -> bool {
        self.post_to(turn, &self.target, text, handed).await
    }

    /// Renders and posts Markdown `text` to `target`, recording a
    /// `message_refs` row for each chunk, of `turn` if a turn made it. A
    /// chunk that can't be posted is skipped and the rest still go. Empty
    /// text posts nothing. Returns false if a chunk was lost.
    ///
    /// A chunk hands off when a turn posted it in the turn's own thread,
    /// with [`HandOffs`]: then a `hand_offs` row is recorded with its row,
    /// in one transaction, for each managed agent it mentions
    /// ([`mentioned`](Self::mentioned)), and held at once, so no replay
    /// takes it while this turn hands it off; the agents go in `handed`,
    /// so a turn hands off to an agent once.
    async fn post_to(
        &self,
        turn: Option<TurnId>,
        target: &ReplyTarget,
        text: &str,
        handed: &mut Vec<HandedOff>,
    ) -> bool {
        let hands_off = self
            .hand_offs
            .as_ref()
            .zip(turn)
            .filter(|_| *target == self.target);
        if text.trim().is_empty() {
            return true;
        }
        let mut complete = true;
        for chunk in self.surface.render(text) {
            let posted = match post_chunk(self.surface, target, &chunk).await {
                Ok(posted) => posted,
                Err(err) => {
                    tracing::warn!(session = %self.session, conv = %target.conv, error = %err, "posting part of a reply failed");
                    complete = false;
                    continue;
                }
            };
            let mentioned = match hands_off {
                Some(_) => self.mentioned(target, &posted, handed).await,
                None => Vec::new(),
            };
            let event = match hands_off.filter(|_| !mentioned.is_empty()) {
                Some((hand_offs, _)) => {
                    let event = hand_offs.event(target, &posted, &chunk);
                    match serde_json::to_string(&event) {
                        Ok(json) => Some((Arc::new(event), json)),
                        Err(err) => {
                            tracing::error!(agent = %self.agent, error = %err, "couldn't encode a hand-off");
                            None
                        }
                    }
                }
                None => None,
            };
            let new_hand_offs: Vec<NewHandOff<'_>> = match (hands_off, &event) {
                (Some((hand_offs, _)), Some((_, json))) => mentioned
                    .iter()
                    .map(|agent| NewHandOff {
                        agent: agent.id,
                        event_json: json,
                        created_at: hand_offs.now,
                        due_at: hand_offs.now + HAND_OFF_LEASE,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let recorded = self
                .store
                .record_post(
                    &NewMessageRef {
                        session: self.session,
                        msg: &posted.msg,
                        thread_root: target.thread_root.as_ref(),
                        agent: Some(self.agent),
                        turn,
                        requester: self.requester,
                        hop: self.hop,
                        consent: match self.answering {
                            Answering::PrivateTask(consent) => Some(consent),
                            Answering::Message(_) => None,
                        },
                        hands_off: hands_off.is_some(),
                    },
                    OffsetDateTime::now_utc(),
                    &new_hand_offs,
                )
                .await;
            let ids = match recorded {
                Ok((_, ids)) => ids,
                Err(err) => {
                    tracing::warn!(session = %self.session, msg = %posted.msg.id, error = %err, "recording a posted message and its hand-offs failed");
                    continue;
                }
            };
            if let (Some((hand_offs, turn)), Some((event, _))) = (hands_off, &event) {
                for (agent, id) in mentioned.iter().zip(ids) {
                    if let Some(holding) = hand_offs.holder.hold(id) {
                        handed.push(HandedOff {
                            holding,
                            agent: agent.id,
                            owner: agent.owner,
                            hop: hop_key(agent.id, turn),
                            event: Arc::clone(event),
                        });
                    }
                }
            }
        }
        complete
    }

    /// The managed agents `posted` mentions whose bot is active on the
    /// conversation's surface and team, other than the poster and the
    /// agents in `handed`, each once.
    async fn mentioned(
        &self,
        target: &ReplyTarget,
        posted: &Posted,
        handed: &[HandedOff],
    ) -> Vec<Agent> {
        let mut mentioned: Vec<Agent> = Vec::new();
        for user in &posted.mentions {
            let key = MemberKey {
                surface: target.conv.surface,
                team: target.conv.team.clone(),
                user: user.clone(),
            };
            match self.store.agent_for_bot(&key).await {
                Ok(Some((agent, _)))
                    if agent.id != self.agent
                        && handed.iter().all(|handed| handed.agent != agent.id)
                        && mentioned.iter().all(|seen| seen.id != agent.id) =>
                {
                    mentioned.push(agent);
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(agent = %self.agent, msg = %posted.msg.id, error = %err, "couldn't look up an agent a post mentions");
                }
            }
        }
        mentioned
    }

    async fn react(&self, msg: &MsgRef, emoji: &str) {
        if let Err(err) = self.surface.react(msg, emoji).await {
            tracing::warn!(session = %self.session, error = %err, "adding a reaction failed");
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
    #[error("handing a private task its files: {0}")]
    HandOver(std::io::Error),
    #[error("a private task's outcome couldn't be posted")]
    NotPosted,
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

    #[test]
    fn a_row_is_held_once_at_a_time_and_set_aside_when_let_go_after_closing() {
        let holder = Holder::default();
        let held = holder.hold(7).unwrap();
        assert!(
            holder.hold(7).is_none(),
            "a row held already isn't held again"
        );
        drop(held);
        let held = holder.hold(7).unwrap();
        assert!(holder.cut().is_empty());
        holder.closed.store(true, Ordering::SeqCst);
        drop(held);
        assert!(holder.held().is_empty());
        assert_eq!(holder.cut(), [7]);
        assert_eq!(
            holder.cut(),
            [7],
            "it stays set aside until released, in case a release fails"
        );
        holder.released(&[7]);
        assert!(holder.cut().is_empty(), "a row released is forgotten");
    }

    /// An agent of a new owner whose bot, `bot`, is active on Slack's team
    /// `T1`.
    async fn bot_agent(store: &Store, name: &str, bot: &str) -> (AgentId, BindingId) {
        let now = OffsetDateTime::now_utc();
        let owner = store
            .ensure_member(
                &MemberKey {
                    surface: core_types::SurfaceKind::Slack,
                    team: "T1".into(),
                    user: format!("owner-of-{name}").as_str().into(),
                },
                name,
                now,
            )
            .await
            .unwrap();
        let store::AgentCreation::Created(agent, binding) = store
            .create_agent(
                &store::NewAgent {
                    owner,
                    name,
                    persona: "p",
                    visibility: store::Visibility::Public,
                    surface: core_types::SurfaceKind::Slack,
                    team: &"T1".into(),
                },
                10,
                now,
            )
            .await
            .unwrap()
        else {
            panic!("created");
        };
        store
            .set_binding_bot_user(binding, &bot.into(), name)
            .await
            .unwrap();
        store
            .activate_binding(binding, &secrecy::SecretString::from("t"), now)
            .await
            .unwrap();
        (agent.id, binding)
    }

    #[tokio::test]
    async fn a_turn_hands_off_its_own_threads_posts_once_to_each_agent_they_mention() {
        let key = store::Sealer::generate_key().unwrap();
        let store = Store::open_in_memory(store::Sealer::from_base64(&key).unwrap())
            .await
            .unwrap();
        let (poster, binding) = bot_agent(&store, "helper", "U1BOT").await;
        let (writer, _) = bot_agent(&store, "writer", "U2").await;
        let (scout, _) = bot_agent(&store, "scout", "U3").await;
        let surface = testkit::MockSurface::new();
        for user in ["U1BOT", "U2", "U3", "U4"] {
            surface.name_user(user, core_types::UserId::from(user));
        }
        let conv = ConvRef {
            surface: core_types::SurfaceKind::Slack,
            team: "T1".into(),
            conversation: "C1".into(),
        };
        let bot = MemberKey {
            surface: core_types::SurfaceKind::Slack,
            team: "T1".into(),
            user: "U1BOT".into(),
        };
        let requester = Requester {
            member: None,
            key: MemberKey {
                user: "U1".into(),
                ..bot.clone()
            },
            outside: None,
        };
        let answered = MsgRef {
            conv: conv.clone(),
            id: "1.1".into(),
        };
        let target = ReplyTarget {
            conv: conv.clone(),
            thread_root: Some("1.1".into()),
        };
        let now = time::macros::datetime!(2030-01-01 0:00 UTC);
        let holder = Holder::default();
        let delivery = |answering, hands_off: bool| Delivery {
            store: &store,
            surface: &surface,
            session: SessionId::new_v4(),
            agent: poster,
            requester: &requester,
            hop: Hop::ZERO,
            credential: CredentialRef::Community,
            target: target.clone(),
            answering,
            hand_offs: hands_off.then(|| HandOffs {
                conv_kind: ConvKind::Channel,
                bot: bot.clone(),
                binding,
                now,
                holder: holder.clone(),
            }),
        };
        let turn = TurnId::new_v4();
        let mut handed = Vec::new();
        let posting = delivery(Answering::Message(&answered), true);
        for text in [
            "@U2 have a look, @U1BOT",
            "@U2 and @U3 and @U4 too",
            "@U3 again",
        ] {
            assert!(posting.post(Some(turn), text, &mut handed).await);
        }
        let to: Vec<(AgentId, &str)> = handed
            .iter()
            .map(|handed| (handed.agent, handed.event.text.as_str()))
            .collect();
        assert_eq!(
            to,
            [
                (writer, "@U2 have a look, @U1BOT"),
                (scout, "@U2 and @U3 and @U4 too")
            ],
            "each agent once, from the first post that mentions it, never the poster"
        );
        let event = &handed[0].event;
        assert_eq!(event.sender, bot);
        assert_eq!(event.binding, binding);
        assert_eq!(event.thread_root, target.thread_root);
        assert_eq!(handed[0].hop, hop_key(writer, turn));
        let due = store
            .take_due_hand_offs(now + HAND_OFF_LEASE, HAND_OFF_LEASE, now, 10, &[])
            .await
            .unwrap();
        assert_eq!(
            due.taken
                .iter()
                .map(|row| (row.id, row.agent))
                .collect::<Vec<_>>(),
            handed
                .iter()
                .map(|handed| (handed.holding.id, handed.agent))
                .collect::<Vec<_>>(),
            "each hand-off is recorded as its post is, due after a lease"
        );
        let mut held = holder.held();
        held.sort_unstable();
        assert_eq!(
            held,
            due.taken.iter().map(|row| row.id).collect::<Vec<_>>(),
            "each is held the moment it is recorded"
        );
        let recorded: InboundEvent = serde_json::from_str(&due.taken[0].event_json).unwrap();
        assert_eq!(recorded, **event);

        let cases = [
            (
                Answering::Message(&answered),
                true,
                None,
                target.clone(),
                "@U2 notice",
            ),
            (
                Answering::Message(&answered),
                true,
                Some(turn),
                ReplyTarget {
                    conv: conv.clone(),
                    thread_root: None,
                },
                "@U2 at the top level",
            ),
            (
                Answering::Message(&answered),
                false,
                Some(turn),
                target.clone(),
                "@U2 in a DM",
            ),
            (
                Answering::PrivateTask(ConsentId::new_v4()),
                false,
                Some(turn),
                target.clone(),
                "@U2 private",
            ),
        ];
        for (answering, hands_off, turn, to, text) in cases {
            let mut handed = Vec::new();
            assert!(
                delivery(answering, hands_off)
                    .post_to(turn, &to, text, &mut handed)
                    .await,
                "{text}"
            );
            assert!(handed.is_empty(), "{text}");
        }
        for (_, text, msg) in surface.calls().iter().filter_map(|call| match call {
            testkit::Call::Post { to, text, msg } => Some((to, text, msg)),
            _ => None,
        }) {
            let row = store.posted_message_ref(msg).await.unwrap().unwrap();
            assert_eq!(
                row.hands_off,
                text.starts_with("@U2 have") || text.ends_with("too") || text.ends_with("again"),
                "{text}"
            );
        }
    }

    fn asker(user: &str, outside: Option<&str>) -> Requester {
        Requester {
            member: None,
            key: MemberKey {
                surface: core_types::SurfaceKind::Slack,
                team: "T1".into(),
                user: user.into(),
            },
            outside: outside.map(|team| core_types::Outside {
                team: (!team.is_empty()).then(|| team.into()),
            }),
        }
    }

    fn capped_for(requester: Requester) -> Decision {
        Decision::Refuse {
            reason: RefuseReason::DailyCap { max: 1 },
            requester,
        }
    }

    fn run_for(requester: Requester) -> Decision {
        Decision::Run {
            requester,
            hop: Hop::ZERO,
            credential: CredentialRef::Community,
            scope: ScopeKind::Channel,
            side: Side::Public,
        }
    }

    #[test]
    fn copy_stands_compares_key_and_outside() {
        let home = asker("U1", None);
        let theirs = asker("U1", Some("T0THEIRS1"));
        let unknown = asker("U1", Some(""));
        assert!(copy_stands(
            &capped_for(home.clone()),
            &run_for(home.clone())
        ));
        assert!(copy_stands(
            &capped_for(theirs.clone()),
            &capped_for(theirs.clone())
        ));
        for (event, copy) in [
            (&home, &theirs),
            (&theirs, &home),
            (&home, &unknown),
            (&theirs, &unknown),
        ] {
            assert!(
                !copy_stands(&capped_for(event.clone()), &run_for(copy.clone())),
                "{event:?} then {copy:?}"
            );
            assert!(!copy_stands(
                &run_for(event.clone()),
                &capped_for(copy.clone())
            ));
        }
        assert!(!copy_stands(
            &capped_for(home),
            &Decision::Ignore(router::IgnoreReason::Outside)
        ));
    }

    #[test]
    fn copy_stands_still_lets_a_member_be_made_between_routings() {
        let before = asker("U1", None);
        let after = Requester {
            member: Some(MemberId::new_v4()),
            ..before.clone()
        };
        assert!(copy_stands(
            &capped_for(before.clone()),
            &run_for(after.clone())
        ));
        assert!(copy_stands(&run_for(before), &capped_for(after)));
        let theirs = asker("U1", Some("T0THEIRS1"));
        let theirs_with_member = Requester {
            member: Some(MemberId::new_v4()),
            ..theirs.clone()
        };
        assert!(copy_stands(
            &capped_for(theirs),
            &capped_for(theirs_with_member)
        ));
    }

    #[test]
    fn an_event_saying_outside_keeps_the_copy_outside() {
        let conv = core_types::ConvRef {
            surface: core_types::SurfaceKind::Slack,
            team: "T1".into(),
            conversation: "C1".into(),
        };
        let binding = core_types::BindingId::new_v4();
        let event = |outside: Option<&str>| InboundEvent {
            event_id: "Ev1".into(),
            binding,
            sender: asker("U1", None).key,
            sender_is_bot: false,
            sender_bot_user: None,
            conv: conv.clone(),
            conv_kind: ConvKind::Channel,
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
            outside: asker("U1", outside).outside,
        };
        let theirs = event(Some("T0THEIRS1"));
        let home = event(None);
        let unknown = event(Some(""));
        assert_eq!(outside_kept(home.clone(), &theirs), theirs);
        assert_eq!(
            outside_kept(unknown.clone(), &theirs),
            unknown,
            "the copy's own outside stands"
        );
        assert_eq!(outside_kept(theirs.clone(), &home), theirs);
        assert_eq!(outside_kept(unknown.clone(), &home), unknown);
        assert_eq!(outside_kept(home.clone(), &home), home);
    }

    #[test]
    fn only_a_limits_refusal_may_differ_between_an_event_and_its_copy() {
        let requester = Requester {
            member: None,
            key: MemberKey {
                surface: core_types::SurfaceKind::Slack,
                team: "T1".into(),
                user: "U1".into(),
            },
            outside: None,
        };
        let refuse = |reason| Decision::Refuse {
            reason,
            requester: requester.clone(),
        };
        for reason in [
            RefuseReason::DailyCap { max: 1 },
            RefuseReason::ThreadTurns { max: 1 },
            RefuseReason::ThreadTokens { max: 1 },
        ] {
            assert!(limited(&refuse(reason)), "{reason}");
            assert!(limit_window(reason).is_some(), "{reason}");
        }
        let hop_cap = RefuseReason::HopCap { max: Hop(1) };
        assert!(
            !limited(&refuse(hop_cap)),
            "the hop a message is at can't change"
        );
        assert_eq!(limit_window(hop_cap), Some(("hop_cap", LimitWindow::Hour)));
        for reason in [
            RefuseReason::Paused,
            RefuseReason::Banned,
            RefuseReason::Denied,
            RefuseReason::PolicyUnavailable,
        ] {
            assert!(!limited(&refuse(reason)), "{reason}");
            assert!(limit_window(reason).is_none(), "{reason}");
        }
        assert!(!limited(&Decision::LinkPrompt {
            requester: requester.clone()
        }));
        assert!(!limited(&Decision::Ignore(
            router::IgnoreReason::NotAddressed
        )));

        let capped = refuse(RefuseReason::DailyCap { max: 1 });
        let run = Decision::Run {
            requester: requester.clone(),
            hop: Hop::ZERO,
            credential: CredentialRef::Community,
            scope: ScopeKind::Channel,
            side: Side::Public,
        };
        let linked = Decision::Run {
            requester: Requester {
                member: Some(MemberId::new_v4()),
                ..requester.clone()
            },
            hop: Hop::ZERO,
            credential: CredentialRef::Community,
            scope: ScopeKind::Channel,
            side: Side::Public,
        };
        let other = Requester {
            member: None,
            key: MemberKey {
                user: "U2".into(),
                ..requester.key.clone()
            },
            outside: None,
        };
        let run_for_other = Decision::Run {
            requester: other.clone(),
            hop: Hop::ZERO,
            credential: CredentialRef::Community,
            scope: ScopeKind::Channel,
            side: Side::Public,
        };
        let capped_for_other = Decision::Refuse {
            reason: RefuseReason::DailyCap { max: 1 },
            requester: other,
        };
        assert!(copy_stands(&run, &run));
        assert!(copy_stands(&capped, &run));
        assert!(copy_stands(&run, &capped));
        assert!(copy_stands(
            &capped,
            &refuse(RefuseReason::ThreadTurns { max: 1 })
        ));
        assert!(
            copy_stands(&capped, &linked),
            "the requester's identity got a member between the two"
        );
        assert!(!copy_stands(&capped, &run_for_other));
        assert!(!copy_stands(&run, &capped_for_other));
        assert!(!copy_stands(&run, &run_for_other));
        assert!(!copy_stands(&run, &refuse(RefuseReason::Denied)));
        assert!(!copy_stands(
            &capped,
            &Decision::Ignore(router::IgnoreReason::NotAddressed)
        ));
    }

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
                                            outside: None,
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
                                        outside: None,
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
        let mut after = runner::TurnStats::default();
        after.init_seen = true;
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

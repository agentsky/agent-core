//! [`SessionManager`]: sessions, their per-session turn queue, and the warm
//! pool of containers and `claude` processes.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::net::IpAddr;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::Duration;

use core_types::{
    AgentId, ConsentId, CredentialKind, ScopeKey, SessionId, Side, ThreadKey, TurnKind, VolumeKey,
};
use futures::{FutureExt, StreamExt};
use sandbox::{Container, ContainerEvent, ContainerId, Sandbox, SessionSpec, SharedAccess};
use store::{Session, SessionKind, Store};
use time::OffsetDateTime;
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use tokio::task::AbortHandle;
use tokio::time::Instant;

use crate::hooks::{HookError, ProcessEnv, TurnHooks, TurnRequest};
use crate::{
    ClaudeProcess, LaunchSpec, PoolConfig, ProcessConfig, Result, RunnerError, SessionStart,
    TurnOutcome, persona_dir,
};

/// How long the event follower waits before subscribing again when a
/// stream ended sooner than this after it subscribed.
const RESUBSCRIBE_BACKOFF: Duration = Duration::from_secs(1);

/// The longest pause between two idle reaps.
const MAX_REAP_TICK: Duration = Duration::from_secs(30);

/// The shortest pause between two idle reaps.
const MIN_REAP_TICK: Duration = Duration::from_millis(100);

/// What a [`SessionManager`] needs besides its store, sandbox and hooks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionConfig {
    /// How processes are started.
    pub process: ProcessConfig,
    /// How containers are kept warm.
    pub pool: PoolConfig,
    /// The sandbox image, normally `[sandbox] image`.
    pub image: String,
    /// agentd's data directory, which holds each agent's persona directory
    /// ([`persona_dir`]).
    pub data_dir: PathBuf,
}

/// How one turn went, as [`SessionManager::run_turn`] returns it.
pub struct TurnReport<F> {
    /// How the turn ended.
    pub outcome: TurnOutcome,
    /// What [`TurnHooks::turn_finished`] returned for the turn.
    pub finished: std::result::Result<F, HookError>,
    /// How the turn's process was started, when the turn started one
    /// rather than using a warm process.
    pub process_start: Option<SessionStart>,
    /// Whether the CLI refused to resume the session for want of a
    /// transcript, and the turn ran again with `--session-id`.
    pub reran: bool,
}

impl<F> fmt::Debug for TurnReport<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnReport")
            .field("outcome", &self.outcome)
            .field("finished", &self.finished.as_ref().map(|_| ()))
            .field("process_start", &self.process_start)
            .field("reran", &self.reran)
            .finish()
    }
}

/// What a container mounts, which is fixed when it starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mounts {
    shared: SharedAccess,
    memory: bool,
}

impl Mounts {
    /// The mounts for a turn on `side` on a volume of `scope`: see
    /// [`TurnRequest::side`].
    fn for_turn(scope: &ScopeKey, side: Side) -> Self {
        match (scope, side) {
            (ScopeKey::Private, Side::Owner) => Self {
                shared: SharedAccess::ReadWrite,
                memory: true,
            },
            (ScopeKey::Private, Side::Public) => Self {
                shared: SharedAccess::ReadOnly,
                memory: false,
            },
            _ => Self {
                shared: SharedAccess::ReadWrite,
                memory: false,
            },
        }
    }
}

/// What to do with a session's process and container once a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterTurn {
    /// Keep both warm.
    Keep,
    /// Stop the process; keep the container.
    StopProcess,
    /// Stop the process and the container.
    StopContainer,
}

/// Decides [`AfterTurn`]. A container that died goes. A process that is
/// gone goes, and so does its container if the process wasn't seen to exit,
/// so two processes never share a transcript. A process whose
/// `turn_finished` failed goes too, since its placeholder may still be
/// pointed.
fn after_turn(dead: bool, running: bool, may_be_alive: bool, finished_ok: bool) -> AfterTurn {
    if dead || (!running && may_be_alive) {
        AfterTurn::StopContainer
    } else if !running || !finished_ok {
        AfterTurn::StopProcess
    } else {
        AfterTurn::Keep
    }
}

/// Checks that a turn's kind matches its session's.
fn check_kind(session: &Session, request: &TurnRequest) -> Result<()> {
    match (session.kind, request.kind) {
        (SessionKind::Normal, TurnKind::Normal) => Ok(()),
        (SessionKind::Private(ours), TurnKind::PrivateTask(theirs)) if ours == theirs => Ok(()),
        _ => Err(RunnerError::InvalidRequest(
            "a private task runs only on its own consent's session, and a normal turn only on a normal session",
        )),
    }
}

/// The per-session slot: its turn queue is this mutex, which tokio hands
/// out in arrival order.
type Slot<H> = Arc<AsyncMutex<Warm<H>>>;

/// What a session has warm. Only whoever holds the slot's lock touches it.
struct Warm<H: TurnHooks> {
    session: SessionId,
    slot: Weak<AsyncMutex<Warm<H>>>,
    held: Option<Held<H>>,
}

/// A running container, and the process in it, if any.
struct Held<H: TurnHooks> {
    container: Container,
    mounts: Mounts,
    tracked: Arc<Tracked<H>>,
    process: Option<Running<H>>,
    _scope_permit: OwnedSemaphorePermit,
    _global_permit: OwnedSemaphorePermit,
}

/// A process, and what [`TurnHooks::process_starting`] made for it.
struct Running<H: TurnHooks> {
    process: ClaudeProcess,
    handle: Arc<H::Process>,
    /// It was started with [`SessionStart::Resume`] and no turn has been
    /// sent to it yet, so a [`TurnOutcome::resume_refused`] result of the
    /// next send is the CLI refusing the `--resume`.
    resume_unsent: bool,
}

/// A container as the event follower, the reaper and other sessions see
/// it, without the slot's lock.
struct Tracked<H: TurnHooks> {
    container: ContainerId,
    session: Session,
    volume: VolumeKey,
    slot: Weak<AsyncMutex<Warm<H>>>,
    state: Mutex<TrackedState<H>>,
}

struct TrackedState<H: TurnHooks> {
    /// The sandbox reported the container dead, its session is stopping
    /// it, or the sandbox failed to stop it: its session must stop it before
    /// using it again.
    dead: bool,
    /// When its last turn ended, or it started.
    last_used: Instant,
    /// The process running in it now.
    process: Option<Arc<H::Process>>,
}

impl<H: TurnHooks> Tracked<H> {
    fn state(&self) -> MutexGuard<'_, TrackedState<H>> {
        lock(&self.state)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether `slot` holds a container, or is locked for a turn or a stop.
fn is_warm<H: TurnHooks>(slot: &Slot<H>) -> bool {
    slot.try_lock().map_or(true, |warm| warm.held.is_some())
}

/// Sessions, their turn queues, and the warm pool: one container and one
/// `claude` process per active session.
///
/// # Turns
///
/// [`run_turn`](Self::run_turn) serializes turns per session: they queue in
/// arrival order, and one runs at a time. A turn runs in a task of its own
/// once it reaches the front of the queue, so a caller that stops waiting
/// doesn't cut it short: [`TurnHooks::turn_finished`] always runs, and
/// returns before the next turn of the session starts. A turn whose
/// `turn_starting`, send or `turn_finished` panicked still has
/// `turn_finished` called, unless it was the one that panicked, and then has
/// its process stopped as after a failed `turn_finished`, before the panic
/// goes on to fail the turn with [`RunnerError::TurnTask`].
///
/// A failed or panicking `process_starting` fails the turn and stops the
/// container, and so does a process that fails or panics while starting. A
/// panic in `process_stopping` is logged like its failure, and the stop goes
/// ahead.
///
/// A turn reuses the session's warm process when its credential kind, its
/// model and its mounts match, and otherwise stops it (and the container,
/// for other mounts) and starts another, resuming from the transcript. A
/// process that crashed, timed out or refused its `--resume` is stopped
/// after the turn; if it wasn't seen to exit, its container is stopped too
/// before the next process starts, so two processes never write one
/// transcript. A container the sandbox fails to stop stays the session's,
/// marked dead: the session's turns fail until a later stop succeeds, rather
/// than start another process on its transcript.
///
/// # Started sessions
///
/// A session is started once a turn's [`TurnStats::init_seen`] is true,
/// whatever the turn's outcome: the CLI read the message, so the transcript
/// exists. Until then processes start with `--session-id`. If the CLI
/// refuses a `--resume` for want of a transcript
/// ([`TurnOutcome::resume_refused`] on the first turn sent to a process
/// started with [`SessionStart::Resume`]), the session is marked unstarted
/// and the turn runs again, once, with `--session-id` under the same id.
///
/// Before a turn goes to an unstarted session's CLI, the store records
/// that the session may have started; the turn's end clears it once the
/// outcome says. A turn cut off by agentd dying leaves it set, so the next
/// process tries `--resume` first.
///
/// # The warm pool
///
/// A container holds a place in its volume's cap
/// ([`PoolConfig::scope_container_cap`]) and in the global cap. A session
/// that needs a container when a cap is full first reaps an idle container
/// under that cap, and otherwise waits until one becomes idle.
///
/// Containers idle for [`PoolConfig::idle_timeout_secs`] are reaped:
/// [`TurnHooks::process_stopping`], then the process and the container
/// stop. A container the sandbox failed to stop keeps its places under the
/// caps, and the reaper tries again each round.
///
/// The manager follows [`Sandbox::events`]: a container that died has
/// `process_stopping` called for its process at once, so its address can't
/// be reused while its placeholder and token live. When the stream ends
/// (it always ends with an error), the manager subscribes again and
/// compares [`Sandbox::list_managed`] with the containers it holds.
///
/// Warm containers live in this process only: agentd reaps every sandbox
/// at startup, since placeholders and agentctl tokens don't survive a
/// restart either.
///
/// [`TurnStats::init_seen`]: crate::TurnStats::init_seen
pub struct SessionManager<H: TurnHooks> {
    inner: Arc<Inner<H>>,
}

impl<H: TurnHooks> fmt::Debug for SessionManager<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionManager").finish_non_exhaustive()
    }
}

struct Inner<H: TurnHooks> {
    store: Store,
    sandbox: Arc<dyn Sandbox>,
    hooks: H,
    config: SessionConfig,
    slots: Mutex<HashMap<SessionId, Slot<H>>>,
    containers: Mutex<HashMap<ContainerId, Arc<Tracked<H>>>>,
    scope_caps: Mutex<HashMap<VolumeKey, Arc<Semaphore>>>,
    global_cap: Arc<Semaphore>,
    idle: Notify,
    tasks: Mutex<Vec<AbortHandle>>,
}

impl<H: TurnHooks> Drop for Inner<H> {
    fn drop(&mut self) {
        for task in lock(&self.tasks).drain(..) {
            task.abort();
        }
    }
}

impl<H: TurnHooks> SessionManager<H> {
    /// A manager over `store`, `sandbox` and `hooks`.
    ///
    /// It spawns the idle reaper and the sandbox event follower, which stop
    /// when the manager is dropped, so it must be called inside a Tokio
    /// runtime.
    ///
    /// # Errors
    ///
    /// [`RunnerError::Config`] if `config.process` or `config.pool` is
    /// invalid.
    pub fn new(
        store: Store,
        sandbox: Arc<dyn Sandbox>,
        hooks: H,
        config: SessionConfig,
    ) -> Result<Self> {
        config.process.validate()?;
        config.pool.validate()?;
        let inner = Arc::new(Inner {
            store,
            sandbox,
            hooks,
            global_cap: Arc::new(Semaphore::new(config.pool.global_container_cap)),
            config,
            slots: Mutex::default(),
            containers: Mutex::default(),
            scope_caps: Mutex::default(),
            idle: Notify::new(),
            tasks: Mutex::default(),
        });
        let tick = (inner.config.pool.idle_timeout() / 4).clamp(MIN_REAP_TICK, MAX_REAP_TICK);
        let reaper = tokio::spawn(reap_loop(Arc::downgrade(&inner), tick));
        let follower = tokio::spawn(follow_events(Arc::downgrade(&inner)));
        lock(&inner.tasks).extend([reaper.abort_handle(), follower.abort_handle()]);
        Ok(Self { inner })
    }

    /// The live session of `agent` in `thread`, on `scope`, created with a
    /// new v4 id if there is none. A DM's session is its conversation's,
    /// with no thread root.
    ///
    /// A session never changes scope: if the thread's live session is on
    /// another scope, it is reset and replaced, and its warm container, if
    /// any, stopped in the background.
    ///
    /// # Errors
    ///
    /// [`RunnerError::Store`].
    pub async fn lookup_or_create(
        &self,
        agent: AgentId,
        thread: &ThreadKey,
        scope: &ScopeKey,
    ) -> Result<Session> {
        let found = self
            .inner
            .store
            .session_for_thread(agent, thread, scope, OffsetDateTime::now_utc())
            .await?;
        if let Some(old) = found.replaced {
            tracing::info!(session = %old, replacement = %found.session.id, "replaced a session on another scope");
            let manager = Self {
                inner: Arc::clone(&self.inner),
            };
            tokio::spawn(async move { manager.stop(old).await });
        }
        Ok(found.session)
    }

    /// A new private task's session for `agent`, on its `Private` volume,
    /// always with a new id, under `consent`. `thread` is where the task's
    /// result is posted. Its turns must be `TurnKind::PrivateTask(consent)`.
    ///
    /// # Errors
    ///
    /// [`RunnerError::Store`].
    pub async fn create_private(
        &self,
        agent: AgentId,
        consent: ConsentId,
        thread: &ThreadKey,
    ) -> Result<Session> {
        Ok(self
            .inner
            .store
            .create_private_session(agent, consent, thread, OffsetDateTime::now_utc())
            .await?)
    }

    /// Resets `session`: once the turns queued before it have run, stops
    /// its warm process and container, marks it reset and, for a normal
    /// session, makes its replacement with a new id, which the next turn
    /// starts with `--session-id`. Returns the replacement, or `None` for a
    /// private task's session and a session that was unknown or already
    /// reset. Turns queued after the reset fail with
    /// [`RunnerError::SessionReset`].
    ///
    /// # Errors
    ///
    /// - [`RunnerError::Sandbox`] if the warm container couldn't be
    ///   stopped. The session isn't reset.
    /// - [`RunnerError::Store`].
    pub async fn reset(&self, session: SessionId) -> Result<Option<Session>> {
        self.with_slot(session, |inner, mut warm| async move {
            inner.release_container(&mut warm).await?;
            Ok(inner
                .store
                .reset_session(warm.session, OffsetDateTime::now_utc())
                .await?)
        })
        .await
    }

    /// Stops `session`'s warm process and container, if it has them, once
    /// the turns queued before it have run. The session itself stays: its
    /// next turn resumes from the transcript.
    pub async fn stop(&self, session: SessionId) {
        let stopped = self
            .with_slot(session, |inner, mut warm| async move {
                inner.release_container(&mut warm).await
            })
            .await;
        if let Err(error) = stopped {
            tracing::warn!(%session, %error, "stopping a session failed");
        }
    }

    /// Whether `session` has a warm container, or a turn running.
    pub fn is_warm(&self, session: SessionId) -> bool {
        let slot = lock(&self.inner.slots).get(&session).cloned();
        slot.is_some_and(|slot| is_warm(&slot))
    }

    /// The sessions that have a warm container, or a turn running.
    pub fn warm_sessions(&self) -> Vec<SessionId> {
        let slots: Vec<(SessionId, Slot<H>)> = lock(&self.inner.slots)
            .iter()
            .map(|(session, slot)| (*session, Arc::clone(slot)))
            .collect();
        slots
            .into_iter()
            .filter(|(_, slot)| is_warm(slot))
            .map(|(session, _)| session)
            .collect()
    }

    /// Runs one turn on `session`, after the turns queued before it.
    ///
    /// Once the turn reaches the front of the session's queue it runs to
    /// the end in a task of its own, even if the caller stops waiting. A
    /// caller that stops waiting before then leaves the queue.
    ///
    /// # Errors
    ///
    /// - [`RunnerError::UnknownSession`] or [`RunnerError::SessionReset`]
    ///   for a session that can't take turns.
    /// - [`RunnerError::InvalidRequest`] for a turn whose kind doesn't match
    ///   the session.
    /// - [`RunnerError::Hook`] if `process_starting` or `turn_starting`
    ///   failed; the turn wasn't sent.
    /// - [`RunnerError::Sandbox`], [`RunnerError::Store`] and the process's
    ///   own errors if the container or the process couldn't be started, or
    ///   the session's old container couldn't be stopped.
    /// - [`RunnerError::TurnTask`] if the turn's task panicked.
    pub async fn run_turn(
        &self,
        session: SessionId,
        request: TurnRequest,
    ) -> Result<TurnReport<H::Finished>> {
        self.with_slot(session, move |inner, mut warm| async move {
            inner.turn(&mut warm, &request).await
        })
        .await
    }

    /// Waits for `session`'s slot, then runs `work` on it in a task of its
    /// own, which the caller's drop doesn't cancel. The slot is released
    /// when `work` ends, even by panicking, and then waiters for an idle
    /// container are woken.
    async fn with_slot<T, F, Fut>(&self, session: SessionId, work: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Inner<H>>, OwnedMutexGuard<Warm<H>>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let slot = self.inner.slot(session);
        let warm = slot.lock_owned().await;
        let inner = Arc::clone(&self.inner);
        let task = tokio::spawn(async move {
            let _released = SlotReleased(Arc::clone(&inner));
            work(inner, warm).await
        });
        task.await.map_err(|_| RunnerError::TurnTask)?
    }
}

/// Wakes waiters for an idle container and prunes when dropped: at the end
/// of a slot's work, however it ends, once the work has dropped the slot's
/// guard.
struct SlotReleased<H: TurnHooks>(Arc<Inner<H>>);

impl<H: TurnHooks> Drop for SlotReleased<H> {
    fn drop(&mut self) {
        self.0.idle.notify_waiters();
        self.0.prune();
    }
}

impl<H: TurnHooks> Inner<H> {
    /// The session's slot, created if it has none.
    fn slot(&self, session: SessionId) -> Slot<H> {
        let mut slots = lock(&self.slots);
        Arc::clone(slots.entry(session).or_insert_with(|| {
            Arc::new_cyclic(|slot| {
                AsyncMutex::new(Warm {
                    session,
                    slot: slot.clone(),
                    held: None,
                })
            })
        }))
    }

    /// Forgets slots nobody uses that hold no container, and scope caps
    /// nobody holds or waits for.
    fn prune(&self) {
        lock(&self.slots).retain(|_, slot| Arc::strong_count(slot) > 1 || is_warm(slot));
        let cap = self.config.pool.scope_container_cap;
        lock(&self.scope_caps).retain(|_, semaphore| {
            Arc::strong_count(semaphore) > 1 || semaphore.available_permits() < cap
        });
    }

    /// One turn, with the session's slot held.
    async fn turn(
        &self,
        warm: &mut Warm<H>,
        request: &TurnRequest,
    ) -> Result<TurnReport<H::Finished>> {
        let mut reran = false;
        loop {
            let session = self
                .store
                .session(warm.session)
                .await?
                .ok_or(RunnerError::UnknownSession)?;
            if session.reset_at.is_some() {
                return Err(RunnerError::SessionReset);
            }
            check_kind(&session, request)?;
            let process_start = self.ensure_process(warm, &session, request).await?;
            let (outcome, finished) = self.exchange(warm, &session, request).await;
            let (outcome, refused) = outcome?;
            if refused && !reran {
                tracing::info!(session = %session.id, "the CLI refused to resume a session without a transcript; running the turn again with --session-id");
                reran = true;
                continue;
            }
            return Ok(TurnReport {
                outcome,
                finished,
                process_start,
                reran,
            });
        }
    }

    /// Sends the turn to the warm process between `turn_starting` and
    /// `turn_finished`, records it, and stops what the outcome says to.
    /// Returns the outcome, and whether it is the CLI refusing the
    /// process's `--resume` ([`Running::resume_unsent`]).
    ///
    /// A panic in `turn_starting` or the send still has `turn_finished`
    /// called. After any panic, `turn_finished`'s included, the turn is
    /// recorded if it has an outcome and the process is stopped as after a
    /// failed `turn_finished`; then the panic resumes.
    async fn exchange(
        &self,
        warm: &mut Warm<H>,
        session: &Session,
        request: &TurnRequest,
    ) -> (
        Result<(TurnOutcome, bool)>,
        std::result::Result<H::Finished, HookError>,
    ) {
        let Some(held) = warm.held.as_mut() else {
            return (Err(RunnerError::NotRunning), Err("no process".into()));
        };
        let Some(running) = held.process.as_mut() else {
            return (Err(RunnerError::NotRunning), Err("no process".into()));
        };
        let handle = Arc::clone(&running.handle);
        let mut panicked = None;
        let sent = AssertUnwindSafe(self.send(session, &handle, running, request))
            .catch_unwind()
            .await;
        let outcome = sent.unwrap_or_else(|panic| {
            panicked = Some(panic);
            Err(RunnerError::TurnTask)
        });
        let finished = AssertUnwindSafe(self.hooks.turn_finished(session, &handle, request))
            .catch_unwind()
            .await;
        let finished = finished.unwrap_or_else(|panic| {
            panicked = panicked.take().or(Some(panic));
            Err("the turn_finished hook panicked".into())
        });
        if let Err(error) = &finished {
            tracing::warn!(session = %session.id, %error, "the turn_finished hook failed; stopping the process");
        } else if panicked.is_some() {
            tracing::warn!(session = %session.id, "the turn panicked; stopping the process");
        }
        if let Ok((outcome, refused)) = &outcome {
            let recorded = if *refused {
                self.store.mark_session_unstarted(session.id).await
            } else {
                self.store
                    .record_session_turn(
                        session.id,
                        outcome.stats().init_seen,
                        OffsetDateTime::now_utc(),
                    )
                    .await
            };
            if let Err(error) = recorded {
                tracing::warn!(session = %session.id, %error, "recording the turn failed");
            }
        }
        let dead = {
            let mut state = held.tracked.state();
            state.last_used = Instant::now();
            state.dead
        };
        let action = after_turn(
            dead,
            running.process.is_running(),
            running.process.may_be_alive(),
            finished.is_ok() && panicked.is_none(),
        );
        match action {
            AfterTurn::Keep => {}
            AfterTurn::StopProcess => {
                if self.stop_process(held).await {
                    self.release_container(warm).await.ok();
                }
            }
            AfterTurn::StopContainer => {
                self.release_container(warm).await.ok();
            }
        }
        if let Some(panic) = panicked {
            std::panic::resume_unwind(panic);
        }
        (outcome, finished)
    }

    /// `turn_starting`, then the turn to `running` once the store records
    /// that the session may have started. Returns the outcome, and whether
    /// it is the CLI refusing the process's `--resume`.
    async fn send(
        &self,
        session: &Session,
        handle: &H::Process,
        running: &mut Running<H>,
        request: &TurnRequest,
    ) -> Result<(TurnOutcome, bool)> {
        self.hooks
            .turn_starting(session, handle, request)
            .await
            .map_err(|source| RunnerError::Hook {
                hook: "turn_starting",
                source,
            })?;
        if !self.store.mark_session_turn_pending(session.id).await? {
            return Err(RunnerError::SessionReset);
        }
        let resumed = std::mem::take(&mut running.resume_unsent);
        let outcome = running.process.send_turn(&request.message).await?;
        let refused = resumed && outcome.resume_refused();
        Ok((outcome, refused))
    }

    /// Makes sure the session has a container with the turn's mounts and a
    /// running process on the turn's credential kind and model. Returns how
    /// the process was started, if it was.
    ///
    /// The container's address is read after the container is tracked, so
    /// a death the sandbox reports from then on is seen, and one before then
    /// makes reading the address fail.
    ///
    /// A failed or panicking `process_starting` stops the container. So
    /// does a process that fails or panics while starting, after
    /// `process_stopping` for it, since it may have been started.
    ///
    /// # Errors
    ///
    /// Besides a failed start, [`RunnerError::Sandbox`] if a container
    /// that had to go first couldn't be stopped: the session still holds
    /// it, and no process starts beside one that may still run.
    /// [`RunnerError::TurnTask`] if the process panicked while starting.
    async fn ensure_process(
        &self,
        warm: &mut Warm<H>,
        session: &Session,
        request: &TurnRequest,
    ) -> Result<Option<SessionStart>> {
        let mounts = Mounts::for_turn(&session.scope, request.side);
        let kind = request.credential.kind();
        if let Some(held) = &warm.held {
            let dead = held.tracked.state().dead;
            if dead || held.mounts != mounts {
                self.release_container(warm).await?;
            }
        }
        if let Some(held) = warm.held.as_mut() {
            let fits = held.process.as_ref().is_none_or(|running| {
                running.process.is_running()
                    && running.process.credential() == kind
                    && running.process.model() == request.model.as_deref()
            });
            if !fits && self.stop_process(held).await {
                self.release_container(warm).await?;
            }
        }
        let held = match warm.held.take() {
            Some(held) => warm.held.insert(held),
            None => {
                let held = self
                    .start_container(session, mounts, warm.slot.clone())
                    .await?;
                warm.held.insert(held)
            }
        };
        if held.process.is_some() {
            return Ok(None);
        }
        let ip = match self.sandbox.ip(held.container.id()).await {
            Ok(ip) => ip,
            Err(error) => {
                self.release_container(warm).await.ok();
                return Err(error.into());
            }
        };
        let start = if session.resumes() {
            SessionStart::Resume
        } else {
            SessionStart::New
        };
        let (env, handle) = match self.process_starting(session, ip, kind).await {
            Ok(started) => started,
            Err(error) => {
                self.release_container(warm).await.ok();
                return Err(error);
            }
        };
        let handle = Arc::new(handle);
        held.tracked.state().process = Some(Arc::clone(&handle));
        let spec = LaunchSpec {
            start,
            model: request.model.clone(),
            credential: kind,
            placeholder: env.placeholder,
            env: env.env,
        };
        let started = AssertUnwindSafe(ClaudeProcess::start(
            self.sandbox.as_ref(),
            &held.container,
            &self.config.process,
            spec,
        ))
        .catch_unwind()
        .await
        .unwrap_or_else(|_| {
            tracing::warn!(session = %session.id, "starting the process panicked; stopping the container");
            Err(RunnerError::TurnTask)
        });
        match started {
            Ok(process) => {
                held.process = Some(Running {
                    process,
                    handle,
                    resume_unsent: start == SessionStart::Resume,
                });
                Ok(Some(start))
            }
            Err(error) => {
                held.tracked.state().process = None;
                self.process_stopping(session, &handle).await;
                self.release_container(warm).await.ok();
                Err(error)
            }
        }
    }

    /// Starts a container for `session` once both caps have room, and
    /// tracks it.
    async fn start_container(
        &self,
        session: &Session,
        mounts: Mounts,
        slot: Weak<AsyncMutex<Warm<H>>>,
    ) -> Result<Held<H>> {
        let volume_key = session.volume();
        let scope_cap = self.scope_cap(&volume_key);
        let scope_permit = self.acquire(&scope_cap, Some(&volume_key)).await;
        let global_permit = self.acquire(&self.global_cap, None).await;
        let volume = self.sandbox.ensure_volume(&volume_key).await?;
        let mut spec = SessionSpec::new(
            session.id,
            volume,
            self.config.image.clone(),
            persona_dir(&self.config.data_dir, session.agent),
        );
        spec.shared = mounts.shared;
        spec.memory = mounts.memory;
        let container = self.sandbox.start(&spec).await?;
        let tracked = Arc::new(Tracked {
            container: container.id().clone(),
            session: session.clone(),
            volume: volume_key,
            slot,
            state: Mutex::new(TrackedState {
                dead: false,
                last_used: Instant::now(),
                process: None,
            }),
        });
        lock(&self.containers).insert(container.id().clone(), Arc::clone(&tracked));
        tracing::info!(session = %session.id, container = %container.id(), "started a session container");
        Ok(Held {
            container,
            mounts,
            tracked,
            process: None,
            _scope_permit: scope_permit,
            _global_permit: global_permit,
        })
    }

    /// `volume`'s cap, created if it has none.
    fn scope_cap(&self, volume: &VolumeKey) -> Arc<Semaphore> {
        let cap = self.config.pool.scope_container_cap;
        Arc::clone(
            lock(&self.scope_caps)
                .entry(volume.clone())
                .or_insert_with(|| Arc::new(Semaphore::new(cap))),
        )
    }

    /// A place under `cap`: at once if it has room, else after reaping an
    /// idle container under it (in `scope`, or anywhere for the global
    /// cap), else once a place frees up or a container becomes idle.
    async fn acquire(
        &self,
        cap: &Arc<Semaphore>,
        scope: Option<&VolumeKey>,
    ) -> OwnedSemaphorePermit {
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if let Ok(permit) = Arc::clone(cap).try_acquire_owned() {
                return permit;
            }
            if self.evict_idle(scope).await {
                continue;
            }
            tokio::select! {
                permit = Arc::clone(cap).acquire_owned() => {
                    if let Ok(permit) = permit {
                        return permit;
                    }
                }
                () = &mut idle => {}
            }
        }
    }

    /// Stops one idle container, in `scope` if given: a dead one first,
    /// then the one idle longest. Returns whether it stopped one.
    async fn evict_idle(&self, scope: Option<&VolumeKey>) -> bool {
        let mut candidates: Vec<(bool, Instant, Arc<Tracked<H>>)> = lock(&self.containers)
            .values()
            .filter(|tracked| scope.is_none_or(|scope| tracked.volume == *scope))
            .map(|tracked| {
                let state = tracked.state();
                (!state.dead, state.last_used, Arc::clone(tracked))
            })
            .collect();
        candidates.sort_by_key(|(alive, last_used, _)| (*alive, *last_used));
        for (_, _, tracked) in candidates {
            if self.stop_if_idle(&tracked, |_| true).await {
                tracing::info!(session = %tracked.session.id, container = %tracked.container, "stopped an idle container to make room");
                return true;
            }
        }
        false
    }

    /// Stops `tracked`'s container if its session has no turn running,
    /// still holds it, and `due` still says so once the session's lock is
    /// held. Returns whether it stopped it.
    async fn stop_if_idle(
        &self,
        tracked: &Tracked<H>,
        due: impl FnOnce(&TrackedState<H>) -> bool,
    ) -> bool {
        let Some(slot) = tracked.slot.upgrade() else {
            return false;
        };
        let Ok(mut warm) = slot.try_lock_owned() else {
            return false;
        };
        if warm
            .held
            .as_ref()
            .is_none_or(|held| *held.container.id() != tracked.container)
            || !due(&tracked.state())
        {
            return false;
        }
        self.release_container(&mut warm).await.is_ok()
    }

    /// Stops every container idle for longer than the idle timeout, and
    /// every dead one nobody is using.
    async fn reap_idle(&self) {
        let timeout = self.config.pool.idle_timeout();
        let is_due = |state: &TrackedState<H>| state.dead || state.last_used.elapsed() >= timeout;
        let due: Vec<Arc<Tracked<H>>> = lock(&self.containers)
            .values()
            .filter(|tracked| is_due(&tracked.state()))
            .cloned()
            .collect();
        for tracked in due {
            if self.stop_if_idle(&tracked, is_due).await {
                tracing::info!(session = %tracked.session.id, container = %tracked.container, "reaped an idle container");
            }
        }
        self.prune();
    }

    /// Stops the held container: `process_stopping` and the process first,
    /// then the container. Its places under the caps are freed.
    ///
    /// It is marked dead first, so the death the sandbox reports for the
    /// stop isn't taken for a crash.
    ///
    /// If the sandbox fails to stop it, the session keeps holding it,
    /// marked dead, with its places under the caps: it may still be running,
    /// so no other process may resume its transcript, and the reaper tries
    /// again.
    ///
    /// # Errors
    ///
    /// [`RunnerError::Sandbox`] if the container couldn't be stopped.
    async fn release_container(&self, warm: &mut Warm<H>) -> Result<()> {
        let Some(held) = warm.held.as_mut() else {
            return Ok(());
        };
        held.tracked.state().dead = true;
        self.stop_process(held).await;
        if let Err(error) = self.sandbox.stop(held.container.id()).await {
            tracing::warn!(session = %warm.session, container = %held.container.id(), %error, "stopping a session container failed; keeping it to try again");
            return Err(error.into());
        }
        tracing::info!(session = %warm.session, container = %held.container.id(), "stopped a session container");
        lock(&self.containers).remove(held.container.id());
        warm.held = None;
        self.idle.notify_waiters();
        Ok(())
    }

    /// Stops the held process, if any, after `process_stopping`. Returns
    /// whether the process may still be running, in which case the
    /// container must be stopped before another process starts in it.
    async fn stop_process(&self, held: &mut Held<H>) -> bool {
        let Some(mut running) = held.process.take() else {
            return false;
        };
        held.tracked.state().process = None;
        self.process_stopping(&held.tracked.session, &running.handle)
            .await;
        running.process.stop().await;
        running.process.may_be_alive()
    }

    /// Calls `process_starting`, turning a panic into a failure.
    async fn process_starting(
        &self,
        session: &Session,
        ip: IpAddr,
        kind: CredentialKind,
    ) -> Result<(ProcessEnv, H::Process)> {
        let started = AssertUnwindSafe(self.hooks.process_starting(session, ip, kind))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err("the process_starting hook panicked".into()));
        started.map_err(|source| {
            tracing::warn!(session = %session.id, error = %source, "the process_starting hook failed; stopping the container");
            RunnerError::Hook {
                hook: "process_starting",
                source,
            }
        })
    }

    /// Calls `process_stopping`, logging a failure or a panic.
    async fn process_stopping(&self, session: &Session, handle: &H::Process) {
        let stopped = AssertUnwindSafe(self.hooks.process_stopping(session, handle))
            .catch_unwind()
            .await
            .unwrap_or_else(|_| Err("the process_stopping hook panicked".into()));
        if let Err(error) = stopped {
            tracing::warn!(session = %session.id, %error, "the process_stopping hook failed");
        }
    }

    /// The sandbox reported `container` dead: revoke its process at once,
    /// and stop what is left of it if its session isn't using it.
    async fn on_died(&self, container: &ContainerId) {
        let Some(tracked) = lock(&self.containers).get(container).cloned() else {
            return;
        };
        let process = {
            let mut state = tracked.state();
            if state.dead {
                return;
            }
            state.dead = true;
            state.process.clone()
        };
        tracing::warn!(session = %tracked.session.id, %container, "a session container died");
        if let Some(process) = process {
            self.process_stopping(&tracked.session, &process).await;
        }
        if self.stop_if_idle(&tracked, |_| true).await {
            self.idle.notify_waiters();
            self.prune();
        }
    }

    /// Compares the containers the sandbox lists with those held, and
    /// treats every held container it doesn't list as running as dead.
    /// Returns false if the sandbox couldn't list them.
    async fn reconcile(&self) -> bool {
        let held: Vec<ContainerId> = lock(&self.containers).keys().cloned().collect();
        let listed = match self.sandbox.list_managed().await {
            Ok(listed) => listed,
            Err(error) => {
                tracing::warn!(%error, "listing sandbox containers failed");
                return false;
            }
        };
        let running: HashSet<ContainerId> = listed
            .into_iter()
            .filter(|container| container.running)
            .map(|container| container.id)
            .collect();
        for container in held {
            if !running.contains(&container) {
                self.on_died(&container).await;
            }
        }
        true
    }
}

/// Reaps idle containers every `tick` while the manager lives.
async fn reap_loop<H: TurnHooks>(inner: Weak<Inner<H>>, tick: Duration) {
    loop {
        tokio::time::sleep(tick).await;
        let Some(inner) = inner.upgrade() else {
            return;
        };
        inner.reap_idle().await;
    }
}

/// Follows the sandbox's container events while the manager lives,
/// subscribing again, and comparing the sandbox's list with the held
/// containers, whenever a stream ends.
async fn follow_events<H: TurnHooks>(weak: Weak<Inner<H>>) {
    loop {
        let subscribed = Instant::now();
        let mut events = {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let events = inner.sandbox.events();
            if inner.reconcile().await {
                Some(events)
            } else {
                None
            }
        };
        while let Some(stream) = events.as_mut() {
            match stream.next().await {
                Some(Ok(ContainerEvent::Died { container, .. })) => {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    inner.on_died(&container).await;
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    tracing::warn!(%error, "sandbox events ended; subscribing again");
                    events = None;
                }
                None => events = None,
            }
        }
        if subscribed.elapsed() < RESUBSCRIBE_BACKOFF {
            tokio::time::sleep(RESUBSCRIBE_BACKOFF).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use core_types::{ConvRef, MemberKey, Requester, SurfaceKind};

    use super::*;

    fn session(scope: ScopeKey, kind: SessionKind) -> Session {
        Session {
            id: SessionId::new_v4(),
            agent: AgentId::new_v4(),
            thread: ThreadKey {
                conv: ConvRef {
                    surface: SurfaceKind::Slack,
                    team: "T1".into(),
                    conversation: "C1".into(),
                },
                root: None,
            },
            scope,
            kind,
            started: false,
            maybe_started: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
            last_turn_at: None,
            reset_at: None,
        }
    }

    fn request(kind: TurnKind) -> TurnRequest {
        TurnRequest {
            turn: core_types::TurnId::new_v4(),
            message: "hi".into(),
            credential: core_types::CredentialRef::Community,
            model: None,
            requester: Requester {
                member: None,
                key: MemberKey {
                    surface: SurfaceKind::Slack,
                    team: "T1".into(),
                    user: "U1".into(),
                },
            },
            hop: core_types::Hop::ZERO,
            side: Side::Public,
            kind,
            trigger: None,
        }
    }

    #[test]
    fn a_process_that_may_be_alive_takes_its_container_with_it() {
        use AfterTurn::*;
        assert_eq!(after_turn(false, true, true, true), Keep);
        assert_eq!(after_turn(false, true, true, false), StopProcess);
        assert_eq!(after_turn(false, false, false, true), StopProcess);
        assert_eq!(after_turn(false, false, true, true), StopContainer);
        assert_eq!(after_turn(false, false, true, false), StopContainer);
        assert_eq!(after_turn(true, true, true, true), StopContainer);
        assert_eq!(after_turn(true, false, false, true), StopContainer);
    }

    #[test]
    fn mounts_follow_the_volume_and_the_side() {
        let channel = ScopeKey::Channel(ConvRef {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            conversation: "C1".into(),
        });
        let rw = Mounts {
            shared: SharedAccess::ReadWrite,
            memory: false,
        };
        assert_eq!(Mounts::for_turn(&channel, Side::Public), rw);
        assert_eq!(Mounts::for_turn(&channel, Side::Owner), rw);
        assert_eq!(
            Mounts::for_turn(&ScopeKey::Private, Side::Owner),
            Mounts {
                shared: SharedAccess::ReadWrite,
                memory: true
            }
        );
        assert_eq!(
            Mounts::for_turn(&ScopeKey::Private, Side::Public),
            Mounts {
                shared: SharedAccess::ReadOnly,
                memory: false
            }
        );
    }

    #[test]
    fn a_turn_must_match_its_sessions_kind() {
        let consent = ConsentId::new_v4();
        let normal = session(ScopeKey::Private, SessionKind::Normal);
        let private = session(ScopeKey::Private, SessionKind::Private(consent));
        check_kind(&normal, &request(TurnKind::Normal)).unwrap();
        check_kind(&private, &request(TurnKind::PrivateTask(consent))).unwrap();
        for (session, kind) in [
            (&normal, TurnKind::PrivateTask(consent)),
            (&private, TurnKind::Normal),
            (&private, TurnKind::PrivateTask(ConsentId::new_v4())),
        ] {
            assert!(matches!(
                check_kind(session, &request(kind)),
                Err(RunnerError::InvalidRequest(_))
            ));
        }
    }
}

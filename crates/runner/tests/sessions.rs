use std::collections::{BTreeMap, HashSet};
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use core_types::{
    AgentId, ConsentId, ConvRef, CredentialKind, CredentialRef, Hop, MemberId, MemberKey,
    MessageId, Requester, ScopeKey, SessionId, Side, SurfaceKind, ThreadKey, TurnId, TurnKind,
    VolumeKey,
};
use futures::stream::{BoxStream, StreamExt};
use runner::{
    HookError, PoolConfig, ProcessConfig, ProcessEnv, RunnerError, Session, SessionConfig,
    SessionManager, SessionStart, TurnHooks, TurnOutcome, TurnReport, TurnRequest,
};
use sandbox::{
    ChildIo, Container, ContainerEvent, ContainerId, ManagedContainer, ProcessSandbox, Sandbox,
    SandboxError, SessionSpec, VolumeRef,
};
use secrecy::SecretString;
use store::{Sealer, Store};
use testkit::{FakeAnthropic, Turn};
use tokio::sync::{Notify, Semaphore};
use tracing_subscriber::fmt::MakeWriter;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("runner-sessions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Event {
    ContainerStarted(SessionId),
    ContainerStopped(SessionId),
    ProcessStarting(SessionId, CredentialKind, u32),
    TurnStarting(SessionId, u32, TurnId),
    TurnFinished(SessionId, u32, TurnId),
    ProcessStopping(SessionId, u32),
}

type Log = Arc<Mutex<Vec<Event>>>;

fn push(log: &Log, event: Event) {
    log.lock().unwrap().push(event);
}

/// Faults the next hook call of each kind makes: an error or a panic.
#[derive(Default)]
struct Faults {
    fail_turn_starting: AtomicBool,
    fail_turn_finished: AtomicBool,
    panic_turn_starting: AtomicBool,
    panic_turn_finished: AtomicBool,
    panic_process_starting: AtomicBool,
    panic_process_stopping: AtomicBool,
}

impl Faults {
    fn take(flag: &AtomicBool) -> bool {
        flag.swap(false, Ordering::SeqCst)
    }
}

struct Hooks {
    log: Log,
    script: PathBuf,
    next: AtomicU32,
    faults: Arc<Faults>,
}

#[async_trait::async_trait]
impl TurnHooks for Hooks {
    type Process = u32;
    type Finished = TurnId;

    async fn process_starting(
        &self,
        session: &Session,
        _container_ip: IpAddr,
        kind: CredentialKind,
    ) -> Result<(ProcessEnv, u32), HookError> {
        let process = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        push(&self.log, Event::ProcessStarting(session.id, kind, process));
        if Faults::take(&self.faults.panic_process_starting) {
            panic!("process_starting panicked");
        }
        let env = ProcessEnv {
            placeholder: SecretString::from(format!("placeholder-{process}")),
            env: BTreeMap::from([
                (
                    testkit::claude::SCRIPT_ENV.to_string(),
                    SecretString::from(self.script.to_str().unwrap()),
                ),
                (
                    "AGENTCTL_TOKEN".to_string(),
                    SecretString::from(format!("ctl-{process}")),
                ),
            ]),
        };
        Ok((env, process))
    }

    async fn turn_starting(
        &self,
        session: &Session,
        process: &u32,
        turn: &TurnRequest,
    ) -> Result<(), HookError> {
        push(
            &self.log,
            Event::TurnStarting(session.id, *process, turn.turn),
        );
        if Faults::take(&self.faults.panic_turn_starting) {
            panic!("turn_starting panicked");
        }
        if Faults::take(&self.faults.fail_turn_starting) {
            return Err("turn_starting refused".into());
        }
        Ok(())
    }

    async fn turn_finished(
        &self,
        session: &Session,
        process: &u32,
        turn: &TurnRequest,
    ) -> Result<TurnId, HookError> {
        push(
            &self.log,
            Event::TurnFinished(session.id, *process, turn.turn),
        );
        if Faults::take(&self.faults.panic_turn_finished) {
            panic!("turn_finished panicked");
        }
        if Faults::take(&self.faults.fail_turn_finished) {
            return Err("turn_finished failed".into());
        }
        Ok(turn.turn)
    }

    async fn process_stopping(&self, session: &Session, process: &u32) -> Result<(), HookError> {
        push(&self.log, Event::ProcessStopping(session.id, *process));
        if Faults::take(&self.faults.panic_process_stopping) {
            panic!("process_stopping panicked");
        }
        Ok(())
    }
}

/// A [`ProcessSandbox`] that logs starts and stops, counts running
/// containers, can fail stops or return from them late, and can hide deaths
/// from its event stream and then break the stream.
struct TestSandbox {
    inner: ProcessSandbox,
    log: Log,
    running: Mutex<HashSet<ContainerId>>,
    most: AtomicU32,
    sessions: Mutex<BTreeMap<ContainerId, SessionId>>,
    fail_stops: AtomicBool,
    failed_stops: AtomicU32,
    stop_delay_ms: AtomicU64,
    hide_deaths: Arc<AtomicBool>,
    break_events: Arc<Notify>,
}

impl TestSandbox {
    fn running(&self) -> usize {
        self.running.lock().unwrap().len()
    }
}

#[async_trait::async_trait]
impl Sandbox for TestSandbox {
    async fn ensure_volume(&self, key: &VolumeKey) -> sandbox::Result<VolumeRef> {
        self.inner.ensure_volume(key).await
    }

    async fn start(&self, spec: &SessionSpec) -> sandbox::Result<Container> {
        let container = self.inner.start(spec).await?;
        push(&self.log, Event::ContainerStarted(spec.session));
        let mut running = self.running.lock().unwrap();
        running.insert(container.id().clone());
        self.most
            .fetch_max(u32::try_from(running.len()).unwrap(), Ordering::SeqCst);
        self.sessions
            .lock()
            .unwrap()
            .insert(container.id().clone(), spec.session);
        Ok(container)
    }

    async fn exec(
        &self,
        container: &Container,
        argv: &[String],
        env: &BTreeMap<String, String>,
    ) -> sandbox::Result<ChildIo> {
        self.inner.exec(container, argv, env).await
    }

    async fn ip(&self, container: &ContainerId) -> sandbox::Result<IpAddr> {
        self.inner.ip(container).await
    }

    async fn stop(&self, container: &ContainerId) -> sandbox::Result<()> {
        if self.fail_stops.load(Ordering::SeqCst) {
            self.failed_stops.fetch_add(1, Ordering::SeqCst);
            return Err(SandboxError::Docker {
                op: "stop container",
                status: Some(500),
                message: None,
            });
        }
        if let Some(session) = self.sessions.lock().unwrap().get(container) {
            push(&self.log, Event::ContainerStopped(*session));
        }
        self.running.lock().unwrap().remove(container);
        self.inner.stop(container).await?;
        let delay = self.stop_delay_ms.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        Ok(())
    }

    async fn list_managed(&self) -> sandbox::Result<Vec<ManagedContainer>> {
        self.inner.list_managed().await
    }

    fn events(&self) -> BoxStream<'static, sandbox::Result<ContainerEvent>> {
        let hide = Arc::clone(&self.hide_deaths);
        let broken = Arc::clone(&self.break_events);
        let inner = self.inner.events();
        futures::stream::unfold(Some(inner), move |state| {
            let hide = Arc::clone(&hide);
            let broken = Arc::clone(&broken);
            async move {
                let mut inner = state?;
                loop {
                    tokio::select! {
                        item = inner.next() => match item {
                            Some(Ok(_)) if hide.load(Ordering::SeqCst) => continue,
                            Some(Ok(event)) => return Some((Ok(event), Some(inner))),
                            Some(Err(err)) => return Some((Err(err), None)),
                            None => return Some((Err(SandboxError::EventsMissed), None)),
                        },
                        () = broken.notified() => {
                            return Some((Err(SandboxError::EventsMissed), None));
                        }
                    }
                }
            }
        })
        .boxed()
    }
}

struct Harness {
    _dir: TempDir,
    store: Store,
    sandbox: Arc<TestSandbox>,
    anthropic: FakeAnthropic,
    log: Log,
    manager: SessionManager<Hooks>,
    agent: AgentId,
    faults: Arc<Faults>,
}

impl Harness {
    async fn new(turns: &[Turn]) -> Self {
        Self::with(turns, |_, _| {}).await
    }

    async fn with(
        turns: &[Turn],
        change: impl FnOnce(&mut ProcessConfig, &mut PoolConfig),
    ) -> Self {
        let bin = testkit::fake_claude_path();
        let dir = TempDir::new();
        let sealer = Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap();
        let store = Store::open(
            &format!("sqlite://{}", dir.0.join("agentd.db").display()),
            sealer,
        )
        .await
        .unwrap();
        let agent = AgentId::new_v4();
        runner::write_persona(&dir.0, agent, "You are a test agent.\n")
            .await
            .unwrap();
        let script = dir.0.join("script.json");
        testkit::write_script(&script, turns).unwrap();
        let anthropic = testkit::fake_anthropic().await;
        let log: Log = Arc::default();
        let sandbox = Arc::new(TestSandbox {
            inner: ProcessSandbox::new(store.clone(), dir.0.clone()).unwrap(),
            log: Arc::clone(&log),
            running: Mutex::default(),
            most: AtomicU32::new(0),
            sessions: Mutex::default(),
            fail_stops: AtomicBool::new(false),
            failed_stops: AtomicU32::new(0),
            stop_delay_ms: AtomicU64::new(0),
            hide_deaths: Arc::default(),
            break_events: Arc::new(Notify::new()),
        });
        let mut process = ProcessConfig {
            claude_bin: bin.to_str().unwrap().to_owned(),
            anthropic_base_url: anthropic.uri(),
            turn_timeout_secs: 60,
        };
        let mut pool = PoolConfig::default();
        change(&mut process, &mut pool);
        let faults = Arc::new(Faults::default());
        let hooks = Hooks {
            log: Arc::clone(&log),
            script,
            next: AtomicU32::new(0),
            faults: Arc::clone(&faults),
        };
        let manager = SessionManager::new(
            store.clone(),
            Arc::clone(&sandbox) as Arc<dyn Sandbox>,
            hooks,
            SessionConfig {
                process,
                pool,
                image: "unused".into(),
                data_dir: dir.0.clone(),
            },
        )
        .unwrap();
        Self {
            _dir: dir,
            store,
            sandbox,
            anthropic,
            log,
            manager,
            agent,
            faults,
        }
    }

    fn events(&self) -> Vec<Event> {
        self.log.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.log.lock().unwrap().clear();
    }

    fn process_starts(&self) -> usize {
        self.events()
            .iter()
            .filter(|event| matches!(event, Event::ProcessStarting(..)))
            .count()
    }

    async fn thread_session(&self, root: &str) -> Session {
        self.manager
            .lookup_or_create(self.agent, &thread(root), &channel())
            .await
            .unwrap()
    }

    async fn run(&self, session: SessionId, request: TurnRequest) -> TurnReport<TurnId> {
        self.manager.run_turn(session, request).await.unwrap()
    }

    fn transcript(&self, session: &Session) -> Vec<String> {
        let volume = VolumeKey {
            agent: self.agent,
            scope: session.scope.clone(),
        };
        let path = self
            ._dir
            .0
            .join(sandbox::volume_rel_path(&volume))
            .join("sessions")
            .join(session.id.to_string())
            .join("claude/projects")
            .join(session.id.to_string())
            .join(format!("{}.jsonl", session.id));
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|entry| entry["type"] == "user")
            .map(|entry| entry["message"]["content"].as_str().unwrap().to_owned())
            .collect()
    }
}

fn conv() -> ConvRef {
    ConvRef {
        surface: SurfaceKind::Slack,
        team: "T1".into(),
        conversation: "C1".into(),
    }
}

fn channel() -> ScopeKey {
    ScopeKey::Channel(conv())
}

fn thread(root: &str) -> ThreadKey {
    ThreadKey {
        conv: conv(),
        root: Some(MessageId::new(root)),
    }
}

fn request(message: &str) -> TurnRequest {
    TurnRequest {
        turn: TurnId::new_v4(),
        message: message.into(),
        credential: CredentialRef::Member(MemberId::new_v4()),
        model: None,
        requester: Requester {
            member: None,
            key: MemberKey {
                surface: SurfaceKind::Slack,
                team: "T1".into(),
                user: "U1".into(),
            },
        },
        hop: Hop::ZERO,
        side: Side::Public,
        kind: TurnKind::Normal,
        trigger: None,
    }
}

fn reply(report: &TurnReport<TurnId>) -> &str {
    match &report.outcome {
        TurnOutcome::Finished(result) if !result.is_error => result.result.as_deref().unwrap(),
        other => panic!("expected a reply, got {other:?}"),
    }
}

async fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'w> MakeWriter<'w> for Captured {
    type Writer = Self;

    fn make_writer(&'w self) -> Self::Writer {
        self.clone()
    }
}

fn projects_dir(h: &Harness, session: &Session) -> PathBuf {
    let volume = VolumeKey {
        agent: h.agent,
        scope: session.scope.clone(),
    };
    h._dir
        .0
        .join(sandbox::volume_rel_path(&volume))
        .join("sessions")
        .join(session.id.to_string())
        .join("claude/projects")
}

fn position(events: &[Event], wanted: &Event) -> usize {
    events
        .iter()
        .position(|event| event == wanted)
        .unwrap_or_else(|| panic!("{wanted:?} not in {events:?}"))
}

#[tokio::test]
async fn concurrent_turns_on_one_session_run_in_order() {
    let h = Harness::new(&[
        Turn::reply("r1").with_delay(Duration::from_millis(300)),
        Turn::reply("r2"),
        Turn::reply("r3"),
    ])
    .await;
    let session = h.thread_session("1.1").await;
    let (one, two, three) = (request("m1"), request("m2"), request("m3"));
    let turns = [one.turn, two.turn, three.turn];
    let (a, b, c) = tokio::join!(
        h.manager.run_turn(session.id, one),
        h.manager.run_turn(session.id, two),
        h.manager.run_turn(session.id, three),
    );
    let (a, b, c) = (a.unwrap(), b.unwrap(), c.unwrap());
    assert_eq!([reply(&a), reply(&b), reply(&c)], ["r1", "r2", "r3"]);
    assert_eq!(a.process_start, Some(SessionStart::New));
    assert_eq!(
        b.process_start, None,
        "the warm process takes the next turn"
    );
    assert_eq!(c.process_start, None);
    assert_eq!(a.finished.as_ref().unwrap(), &turns[0]);
    assert_eq!(h.transcript(&session), ["m1", "m2", "m3"]);
    let id = session.id;
    let mut expected = vec![
        Event::ContainerStarted(id),
        Event::ProcessStarting(id, CredentialKind::Subscription, 1),
    ];
    for turn in turns {
        expected.push(Event::TurnStarting(id, 1, turn));
        expected.push(Event::TurnFinished(id, 1, turn));
    }
    assert_eq!(h.events(), expected);
    let stored = h.store.session(id).await.unwrap().unwrap();
    assert!(stored.started && !stored.maybe_started);
    assert!(stored.last_turn_at.is_some());
    assert!(h.manager.is_warm(id));
}

#[tokio::test]
async fn turns_on_two_sessions_of_one_scope_run_concurrently_in_two_containers() {
    let h = Harness::new(&[Turn::reply("slow").with_delay(Duration::from_millis(1500))]).await;
    let one = h.thread_session("1.1").await;
    let two = h.thread_session("2.2").await;
    assert_ne!(one.id, two.id);
    let (a, b) = tokio::join!(
        h.manager.run_turn(one.id, request("to one")),
        h.manager.run_turn(two.id, request("to two")),
    );
    assert_eq!(reply(&a.unwrap()), "slow");
    assert_eq!(reply(&b.unwrap()), "slow");
    let events = h.events();
    let first_finish = events
        .iter()
        .position(|event| matches!(event, Event::TurnFinished(..)))
        .unwrap();
    let starts = events[..first_finish]
        .iter()
        .filter(|event| matches!(event, Event::TurnStarting(..)))
        .count();
    assert_eq!(
        starts, 2,
        "both turns started before either finished: {events:?}"
    );
    assert_eq!(h.sandbox.running(), 2);
    assert_eq!(h.sandbox.most.load(Ordering::SeqCst), 2);
    assert_eq!(h.transcript(&one), ["to one"]);
    assert_eq!(h.transcript(&two), ["to two"]);
}

#[tokio::test]
async fn the_scope_cap_queues_the_third_session() {
    let h = Harness::with(
        &[Turn::reply("slow").with_delay(Duration::from_millis(1000))],
        |_, pool| pool.scope_container_cap = 2,
    )
    .await;
    let sessions = [
        h.thread_session("1.1").await,
        h.thread_session("2.2").await,
        h.thread_session("3.3").await,
    ];
    let (a, b, c) = tokio::join!(
        h.manager.run_turn(sessions[0].id, request("a")),
        h.manager.run_turn(sessions[1].id, request("b")),
        h.manager.run_turn(sessions[2].id, request("c")),
    );
    for report in [a, b, c] {
        assert_eq!(reply(&report.unwrap()), "slow");
    }
    assert_eq!(h.sandbox.most.load(Ordering::SeqCst), 2);
    let events = h.events();
    let (third_start, third) = events
        .iter()
        .enumerate()
        .rev()
        .find_map(|(at, event)| match event {
            Event::ContainerStarted(session) => Some((at, *session)),
            _ => None,
        })
        .unwrap();
    assert!(sessions.iter().any(|session| session.id == third));
    let first_finish = events
        .iter()
        .position(|event| matches!(event, Event::TurnFinished(..)))
        .unwrap();
    assert!(
        first_finish < third_start,
        "the third session waited for a turn to end: {events:?}"
    );
    let evicted = events[..third_start]
        .iter()
        .find_map(|event| match event {
            Event::ContainerStopped(session) => Some(*session),
            _ => None,
        })
        .expect("an idle container was stopped to make room");
    assert_ne!(evicted, third);
    let stopping = events
        .iter()
        .position(|event| matches!(event, Event::ProcessStopping(s, _) if *s == evicted))
        .unwrap();
    assert!(stopping < position(&events, &Event::ContainerStopped(evicted)));
    assert!(!h.manager.is_warm(evicted));
}

#[tokio::test]
async fn the_global_cap_reaps_an_idle_container_of_another_scope() {
    let h = Harness::with(&[Turn::reply("one"), Turn::reply("two")], |_, pool| {
        pool.global_container_cap = 1;
    })
    .await;
    let first = h.thread_session("1.1").await;
    reply(&h.run(first.id, request("hi")).await);
    let dm = ThreadKey {
        conv: ConvRef {
            conversation: "D1".into(),
            ..conv()
        },
        root: None,
    };
    let second = h
        .manager
        .lookup_or_create(h.agent, &dm, &ScopeKey::Private)
        .await
        .unwrap();
    let mut owner = request("hello");
    owner.side = Side::Owner;
    reply(&h.run(second.id, owner).await);
    assert_eq!(h.sandbox.most.load(Ordering::SeqCst), 1);
    assert!(!h.manager.is_warm(first.id));
    assert!(h.manager.is_warm(second.id));
    assert_eq!(h.manager.warm_sessions(), [second.id]);
    let events = h.events();
    assert!(
        position(&events, &Event::ProcessStopping(first.id, 1))
            < position(&events, &Event::ContainerStarted(second.id))
    );
}

#[tokio::test]
async fn an_idle_reap_then_a_message_resumes_and_keeps_the_transcript() {
    let h = Harness::with(&[Turn::reply("first"), Turn::reply("second")], |_, pool| {
        pool.idle_timeout_secs = 1;
    })
    .await;
    let session = h.thread_session("1.1").await;
    let first = h.run(session.id, request("one")).await;
    assert_eq!(first.process_start, Some(SessionStart::New));
    eventually("the idle container is reaped", || {
        !h.manager.is_warm(session.id)
    })
    .await;
    assert_eq!(h.sandbox.running(), 0);
    let events = h.events();
    assert!(
        position(&events, &Event::ProcessStopping(session.id, 1))
            < position(&events, &Event::ContainerStopped(session.id))
    );
    let second = h.run(session.id, request("two")).await;
    assert_eq!(reply(&second), "second");
    assert_eq!(second.process_start, Some(SessionStart::Resume));
    assert_eq!(h.transcript(&session), ["one", "two"]);
}

#[tokio::test]
async fn a_credential_kind_change_restarts_the_process() {
    let h = Harness::new(&[Turn::reply("linked"), Turn::reply("community")]).await;
    let session = h.thread_session("1.1").await;
    let first = request("one");
    reply(&h.run(session.id, first.clone()).await);
    let mut second = request("two");
    second.credential = CredentialRef::Community;
    let report = h.run(session.id, second.clone()).await;
    assert_eq!(reply(&report), "community");
    assert_eq!(report.process_start, Some(SessionStart::Resume));
    let id = session.id;
    assert_eq!(
        h.events(),
        [
            Event::ContainerStarted(id),
            Event::ProcessStarting(id, CredentialKind::Subscription, 1),
            Event::TurnStarting(id, 1, first.turn),
            Event::TurnFinished(id, 1, first.turn),
            Event::ProcessStopping(id, 1),
            Event::ProcessStarting(id, CredentialKind::ApiKey, 2),
            Event::TurnStarting(id, 2, second.turn),
            Event::TurnFinished(id, 2, second.turn),
        ]
    );
    let requests = h.anthropic.message_requests().await;
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].headers.get("authorization").unwrap(),
        "Bearer placeholder-1"
    );
    assert!(requests[0].headers.get("x-api-key").is_none());
    assert_eq!(
        requests[1].headers.get("x-api-key").unwrap(),
        "placeholder-2"
    );
    assert!(requests[1].headers.get("authorization").is_none());
    assert_eq!(h.transcript(&session), ["one", "two"]);
}

#[tokio::test]
async fn a_model_change_restarts_the_process() {
    let h = Harness::new(&[Turn::reply("a"), Turn::reply("b"), Turn::reply("c")]).await;
    let session = h.thread_session("1.1").await;
    reply(&h.run(session.id, request("one")).await);
    let mut opus = request("two");
    opus.model = Some("claude-opus-5-5".into());
    let report = h.run(session.id, opus.clone()).await;
    assert_eq!(report.process_start, Some(SessionStart::Resume));
    let TurnOutcome::Finished(result) = &report.outcome else {
        panic!("{report:?}");
    };
    assert_eq!(result.session_id, Some(session.id));
    let same = h.run(session.id, opus).await;
    assert_eq!(same.process_start, None, "the same model keeps the process");
    let starts = h
        .events()
        .into_iter()
        .filter(|event| matches!(event, Event::ProcessStarting(..)))
        .count();
    assert_eq!(starts, 2);
    assert!(h.events().contains(&Event::ProcessStopping(session.id, 1)));
}

#[tokio::test]
async fn reset_starts_with_a_new_id() {
    let h = Harness::new(&[Turn::reply("old"), Turn::reply("new")]).await;
    let old = h.thread_session("1.1").await;
    reply(&h.run(old.id, request("one")).await);
    let new = h
        .manager
        .reset(old.id, permits())
        .await
        .unwrap()
        .expect("a replacement");
    assert_ne!(new.id, old.id);
    assert!(!h.manager.is_warm(old.id));
    let events = h.events();
    assert!(
        position(&events, &Event::ProcessStopping(old.id, 1))
            < position(&events, &Event::ContainerStopped(old.id))
    );
    let found = h.thread_session("1.1").await;
    assert_eq!(found.id, new.id);
    let report = h.run(new.id, request("fresh")).await;
    assert_eq!(report.process_start, Some(SessionStart::New));
    assert_eq!(
        reply(&report),
        "old",
        "the new session's transcript starts over"
    );
    assert_eq!(h.transcript(&new), ["fresh"]);
    assert!(matches!(
        h.manager.run_turn(old.id, request("late")).await,
        Err(RunnerError::SessionReset)
    ));
    assert!(matches!(
        h.manager
            .run_turn(SessionId::new_v4(), request("who"))
            .await,
        Err(RunnerError::UnknownSession)
    ));
    assert_eq!(h.manager.reset(old.id, permits()).await.unwrap(), None);
}

fn permits() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(1))
}

#[tokio::test]
async fn a_reset_holds_its_session_until_a_permit_is_free() {
    let h = Harness::new(&[Turn::reply("old")]).await;
    let old = h.thread_session("1.1").await;
    reply(&h.run(old.id, request("one")).await);
    let permits = Arc::new(Semaphore::new(0));
    let waiting = tokio::time::timeout(
        Duration::from_millis(100),
        h.manager.reset(old.id, Arc::clone(&permits)),
    )
    .await;
    assert!(waiting.is_err(), "the reset waits for a permit");
    assert!(!h.events().contains(&Event::ContainerStopped(old.id)));
    assert_eq!(
        h.store.session(old.id).await.unwrap().unwrap().reset_at,
        None
    );
    permits.add_permits(1);
    assert_eq!(
        h.manager.reset(old.id, permits).await.unwrap(),
        None,
        "a reset queued behind the first finds the session reset"
    );
    assert!(h.events().contains(&Event::ContainerStopped(old.id)));
    assert!(!h.manager.is_warm(old.id));
}

#[tokio::test]
async fn a_refused_resume_reruns_the_turn_with_session_id() {
    let h = Harness::new(&[Turn::reply("first"), Turn::reply("again")]).await;
    let session = h.thread_session("1.1").await;
    reply(&h.run(session.id, request("one")).await);
    h.manager.stop(session.id).await;
    assert!(!h.manager.is_warm(session.id));
    std::fs::remove_dir_all(projects_dir(&h, &session)).unwrap();
    h.clear();
    let second = request("two");
    let report = h.run(session.id, second.clone()).await;
    assert!(report.reran);
    assert_eq!(report.process_start, Some(SessionStart::New));
    assert_eq!(
        reply(&report),
        "first",
        "a new transcript plays from the start"
    );
    let id = session.id;
    assert_eq!(
        h.events(),
        [
            Event::ContainerStarted(id),
            Event::ProcessStarting(id, CredentialKind::Subscription, 2),
            Event::TurnStarting(id, 2, second.turn),
            Event::TurnFinished(id, 2, second.turn),
            Event::ProcessStopping(id, 2),
            Event::ProcessStarting(id, CredentialKind::Subscription, 3),
            Event::TurnStarting(id, 3, second.turn),
            Event::TurnFinished(id, 3, second.turn),
        ]
    );
    assert_eq!(h.transcript(&session), ["two"]);
    let stored = h.store.session(id).await.unwrap().unwrap();
    assert!(stored.started);
}

#[tokio::test]
async fn hooks_see_every_process_and_turn_in_order() {
    let h = Harness::new(&[Turn::reply("one"), Turn::crash(), Turn::reply("three")]).await;
    let session = h.thread_session("1.1").await;
    let id = session.id;
    let (one, two, three) = (request("1"), request("2"), request("3"));
    reply(&h.run(id, one.clone()).await);
    let crashed = h.run(id, two.clone()).await;
    assert!(
        matches!(crashed.outcome, TurnOutcome::Crashed { .. }),
        "{crashed:?}"
    );
    let resumed = h.run(id, three.clone()).await;
    assert_eq!(reply(&resumed), "three");
    assert_eq!(resumed.process_start, Some(SessionStart::Resume));
    h.manager.stop(id).await;
    assert_eq!(
        h.events(),
        [
            Event::ContainerStarted(id),
            Event::ProcessStarting(id, CredentialKind::Subscription, 1),
            Event::TurnStarting(id, 1, one.turn),
            Event::TurnFinished(id, 1, one.turn),
            Event::TurnStarting(id, 1, two.turn),
            Event::TurnFinished(id, 1, two.turn),
            Event::ProcessStopping(id, 1),
            Event::ProcessStarting(id, CredentialKind::Subscription, 2),
            Event::TurnStarting(id, 2, three.turn),
            Event::TurnFinished(id, 2, three.turn),
            Event::ProcessStopping(id, 2),
            Event::ContainerStopped(id),
        ]
    );
    assert_eq!(h.transcript(&session), ["1", "2", "3"]);
}

#[tokio::test]
async fn a_killed_container_has_its_process_stopped_at_once() {
    let h = Harness::new(&[Turn::reply("one"), Turn::reply("two")]).await;
    let session = h.thread_session("1.1").await;
    reply(&h.run(session.id, request("1")).await);
    let container = h.sandbox.inner.list_managed().await.unwrap()[0].id.clone();
    h.sandbox.inner.stop(&container).await.unwrap();
    eventually("process_stopping after the death", || {
        h.events().contains(&Event::ProcessStopping(session.id, 1))
    })
    .await;
    eventually("the dead container is let go", || {
        !h.manager.is_warm(session.id)
    })
    .await;
    let report = h.run(session.id, request("2")).await;
    assert_eq!(reply(&report), "two");
    assert_eq!(report.process_start, Some(SessionStart::Resume));
    let stops = h
        .events()
        .iter()
        .filter(|event| matches!(event, Event::ProcessStopping(_, 1)))
        .count();
    assert!(stops >= 1);
}

#[tokio::test]
async fn deaths_missed_while_the_event_stream_broke_are_found_by_listing() {
    let h = Harness::new(&[Turn::reply("one")]).await;
    let session = h.thread_session("1.1").await;
    reply(&h.run(session.id, request("1")).await);
    h.sandbox.hide_deaths.store(true, Ordering::SeqCst);
    let container = h.sandbox.inner.list_managed().await.unwrap()[0].id.clone();
    h.sandbox.inner.stop(&container).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !h.events().contains(&Event::ProcessStopping(session.id, 1)),
        "the death was hidden"
    );
    h.sandbox.hide_deaths.store(false, Ordering::SeqCst);
    h.sandbox.break_events.notify_waiters();
    eventually("the listing finds the dead container", || {
        h.events().contains(&Event::ProcessStopping(session.id, 1))
    })
    .await;
    eventually("the dead container is let go", || {
        !h.manager.is_warm(session.id)
    })
    .await;
}

#[tokio::test]
async fn a_timeout_stops_the_process_and_the_next_turn_resumes() {
    let h = Harness::with(
        &[
            Turn::reply("late").with_delay(Duration::from_secs(30)),
            Turn::reply("resumed"),
        ],
        |process, _| process.turn_timeout_secs = 1,
    )
    .await;
    let session = h.thread_session("1.1").await;
    let timed_out = h.run(session.id, request("1")).await;
    assert!(matches!(timed_out.outcome, TurnOutcome::TimedOut { .. }));
    assert!(h.events().contains(&Event::ProcessStopping(session.id, 1)));
    let stored = h.store.session(session.id).await.unwrap().unwrap();
    assert!(stored.started, "the CLI had read the message");
    let report = h.run(session.id, request("2")).await;
    assert_eq!(reply(&report), "resumed");
    assert_eq!(report.process_start, Some(SessionStart::Resume));
}

#[tokio::test]
async fn a_failed_turn_starting_skips_the_turn_but_still_finishes_it() {
    let h = Harness::new(&[Turn::reply("one")]).await;
    let session = h.thread_session("1.1").await;
    h.faults.fail_turn_starting.store(true, Ordering::SeqCst);
    let refused = request("never sent");
    let err = h
        .manager
        .run_turn(session.id, refused.clone())
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RunnerError::Hook {
                hook: "turn_starting",
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(
        h.events()[2..],
        [
            Event::TurnStarting(session.id, 1, refused.turn),
            Event::TurnFinished(session.id, 1, refused.turn),
        ]
    );
    assert!(h.transcript(&session).is_empty());
    let stored = h.store.session(session.id).await.unwrap().unwrap();
    assert!(!stored.started && !stored.maybe_started);
    let report = h.run(session.id, request("sent")).await;
    assert_eq!(reply(&report), "one");
    assert_eq!(report.process_start, None, "the process was kept");
}

#[tokio::test]
async fn a_failed_turn_finished_stops_the_process() {
    let h = Harness::new(&[Turn::reply("one"), Turn::reply("two")]).await;
    let session = h.thread_session("1.1").await;
    h.faults.fail_turn_finished.store(true, Ordering::SeqCst);
    let report = h.run(session.id, request("1")).await;
    assert_eq!(reply(&report), "one");
    assert!(report.finished.is_err());
    assert!(h.events().contains(&Event::ProcessStopping(session.id, 1)));
    let next = h.run(session.id, request("2")).await;
    assert_eq!(next.process_start, Some(SessionStart::Resume));
    assert_eq!(reply(&next), "two");
}

#[tokio::test]
async fn a_side_change_on_the_private_volume_restarts_the_container() {
    let h = Harness::new(&[Turn::reply("owner"), Turn::reply("public")]).await;
    let consent = ConsentId::new_v4();
    let private = h
        .manager
        .create_private(h.agent, consent, &thread("1.1"))
        .await
        .unwrap();
    assert_eq!(private.scope, ScopeKey::Private);
    assert!(matches!(
        h.manager.run_turn(private.id, request("normal")).await,
        Err(RunnerError::InvalidRequest(_))
    ));
    let mut owner = request("1");
    owner.kind = TurnKind::PrivateTask(consent);
    owner.side = Side::Owner;
    reply(&h.run(private.id, owner.clone()).await);
    let mut public = owner.clone();
    public.turn = TurnId::new_v4();
    public.message = "2".into();
    public.side = Side::Public;
    let report = h.run(private.id, public).await;
    assert_eq!(reply(&report), "public");
    assert_eq!(report.process_start, Some(SessionStart::Resume));
    let starts = h
        .events()
        .into_iter()
        .filter(|event| matches!(event, Event::ContainerStarted(_)))
        .count();
    assert_eq!(starts, 2, "the mounts changed, so the container did");
    assert_eq!(h.sandbox.running(), 1);
    let other = h
        .manager
        .create_private(h.agent, consent, &thread("1.1"))
        .await
        .unwrap();
    assert_ne!(other.id, private.id);
}

#[tokio::test]
async fn two_dm_lookups_create_one_session_and_a_scope_change_replaces_it() {
    let h = Harness::new(&[Turn::reply("one")]).await;
    let dm = ThreadKey {
        conv: ConvRef {
            conversation: "D1".into(),
            ..conv()
        },
        root: None,
    };
    let (a, b) = tokio::join!(
        h.manager.lookup_or_create(h.agent, &dm, &ScopeKey::Private),
        h.manager.lookup_or_create(h.agent, &dm, &ScopeKey::Private),
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a.id, b.id);
    let mut owner = request("hi");
    owner.side = Side::Owner;
    reply(&h.run(a.id, owner).await);
    let public = ScopeKey::Dm(dm.conv.clone());
    let replaced = h
        .manager
        .lookup_or_create(h.agent, &dm, &public)
        .await
        .unwrap();
    assert_ne!(replaced.id, a.id);
    assert_eq!(replaced.scope, public);
    eventually("the old session's container stops", || {
        !h.manager.is_warm(a.id)
    })
    .await;
    assert!(matches!(
        h.manager.run_turn(a.id, request("late")).await,
        Err(RunnerError::SessionReset)
    ));
}

#[tokio::test]
async fn a_caller_that_stops_waiting_does_not_cut_the_turn_short() {
    let h = Harness::new(&[
        Turn::reply("slow").with_delay(Duration::from_millis(800)),
        Turn::reply("next"),
    ])
    .await;
    let session = h.thread_session("1.1").await;
    let abandoned = request("1");
    let waited = tokio::time::timeout(
        Duration::from_millis(200),
        h.manager.run_turn(session.id, abandoned.clone()),
    )
    .await;
    assert!(waited.is_err());
    let next = request("2");
    let report = h.run(session.id, next.clone()).await;
    assert_eq!(reply(&report), "next");
    assert_eq!(report.process_start, None);
    let events = h.events();
    assert!(
        position(&events, &Event::TurnFinished(session.id, 1, abandoned.turn))
            < position(&events, &Event::TurnStarting(session.id, 1, next.turn))
    );
    assert_eq!(h.transcript(&session), ["1", "2"]);
}

#[tokio::test]
async fn a_turn_cut_off_before_its_outcome_was_recorded_resumes_first() {
    let h = Harness::new(&[Turn::reply("one"), Turn::reply("two"), Turn::reply("three")]).await;
    let unknown = h.thread_session("1.1").await;
    assert!(h.store.mark_session_turn_pending(unknown.id).await.unwrap());
    let report = h.run(unknown.id, request("a")).await;
    assert_eq!(reply(&report), "one");
    assert!(report.reran, "no transcript: the --resume is refused");
    assert_eq!(report.process_start, Some(SessionStart::New));

    let read = h.thread_session("2.2").await;
    reply(&h.run(read.id, request("b")).await);
    h.manager.stop(read.id).await;
    h.store.mark_session_unstarted(read.id).await.unwrap();
    h.store.mark_session_turn_pending(read.id).await.unwrap();
    let report = h.run(read.id, request("c")).await;
    assert_eq!(reply(&report), "two");
    assert!(!report.reran, "the transcript exists: the --resume works");
    assert_eq!(report.process_start, Some(SessionStart::Resume));
    assert_eq!(h.transcript(&read), ["b", "c"]);
}

#[tokio::test]
async fn a_panic_in_turn_starting_still_finishes_the_turn_and_stops_the_process() {
    let h = Harness::new(&[Turn::reply("one")]).await;
    let session = h.thread_session("1.1").await;
    let id = session.id;
    h.faults.panic_turn_starting.store(true, Ordering::SeqCst);
    let panicked = request("never sent");
    let err = h.manager.run_turn(id, panicked.clone()).await.unwrap_err();
    assert!(matches!(err, RunnerError::TurnTask), "{err}");
    assert_eq!(
        h.events(),
        [
            Event::ContainerStarted(id),
            Event::ProcessStarting(id, CredentialKind::Subscription, 1),
            Event::TurnStarting(id, 1, panicked.turn),
            Event::TurnFinished(id, 1, panicked.turn),
            Event::ProcessStopping(id, 1),
        ]
    );
    assert!(h.transcript(&session).is_empty());
    let stored = h.store.session(id).await.unwrap().unwrap();
    assert!(!stored.started && !stored.maybe_started);
    let report = h.run(id, request("sent")).await;
    assert_eq!(reply(&report), "one");
    assert_eq!(
        report.process_start,
        Some(SessionStart::New),
        "the process was not reused"
    );
}

#[tokio::test]
async fn a_panic_in_turn_finished_records_the_turn_and_stops_the_process() {
    let h = Harness::new(&[Turn::reply("one"), Turn::reply("two")]).await;
    let session = h.thread_session("1.1").await;
    let id = session.id;
    h.faults.panic_turn_finished.store(true, Ordering::SeqCst);
    let first = request("1");
    let err = h.manager.run_turn(id, first.clone()).await.unwrap_err();
    assert!(matches!(err, RunnerError::TurnTask), "{err}");
    assert_eq!(
        h.events(),
        [
            Event::ContainerStarted(id),
            Event::ProcessStarting(id, CredentialKind::Subscription, 1),
            Event::TurnStarting(id, 1, first.turn),
            Event::TurnFinished(id, 1, first.turn),
            Event::ProcessStopping(id, 1),
        ]
    );
    let stored = h.store.session(id).await.unwrap().unwrap();
    assert!(stored.started && !stored.maybe_started, "{stored:?}");
    let next = h.run(id, request("2")).await;
    assert_eq!(reply(&next), "two");
    assert_eq!(next.process_start, Some(SessionStart::Resume));
    assert_eq!(h.transcript(&session), ["1", "2"]);
}

#[tokio::test]
async fn a_panicked_turn_wakes_a_session_waiting_for_its_container() {
    let h = Harness::with(
        &[Turn::reply("slow").with_delay(Duration::from_millis(600))],
        |_, pool| pool.global_container_cap = 1,
    )
    .await;
    let first = h.thread_session("1.1").await;
    let second = h.thread_session("2.2").await;
    h.faults.panic_turn_finished.store(true, Ordering::SeqCst);
    let waiting = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        h.manager.run_turn(second.id, request("b")).await
    };
    let (panicked, waited) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(h.manager.run_turn(first.id, request("a")), waiting)
    })
    .await
    .expect("the waiting session was woken when the panicked turn let go");
    assert!(matches!(panicked, Err(RunnerError::TurnTask)));
    assert_eq!(reply(&waited.unwrap()), "slow");
    assert_eq!(h.sandbox.most.load(Ordering::SeqCst), 1);
    assert!(!h.manager.is_warm(first.id));
    assert!(h.manager.is_warm(second.id));
}

#[tokio::test]
async fn a_container_that_fails_to_stop_keeps_its_places_until_the_reaper_stops_it() {
    let h = Harness::with(&[Turn::reply("one"), Turn::reply("two")], |_, pool| {
        pool.global_container_cap = 1;
        pool.idle_timeout_secs = 1;
    })
    .await;
    let first = h.thread_session("1.1").await;
    let second = h.thread_session("2.2").await;
    h.sandbox.fail_stops.store(true, Ordering::SeqCst);
    reply(&h.run(first.id, request("1")).await);
    h.manager.stop(first.id).await;
    assert!(h.events().contains(&Event::ProcessStopping(first.id, 1)));
    assert!(
        h.manager.is_warm(first.id),
        "the session still holds the container"
    );
    assert_eq!(h.sandbox.running(), 1);
    let err = h
        .manager
        .run_turn(first.id, request("2"))
        .await
        .unwrap_err();
    assert!(matches!(err, RunnerError::Sandbox(_)), "{err}");
    assert_eq!(
        h.process_starts(),
        1,
        "no second process resumes the transcript"
    );
    let waiting = h.manager.run_turn(second.id, request("b"));
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(Duration::from_millis(800), &mut waiting)
            .await
            .is_err(),
        "the global cap still counts the container"
    );
    assert_eq!(h.sandbox.most.load(Ordering::SeqCst), 1);
    assert!(
        h.sandbox.failed_stops.load(Ordering::SeqCst) >= 4,
        "the reaper kept trying"
    );
    h.sandbox.fail_stops.store(false, Ordering::SeqCst);
    let report = tokio::time::timeout(Duration::from_secs(20), waiting)
        .await
        .expect("the reaper stopped the container")
        .unwrap();
    assert_eq!(reply(&report), "one");
    assert!(!h.manager.is_warm(first.id));
    assert_eq!(h.sandbox.most.load(Ordering::SeqCst), 1);
    assert_eq!(h.transcript(&first), ["1"]);
}

#[tokio::test]
async fn a_refusal_shaped_result_from_a_new_process_is_not_a_refused_resume() {
    let dir = TempDir::new();
    let refusing = dir.0.join("refusing-claude");
    std::fs::write(
        &refusing,
        "#!/bin/sh\nread -r line\necho '{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true}'\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&refusing, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bin = refusing.to_str().unwrap().to_owned();
    let h = Harness::with(&[], |process, _| process.claude_bin = bin).await;
    let session = h.thread_session("1.1").await;
    let report = h.run(session.id, request("hi")).await;
    assert!(report.outcome.resume_refused(), "{report:?}");
    assert_eq!(report.process_start, Some(SessionStart::New));
    assert!(!report.reran, "a --session-id start refuses no --resume");
    assert_eq!(h.process_starts(), 1);
    let stored = h.store.session(session.id).await.unwrap().unwrap();
    assert!(
        !stored.started && stored.maybe_started,
        "whether the CLI read the message is still unknown: {stored:?}"
    );
}

#[tokio::test]
async fn a_refused_resume_is_caught_on_the_first_turn_sent_to_a_resumed_process() {
    let h = Harness::new(&[Turn::reply("first"), Turn::reply("again")]).await;
    let session = h.thread_session("1.1").await;
    reply(&h.run(session.id, request("one")).await);
    h.manager.stop(session.id).await;
    std::fs::remove_dir_all(projects_dir(&h, &session)).unwrap();
    h.faults.fail_turn_starting.store(true, Ordering::SeqCst);
    let err = h
        .manager
        .run_turn(session.id, request("never sent"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RunnerError::Hook {
                hook: "turn_starting",
                ..
            }
        ),
        "{err}"
    );
    assert!(
        h.manager.is_warm(session.id),
        "the resumed process was kept"
    );
    let report = h.run(session.id, request("two")).await;
    assert!(report.reran, "{report:?}");
    assert_eq!(report.process_start, Some(SessionStart::New));
    assert_eq!(reply(&report), "first");
    assert_eq!(h.process_starts(), 3);
    assert_eq!(h.transcript(&session), ["two"]);
    let stored = h.store.session(session.id).await.unwrap().unwrap();
    assert!(stored.started && !stored.maybe_started, "{stored:?}");
}

#[tokio::test]
async fn a_normal_stop_is_not_logged_as_a_death() {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(captured.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let h = Harness::new(&[Turn::reply("one")]).await;
    let session = h.thread_session("1.1").await;
    reply(&h.run(session.id, request("1")).await);
    h.sandbox.stop_delay_ms.store(300, Ordering::SeqCst);
    h.manager.stop(session.id).await;
    assert!(!h.manager.is_warm(session.id));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let logs = captured.text();
    assert!(logs.contains("stopped a session container"), "{logs}");
    assert!(!logs.contains("a session container died"), "{logs}");
    let stops = h
        .events()
        .iter()
        .filter(|event| matches!(event, Event::ProcessStopping(..)))
        .count();
    assert_eq!(stops, 1, "{:?}", h.events());
}

#[tokio::test]
async fn a_panicking_process_stopping_still_stops_the_process() {
    let h = Harness::new(&[Turn::reply("one"), Turn::reply("two")]).await;
    let session = h.thread_session("1.1").await;
    let id = session.id;
    reply(&h.run(id, request("1")).await);
    h.faults
        .panic_process_stopping
        .store(true, Ordering::SeqCst);
    let mut community = request("2");
    community.credential = CredentialRef::Community;
    let report = h.run(id, community).await;
    assert_eq!(reply(&report), "two");
    assert_eq!(report.process_start, Some(SessionStart::Resume));
    let events = h.events();
    assert!(
        position(&events, &Event::ProcessStopping(id, 1))
            < position(
                &events,
                &Event::ProcessStarting(id, CredentialKind::ApiKey, 2)
            ),
        "{events:?}"
    );
    assert_eq!(h.transcript(&session), ["1", "2"]);
    assert_eq!(h.sandbox.running(), 1);
}

#[tokio::test]
async fn a_panicking_process_starting_fails_the_turn_and_stops_the_container() {
    let h = Harness::new(&[Turn::reply("one")]).await;
    let session = h.thread_session("1.1").await;
    h.faults
        .panic_process_starting
        .store(true, Ordering::SeqCst);
    let err = h
        .manager
        .run_turn(session.id, request("never sent"))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            RunnerError::Hook {
                hook: "process_starting",
                ..
            }
        ),
        "{err}"
    );
    assert!(!h.manager.is_warm(session.id));
    assert_eq!(h.sandbox.running(), 0);
    let report = h.run(session.id, request("sent")).await;
    assert_eq!(reply(&report), "one");
    assert_eq!(report.process_start, Some(SessionStart::New));
}

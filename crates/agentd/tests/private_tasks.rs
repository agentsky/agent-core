//! Private tasks end to end: a channel turn in `fake-claude` asks for one
//! with the real `agentctl private`, the owner decides it (or doesn't),
//! and the task runs in a fresh session of its own in a process sandbox,
//! with the real credential proxy and agentctl API on agentd's listeners
//! and `fake_anthropic()` upstream.

mod common;

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd::commands::{ManagerBot, OpenDm, Replies};
use agentd::consents::Decided;
use agentd::ctl::SurfaceLookup;
use agentd::pipeline::{Pipeline, PipelineSettings, TurnSettings, Turns};
use agentd::server::{Routers, Server};
use agentd::{App, Config};
use core_types::{
    AgentId, BindingId, ConsentId, ConvKind, ConvRef, InboundEvent, MemberId, MemberKey, MessageId,
    Msg, MsgRef, ReplyTarget, ScopeKey, SessionId, Surface, SurfaceError, SurfaceKind, ThreadKey,
    UserId, VolumeKey,
};
use futures::stream::BoxStream;
use runner::{PoolConfig, ProcessConfig};
use sandbox::{
    ChildIo, Container, ContainerEvent, ContainerId, ManagedContainer, ProcessSandbox, Sandbox,
    SessionSpec, SharedAccess, VolumeRef,
};
use secrecy::SecretString;
use store::{
    AgentCreation, ConsentState, NewAgent, NewClaudeLink, SessionKind, Store, StoreError,
    Visibility,
};
use testkit::{
    Call, FakeAnthropic, MockSurface, Turn, agentctl_path, fake_anthropic, fake_claude_path,
};
use time::OffsetDateTime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use common::{TempDir, env};

const TEAM: &str = "chat.example";
const BOT: &str = "UBOT";
const TASK: &str = "Summarize in.txt";
const WAIT: Duration = Duration::from_secs(60);

/// Every agent's bot acts through the one mock. While `failing` is above
/// 0, a lookup fails as the store would, and counts it down.
#[derive(Debug)]
struct Mocks {
    mock: Arc<MockSurface>,
    failing: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl SurfaceLookup for Mocks {
    async fn surface(
        &self,
        _agent: AgentId,
        _conv: &ConvRef,
    ) -> Result<Option<Arc<dyn Surface>>, StoreError> {
        let mut left = self.failing.load(Ordering::SeqCst);
        let failed = loop {
            let Some(next) = left.checked_sub(1) else {
                break false;
            };
            match self
                .failing
                .compare_exchange(left, next, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => break true,
                Err(now) => left = now,
            }
        };
        if failed {
            return Err(StoreError::Corrupt {
                table: "agent_bindings",
                column: "state",
            });
        }
        Ok(Some(self.mock.clone()))
    }
}

/// The manager bot's DM with a member is `dm-<user>`.
struct Dms;

#[async_trait::async_trait]
impl OpenDm for Dms {
    async fn open_dm(
        &self,
        member: &MemberKey,
    ) -> Result<core_types::ConversationId, SurfaceError> {
        Ok(format!("dm-{}", member.user.as_str()).into())
    }
}

/// What a container was started with.
#[derive(Debug, Clone)]
struct Started {
    session: SessionId,
    volume: VolumeKey,
    volume_dir: PathBuf,
    shared: SharedAccess,
    memory: bool,
}

/// A [`ProcessSandbox`] that records every container's mounts.
struct Recording {
    inner: ProcessSandbox,
    started: Mutex<Vec<Started>>,
    stop_delay: Mutex<Duration>,
    stops: AtomicUsize,
}

#[async_trait::async_trait]
impl Sandbox for Recording {
    async fn ensure_volume(&self, key: &VolumeKey) -> sandbox::Result<VolumeRef> {
        self.inner.ensure_volume(key).await
    }

    async fn start(&self, spec: &SessionSpec) -> sandbox::Result<Container> {
        self.started.lock().unwrap().push(Started {
            session: spec.session,
            volume: spec.volume.key().clone(),
            volume_dir: spec.volume.path().to_owned(),
            shared: spec.shared,
            memory: spec.memory,
        });
        self.inner.start(spec).await
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
        self.stops.fetch_add(1, Ordering::SeqCst);
        let delay = *self.stop_delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        self.inner.stop(container).await
    }

    async fn list_managed(&self) -> sandbox::Result<Vec<ManagedContainer>> {
        self.inner.list_managed().await
    }

    fn events(&self) -> BoxStream<'static, sandbox::Result<ContainerEvent>> {
        self.inner.events()
    }
}

struct Stack {
    app: App,
    turns: Turns,
    pipeline: Pipeline,
    mock: Arc<MockSurface>,
    manager: Arc<MockSurface>,
    sandbox: Arc<Recording>,
    script: PathBuf,
    data: PathBuf,
    agent: AgentId,
    binding: BindingId,
    alice: MemberId,
    failing: Arc<AtomicUsize>,
    stop: oneshot::Sender<()>,
    abort: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
    fake: FakeAnthropic,
    _dir: TempDir,
}

fn key(user: &str) -> MemberKey {
    MemberKey {
        surface: SurfaceKind::RocketChat,
        team: TEAM.into(),
        user: user.into(),
    }
}

fn conv(id: &str) -> ConvRef {
    ConvRef {
        surface: SurfaceKind::RocketChat,
        team: TEAM.into(),
        conversation: id.into(),
    }
}

fn msg(conv_id: &str, id: &str) -> MsgRef {
    MsgRef {
        conv: conv(conv_id),
        id: id.into(),
    }
}

fn thread(root: &str) -> ThreadKey {
    ThreadKey {
        conv: conv("GENERAL"),
        root: Some(root.into()),
    }
}

async fn link(store: &Store, user: &str) -> MemberId {
    let now = OffsetDateTime::now_utc();
    let member = store.ensure_member(&key(user), user, now).await.unwrap();
    store
        .put_claude_link(
            member,
            &NewClaudeLink {
                access_token: SecretString::from(format!("token-of-{user}")),
                refresh_token: SecretString::from("refresh"),
                expires_at: now + Duration::from_secs(24 * 60 * 60),
                plan: None,
                rate_limit_tier: None,
            },
            now,
        )
        .await
        .unwrap();
    member
}

/// A channel turn's script, for every session's first turn: in a channel
/// session, whose work directory has no `in.txt`, write one and ask for a
/// private task handed it, attaching what `agentctl private` printed; in
/// the private task's session, where `in.txt` was handed over, run
/// `private`.
fn script(private: &str) -> Turn {
    Turn::reply("Done.").with_command([
        "sh",
        "-c",
        &format!(
            "if [ -f in.txt ]; then {private}; else printf data > in.txt && \
             agentctl private --file in.txt '{TASK}' > consent.txt && \
             agentctl attach consent.txt; fi"
        ),
    ])
}

/// What the private task does unless a test says otherwise: waits a
/// moment, then attaches a copy of the file it was handed.
const SEEN: &str = "sleep 1; cat in.txt > seen.txt && agentctl attach seen.txt";

/// agentd with its listeners and a runner over a recording process
/// sandbox running `fake-claude`, alice and bob linked, and alice's agent
/// `helper`, whose bot is `UBOT`. `limits` goes in `[limits]`.
async fn start(limits: &str) -> Stack {
    start_with_drain(limits, 5).await
}

/// [`start`], with a drain timeout of `drain_timeout_secs`.
async fn start_with_drain(limits: &str, drain_timeout_secs: u64) -> Stack {
    let claude = fake_claude_path();
    let agentctl = agentctl_path();
    let dir = TempDir::new();
    let fake = fake_anthropic().await;
    let text = format!(
        "{}\n[proxy]\nupstream = \"{}\"\n[limits]\n{limits}\n",
        common::CONFIG
            .replace("/nonexistent/agentd", &dir.path().display().to_string())
            .replace(
                "sqlite::memory:",
                &format!("sqlite://{}", dir.path().join("agentd.db").display())
            )
            .replace(
                "drain_timeout_secs = 5",
                &format!("drain_timeout_secs = {drain_timeout_secs}")
            ),
        fake.uri()
    );
    let config = Config::parse(&text, env()).unwrap();
    let store = agentd::app::open_store(&config).await.unwrap();
    let mock = Arc::new(MockSurface::new());
    let failing = Arc::new(AtomicUsize::new(0));
    let lookup = Mocks {
        mock: mock.clone(),
        failing: failing.clone(),
    };
    let app = App::with_surfaces(config, store.clone(), None, Arc::new(lookup)).unwrap();
    let alice = link(&store, "alice").await;
    link(&store, "bob").await;
    let team = TEAM.into();
    let AgentCreation::Created(agent, binding) = store
        .create_agent(
            &NewAgent {
                owner: alice,
                name: "helper",
                persona: "You are helper.",
                visibility: Visibility::Public,
                surface: SurfaceKind::RocketChat,
                team: &team,
            },
            10,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap()
    else {
        panic!("the agent was created");
    };
    store
        .set_binding_bot_user(binding, &UserId::new(BOT), "helper")
        .await
        .unwrap();
    store
        .activate_binding(
            binding,
            &SecretString::from("bot-token"),
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();

    let server = Server::bind(app.clone(), Routers::new(&app).unwrap())
        .await
        .unwrap();
    let addrs = server.addrs();
    let script = dir.path().join("script.json");
    let path = format!("{}:/usr/bin:/bin", agentctl.parent().unwrap().display());
    let mut vars = BTreeMap::from([
        (
            testkit::claude::SCRIPT_ENV.to_owned(),
            script.display().to_string(),
        ),
        ("PATH".to_owned(), path),
    ]);
    for name in ["NO_PROXY", "no_proxy"] {
        vars.insert(name.to_owned(), addrs.proxy.ip().to_string());
    }
    let settings = TurnSettings {
        process: ProcessConfig {
            claude_bin: claude.display().to_string(),
            anthropic_base_url: format!("http://{}", addrs.proxy),
            turn_timeout_secs: 60,
        },
        pool: PoolConfig {
            global_container_cap: 1,
            ..PoolConfig::default()
        },
        image: "unused".to_owned(),
        data_dir: dir.path().to_owned(),
        agentctl_url: format!("http://{}", addrs.ctl),
        env: vars,
    };
    let sandbox = Arc::new(Recording {
        inner: ProcessSandbox::new(store.clone(), dir.path()).unwrap(),
        started: Mutex::default(),
        stop_delay: Mutex::default(),
        stops: AtomicUsize::default(),
    });
    let turns = Turns::start(&app, sandbox.clone(), settings).unwrap();
    let manager = Arc::new(MockSurface::new());
    let replies = Replies::new(Some(Arc::new(ManagerBot::new(
        key("manager"),
        manager.clone(),
        Arc::new(Dms),
    ))));
    let pipeline = Pipeline::new(
        store.clone(),
        turns.clone(),
        Arc::clone(app.surfaces()),
        replies,
        PipelineSettings::from_app(&app),
    );
    let server = server.with_pipeline(pipeline.clone());
    let (stop, stopped) = oneshot::channel::<()>();
    let (abort, aborted) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(
        async {
            let _ = stopped.await;
        },
        async {
            if aborted.await.is_err() {
                std::future::pending::<()>().await;
            }
        },
    ));
    Stack {
        app,
        turns,
        pipeline,
        mock,
        manager,
        sandbox,
        script,
        data: dir.path().to_owned(),
        agent: agent.id,
        binding,
        alice,
        failing,
        stop,
        abort,
        task,
        fake,
        _dir: dir,
    }
}

impl Stack {
    fn store(&self) -> &Store {
        self.app.store()
    }

    /// Makes every session's next turn play `turn`.
    fn next_turn(&self, turn: Turn) {
        testkit::write_script(&self.script, &vec![turn; 8]).unwrap();
    }

    /// `sender`'s message `id` mentioning the agent in GENERAL, in the
    /// thread of `root` if given.
    fn mention(&self, sender: &str, id: &str, root: Option<&str>) -> InboundEvent {
        InboundEvent {
            event_id: id.to_owned(),
            binding: self.binding,
            sender: key(sender),
            sender_is_bot: false,
            sender_bot_user: None,
            conv: conv("GENERAL"),
            conv_kind: ConvKind::Channel,
            thread_root: root.map(MessageId::new),
            message: msg("GENERAL", id),
            text: format!("@{BOT} hello from the channel"),
            mentions: vec![UserId::new(BOT)],
            reply_to: root.map(|root| msg("GENERAL", root)),
            files: vec![],
            received_at: OffsetDateTime::now_utc(),
        }
    }

    /// The owner alice's message `id` in her own one-to-one DM with the
    /// agent.
    fn in_dm(&self, id: &str) -> InboundEvent {
        InboundEvent {
            conv: conv("DM-ALICE"),
            conv_kind: ConvKind::Dm,
            message: msg("DM-ALICE", id),
            text: "hello in my DM".to_owned(),
            mentions: vec![],
            ..self.mention("alice", id, None)
        }
    }

    /// A connection of its own to the stack's database, to change what
    /// the store's API can't.
    async fn db(&self) -> sqlx::SqlitePool {
        sqlx::SqlitePool::connect(&format!(
            "sqlite://{}",
            self.data.join("agentd.db").display()
        ))
        .await
        .unwrap()
    }

    /// Approves `consent` as its agent's owner, alice.
    async fn approve(&self, consent: ConsentId) {
        self.app
            .ctl()
            .consents()
            .decide(&key("alice"), consent, true)
            .await
            .unwrap();
    }

    /// Has `sender` ask the agent, in a new thread rooted at `id`, for the
    /// private task [`script`] asks for, with the task's session doing
    /// `private`, and returns the consent the channel turn got.
    async fn ask(&self, sender: &str, id: &str, private: &str) -> ConsentId {
        self.ask_with(self.mention(sender, id, None), private).await
    }

    /// [`ask`](Self::ask), with `event` as the message that asks.
    async fn ask_with(&self, event: InboundEvent, private: &str) -> ConsentId {
        self.next_turn(script(private));
        let before = self.mock.calls().len();
        self.pipeline.handle(event, MockSurface::DEFAULT_CAPS).await;
        let calls = self.mock.calls().split_off(before);
        let printed = uploads(&calls)
            .into_iter()
            .find(|(_, name, _)| name == "consent.txt")
            .map(|(_, _, contents)| contents)
            .unwrap_or_else(|| panic!("the channel turn attached its consent: {calls:#?}"));
        let id = printed
            .strip_prefix("Asked for consent ")
            .and_then(|rest| rest.split('.').next())
            .unwrap_or_else(|| panic!("{printed}"));
        id.parse().unwrap()
    }

    /// Waits until the agent's bot posted a message containing `text`
    /// after call number `from`, and returns it.
    async fn posted(&self, from: usize, text: &str) -> (ReplyTarget, String, MsgRef) {
        let started = Instant::now();
        loop {
            let calls = self.mock.calls().split_off(from);
            if let Some(found) = posts(&calls)
                .into_iter()
                .find(|(_, posted, _)| posted.contains(text))
            {
                return found;
            }
            assert!(
                started.elapsed() < WAIT,
                "never posted {text:?}: {calls:#?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Approves consent `id`, waits until its task's turn reached the
    /// model, and returns its session. A turn cut before then ends in an
    /// error, not a crash, and isn't billed.
    async fn run_task(&self, id: ConsentId) -> SessionId {
        let upstream = self.fake.message_requests().await.len();
        self.approve(id).await;
        let started = Instant::now();
        loop {
            if let Some(session) = self
                .store()
                .consent(id)
                .await
                .unwrap()
                .unwrap()
                .private_session
                && self.turns.sessions().is_warm(session)
                && self.fake.message_requests().await.len() > upstream
            {
                return session;
            }
            assert!(
                started.elapsed() < WAIT,
                "consent {id}'s task never reached the model"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until consent `id`'s work is done.
    async fn finished(&self, id: ConsentId) -> store::Consent {
        let started = Instant::now();
        loop {
            let consent = self.store().consent(id).await.unwrap().unwrap();
            if consent.finished_at.is_some() {
                return consent;
            }
            assert!(started.elapsed() < WAIT, "consent {id} never finished");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits for the manager bot's first message to `user`, a card, and
    /// returns it.
    async fn card_to(&self, user: &str) -> String {
        let started = Instant::now();
        loop {
            if let Some(card) = self.dms_to(user).into_iter().next() {
                return card;
            }
            assert!(started.elapsed() < WAIT, "{user} never got a card");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits until consent `id`'s card is recorded as posted, which comes
    /// after the post itself, and returns where it was posted.
    async fn recorded_card(&self, id: ConsentId) -> MsgRef {
        let started = Instant::now();
        loop {
            if let Some(card) = self.store().consent(id).await.unwrap().unwrap().card {
                return card;
            }
            assert!(
                started.elapsed() < WAIT,
                "consent {id}'s card was never recorded"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Every text the manager bot sent `user` in their DM.
    fn dms_to(&self, user: &str) -> Vec<String> {
        let dm = conv(&format!("dm-{user}"));
        self.manager
            .posts()
            .into_iter()
            .filter(|(to, _)| to.conv == dm)
            .map(|(_, text)| text)
            .collect()
    }

    /// The containers started for `session`.
    fn started(&self, session: SessionId) -> Vec<Started> {
        self.sandbox
            .started
            .lock()
            .unwrap()
            .iter()
            .filter(|started| started.session == session)
            .cloned()
            .collect()
    }

    /// The upstream request bodies of every turn so far.
    async fn upstream_bodies(&self) -> Vec<String> {
        self.fake
            .message_requests()
            .await
            .iter()
            .map(|request| String::from_utf8_lossy(&request.body).into_owned())
            .collect()
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        self.task.await.unwrap().unwrap();
    }

    /// Shuts agentd down and, while the shutdown is still under way,
    /// forces it with a second signal: at once, or, `once_killing`, once
    /// the shutdown has started stopping a container, so it is past the
    /// turn drain and waits for its kills. Returns how long the shutdown
    /// took once forced. Then kills `session`, whose turn the forced
    /// shutdown left to its kills, at once, and stops every session, so no
    /// turn outlives the test.
    async fn force(self, once_killing: bool, session: SessionId) -> Duration {
        let stops = self.sandbox.stops.load(Ordering::SeqCst);
        self.stop.send(()).unwrap();
        let started = Instant::now();
        while once_killing
            && self.sandbox.stops.load(Ordering::SeqCst) == stops
            && !self.task.is_finished()
        {
            assert!(started.elapsed() < WAIT, "the shutdown never killed a turn");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !self.task.is_finished(),
            "the shutdown ended before it was forced"
        );
        let forced = Instant::now();
        self.abort.send(()).unwrap();
        self.task.await.unwrap().unwrap();
        let took = forced.elapsed();
        *self.sandbox.stop_delay.lock().unwrap() = Duration::ZERO;
        self.turns.sessions().kill(session).await;
        self.turns.sessions().stop_all().await;
        took
    }
}

fn posts(calls: &[Call]) -> Vec<(ReplyTarget, String, MsgRef)> {
    calls
        .iter()
        .filter_map(|call| match call {
            Call::Post { to, text, msg } => Some((to.clone(), text.clone(), msg.clone())),
            _ => None,
        })
        .collect()
}

fn uploads(calls: &[Call]) -> Vec<(ReplyTarget, String, String)> {
    calls
        .iter()
        .filter_map(|call| match call {
            Call::Upload { to, files } => Some(files.iter().map(|file| {
                (
                    to.clone(),
                    file.file.name.clone(),
                    String::from_utf8_lossy(&file.contents).into_owned(),
                )
            })),
            _ => None,
        })
        .flatten()
        .collect()
}

fn in_thread(root: &str) -> ReplyTarget {
    ReplyTarget {
        conv: conv("GENERAL"),
        thread_root: Some(root.into()),
    }
}

fn heading(id: ConsentId) -> String {
    format!("*Private task `{id}`:*")
}

#[tokio::test]
async fn private_returns_at_once_and_channel_turn_ends() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("alice", "t1", SEEN).await;
    let calls = stack.mock.calls().split_off(before);
    let replies = posts(&calls);
    assert_eq!(replies.len(), 1, "{calls:#?}");
    assert_eq!(replies[0].0, in_thread("t1"));
    assert_eq!(
        replies[0].1, "Done.",
        "the channel turn ended with its own reply"
    );
    let row = stack.store().consent(consent).await.unwrap().unwrap();
    assert_eq!(
        row.finished_at, None,
        "the channel turn didn't wait for the private task"
    );
    assert_eq!(row.task, TASK);
    assert_eq!(row.thread, thread("t1"));
    assert_eq!(row.state, ConsentState::Pending, "asked in a channel");
    stack.approve(consent).await;

    stack.posted(before, &heading(consent)).await;
    stack.finished(consent).await;
    stack.stop().await;
}

#[tokio::test]
async fn owner_requester_skips_card() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask_with(stack.in_dm("d1"), SEEN).await;
    let row = stack.store().consent(consent).await.unwrap().unwrap();
    assert_eq!(row.state, ConsentState::Approved);
    assert_eq!(row.decided_by, Some(key("alice")));
    stack.posted(before, &heading(consent)).await;
    let row = stack.finished(consent).await;
    assert_eq!(row.card, None);
    assert!(
        stack.manager.posts().is_empty(),
        "no card for a task the owner asked for in their own DM"
    );
    let private = stack.started(row.private_session.unwrap());
    assert_eq!(private[0].shared, SharedAccess::ReadWrite);
    assert!(private[0].memory, "the owner's own task");
    stack.stop().await;
}

#[tokio::test]
async fn non_owner_requires_approval() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("bob", "t1", SEEN).await;
    let row = stack.store().consent(consent).await.unwrap().unwrap();
    assert_eq!(row.state, ConsentState::Pending);

    let recorded = stack.recorded_card(consent).await;
    let card = stack.dms_to("alice").join("\n");
    assert!(card.contains(TASK), "{card}");
    assert!(card.contains("`bob`"), "{card}");
    assert!(card.contains(&format!("`approve {consent}`")), "{card}");
    assert!(card.contains("can read your agent's shared files but not change them"));
    assert!(card.contains("Files handed to it: `in.txt`."), "{card}");
    assert!(stack.dms_to("bob").is_empty());
    stack
        .pipeline
        .settle_consents(stack.app.ctl().consents())
        .await
        .unwrap();
    let row = stack.store().consent(consent).await.unwrap().unwrap();
    assert_eq!(
        row.work_attempts, 0,
        "a pass over the consents runs nothing before the owner approves"
    );
    assert_eq!(row.private_session, None);
    assert_eq!(row.card.as_ref(), Some(&recorded));

    let decided = stack
        .app
        .ctl()
        .consents()
        .decide(&key("alice"), consent, true)
        .await
        .unwrap();
    assert!(matches!(decided, Decided::Recorded), "{decided:?}");
    stack.posted(before, &heading(consent)).await;
    let row = stack.finished(consent).await;
    assert!(row.private_session.is_some());
    let started = Instant::now();
    while !stack.manager.calls().iter().any(|call| {
        matches!(call, Call::Edit { msg, text } if Some(msg) == row.card.as_ref() && text.contains("Approved."))
    }) {
        assert!(started.elapsed() < WAIT, "the card was never closed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stack.stop().await;
}

#[tokio::test]
async fn decline_posts_outcome() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("bob", "t1", SEEN).await;
    let staged = stack.data.join("consents").join(consent.to_string());
    assert!(staged.join("0").exists());
    let decided = stack
        .app
        .ctl()
        .consents()
        .decide(&key("alice"), consent, false)
        .await
        .unwrap();
    assert!(matches!(decided, Decided::Recorded));
    let (to, text, posted) = stack.posted(before, "declined it").await;
    assert_eq!(to, in_thread("t1"));
    assert!(text.contains(&consent.to_string()), "{text}");
    let row = stack.finished(consent).await;
    assert_eq!(row.state, ConsentState::Declined);
    assert_eq!(row.private_session, None, "a declined task never runs");
    let attributed = stack
        .store()
        .posted_message_ref(&posted)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attributed.agent, Some(stack.agent));
    assert_eq!(attributed.session, SessionId::from_uuid(*consent.as_uuid()));
    assert_eq!(attributed.requester.key, key("bob"));
    assert!(!staged.exists(), "its files are deleted");
    assert!(
        !stack
            .upstream_bodies()
            .await
            .iter()
            .any(|body| body.contains(TASK) && !body.contains("hello")),
        "no private turn ran"
    );
    assert!(matches!(
        stack
            .app
            .ctl()
            .consents()
            .decide(&key("alice"), consent, true)
            .await
            .unwrap(),
        Decided::Settled(ConsentState::Declined)
    ));
    stack.stop().await;
}

#[tokio::test]
async fn expiry_posts_outcome() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("bob", "t1", SEEN).await;
    stack.recorded_card(consent).await;
    let db = stack.db().await;
    sqlx::query("UPDATE consents SET expires_at = created_at + 1 WHERE id = ?")
        .bind(consent.to_string())
        .execute(&db)
        .await
        .unwrap();
    db.close().await;
    stack.app.ctl().consents().wake();
    let (to, text, _) = stack.posted(before, "expired").await;
    assert_eq!(to, in_thread("t1"));
    assert!(text.contains("within 1 second"), "{text}");
    let row = stack.finished(consent).await;
    assert_eq!(row.state, ConsentState::Expired);
    assert_eq!(row.private_session, None);
    assert!(matches!(
        stack
            .app
            .ctl()
            .consents()
            .decide(&key("alice"), consent, true)
            .await
            .unwrap(),
        Decided::Settled(ConsentState::Expired)
    ));
    stack.stop().await;
}

#[tokio::test]
async fn private_session_is_fresh_and_not_dm() {
    let stack = start("").await;
    let dm = ThreadKey {
        conv: conv("dm-helper-alice"),
        root: None,
    };
    let owners_dm = stack
        .store()
        .session_for_thread(
            stack.agent,
            &dm,
            &ScopeKey::Private,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap()
        .session;
    let consent = stack.ask("alice", "t1", SEEN).await;
    stack.approve(consent).await;
    let row = stack.finished(consent).await;
    let private = row.private_session.expect("the task ran in a session");
    assert_ne!(private, owners_dm.id);
    assert_ne!(private, row.origin_session);
    let session = stack.store().session(private).await.unwrap().unwrap();
    assert_eq!(session.kind, SessionKind::Private(consent));
    assert_eq!(session.scope, ScopeKey::Private);
    assert_eq!(session.thread, thread("t1"));
    assert!(
        !stack.turns.sessions().is_warm(private),
        "its container is reaped right after the task"
    );
    assert_eq!(stack.started(private).len(), 1);

    let again = stack.ask("alice", "t2", SEEN).await;
    stack.approve(again).await;
    let second = stack.finished(again).await.private_session.unwrap();
    assert_ne!(second, private, "each task gets a session of its own");
    stack.stop().await;
}

#[tokio::test]
async fn only_task_text_and_attachments_cross_in() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack
        .ask(
            "bob",
            "t1",
            "ls -A > ls.txt; cat in.txt > seen.txt && agentctl attach seen.txt && \
             agentctl attach ls.txt",
        )
        .await;
    stack
        .app
        .ctl()
        .consents()
        .decide(&key("alice"), consent, true)
        .await
        .unwrap();
    stack.posted(before, &heading(consent)).await;
    let row = stack.finished(consent).await;

    let requests = stack.fake.message_requests().await;
    let task_turns: Vec<_> = requests
        .iter()
        .filter(|request| {
            !String::from_utf8_lossy(&request.body).contains("hello from the channel")
        })
        .collect();
    assert_eq!(task_turns.len(), 1, "one turn besides the channel's");
    let body = String::from_utf8_lossy(&task_turns[0].body);
    assert!(
        body.contains(&format!(
            "{TASK}\\n\\nFiles handed to this task, in the working directory: in.txt"
        )),
        "{body}"
    );
    for leaked in ["GENERAL", "bob", "t1", "consent.txt"] {
        assert!(!body.contains(leaked), "{leaked} crossed in: {body}");
    }
    assert_eq!(
        task_turns[0]
            .headers
            .get("authorization")
            .map(|value| value.to_str().unwrap()),
        Some("Bearer token-of-alice"),
        "it runs on the owner's account"
    );

    let session = row.private_session.unwrap();
    let volume = stack
        .started(session)
        .pop()
        .expect("the task's container")
        .volume_dir;
    assert!(
        !volume.join("sessions").join(session.to_string()).exists(),
        "the task's session directory is deleted once it is delivered"
    );
    let calls = stack.mock.calls().split_off(before);
    let listed = uploads(&calls)
        .into_iter()
        .find(|(_, name, _)| name == "ls.txt")
        .unwrap();
    assert_eq!(
        listed.2, "in.txt\nls.txt\n",
        "only the handed file was there"
    );
    let calls = stack.mock.calls().split_off(before);
    let seen = uploads(&calls)
        .into_iter()
        .find(|(_, name, _)| name == "seen.txt")
        .unwrap();
    assert_eq!(
        seen.2, "data",
        "the file crossed in as the channel turn wrote it"
    );
    stack.stop().await;
}

#[tokio::test]
async fn only_reply_and_attachments_cross_out() {
    let stack = start("").await;
    let consent = stack.ask("bob", "t1", SEEN).await;
    let before = stack.mock.calls().len();
    let manager_before = stack.manager.calls().len();
    stack
        .app
        .ctl()
        .consents()
        .decide(&key("alice"), consent, true)
        .await
        .unwrap();
    stack.posted(before, &heading(consent)).await;
    stack.finished(consent).await;
    let calls = stack.mock.calls().split_off(before);
    let sent = posts(&calls);
    assert_eq!(sent.len(), 1, "{calls:#?}");
    assert_eq!(sent[0].0, in_thread("t1"));
    assert_eq!(sent[0].1, format!("{}\n\nDone.", heading(consent)));
    let files = uploads(&calls);
    assert_eq!(files.len(), 1, "{calls:#?}");
    assert_eq!(
        files[0],
        (in_thread("t1"), "seen.txt".to_owned(), "data".to_owned())
    );
    assert!(
        calls
            .iter()
            .all(|call| matches!(call, Call::Post { .. } | Call::Upload { .. })),
        "nothing but the reply and its files: {calls:#?}"
    );
    let manager = stack.manager.calls().split_off(manager_before);
    assert!(
        manager.iter().all(|call| matches!(call, Call::Edit { .. })),
        "only the card's outcome, nothing about the task's content: {manager:#?}"
    );
    stack.stop().await;
}

#[tokio::test]
async fn ask_agent_and_private_refused_inside_private_task() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack
        .ask(
            "alice",
            "t1",
            "agentctl ask-agent other hi 2> refused.txt; agentctl private again 2>> refused.txt; \
             agentctl post --to here hi 2>> refused.txt; agentctl attach refused.txt",
        )
        .await;
    stack.approve(consent).await;
    stack.posted(before, &heading(consent)).await;
    stack.finished(consent).await;
    let calls = stack.mock.calls().split_off(before);
    let refused = uploads(&calls)
        .into_iter()
        .find(|(_, name, _)| name == "refused.txt")
        .map(|(_, _, contents)| contents)
        .unwrap();
    let lines: Vec<&str> = refused.lines().collect();
    assert_eq!(lines.len(), 3, "{refused}");
    for line in lines {
        assert_eq!(
            line,
            "agentctl: only `agentctl attach` is available inside a private task"
        );
    }
    let asked = stack.store().consent(consent).await.unwrap().unwrap();
    assert_eq!(asked.task, TASK, "the task asked for no consent of its own");
    stack.stop().await;
}

#[tokio::test]
async fn result_message_ref_inherits_requester_and_hop() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("bob", "t1", SEEN).await;
    stack
        .app
        .ctl()
        .consents()
        .decide(&key("alice"), consent, true)
        .await
        .unwrap();
    let (_, text, result) = stack.posted(before, &heading(consent)).await;
    let row = stack.finished(consent).await;
    let attributed = stack
        .store()
        .posted_message_ref(&result)
        .await
        .unwrap()
        .expect("the result is attributed");
    let bob = stack
        .store()
        .member_for_identity(&key("bob"))
        .await
        .unwrap();
    assert_eq!(attributed.requester.key, key("bob"));
    assert_eq!(attributed.requester.member, bob);
    assert_eq!(attributed.hop, row.hop);
    assert_eq!(Some(attributed.session), row.private_session);
    assert_eq!(attributed.agent, Some(stack.agent));
    assert!(attributed.turn.is_some());
    assert_eq!(attributed.msg.conv, conv("GENERAL"));

    let said = |id: &str, sender: &str, text: &str| Msg {
        id: id.into(),
        sender: key(sender),
        sender_is_bot: sender == BOT,
        text: text.into(),
        files: vec![],
        sent_at: OffsetDateTime::now_utc(),
    };
    stack.mock.set_history(
        thread("t1"),
        vec![
            said("t1", "bob", "@UBOT hello from the channel"),
            said(result.id.as_str(), BOT, &text),
            said("t2", "bob", "@UBOT and now?"),
        ],
    );
    let upstream = stack.fake.message_requests().await.len();
    stack.next_turn(Turn::reply("Seen it."));
    stack
        .pipeline
        .handle(
            stack.mention("bob", "t2", Some("t1")),
            MockSurface::DEFAULT_CAPS,
        )
        .await;
    let bodies: Vec<String> = stack.upstream_bodies().await.split_off(upstream);
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].contains("] you, outside this session: *Private task"),
        "the channel session's next turn sees the result: {}",
        bodies[0]
    );
    stack.stop().await;
}

#[tokio::test]
async fn channel_volume_never_mounts_private_paths() {
    let stack = start("").await;
    let consent = stack.ask("alice", "t1", SEEN).await;
    stack.approve(consent).await;
    let row = stack.finished(consent).await;
    let channel = stack.started(row.origin_session);
    assert!(!channel.is_empty());
    let private = stack.started(row.private_session.unwrap());
    let private_volume = private[0].volume_dir.clone();
    assert_eq!(private[0].volume.scope, ScopeKey::Private);
    for started in &channel {
        assert_eq!(
            started.volume.scope,
            ScopeKey::Channel(conv("GENERAL")),
            "a channel session mounts its channel's volume"
        );
        assert!(!started.memory, "a channel session never mounts memory/");
        assert!(!started.volume_dir.starts_with(&private_volume));
        assert!(!private_volume.starts_with(&started.volume_dir));
    }
    for started in &private {
        assert_eq!(started.shared, SharedAccess::ReadWrite);
        assert!(started.memory, "the owner's own task gets memory/");
    }
    let consents = stack.data.join("consents");
    assert!(
        !channel
            .iter()
            .any(|started| started.volume_dir.starts_with(&consents))
    );
    stack.stop().await;
}

#[tokio::test]
async fn non_owner_task_gets_read_only_shared_and_no_memory() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("bob", "t1", SEEN).await;
    stack
        .app
        .ctl()
        .consents()
        .decide(&key("alice"), consent, true)
        .await
        .unwrap();
    stack.posted(before, &heading(consent)).await;
    let row = stack.finished(consent).await;
    let private = stack.started(row.private_session.unwrap());
    assert_eq!(private.len(), 1);
    assert_eq!(private[0].volume.scope, ScopeKey::Private);
    assert_eq!(private[0].shared, SharedAccess::ReadOnly);
    assert!(!private[0].memory);
    assert_eq!(
        stack
            .store()
            .consent(consent)
            .await
            .unwrap()
            .unwrap()
            .requester
            .member,
        stack
            .store()
            .member_for_identity(&key("bob"))
            .await
            .unwrap()
    );
    assert_ne!(
        Some(stack.alice),
        stack
            .store()
            .member_for_identity(&key("bob"))
            .await
            .unwrap()
    );
    stack.stop().await;
}

#[tokio::test]
async fn owner_requester_at_hop_one_needs_a_card() {
    let stack = start("").await;
    let store = stack.store();
    let bob = store
        .member_for_identity(&key("bob"))
        .await
        .unwrap()
        .unwrap();
    let team = TEAM.into();
    let AgentCreation::Created(writer, binding) = store
        .create_agent(
            &NewAgent {
                owner: bob,
                name: "writer",
                persona: "You write.",
                visibility: Visibility::Public,
                surface: SurfaceKind::RocketChat,
                team: &team,
            },
            10,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap()
    else {
        panic!("the agent was created");
    };
    store
        .set_binding_bot_user(binding, &UserId::new("UWRITER"), "writer")
        .await
        .unwrap();
    store
        .activate_binding(binding, &SecretString::from("t"), OffsetDateTime::now_utc())
        .await
        .unwrap();
    let by_writer = msg("GENERAL", "w1");
    store
        .record_message_ref(
            &store::NewMessageRef {
                session: SessionId::new_v4(),
                msg: &by_writer,
                thread_root: None,
                agent: Some(writer.id),
                turn: Some(core_types::TurnId::new_v4()),
                requester: &core_types::Requester {
                    member: Some(stack.alice),
                    key: key("alice"),
                },
                hop: core_types::Hop::ZERO,
                consent: None,
                hands_off: true,
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let mut hop = stack.mention("UWRITER", "w1", None);
    hop.sender_is_bot = true;
    hop.sender_bot_user = Some(UserId::new("UWRITER"));

    let before = stack.mock.calls().len();
    let consent = stack.ask_with(hop, SEEN).await;
    let row = store.consent(consent).await.unwrap().unwrap();
    assert_eq!(row.requester.member, Some(stack.alice));
    assert_eq!(row.hop, core_types::Hop(1));
    assert_eq!(
        row.state,
        ConsentState::Pending,
        "another agent's post can't approve a task in the owner's name"
    );
    let card = stack.card_to("alice").await;
    assert!(card.contains("hop 1"), "{card}");
    assert!(
        card.contains("can read and change your agent's shared files and its memory"),
        "{card}"
    );
    assert_eq!(
        store
            .consent(consent)
            .await
            .unwrap()
            .unwrap()
            .private_session,
        None,
        "nothing runs before the owner approves"
    );

    stack
        .app
        .ctl()
        .consents()
        .decide(&key("alice"), consent, true)
        .await
        .unwrap();
    stack.posted(before, &heading(consent)).await;
    let row = stack.finished(consent).await;
    let private = stack.started(row.private_session.unwrap());
    assert_eq!(private[0].shared, SharedAccess::ReadWrite);
    assert!(private[0].memory, "the owner approved their own task");
    stack.stop().await;
}

/// `owner`'s agent `name` on the team, whose bot is `bot`, active.
async fn agent_of(store: &Store, owner: MemberId, name: &str, bot: &str) -> AgentId {
    let team = TEAM.into();
    let AgentCreation::Created(agent, binding) = store
        .create_agent(
            &NewAgent {
                owner,
                name,
                persona: "You write.",
                visibility: Visibility::Public,
                surface: SurfaceKind::RocketChat,
                team: &team,
            },
            10,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap()
    else {
        panic!("the agent was created");
    };
    store
        .set_binding_bot_user(binding, &UserId::new(bot), name)
        .await
        .unwrap();
    store
        .activate_binding(binding, &SecretString::from("t"), OffsetDateTime::now_utc())
        .await
        .unwrap();
    agent.id
}

#[tokio::test]
async fn a_private_tasks_result_starts_no_hop() {
    let stack = start("").await;
    let store = stack.store();
    let bob = store
        .member_for_identity(&key("bob"))
        .await
        .unwrap()
        .unwrap();
    agent_of(store, bob, "writer", "UWRITER").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("alice", "t1", SEEN).await;
    stack.approve(consent).await;
    let (_, _, result) = stack.posted(before, &heading(consent)).await;
    stack.finished(consent).await;
    let attributed = store.posted_message_ref(&result).await.unwrap().unwrap();
    assert_eq!(attributed.consent, Some(consent));

    let upstream = stack.fake.message_requests().await.len();
    let after = stack.mock.calls().len();
    stack.next_turn(Turn::reply("Writing it up."));
    let mut mention = stack.mention(BOT, result.id.as_str(), Some("t1"));
    mention.sender_is_bot = true;
    mention.sender_bot_user = Some(UserId::new(BOT));
    mention.text = "@UWRITER please publish this".to_owned();
    mention.mentions = vec![UserId::new("UWRITER")];
    stack
        .pipeline
        .handle(mention, MockSurface::DEFAULT_CAPS)
        .await;
    assert_eq!(
        stack.fake.message_requests().await.len(),
        upstream,
        "the writer took no turn on the owner's private result"
    );
    assert!(posts(&stack.mock.calls().split_off(after)).is_empty());
    stack.stop().await;
}

#[tokio::test]
async fn a_store_failure_before_the_task_runs_is_retried() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("bob", "t1", SEEN).await;
    stack.card_to("alice").await;
    stack.failing.store(1, Ordering::SeqCst);
    stack
        .app
        .ctl()
        .consents()
        .decide(&key("alice"), consent, true)
        .await
        .unwrap();
    let started = Instant::now();
    let failed = loop {
        let row = stack.store().consent(consent).await.unwrap().unwrap();
        if row.work_failures > 0 {
            break row;
        }
        assert!(started.elapsed() < WAIT, "the failure was never recorded");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        failed.finished_at, None,
        "a store error doesn't end the task"
    );
    assert_eq!(stack.failing.load(Ordering::SeqCst), 0);
    assert!(
        stack
            .store()
            .release_consent_work(consent, failed.work_attempts, OffsetDateTime::now_utc())
            .await
            .unwrap(),
        "skip the wait for the retry"
    );
    stack.app.ctl().consents().wake();
    stack.posted(before, &heading(consent)).await;
    let row = stack.finished(consent).await;
    assert_eq!(row.work_failures, 1);
    assert!(row.private_session.is_some());
    stack.stop().await;
}

#[tokio::test]
async fn a_turn_that_fails_after_it_may_have_started_leaves_nothing_behind() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("bob", "t1", SEEN).await;
    stack.card_to("alice").await;
    let db = stack.db().await;
    sqlx::query(
        "CREATE TRIGGER fail_private_sends BEFORE UPDATE OF maybe_started ON sessions \
         WHEN NEW.kind = 'private' BEGIN SELECT RAISE(ABORT, 'injected'); END",
    )
    .execute(&db)
    .await
    .unwrap();
    stack.approve(consent).await;
    let started = Instant::now();
    let failed = loop {
        let row = stack.store().consent(consent).await.unwrap().unwrap();
        if row.work_failures > 0 {
            break row;
        }
        assert!(started.elapsed() < WAIT, "the failure was never recorded");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let earlier = failed.private_session.expect("the task had a session");
    assert!(
        !stack.turns.sessions().is_warm(earlier),
        "a turn that failed still has its container stopped"
    );
    let session_dir = stack.started(earlier)[0]
        .volume_dir
        .join("sessions")
        .join(earlier.to_string());
    assert!(session_dir.join("work/in.txt").exists());
    sqlx::query("DROP TRIGGER fail_private_sends")
        .execute(&db)
        .await
        .unwrap();
    db.close().await;
    assert!(
        stack
            .store()
            .mark_session_turn_pending(earlier)
            .await
            .unwrap(),
        "as if the send failed after it may have reached the model"
    );
    assert!(
        stack
            .store()
            .release_consent_work(consent, failed.work_attempts, OffsetDateTime::now_utc())
            .await
            .unwrap(),
        "skip the wait for the retry"
    );
    stack.app.ctl().consents().wake();
    stack.posted(before, "interrupted").await;
    stack.finished(consent).await;
    let started = Instant::now();
    while session_dir.exists() {
        assert!(
            started.elapsed() < WAIT,
            "the failed session's directory was kept"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stack.stop().await;
}

#[tokio::test]
async fn a_stale_claim_leaves_the_newer_claims_session_alone() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack
        .ask(
            "bob",
            "t1",
            "sleep 2; cat in.txt > seen.txt && agentctl attach seen.txt",
        )
        .await;
    stack.card_to("alice").await;
    stack.run_task(consent).await;
    let store = stack.store();
    let lapsed = OffsetDateTime::now_utc() + agentd::pipeline::WORK_LEASE + Duration::from_secs(1);
    let newer = store
        .claim_consent_work(consent, lapsed, lapsed + agentd::pipeline::WORK_LEASE)
        .await
        .unwrap()
        .expect("a lapsed claim is taken over");
    let second = store
        .create_private_session(
            stack.agent,
            consent,
            &thread("t1"),
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    assert!(
        store
            .set_consent_session(consent, newer.work_attempts, second.id)
            .await
            .unwrap()
    );
    let work = stack.turns.sessions().work_dir(&second).await.unwrap().work;
    std::fs::write(work.join("newer.txt"), "the newer claim's").unwrap();
    stack.posted(before, &heading(consent)).await;
    stack.pipeline.close();
    stack.pipeline.drain().await;
    assert!(
        work.join("newer.txt").exists(),
        "the stale claim, now done, deleted the newer claim's session"
    );
    let row = store.consent(consent).await.unwrap().unwrap();
    assert_eq!(row.finished_at, None, "only the newer claim can finish it");
    assert_eq!(row.private_session, Some(second.id));
    stack.stop().await;
}

#[tokio::test]
async fn a_task_that_reached_the_model_is_never_run_again() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("bob", "t1", SEEN).await;
    stack.card_to("alice").await;
    let store = stack.store();
    let earlier = store
        .create_private_session(
            stack.agent,
            consent,
            &thread("t1"),
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    assert!(store.mark_session_turn_pending(earlier.id).await.unwrap());
    let work = stack
        .turns
        .sessions()
        .work_dir(&earlier)
        .await
        .unwrap()
        .work;
    std::fs::write(work.join("copied-from-memory.txt"), "private").unwrap();
    assert!(
        store
            .set_consent_session(consent, 0, earlier.id)
            .await
            .unwrap()
    );
    let upstream = stack.fake.message_requests().await.len();
    stack
        .app
        .ctl()
        .consents()
        .decide(&key("alice"), consent, true)
        .await
        .unwrap();
    let (to, text, _) = stack.posted(before, "interrupted").await;
    assert_eq!(to, in_thread("t1"));
    assert!(text.contains(&consent.to_string()), "{text}");
    let row = stack.finished(consent).await;
    assert_eq!(row.private_session, Some(earlier.id));
    assert_eq!(
        stack.fake.message_requests().await.len(),
        upstream,
        "the task didn't run a second time"
    );
    let session_dir = work.parent().unwrap().to_owned();
    let started = Instant::now();
    while session_dir.exists() {
        assert!(
            started.elapsed() < WAIT,
            "the interrupted session's directory was kept"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    stack.stop().await;
}

#[tokio::test]
async fn a_shutdown_kills_and_meters_the_turn_it_cuts() {
    let stack = start("").await;
    let consent = stack
        .ask(
            "bob",
            "t1",
            "sleep 90; cat in.txt > seen.txt && agentctl attach seen.txt",
        )
        .await;
    stack.card_to("alice").await;
    let session = stack.run_task(consent).await;
    let (store, alice) = (stack.store(), stack.alice);
    let billed = || async move {
        store
            .member_usage(alice, OffsetDateTime::now_utc())
            .await
            .unwrap()
            .today
            .turns
    };
    let before = billed().await;
    let cutting = Instant::now();
    stack.pipeline.cut_short().await;
    stack.pipeline.wait_for_kills().await;
    assert!(
        cutting.elapsed() < Duration::from_secs(10),
        "the shutdown waited for the kill, not for the turn"
    );
    assert!(
        !stack.turns.sessions().is_warm(session),
        "the cut turn was killed before the shutdown went on"
    );
    assert_eq!(
        billed().await,
        before + 1,
        "the owner pays for the cut turn, metered before the store closes"
    );
    let row = stack.store().consent(consent).await.unwrap().unwrap();
    assert_eq!(row.finished_at, None);
    assert_eq!(row.work_failures, 0, "a shutdown is no failure");
    assert_eq!(
        store
            .consent_work_owed(OffsetDateTime::now_utc())
            .await
            .unwrap()
            .iter()
            .map(|owed| owed.id)
            .collect::<Vec<_>>(),
        [consent],
        "once its turn is known to have ended, another instance may take it up at once"
    );
    stack.stop().await;
}

#[tokio::test]
async fn a_shutdown_tells_cut_threads_before_its_kills_end() {
    let stack = start("").await;
    let consent = stack.ask("bob", "t1", "sleep 90").await;
    stack.card_to("alice").await;
    let session = stack.run_task(consent).await;
    let before = stack.mock.calls().len();
    let again = stack.mention("bob", "t1-again", Some("t1"));
    let pipeline = stack.pipeline.clone();
    let channel = tokio::spawn(async move {
        pipeline.handle(again, MockSurface::DEFAULT_CAPS).await;
    });
    let started = Instant::now();
    while !stack
        .mock
        .calls()
        .split_off(before)
        .iter()
        .any(|call| matches!(call, Call::React { msg, .. } if msg.id.as_str() == "t1-again"))
    {
        assert!(started.elapsed() < WAIT, "the channel turn never started");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    *stack.sandbox.stop_delay.lock().unwrap() = Duration::from_secs(3);
    let cutting = Instant::now();
    stack.pipeline.cut_short().await;
    assert!(
        cutting.elapsed() < Duration::from_secs(3),
        "the shutdown went on without waiting for the kill"
    );
    assert!(
        posts(&stack.mock.calls().split_off(before))
            .iter()
            .any(|(_, text, _)| text.contains(agentd::pipeline::RESTARTING_TEXT)),
        "the cut channel turn's thread was told to ask again first"
    );
    assert!(
        stack.turns.sessions().is_warm(session),
        "the private turn's kill is still under way"
    );
    stack.pipeline.wait_for_kills().await;
    assert!(!stack.turns.sessions().is_warm(session));
    channel.abort();
    *stack.sandbox.stop_delay.lock().unwrap() = Duration::ZERO;
    stack.stop().await;
}

#[tokio::test]
async fn a_second_signal_leaves_the_kills_of_the_turns_it_cuts_short() {
    let stack = start("").await;
    let consent = stack.ask("bob", "t1", "sleep 90").await;
    stack.card_to("alice").await;
    let session = stack.run_task(consent).await;
    *stack.sandbox.stop_delay.lock().unwrap() = Duration::from_secs(20);
    let took = stack.force(false, session).await;
    assert!(
        took < Duration::from_secs(10),
        "the forced shutdown waited {took:?} for the kills"
    );
}

#[tokio::test]
async fn a_second_signal_while_a_shutdown_waits_for_its_kills_stops_the_wait() {
    let stack = start_with_drain("", 1).await;
    let consent = stack.ask("bob", "t1", "sleep 90").await;
    stack.card_to("alice").await;
    let session = stack.run_task(consent).await;
    *stack.sandbox.stop_delay.lock().unwrap() = Duration::from_secs(20);
    let took = stack.force(true, session).await;
    assert!(
        took < Duration::from_secs(10),
        "the second signal still waited {took:?} for the kills"
    );
}

#[tokio::test]
async fn an_approved_task_waits_for_its_paused_agent() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("bob", "t1", SEEN).await;
    let card = stack.card_to("alice").await;
    assert!(!card.contains("is paused"), "{card}");
    let store = stack.store();
    assert!(store.set_agent_paused(stack.agent, true).await.unwrap());
    stack
        .app
        .ctl()
        .consents()
        .decide(&key("alice"), consent, true)
        .await
        .unwrap();
    stack
        .pipeline
        .settle_consents(stack.app.ctl().consents())
        .await
        .unwrap();
    let held = store.consent(consent).await.unwrap().unwrap();
    assert_eq!(
        held.work_attempts, 0,
        "nothing runs while the agent is paused"
    );
    assert_eq!(held.finished_at, None, "and the task isn't dropped");

    assert!(store.set_agent_paused(stack.agent, false).await.unwrap());
    stack.app.ctl().consents().wake();
    stack.posted(before, &heading(consent)).await;
    stack.finished(consent).await;
    stack.stop().await;
}

#[tokio::test]
async fn a_deleted_agents_consent_expires_without_a_word() {
    let stack = start("").await;
    let consent = stack.ask("bob", "t1", SEEN).await;
    stack.card_to("alice").await;
    let before = stack.mock.calls().len();
    assert!(
        stack
            .store()
            .delete_agent(stack.agent, OffsetDateTime::now_utc())
            .await
            .unwrap()
    );
    assert!(matches!(
        stack
            .app
            .ctl()
            .consents()
            .decide(&key("alice"), consent, true)
            .await
            .unwrap(),
        Decided::NotYours
    ));
    stack.app.ctl().consents().wake();
    let row = stack.finished(consent).await;
    assert_eq!(row.state, ConsentState::Expired);
    assert!(
        posts(&stack.mock.calls().split_off(before)).is_empty(),
        "nothing is posted for a deleted agent"
    );
    stack.stop().await;
}

#[tokio::test]
async fn a_card_that_cant_reach_the_owner_expires_without_running() {
    let stack = start("").await;
    let store = stack.store();
    let carol = store
        .ensure_member(
            &MemberKey {
                surface: SurfaceKind::Slack,
                team: "T0OTHER".into(),
                user: "U0CAROL".into(),
            },
            "carol",
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let agent = agent_of(store, carol, "carols", "UCAROL").await;
    let before = stack.mock.calls().len();
    let thread = thread("t9");
    let now = OffsetDateTime::now_utc();
    let created = store
        .create_consent(
            &store::NewConsent {
                id: ConsentId::new_v4(),
                agent,
                requester: &core_types::Requester {
                    member: None,
                    key: key("bob"),
                },
                hop: core_types::Hop::ZERO,
                task: TASK,
                attachments_json: "[]",
                thread: &thread,
                origin_session: SessionId::new_v4(),
                expires_at: now + Duration::from_secs(2),
                approved_by_owner: None,
            },
            store::OpenLimits {
                per_requester: 1,
                per_agent: 1,
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
    stack.app.ctl().consents().wake();
    let (to, text, _) = stack.posted(before, "couldn't reach the owner").await;
    assert!(
        OffsetDateTime::now_utc() >= created.expires_at,
        "a card that can't be sent is tried again until the consent expires"
    );
    assert_eq!(to, in_thread("t9"));
    assert!(text.contains(&created.id.to_string()), "{text}");
    let row = stack.finished(created.id).await;
    assert_eq!(row.state, ConsentState::Expired);
    assert_eq!(row.card, None);
    stack.stop().await;
}

#[tokio::test]
async fn a_capped_thread_runs_no_private_task() {
    let stack = start("thread_turns_per_hour = 1").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("bob", "t1", SEEN).await;
    stack.card_to("alice").await;
    stack
        .app
        .ctl()
        .consents()
        .decide(&key("alice"), consent, true)
        .await
        .unwrap();
    let (to, text, _) = stack.posted(before, "didn't run").await;
    assert_eq!(to, in_thread("t1"));
    assert!(text.contains("hourly turn limit (1)"), "{text}");
    let row = stack.finished(consent).await;
    assert_eq!(row.private_session, None);
    stack.stop().await;
}

#[tokio::test]
async fn a_requester_has_only_so_many_tasks_waiting() {
    let stack = start("").await;
    for id in ["t1", "t2", "t3"] {
        stack.ask("bob", id, SEEN).await;
    }
    stack.next_turn(Turn::reply("Done.").with_command([
        "sh",
        "-c",
        "printf data > in.txt; agentctl private --file in.txt 'once more' 2> refused.txt; \
         agentctl attach refused.txt",
    ]));
    let before = stack.mock.calls().len();
    stack
        .pipeline
        .handle(stack.mention("bob", "t4", None), MockSurface::DEFAULT_CAPS)
        .await;
    let calls = stack.mock.calls().split_off(before);
    let refused = uploads(&calls)
        .into_iter()
        .find(|(_, name, _)| name == "refused.txt")
        .map(|(_, _, contents)| contents)
        .unwrap_or_else(|| panic!("{calls:#?}"));
    assert!(
        refused.contains("too many private tasks"),
        "the fourth request was refused: {refused}"
    );
    assert_eq!(
        stack
            .store()
            .consent_cards_owed(OffsetDateTime::now_utc())
            .await
            .unwrap()
            .len()
            + stack.dms_to("alice").len(),
        3,
        "the owner gets three cards at most"
    );
    stack.stop().await;
}

#[tokio::test]
async fn owner_request_in_a_channel_needs_a_card() {
    let stack = start("").await;
    let before = stack.mock.calls().len();
    let consent = stack.ask("alice", "t1", SEEN).await;
    let row = stack.store().consent(consent).await.unwrap().unwrap();
    assert_eq!(row.requester.member, Some(stack.alice));
    assert_eq!(row.hop, core_types::Hop::ZERO);
    assert_eq!(
        row.state,
        ConsentState::Pending,
        "others' messages in the thread reach the turn, so the owner decides on a card"
    );
    let card = stack.card_to("alice").await;
    assert!(
        card.contains("can read and change your agent's shared files and its memory"),
        "{card}"
    );
    assert!(card.contains("outside your own DM"), "{card}");
    stack.approve(consent).await;
    stack.posted(before, &heading(consent)).await;
    let row = stack.finished(consent).await;
    let private = stack.started(row.private_session.unwrap());
    assert_eq!(private[0].shared, SharedAccess::ReadWrite);
    assert!(private[0].memory, "the owner approved their own task");
    stack.stop().await;
}

//! The turn pipeline end to end: messages from a `MockSurface`, turns in
//! `fake-claude` in a process sandbox, the real credential proxy and
//! agentctl API on agentd's listeners, and `fake_anthropic()` upstream.

mod common;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd::ctl::SurfaceLookup;
use agentd::pipeline::{
    DELIVERY_FAILED_TEXT, FAILED_TEXT, Pipeline, PipelineSettings, RESTARTING_TEXT, TurnSettings,
    Turns, USAGE_LIMIT_TEXT,
};
use agentd::server::{Routers, Server};
use agentd::{App, Config};
use core_types::{
    AgentId, Binding, BindingId, Caps, ConvKind, ConvRef, Cursor, InboundEvent, MemberId,
    MemberKey, MessageId, Msg, MsgRef, OutFile, ReplyTarget, ScopeKey, Sender, SessionId, Surface,
    SurfaceError, SurfaceKind, ThreadKey, UserId, VolumeKey,
};
use runner::{PoolConfig, ProcessConfig};
use sandbox::ProcessSandbox;
use secrecy::SecretString;
use store::{AgentCreation, NewAgent, NewClaudeLink, Store, Visibility};
use testkit::{
    Call, FakeAnthropic, MockSurface, Op, TempDir, Turn, agentctl_path, fake_anthropic,
    fake_claude_path,
};
use time::OffsetDateTime;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use common::env;

const TEAM: &str = "chat.example";
const BOT: &str = "UBOT";

/// Every agent's bot acts through the one mock, past the [`Holds`].
#[derive(Debug)]
struct Mocks {
    mock: Arc<MockSurface>,
    holds: Arc<Holds>,
}

#[async_trait::async_trait]
impl SurfaceLookup for Mocks {
    async fn surface(&self, _agent: AgentId, _conv: &ConvRef) -> Option<Arc<dyn Surface>> {
        Some(Arc::new(Held {
            mock: self.mock.clone(),
            holds: self.holds.clone(),
        }))
    }
}

/// Holds what passes through it until it is opened.
#[derive(Debug, Clone)]
struct Gate {
    open: Arc<watch::Sender<bool>>,
    waiting: Arc<AtomicUsize>,
}

impl Gate {
    fn closed() -> Self {
        Self {
            open: Arc::new(watch::channel(false).0),
            waiting: Arc::default(),
        }
    }

    fn open(&self) {
        self.open.send_replace(true);
    }

    /// How many are held at the gate now.
    fn waiting(&self) -> usize {
        self.waiting.load(Ordering::SeqCst)
    }

    async fn pass(&self) {
        let mut open = self.open.subscribe();
        let _waiting = Waiting::new(&self.waiting);
        let _ = open.wait_for(|open| *open).await;
    }
}

/// One caller counted at a gate, until it passes or is dropped.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Self(count)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What the agents' surfaces hold up, besides the mock's own delays.
#[derive(Debug, Default)]
struct Holds {
    posts: Mutex<HashMap<String, Gate>>,
    histories: Mutex<HashMap<ThreadKey, VecDeque<Gate>>>,
}

impl Holds {
    /// Holds the next read of `thread`'s history, by any agent, at `gate`,
    /// after those queued before it for that thread.
    fn history_at(&self, thread: ThreadKey, gate: &Gate) {
        self.histories
            .lock()
            .unwrap()
            .entry(thread)
            .or_default()
            .push_back(gate.clone());
    }

    /// Holds every post of `text` at `gate`.
    fn posts_of(&self, text: &str, gate: &Gate) {
        self.posts
            .lock()
            .unwrap()
            .insert(text.to_owned(), gate.clone());
    }
}

/// One agent's bot: the mock, past the holds.
struct Held {
    mock: Arc<MockSurface>,
    holds: Arc<Holds>,
}

#[async_trait::async_trait]
impl Surface for Held {
    async fn events(
        &self,
        binding: &Binding,
        tx: Sender<InboundEvent>,
    ) -> Result<(), SurfaceError> {
        self.mock.events(binding, tx).await
    }

    async fn post(&self, to: &ReplyTarget, text: &str) -> Result<MsgRef, SurfaceError> {
        let gate = self.holds.posts.lock().unwrap().get(text).cloned();
        if let Some(gate) = gate {
            gate.pass().await;
        }
        self.mock.post(to, text).await
    }

    async fn edit(&self, msg: &MsgRef, text: &str) -> Result<(), SurfaceError> {
        self.mock.edit(msg, text).await
    }

    async fn react(&self, msg: &MsgRef, emoji: &str) -> Result<(), SurfaceError> {
        self.mock.react(msg, emoji).await
    }

    async fn unreact(&self, msg: &MsgRef, emoji: &str) -> Result<(), SurfaceError> {
        self.mock.unreact(msg, emoji).await
    }

    async fn can_post(&self, conv: &ConvRef) -> Result<bool, SurfaceError> {
        self.mock.can_post(conv).await
    }

    async fn upload(&self, to: &ReplyTarget, files: &[OutFile]) -> Result<(), SurfaceError> {
        self.mock.upload(to, files).await
    }

    async fn history(
        &self,
        thread: &ThreadKey,
        before: Option<Cursor>,
        limit: usize,
    ) -> Result<Vec<Msg>, SurfaceError> {
        let gate = self
            .holds
            .histories
            .lock()
            .unwrap()
            .get_mut(thread)
            .and_then(VecDeque::pop_front);
        if let Some(gate) = gate {
            gate.pass().await;
        }
        self.mock.history(thread, before, limit).await
    }

    fn render(&self, markdown: &str) -> Vec<String> {
        self.mock.render(markdown)
    }

    fn caps(&self) -> Caps {
        self.mock.caps()
    }
}

struct Stack {
    app: App,
    pipeline: Pipeline,
    turns: Turns,
    mock: Arc<MockSurface>,
    holds: Arc<Holds>,
    script: PathBuf,
    agent: AgentId,
    binding: BindingId,
    alice: MemberId,
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

/// How [`start_with`] differs from the defaults.
struct Setup {
    drain_timeout_secs: u64,
    pipeline: fn(&mut PipelineSettings),
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            drain_timeout_secs: 5,
            pipeline: |_| {},
        }
    }
}

/// agentd with its listeners and a runner over a process sandbox running
/// `fake-claude`, alice and bob linked, and alice's agent `helper`, whose
/// bot is `UBOT`. The store is a file in the stack's directory.
async fn start() -> Stack {
    start_with(Setup::default()).await
}

async fn start_with(setup: Setup) -> Stack {
    let claude = fake_claude_path();
    let agentctl = agentctl_path();
    let dir = TempDir::new("agentd-test");
    let fake = fake_anthropic().await;
    let text = format!(
        "{}\n[proxy]\nupstream = \"{}\"\n[runner]\nworking_emoji = \"hourglass\"\n",
        common::CONFIG
            .replace("/nonexistent/agentd", &dir.path().display().to_string())
            .replace("sqlite::memory:", &dir.db_url())
            .replace(
                "drain_timeout_secs = 5",
                &format!("drain_timeout_secs = {}", setup.drain_timeout_secs)
            ),
        fake.uri()
    );
    let config = Config::parse(&text, env()).unwrap();
    let store = agentd::app::open_store(&config).await.unwrap();
    let mock = Arc::new(MockSurface::new());
    let holds = Arc::new(Holds::default());
    let mocks = Mocks {
        mock: mock.clone(),
        holds: holds.clone(),
    };
    let app = App::with_surfaces(config, store.clone(), None, Arc::new(mocks)).unwrap();
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
    assert!(
        store
            .activate_binding(
                binding,
                &SecretString::from("bot-token"),
                OffsetDateTime::now_utc()
            )
            .await
            .unwrap()
    );

    let server = Server::bind(app.clone(), Routers::new(&app).unwrap())
        .await
        .unwrap();
    let addrs = server.addrs();
    let script = dir.join("script.json");
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
    let sandbox = ProcessSandbox::new(store.clone(), dir.path()).unwrap();
    let turns = Turns::start(&app, Arc::new(sandbox), settings).unwrap();
    let mut pipeline_settings = PipelineSettings::from_app(&app);
    (setup.pipeline)(&mut pipeline_settings);
    let pipeline = Pipeline::new(
        store.clone(),
        turns.clone(),
        Arc::clone(app.surfaces()),
        app.commands().replies().clone(),
        pipeline_settings,
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
        pipeline,
        turns,
        mock,
        holds,
        script,
        agent: agent.id,
        binding,
        alice,
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

    /// Alice's second agent, `name`, whose bot is `bot`.
    async fn other_agent(&self, name: &str, bot: &str) -> AgentId {
        let store = self.store();
        let team = TEAM.into();
        let AgentCreation::Created(agent, binding) = store
            .create_agent(
                &NewAgent {
                    owner: self.alice,
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
            panic!("created");
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

    /// Makes every session's next turn, whichever it is, play `turn`.
    fn next_turn(&self, turn: Turn) {
        testkit::write_script(&self.script, &vec![turn; 8]).unwrap();
    }

    /// A message from `sender` in `conv_id`: `id`, in the thread of `root`
    /// if given, mentioning `mentions`.
    fn event(
        &self,
        sender: &str,
        conv_id: &str,
        kind: ConvKind,
        id: &str,
        root: Option<&str>,
        mentions: &[&str],
    ) -> InboundEvent {
        InboundEvent {
            event_id: id.to_owned(),
            binding: self.binding,
            sender: key(sender),
            sender_is_bot: false,
            sender_bot_user: None,
            conv: conv(conv_id),
            conv_kind: kind,
            thread_root: root.map(MessageId::new),
            message: msg(conv_id, id),
            text: format!(
                "{} hello",
                mentions
                    .iter()
                    .map(|m| format!("@{m}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
            mentions: mentions.iter().map(|m| UserId::new(*m)).collect(),
            reply_to: root.map(|root| msg(conv_id, root)),
            files: vec![],
            received_at: OffsetDateTime::now_utc(),
        }
    }

    async fn handle(&self, event: InboundEvent) {
        self.pipeline.handle(event, MockSurface::DEFAULT_CAPS).await;
    }

    /// The calls since `from`.
    fn calls_since(&self, from: usize) -> Vec<Call> {
        self.mock.calls().split_off(from)
    }

    async fn session_of(&self, posted: &MsgRef) -> SessionId {
        self.store()
            .posted_message_ref(posted)
            .await
            .unwrap()
            .expect("the post is attributed")
            .session
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        self.task.await.unwrap().unwrap();
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

fn in_thread(conv_id: &str, root: Option<&str>) -> ReplyTarget {
    ReplyTarget {
        conv: conv(conv_id),
        thread_root: root.map(MessageId::new),
    }
}

#[tokio::test]
async fn a_mention_runs_a_turn_and_the_reply_is_delivered_in_the_thread() {
    let stack = start().await;

    stack.next_turn(
        Turn::reply("Here it is. [[react: eyes]]")
            .with_command([
                "sh",
                "-c",
                "printf report > report.txt && agentctl attach report.txt",
            ])
            .with_command([
                "agentctl",
                "post",
                "--to",
                "GENERAL",
                "A note for everyone.",
            ]),
    );
    stack
        .handle(stack.event("alice", "GENERAL", ConvKind::Channel, "u1", None, &[BOT]))
        .await;
    let calls = stack.calls_since(0);
    let sent = posts(&calls);
    assert_eq!(sent.len(), 2, "{calls:#?}");
    let (to, text, reply) = &sent[0];
    assert_eq!(
        *to,
        in_thread("GENERAL", Some("u1")),
        "one reply in the thread"
    );
    assert_eq!(text, "Here it is.", "the directive is stripped");
    let upload = calls
        .iter()
        .position(|call| matches!(call, Call::Upload { .. }))
        .expect("an upload");
    let first_post = calls
        .iter()
        .position(|call| matches!(call, Call::Post { .. }))
        .unwrap();
    assert!(upload < first_post, "the attachment goes before the text");
    let Call::Upload { to, files } = &calls[upload] else {
        unreachable!()
    };
    assert_eq!(*to, in_thread("GENERAL", Some("u1")));
    assert_eq!(files[0].file.name, "report.txt");
    assert_eq!(files[0].contents, b"report");
    assert!(calls.contains(&Call::React {
        msg: msg("GENERAL", "u1"),
        emoji: "eyes".into()
    }));
    assert_eq!(
        calls[0],
        Call::React {
            msg: msg("GENERAL", "u1"),
            emoji: "hourglass".into()
        },
        "the working emoji goes up first"
    );
    assert!(calls.contains(&Call::Unreact {
        msg: msg("GENERAL", "u1"),
        emoji: "hourglass".into()
    }));
    let (note_to, note, note_msg) = &sent[1];
    assert_eq!(
        *note_to,
        in_thread("GENERAL", None),
        "the queued post is delivered"
    );
    assert_eq!(note, "A note for everyone.");

    let attributed = stack
        .store()
        .posted_message_ref(reply)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(attributed.agent, Some(stack.agent));
    assert_eq!(attributed.requester.member, Some(stack.alice));
    assert_eq!(attributed.requester.key, key("alice"));
    assert_eq!(attributed.hop.0, 0);
    assert!(attributed.turn.is_some());
    let first_session = attributed.session;
    let shown = stack
        .store()
        .session_message_ref(first_session, &msg("GENERAL", "u1"))
        .await
        .unwrap()
        .expect("the message shown to the model has a short id");
    assert_eq!(shown.agent, None);
    assert_eq!(shown.requester.key, key("alice"));

    let said = |id: &str, sender: &str, text: &str| core_types::Msg {
        id: id.into(),
        sender: key(sender),
        sender_is_bot: sender == BOT,
        text: text.into(),
        files: vec![],
        sent_at: OffsetDateTime::now_utc(),
    };
    stack.mock.set_history(
        core_types::ThreadKey {
            conv: conv("GENERAL"),
            root: Some("u1".into()),
        },
        vec![
            said("u1", "alice", "@UBOT first question"),
            said(reply.id.as_str(), BOT, "Here it is."),
            said("u1b", "carol", "a side remark"),
            said("u2", "alice", "@UBOT second question"),
        ],
    );
    let before = stack.mock.calls().len();
    stack.next_turn(Turn::reply("Second answer."));
    stack
        .handle(stack.event(
            "alice",
            "GENERAL",
            ConvKind::Channel,
            "u2",
            Some("u1"),
            &[BOT],
        ))
        .await;
    let sent = posts(&stack.calls_since(before));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, in_thread("GENERAL", Some("u1")));
    assert_eq!(
        stack.session_of(&sent[0].2).await,
        first_session,
        "a second mention in the thread resumes the same session"
    );
    let upstream = stack.fake.message_requests().await;
    let last = String::from_utf8_lossy(&upstream.last().unwrap().body).into_owned();
    assert!(
        last.contains("a side remark"),
        "the unseen message is shown: {last}"
    );
    assert!(
        !last.contains("first question"),
        "what the transcript has is not shown again: {last}"
    );

    let before = stack.mock.calls().len();
    stack.next_turn(Turn::reply("Replying to your reply."));
    stack
        .handle(stack.event(
            "bob",
            "GENERAL",
            ConvKind::Channel,
            "u3",
            Some(note_msg.id.as_str()),
            &[],
        ))
        .await;
    let sent = posts(&stack.calls_since(before));
    assert_eq!(
        sent.len(),
        1,
        "a reply in a thread the agent started runs a turn without a mention"
    );
    assert_eq!(sent[0].0, in_thread("GENERAL", Some(note_msg.id.as_str())));
    let bobs = stack
        .store()
        .posted_message_ref(&sent[0].2)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bobs.requester.key, key("bob"));
    assert_ne!(bobs.session, first_session);

    stack.stop().await;
}

#[tokio::test]
async fn dms_and_channels_use_their_own_sessions_and_volumes() {
    let stack = start().await;

    stack.next_turn(Turn::reply("In the DM."));
    stack
        .handle(stack.event("alice", "DM1", ConvKind::Dm, "d1", None, &[]))
        .await;
    let sent = posts(&stack.calls_since(0));
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].0,
        in_thread("DM1", None),
        "a DM replies at the top level"
    );
    let dm_session = stack.session_of(&sent[0].2).await;
    let row = stack.store().session(dm_session).await.unwrap().unwrap();
    assert_eq!(
        row.scope,
        ScopeKey::Private,
        "the owner's DM is the private side"
    );
    assert_eq!(row.thread.root, None, "a DM has one session");

    let before = stack.mock.calls().len();
    stack
        .handle(stack.event("alice", "DM1", ConvKind::Dm, "d2", None, &[]))
        .await;
    let sent = posts(&stack.calls_since(before));
    assert_eq!(stack.session_of(&sent[0].2).await, dm_session);

    for room in ["GENERAL", "RANDOM"] {
        stack.next_turn(Turn::reply("In a channel."));
        stack
            .handle(stack.event(
                "alice",
                room,
                ConvKind::Channel,
                &format!("{room}-1"),
                None,
                &[BOT],
            ))
            .await;
    }
    let mut paths = Vec::new();
    for room in ["GENERAL", "RANDOM"] {
        let volume = stack
            .store()
            .volume(&VolumeKey {
                agent: stack.agent,
                scope: ScopeKey::Channel(conv(room)),
            })
            .await
            .unwrap()
            .expect("a volume per channel");
        paths.push(volume.path);
    }
    assert_ne!(paths[0], paths[1], "two channels, two volumes");
    stack.stop().await;
}

#[tokio::test]
async fn an_owners_dm_whose_member_lookup_fails_is_refused_not_run() {
    use sqlx::Connection as _;
    let stack = start().await;
    let mut db = sqlx::SqliteConnection::connect(&stack._dir.db_url())
        .await
        .unwrap();
    sqlx::raw_sql(
        "PRAGMA foreign_keys = OFF; \
         UPDATE surface_identities SET member_id = 'not-a-member-id' WHERE user_id = 'alice';",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    assert!(
        stack
            .store()
            .member_for_identity(&key("alice"))
            .await
            .is_err(),
        "the owner's member lookup fails"
    );

    stack.next_turn(Turn::reply("Ran anyway."));
    let requests = stack.fake.message_requests().await.len();
    stack
        .handle(stack.event("alice", "DM1", ConvKind::Dm, "d1", None, &[]))
        .await;
    let sent = posts(&stack.calls_since(0));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, in_thread("DM1", None));
    assert_eq!(
        sent[0].1,
        "helper can't check who may use it right now. Try again later."
    );
    assert_eq!(
        stack.fake.message_requests().await.len(),
        requests,
        "no turn ran, on any key"
    );
    stack.stop().await;
}

#[tokio::test]
async fn failures_and_refusals_say_why_and_a_bot_never_joins_a_room() {
    let stack = start().await;

    stack.next_turn(Turn::api_error(429, "You've hit your usage limit."));
    stack
        .handle(stack.event("alice", "GENERAL", ConvKind::Channel, "e1", None, &[BOT]))
        .await;
    let sent = posts(&stack.calls_since(0));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].1, USAGE_LIMIT_TEXT);

    stack.mock.keep_out_of(conv("ELSEWHERE"));
    let before = stack.mock.calls().len();
    stack
        .handle(stack.event("alice", "ELSEWHERE", ConvKind::Channel, "x1", None, &[BOT]))
        .await;
    assert!(
        stack.calls_since(before).is_empty(),
        "a mentioned agent whose bot isn't in the room doesn't answer there"
    );

    let before = stack.mock.calls().len();
    stack
        .handle(stack.event("carol", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    assert!(
        stack.calls_since(before).is_empty(),
        "an unlinked member gets a link prompt, privately"
    );

    let before = stack.mock.calls().len();
    let mut from_bot = stack.event(BOT, "GENERAL", ConvKind::Channel, "b1", None, &[BOT]);
    from_bot.sender_is_bot = true;
    from_bot.sender_bot_user = Some(UserId::new(BOT));
    stack.handle(from_bot).await;
    let mut unaddressed = stack.event("alice", "GENERAL", ConvKind::Channel, "n1", None, &[]);
    unaddressed.text = "just chatting".into();
    stack.handle(unaddressed).await;
    assert!(stack.calls_since(before).is_empty());

    assert!(
        stack
            .store()
            .set_agent_paused(stack.agent, true)
            .await
            .unwrap()
    );
    let before = stack.mock.calls().len();
    stack
        .handle(stack.event("alice", "GENERAL", ConvKind::Channel, "p1", None, &[BOT]))
        .await;
    let sent = posts(&stack.calls_since(before));
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, in_thread("GENERAL", Some("p1")));
    assert!(sent[0].1.contains("paused"), "{}", sent[0].1);

    let caps = Caps {
        per_binding_delivery: true,
        ..MockSurface::DEFAULT_CAPS
    };
    assert!(
        stack
            .store()
            .set_agent_paused(stack.agent, false)
            .await
            .unwrap()
    );
    let before = stack.mock.calls().len();
    stack.next_turn(Turn::reply("Mine."));
    let mut other_binding = stack.event("alice", "GENERAL", ConvKind::Channel, "o1", None, &[BOT]);
    other_binding.binding = BindingId::new_v4();
    stack.pipeline.handle(other_binding, caps).await;
    assert!(
        stack.calls_since(before).is_empty(),
        "with a copy per binding, only the receiving binding's agent is a candidate"
    );
    stack.stop().await;
}

#[tokio::test]
async fn failed_turns_say_why_and_a_hop_bills_the_requester_of_the_turn_that_mentioned() {
    let stack = start().await;
    let store = stack.store();

    stack.next_turn(Turn::api_error(401, "Invalid bearer token"));
    stack
        .handle(stack.event("alice", "GENERAL", ConvKind::Channel, "a1", None, &[BOT]))
        .await;
    let sent = posts(&stack.calls_since(0));
    assert_eq!(sent[0].1, agentd::pipeline::LOGIN_EXPIRED_TEXT);

    let before = stack.mock.calls().len();
    stack.next_turn(Turn::crash());
    stack
        .handle(stack.event("alice", "GENERAL", ConvKind::Channel, "a2", None, &[BOT]))
        .await;
    let sent = posts(&stack.calls_since(before));
    assert_eq!(sent[0].1, agentd::pipeline::FAILED_TEXT);

    let writer = stack.other_agent("writer", "UWRITER").await;
    let bob = store.member_for_identity(&key("bob")).await.unwrap();
    let by_writer = msg("GENERAL", "w1");
    let recording = store.clone();
    let recorded = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        recording
            .record_message_ref(
                &store::NewMessageRef {
                    session: SessionId::new_v4(),
                    msg: &by_writer,
                    thread_root: None,
                    agent: Some(writer),
                    turn: None,
                    requester: &core_types::Requester {
                        member: bob,
                        key: key("bob"),
                    },
                    hop: core_types::Hop(1),
                },
                OffsetDateTime::now_utc(),
            )
            .await
            .unwrap();
    });
    let before = stack.mock.calls().len();
    stack.next_turn(Turn::reply("Picking this up."));
    let mut hop = stack.event("UWRITER", "GENERAL", ConvKind::Channel, "w1", None, &[BOT]);
    hop.sender_is_bot = true;
    hop.sender_bot_user = Some(UserId::new("UWRITER"));
    stack.handle(hop).await;
    recorded.await.unwrap();
    let sent = posts(&stack.calls_since(before));
    assert_eq!(
        sent.len(),
        1,
        "the writer's post is attributed a moment after it arrives, and still starts a turn"
    );
    let attributed = store.posted_message_ref(&sent[0].2).await.unwrap().unwrap();
    assert_eq!(attributed.requester.key, key("bob"), "the hop is bob's");
    assert_eq!(attributed.hop.0, 2);
    stack.stop().await;
}

/// Waits up to 30 seconds for `done`.
async fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn said(id: &str, sender: &str, text: &str) -> Msg {
    Msg {
        id: id.into(),
        sender: key(sender),
        sender_is_bot: sender == BOT,
        text: text.into(),
        files: vec![],
        sent_at: OffsetDateTime::now_utc(),
    }
}

fn thread(conv_id: &str, root: &str) -> ThreadKey {
    ThreadKey {
        conv: conv(conv_id),
        root: Some(root.into()),
    }
}

fn working_on(id: &str) -> Call {
    Call::React {
        msg: msg("GENERAL", id),
        emoji: "hourglass".into(),
    }
}

impl Stack {
    /// Answers `id`, a mention of the agent starting a thread in GENERAL,
    /// with `reply`, and returns the reply's message.
    async fn answered_root(&self, id: &str, reply: &str) -> MsgRef {
        let before = self.mock.calls().len();
        self.next_turn(Turn::reply(reply));
        self.handle(self.event("alice", "GENERAL", ConvKind::Channel, id, None, &[BOT]))
            .await;
        let sent = posts(&self.calls_since(before));
        assert_eq!(sent.len(), 1);
        sent[0].2.clone()
    }

    async fn upstream_bodies_since(&self, from: usize) -> Vec<String> {
        self.fake
            .message_requests()
            .await
            .split_off(from)
            .iter()
            .map(|request| String::from_utf8_lossy(&request.body).into_owned())
            .collect()
    }
}

#[tokio::test]
async fn shutdown_waits_for_a_running_turn_within_the_drain_timeout() {
    let stack = start_with(Setup {
        drain_timeout_secs: 30,
        ..Setup::default()
    })
    .await;
    stack.next_turn(Turn::reply("Finished anyway.").with_delay(Duration::from_millis(1500)));
    let event = stack.event("alice", "GENERAL", ConvKind::Channel, "s1", None, &[BOT]);
    stack
        .pipeline
        .sink(MockSurface::DEFAULT_CAPS)
        .send(event)
        .await
        .unwrap();
    wait_until("the turn runs", || {
        stack.mock.calls().contains(&working_on("s1"))
    })
    .await;
    let Stack {
        app,
        pipeline,
        mock,
        agent,
        stop,
        task,
        _dir: dir,
        ..
    } = stack;
    drop(pipeline);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    let sent = posts(&mock.calls());
    assert_eq!(sent.len(), 1, "{:#?}", mock.calls());
    assert_eq!(sent[0].1, "Finished anyway.");
    let store = agentd::app::open_store(app.config()).await.unwrap();
    let attributed = store
        .posted_message_ref(&sent[0].2)
        .await
        .unwrap()
        .expect("the reply was recorded before the store closed");
    assert_eq!(attributed.agent, Some(agent));
    drop(dir);
}

#[tokio::test]
async fn a_turn_past_the_drain_timeout_is_cut_short_and_its_thread_told() {
    let stack = start_with(Setup {
        drain_timeout_secs: 1,
        ..Setup::default()
    })
    .await;
    stack.next_turn(Turn::reply("Too late.").with_delay(Duration::from_secs(3)));
    let event = stack.event("alice", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]);
    stack
        .pipeline
        .sink(MockSurface::DEFAULT_CAPS)
        .send(event)
        .await
        .unwrap();
    wait_until("the turn runs", || {
        stack.mock.calls().contains(&working_on("c1"))
    })
    .await;
    let Stack {
        pipeline,
        turns,
        mock,
        stop,
        task,
        ..
    } = stack;
    drop(pipeline);
    let started = Instant::now();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "shutdown took {:?}",
        started.elapsed()
    );
    let calls = mock.calls();
    assert!(
        calls.contains(&Call::Unreact {
            msg: msg("GENERAL", "c1"),
            emoji: "hourglass".into()
        }),
        "{calls:#?}"
    );
    let sent = posts(&calls);
    assert_eq!(sent.len(), 1, "{calls:#?}");
    assert_eq!(sent[0].0, in_thread("GENERAL", Some("c1")));
    assert_eq!(sent[0].1, RESTARTING_TEXT);
    turns.sessions().stop_all().await;
}

#[tokio::test]
async fn a_second_signal_while_a_turn_runs_cuts_it_short_at_once() {
    let stack = start_with(Setup {
        drain_timeout_secs: 30,
        ..Setup::default()
    })
    .await;
    stack.next_turn(Turn::reply("Too late.").with_delay(Duration::from_secs(3)));
    let event = stack.event("alice", "GENERAL", ConvKind::Channel, "f1", None, &[BOT]);
    stack
        .pipeline
        .sink(MockSurface::DEFAULT_CAPS)
        .send(event)
        .await
        .unwrap();
    wait_until("the turn runs", || {
        stack.mock.calls().contains(&working_on("f1"))
    })
    .await;
    let Stack {
        pipeline,
        turns,
        mock,
        stop,
        abort,
        task,
        ..
    } = stack;
    drop(pipeline);
    let started = Instant::now();
    stop.send(()).unwrap();
    abort.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "shutdown took {:?}",
        started.elapsed()
    );
    let calls = mock.calls();
    let sent = posts(&calls);
    assert_eq!(sent.len(), 1, "{calls:#?}");
    assert_eq!(sent[0].0, in_thread("GENERAL", Some("f1")));
    assert_eq!(sent[0].1, RESTARTING_TEXT);
    turns.sessions().stop_all().await;
}

#[tokio::test]
async fn a_turn_that_never_ran_forgets_what_it_recorded_and_its_notice_has_no_ref() {
    let stack = start().await;
    let store = stack.store();
    let first = stack.answered_root("r1", "First.").await;
    let s1 = stack.session_of(&first).await;

    let before = stack.mock.calls().len();
    let first_read = Gate::closed();
    let second_read = Gate::closed();
    stack.holds.history_at(thread("GENERAL", "r1"), &first_read);
    stack
        .holds
        .history_at(thread("GENERAL", "r1"), &second_read);
    let handling = stack.handle(stack.event(
        "alice",
        "GENERAL",
        ConvKind::Channel,
        "r2",
        Some("r1"),
        &[BOT],
    ));
    let resetting = async {
        wait_until("the first attempt reads the thread", || {
            first_read.waiting() == 1
        })
        .await;
        let s2 = store
            .reset_session(s1, OffsetDateTime::now_utc())
            .await
            .unwrap()
            .expect("a replacement")
            .id;
        first_read.open();
        wait_until("the second attempt reads the thread", || {
            second_read.waiting() == 1
        })
        .await;
        store
            .reset_session(s2, OffsetDateTime::now_utc())
            .await
            .unwrap();
        second_read.open();
        s2
    };
    let ((), s2) = tokio::join!(handling, resetting);
    let calls = stack.calls_since(before);
    let sent = posts(&calls);
    assert_eq!(sent.len(), 1, "{calls:#?}");
    assert_eq!(sent[0].0, in_thread("GENERAL", Some("r1")));
    assert_eq!(sent[0].1, FAILED_TEXT);
    assert_eq!(
        store.posted_message_ref(&sent[0].2).await.unwrap(),
        None,
        "a notice of agentd's own isn't attributed to the agent"
    );
    for session in [s1, s2] {
        assert_eq!(
            store
                .session_message_ref(session, &msg("GENERAL", "r2"))
                .await
                .unwrap(),
            None,
            "what a turn that never ran recorded is forgotten"
        );
    }
    assert!(calls.contains(&Call::Unreact {
        msg: msg("GENERAL", "r2"),
        emoji: "hourglass".into()
    }));
    stack.stop().await;
}

#[tokio::test]
async fn a_request_whose_turn_crashed_before_the_cli_read_it_reaches_the_next_turn() {
    let stack = start().await;
    let store = stack.store();
    let upstream = stack.fake.message_requests().await.len();
    stack.next_turn(Turn::crash_at_start());
    let mut first = stack.event("alice", "GENERAL", ConvKind::Channel, "r1", None, &[BOT]);
    first.text = "@UBOT do X".into();
    stack.handle(first).await;
    let sent = posts(&stack.calls_since(0));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].0, in_thread("GENERAL", Some("r1")));
    assert_eq!(sent[0].1, FAILED_TEXT);
    assert_eq!(
        store.posted_message_ref(&sent[0].2).await.unwrap(),
        None,
        "the failure isn't recorded as the agent's reply"
    );

    stack.mock.set_history(
        thread("GENERAL", "r1"),
        vec![
            said("r1", "alice", "@UBOT do X"),
            said(sent[0].2.id.as_str(), BOT, FAILED_TEXT),
            said("r2", "alice", "@UBOT try again"),
        ],
    );
    stack.next_turn(Turn::reply("Done."));
    let before = stack.mock.calls().len();
    let mut again = stack.event(
        "alice",
        "GENERAL",
        ConvKind::Channel,
        "r2",
        Some("r1"),
        &[BOT],
    );
    again.text = "@UBOT try again".into();
    stack.handle(again).await;
    let sent = posts(&stack.calls_since(before));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].1, "Done.");
    let bodies = stack.upstream_bodies_since(upstream).await;
    assert_eq!(bodies.len(), 1, "the crashed turn reached no upstream");
    assert!(bodies[0].contains("do X"), "{}", bodies[0]);
    assert!(bodies[0].contains("try again"), "{}", bodies[0]);
    stack.stop().await;
}

#[tokio::test]
async fn a_message_said_while_a_turn_ran_reaches_the_next_turn() {
    let stack = start().await;
    let first = stack.answered_root("r1", "First.").await;
    stack.mock.set_history(
        thread("GENERAL", "r1"),
        vec![
            said("r1", "alice", "@UBOT hello"),
            said("x1", "carol", "said while the turn ran"),
            said(first.id.as_str(), BOT, "First."),
            said("r2", "alice", "@UBOT again"),
        ],
    );
    let upstream = stack.fake.message_requests().await.len();
    stack.next_turn(Turn::reply("Second."));
    stack
        .handle(stack.event(
            "alice",
            "GENERAL",
            ConvKind::Channel,
            "r2",
            Some("r1"),
            &[BOT],
        ))
        .await;
    let bodies = stack.upstream_bodies_since(upstream).await;
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].contains("said while the turn ran"),
        "{}",
        bodies[0]
    );
    assert!(
        !bodies[0].contains("outside this session"),
        "the session's own reply isn't shown again: {}",
        bodies[0]
    );
    stack.stop().await;
}

#[tokio::test]
async fn messages_in_a_thread_are_answered_once_each_in_arrival_order() {
    let stack = start().await;
    let first = stack.answered_root("r1", "First.").await;
    stack.mock.set_history(
        thread("GENERAL", "r1"),
        vec![
            said("r1", "alice", "@UBOT hello"),
            said(first.id.as_str(), BOT, "First."),
            said("q2", "alice", "@UBOT question two"),
            said("q3", "alice", "@UBOT question three"),
        ],
    );
    let upstream = stack.fake.message_requests().await.len();
    let before = stack.mock.calls().len();
    stack.next_turn(Turn::reply("Answer."));
    let reading = Gate::closed();
    stack.holds.history_at(thread("GENERAL", "r1"), &reading);
    let sink = stack.pipeline.sink(MockSurface::DEFAULT_CAPS);
    for (id, text) in [("q2", "question two"), ("q3", "question three")] {
        let mut event = stack.event(
            "alice",
            "GENERAL",
            ConvKind::Channel,
            id,
            Some("r1"),
            &[BOT],
        );
        event.text = format!("@UBOT {text}");
        sink.send(event).await.unwrap();
        if id == "q2" {
            wait_until("question two's turn reads the thread", || {
                reading.waiting() == 1
            })
            .await;
        }
    }
    reading.open();
    wait_until("both are answered", || {
        posts(&stack.calls_since(before)).len() >= 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(posts(&stack.calls_since(before)).len(), 2);
    let bodies = stack.upstream_bodies_since(upstream).await;
    assert_eq!(bodies.len(), 2, "one turn each");
    assert!(bodies[0].contains("question two"), "{}", bodies[0]);
    assert!(!bodies[0].contains("question three"), "{}", bodies[0]);
    assert!(bodies[1].contains("question three"), "{}", bodies[1]);
    assert!(
        !bodies[1].contains("question two"),
        "the first message was answered by its own turn: {}",
        bodies[1]
    );
    stack.stop().await;
}

#[tokio::test]
async fn a_failed_reply_post_still_delivers_the_rest_and_says_so() {
    let stack = start().await;
    stack.next_turn(
        Turn::reply("Here. [[react: eyes]]")
            .with_command(["agentctl", "post", "--to", "GENERAL", "A note."]),
    );
    stack
        .mock
        .fail_next(Op::Post, SurfaceError::Api("boom".into()));
    stack
        .handle(stack.event("alice", "GENERAL", ConvKind::Channel, "d1", None, &[BOT]))
        .await;
    let calls = stack.calls_since(0);
    assert!(
        calls.contains(&Call::React {
            msg: msg("GENERAL", "d1"),
            emoji: "eyes".into()
        }),
        "{calls:#?}"
    );
    let sent: Vec<_> = posts(&calls)
        .into_iter()
        .map(|(to, text, _)| (to, text))
        .collect();
    assert_eq!(
        sent,
        [
            (in_thread("GENERAL", None), "A note.".to_owned()),
            (
                in_thread("GENERAL", Some("d1")),
                DELIVERY_FAILED_TEXT.to_owned()
            ),
        ]
    );

    let before = stack.mock.calls().len();
    stack.next_turn(Turn::reply("Again."));
    stack.mock.fail_next(
        Op::Post,
        SurfaceError::RateLimited {
            retry_after: Duration::from_millis(10),
        },
    );
    stack
        .handle(stack.event("alice", "GENERAL", ConvKind::Channel, "d2", None, &[BOT]))
        .await;
    let sent = posts(&stack.calls_since(before));
    assert_eq!(sent.len(), 1, "a rate-limited post is tried again");
    assert_eq!(sent[0].1, "Again.");
    stack.stop().await;
}

#[tokio::test]
async fn past_the_queue_bounds_a_message_gets_one_busy_line() {
    let stack = start_with(Setup {
        pipeline: |settings| {
            settings.queue_per_thread = 0;
            settings.max_pending = 2;
        },
        ..Setup::default()
    })
    .await;
    stack.next_turn(Turn::reply("Done.").with_delay(Duration::from_millis(1500)));
    let sink = stack.pipeline.sink(MockSurface::DEFAULT_CAPS);
    sink.send(stack.event("alice", "GENERAL", ConvKind::Channel, "b1", None, &[BOT]))
        .await
        .unwrap();
    wait_until("the first turn runs", || {
        stack.mock.calls().contains(&working_on("b1"))
    })
    .await;
    sink.send(stack.event(
        "alice",
        "GENERAL",
        ConvKind::Channel,
        "b2",
        Some("b1"),
        &[BOT],
    ))
    .await
    .unwrap();
    sink.send(stack.event("alice", "GENERAL", ConvKind::Channel, "b3", None, &[BOT]))
        .await
        .unwrap();
    sink.send(stack.event("alice", "GENERAL", ConvKind::Channel, "b4", None, &[BOT]))
        .await
        .unwrap();
    let busy: Vec<_> = posts(&stack.mock.calls())
        .into_iter()
        .map(|(to, text, _)| (to, text))
        .collect();
    let line = "helper is busy with other requests. Ask again in a few minutes.".to_owned();
    assert_eq!(
        busy,
        [
            (in_thread("GENERAL", Some("b1")), line.clone()),
            (in_thread("GENERAL", Some("b4")), line),
        ],
        "one line for the message past its thread's queue, one for the message past the total"
    );
    wait_until("the two messages taken are answered", || {
        stack
            .mock
            .posts()
            .iter()
            .filter(|(_, text)| text == "Done.")
            .count()
            == 2
    })
    .await;
    stack.stop().await;
}

#[tokio::test]
async fn a_reply_still_being_delivered_at_the_drain_timeout_is_cut_short_and_its_thread_told() {
    let stack = start_with(Setup {
        drain_timeout_secs: 1,
        ..Setup::default()
    })
    .await;
    stack.next_turn(Turn::reply("Posted too late."));
    let posting = Gate::closed();
    stack.holds.posts_of("Posted too late.", &posting);
    let event = stack.event("alice", "GENERAL", ConvKind::Channel, "c2", None, &[BOT]);
    stack
        .pipeline
        .sink(MockSurface::DEFAULT_CAPS)
        .send(event)
        .await
        .unwrap();
    wait_until("the reply is being delivered", || posting.waiting() == 1).await;
    let Stack {
        pipeline,
        turns,
        mock,
        stop,
        task,
        ..
    } = stack;
    drop(pipeline);
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    let calls = mock.calls();
    assert!(
        calls.contains(&Call::Unreact {
            msg: msg("GENERAL", "c2"),
            emoji: "hourglass".into()
        }),
        "{calls:#?}"
    );
    let sent = posts(&calls);
    assert_eq!(sent.len(), 1, "{calls:#?}");
    assert_eq!(sent[0].0, in_thread("GENERAL", Some("c2")));
    assert_eq!(sent[0].1, RESTARTING_TEXT);
    turns.sessions().stop_all().await;
}

#[tokio::test]
async fn a_bot_past_the_queue_bounds_gets_no_busy_line() {
    let stack = start_with(Setup {
        pipeline: |settings| settings.max_pending = 0,
        ..Setup::default()
    })
    .await;
    let mut from_bot = stack.event("UOTHERBOT", "DM1", ConvKind::Dm, "o1", None, &[]);
    from_bot.sender_is_bot = true;
    stack.handle(from_bot).await;
    stack.other_agent("writer", "UWRITER").await;
    stack
        .handle(stack.event("UWRITER", "GENERAL", ConvKind::Channel, "o2", None, &[BOT]))
        .await;
    assert!(
        posts(&stack.mock.calls()).is_empty(),
        "{:#?}",
        stack.mock.calls()
    );
    stack
        .handle(stack.event("alice", "DM1", ConvKind::Dm, "o3", None, &[]))
        .await;
    let sent = posts(&stack.mock.calls());
    assert_eq!(sent.len(), 1, "a person still hears the agent is busy");
    assert_eq!(
        sent[0].1,
        "helper is busy with other requests. Ask again in a few minutes."
    );
    stack.stop().await;
}

#[tokio::test]
async fn an_agents_post_that_names_no_other_agent_holds_no_lane_up() {
    let stack = start().await;
    let first = stack.answered_root("r1", "First.").await;
    stack.other_agent("writer", "UWRITER").await;
    let before = stack.mock.calls().len();
    let mut upload = stack.event(
        "UWRITER",
        "GENERAL",
        ConvKind::Channel,
        "f1",
        Some(first.id.as_str()),
        &[],
    );
    upload.sender_bot_user = Some(UserId::new("UWRITER"));
    upload.text = String::new();
    let started = Instant::now();
    stack.handle(upload).await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the unattributed post was ignored without waiting for its attribution: {:?}",
        started.elapsed()
    );
    assert!(posts(&stack.calls_since(before)).is_empty());
    stack.stop().await;
}

#[tokio::test]
async fn only_an_attributed_post_of_the_bot_is_shown_as_from_outside_the_session() {
    let stack = start().await;
    let first = stack.answered_root("r1", "First.").await;
    let alice = stack
        .store()
        .member_for_identity(&key("alice"))
        .await
        .unwrap();
    stack
        .store()
        .record_message_ref(
            &store::NewMessageRef {
                session: SessionId::new_v4(),
                msg: &msg("GENERAL", "p1"),
                thread_root: Some(&MessageId::new("r1")),
                agent: Some(stack.agent),
                turn: None,
                requester: &core_types::Requester {
                    member: alice,
                    key: key("alice"),
                },
                hop: core_types::Hop::ZERO,
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    stack.mock.set_history(
        thread("GENERAL", "r1"),
        vec![
            said("r1", "alice", "@UBOT hello"),
            said(first.id.as_str(), BOT, "First."),
            said("n1", BOT, "report.txt"),
            said("p1", BOT, "a private task's result"),
            said("r2", "alice", "@UBOT again"),
        ],
    );
    let upstream = stack.fake.message_requests().await.len();
    stack.next_turn(Turn::reply("Second."));
    stack
        .handle(stack.event(
            "alice",
            "GENERAL",
            ConvKind::Channel,
            "r2",
            Some("r1"),
            &[BOT],
        ))
        .await;
    let bodies = stack.upstream_bodies_since(upstream).await;
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("] you: report.txt"), "{}", bodies[0]);
    assert!(
        bodies[0].contains("] you, outside this session: a private task's result"),
        "{}",
        bodies[0]
    );
    stack.stop().await;
}

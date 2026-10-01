//! The turn pipeline end to end: messages from a `MockSurface`, turns in
//! `fake-claude` in a process sandbox, the real credential proxy and
//! agentctl API on agentd's listeners, and `fake_anthropic()` upstream.

mod common;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentd::commands::{ManagerBot, OpenDm, Origin, Replies};
use agentd::ctl::SurfaceLookup;
use agentd::pipeline::{
    DELIVERY_FAILED_TEXT, FAILED_TEXT, Pipeline, PipelineSettings, RESTARTING_TEXT, TurnSettings,
    Turns, UNCONFIRMED_TEXT, USAGE_LIMIT_TEXT,
};
use agentd::server::{Routers, Server};
use agentd::skills::{Added, Confirmed, Source};
use agentd::{App, Config};
use core_types::{
    AgentId, Binding, BindingId, Caps, ConvKind, ConvRef, Cursor, InboundEvent, MemberId,
    MemberKey, MessageId, Msg, MsgRef, OutFile, Posted, ReplyTarget, ScopeKey, Sender, SessionId,
    Surface, SurfaceError, SurfaceKind, ThreadKey, UserId, VolumeKey,
};
use runner::{PoolConfig, ProcessConfig};
use sandbox::ProcessSandbox;
use secrecy::SecretString;
use store::{AgentCreation, NewAgent, NewClaudeLink, Store, StoreError, Visibility};
use testkit::{
    Call, FakeAnthropic, MockSurface, Op, Turn, agentctl_path, fake_anthropic, fake_claude_path,
};
use time::OffsetDateTime;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use common::{TempDir, env};

const TEAM: &str = "chat.example";
const BOT: &str = "UBOT";

/// The pipeline's clock in these tests: one instant, so a test's turns and
/// counts never fall in two hours or days.
fn pinned_now() -> OffsetDateTime {
    time::macros::datetime!(2030-06-15 12:30 UTC)
}

/// Every agent's bot acts through the one mock, past the [`Holds`].
#[derive(Debug)]
struct Mocks {
    mock: Arc<MockSurface>,
    holds: Arc<Holds>,
}

#[async_trait::async_trait]
impl SurfaceLookup for Mocks {
    async fn surface(
        &self,
        agent: AgentId,
        _conv: &ConvRef,
    ) -> Result<Option<Arc<dyn Surface>>, StoreError> {
        Ok(Some(Arc::new(Held {
            mock: self.mock.clone(),
            holds: self.holds.clone(),
            agent,
        })))
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
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let _ = open.wait_for(|open| *open).await;
        self.waiting.fetch_sub(1, Ordering::SeqCst);
    }
}

/// What the agents' surfaces hold up or fail, besides the mock's own.
#[derive(Debug, Default)]
struct Holds {
    confirms: Mutex<HashMap<AgentId, Gate>>,
    failing_confirms: Mutex<VecDeque<SurfaceError>>,
    failing_can_posts: Mutex<HashMap<AgentId, usize>>,
    posts: Mutex<HashMap<String, Gate>>,
}

impl Holds {
    /// Holds `agent`'s confirmations at `gate`.
    fn confirms_of(&self, agent: AgentId, gate: &Gate) {
        self.confirms.lock().unwrap().insert(agent, gate.clone());
    }

    /// Makes the next confirmation fail with `error`.
    fn fail_next_confirm(&self, error: SurfaceError) {
        self.failing_confirms.lock().unwrap().push_back(error);
    }

    /// Makes `agent`'s next `times` checks of whether its bot may post fail,
    /// as the platform being unreachable would.
    fn fail_can_post(&self, agent: AgentId, times: usize) {
        self.failing_can_posts.lock().unwrap().insert(agent, times);
    }

    /// How many failures [`fail_next_confirm`](Self::fail_next_confirm)
    /// queued are still to happen.
    fn confirm_failures_left(&self) -> usize {
        self.failing_confirms.lock().unwrap().len()
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
    agent: AgentId,
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

    async fn post(&self, to: &ReplyTarget, text: &str) -> Result<Posted, SurfaceError> {
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
        if let Some(left) = self
            .holds
            .failing_can_posts
            .lock()
            .unwrap()
            .get_mut(&self.agent)
            && *left > 0
        {
            *left -= 1;
            return Err(SurfaceError::Transport("unreachable".into()));
        }
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
        self.mock.history(thread, before, limit).await
    }

    async fn confirm(&self, event: &InboundEvent) -> Result<Option<InboundEvent>, SurfaceError> {
        if let Some(error) = self.holds.failing_confirms.lock().unwrap().pop_front() {
            return Err(error);
        }
        let gate = self
            .holds
            .confirms
            .lock()
            .unwrap()
            .get(&self.agent)
            .cloned();
        if let Some(gate) = gate {
            gate.pass().await;
        }
        self.mock.confirm(event).await
    }

    fn render(&self, markdown: &str) -> Vec<String> {
        self.mock.render(markdown)
    }

    fn caps(&self) -> Caps {
        self.mock.caps()
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

struct Stack {
    app: App,
    pipeline: Pipeline,
    turns: Turns,
    mock: Arc<MockSurface>,
    manager: Arc<MockSurface>,
    holds: Arc<Holds>,
    script: PathBuf,
    agent: AgentId,
    binding: BindingId,
    alice: MemberId,
    stop: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
    fake: FakeAnthropic,
    dir: TempDir,
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
    let dir = TempDir::new();
    let fake = fake_anthropic().await;
    let text = format!(
        "{}\n[proxy]\nupstream = \"{}\"\n[runner]\nworking_emoji = \"hourglass\"\n",
        common::CONFIG
            .replace("/nonexistent/agentd", &dir.path().display().to_string())
            .replace(
                "sqlite::memory:",
                &format!("sqlite://{}", dir.path().join("agentd.db").display())
            )
            .replace(
                "drain_timeout_secs = 5",
                &format!("drain_timeout_secs = {}", setup.drain_timeout_secs)
            ),
        fake.uri()
    );
    let config = Config::parse(&text, env()).unwrap();
    let store = agentd::app::open_store(&config).await.unwrap();
    let mock = Arc::new(MockSurface::new());
    for name in [BOT, "helper"] {
        mock.name_user(name, UserId::new(BOT));
    }
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
    let sandbox = ProcessSandbox::new(store.clone(), dir.path()).unwrap();
    let turns = Turns::start(&app, Arc::new(sandbox), settings).unwrap();
    let mut pipeline_settings = PipelineSettings::from_app(&app);
    pipeline_settings.now = pinned_now;
    (setup.pipeline)(&mut pipeline_settings);
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
        pipeline_settings,
    );
    let server = server.with_pipeline(pipeline.clone());
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(
        async {
            let _ = stopped.await;
        },
        std::future::pending(),
    ));
    Stack {
        app,
        pipeline,
        turns,
        mock,
        manager,
        holds,
        script,
        agent: agent.id,
        binding,
        alice,
        stop,
        task,
        fake,
        dir,
    }
}

impl Stack {
    fn store(&self) -> &Store {
        self.app.store()
    }

    /// The turns billed no cost, by the reason the `usage` table records,
    /// as an operator counts them.
    async fn unbilled_turns(&self) -> Vec<(String, i64)> {
        let db = self.dir.path().join("agentd.db");
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=ro", db.display()))
            .await
            .unwrap();
        let rows = sqlx::query_as(
            "SELECT cost_unknown, SUM(turns) FROM usage WHERE cost_unknown != '' \
             GROUP BY cost_unknown ORDER BY cost_unknown",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        pool.close().await;
        rows
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

    /// Alice's second agent, `name`, whose bot is `bot`.
    async fn other_agent(&self, name: &str, bot: &str) -> AgentId {
        self.agent_of(self.alice, name, bot).await
    }

    /// `owner`'s agent `name`, whose bot is `bot`.
    async fn agent_of(&self, owner: MemberId, name: &str, bot: &str) -> AgentId {
        let store = self.store();
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
        for handle in [name, bot] {
            self.mock.name_user(handle, UserId::new(bot));
        }
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
async fn skills_are_in_every_session_and_their_confirmed_hosts_extend_egress() {
    let stack = start().await;
    let skill =
        "---\nname: docs\ndescription: Read the docs.\nallowed-hosts: [docs.skill.invalid]\n---\n";
    let added = stack
        .app
        .skills()
        .add(
            stack.agent,
            Source::Upload {
                name: "SKILL.md",
                bytes: skill.as_bytes(),
            },
            stack.alice,
        )
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(added, Added::Pending(_)), "{added:?}");
    assert!(matches!(
        stack
            .app
            .skills()
            .confirm(stack.agent, "docs")
            .await
            .unwrap(),
        Confirmed::Active(_)
    ));

    let probe = "{ ls \"$CLAUDE_CONFIG_DIR/skills\"; sed -n 2p \"$CLAUDE_CONFIG_DIR/skills/docs/SKILL.md\"; \
                 for host in docs.skill.invalid other.skill.invalid; do \
                 curl -s -o /dev/null -w '%{http_connect}\\n' --proxy \"$ANTHROPIC_BASE_URL\" \"https://$host/\"; \
                 done; } > skills.txt 2>&1; agentctl attach skills.txt";
    stack.next_turn(Turn::reply("Checked.").with_command(["sh", "-c", probe]));
    stack
        .handle(stack.event("alice", "GENERAL", ConvKind::Channel, "s1", None, &[BOT]))
        .await;
    let calls = stack.calls_since(0);
    let uploaded = calls
        .iter()
        .find_map(|call| match call {
            Call::Upload { files, .. } => {
                Some(String::from_utf8(files[0].contents.clone()).unwrap())
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no upload: {calls:#?}"));
    assert_eq!(
        uploaded, "agentctl\ndocs\nname: docs\n502\n403\n",
        "both skills are in the session; the skill's host passes the allowlist and fails only \
         to resolve, another host is refused"
    );
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
                    turn: Some(core_types::TurnId::new_v4()),
                    requester: &core_types::Requester {
                        member: bob,
                        key: key("bob"),
                    },
                    hop: core_types::Hop(1),
                    consent: None,
                    hands_off: true,
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

#[tokio::test]
async fn a_turn_is_billed_to_its_requester_and_counted_in_its_thread() {
    let stack = start().await;
    let store = stack.store();
    let bob = store
        .member_for_identity(&key("bob"))
        .await
        .unwrap()
        .unwrap();

    stack.next_turn(Turn::reply("Done."));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "m1", None, &[BOT]))
        .await;
    let billed = store.member_usage(bob, pinned_now()).await.unwrap().today;
    assert_eq!(
        (billed.turns, billed.input_tokens, billed.output_tokens),
        (1, 10, 1),
        "fake-claude's usage for one reply"
    );
    assert_eq!(billed.cost_usd, testkit::claude::REPLY_COST_USD);
    assert_eq!(
        store
            .member_usage(stack.alice, pinned_now())
            .await
            .unwrap()
            .today
            .turns,
        0,
        "the owner pays nothing for bob's turn"
    );
    let spend = store
        .thread_spend(&thread("GENERAL", "m1"), pinned_now())
        .await
        .unwrap();
    assert_eq!((spend.turns_this_hour, spend.tokens_today), (1, 11));
    assert_eq!(
        store
            .capped_turns_on(stack.agent, pinned_now())
            .await
            .unwrap(),
        1
    );

    stack.next_turn(Turn::crash());
    stack
        .handle(stack.event("carol", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    assert!(
        store
            .member_for_identity(&key("carol"))
            .await
            .unwrap()
            .is_none(),
        "an unlinked member without the community key runs nothing and isn't billed"
    );

    stack.next_turn(Turn {
        commands: vec![vec!["sh".into(), "-c".into(), "kill -9 $PPID".into()]],
        ..Turn::reply("Never said.")
    });
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "k1", None, &[BOT]))
        .await;
    let billed = store.member_usage(bob, pinned_now()).await.unwrap().today;
    assert_eq!(
        (billed.turns, billed.input_tokens, billed.output_tokens),
        (2, 20, 2),
        "a turn whose CLI the agent killed is billed what its messages used"
    );
    assert_eq!(billed.cost_usd, testkit::claude::REPLY_COST_USD);
    assert_eq!(
        stack.unbilled_turns().await,
        [("no_result".to_owned(), 1)],
        "and recorded as a turn of unknown cost"
    );
    let spend = store
        .thread_spend(&thread("GENERAL", "k1"), pinned_now())
        .await
        .unwrap();
    assert_eq!((spend.turns_this_hour, spend.tokens_today), (1, 11));
    stack.stop().await;
}

#[tokio::test]
async fn past_the_daily_cap_the_thread_is_told_once_and_the_owner_still_runs() {
    let stack = start().await;
    let store = stack.store();
    store
        .update_agent_settings(stack.agent, |settings| settings.turns_per_day = Some(1))
        .await
        .unwrap();
    stack.next_turn(Turn::reply("Once."));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "d1", None, &[BOT]))
        .await;
    assert_eq!(posts(&stack.calls_since(0))[0].1, "Once.");

    let before = stack.mock.calls().len();
    stack
        .handle(stack.event(
            "bob",
            "GENERAL",
            ConvKind::Channel,
            "d2",
            Some("d1"),
            &[BOT],
        ))
        .await;
    stack
        .handle(stack.event(
            "bob",
            "GENERAL",
            ConvKind::Channel,
            "d3",
            Some("d1"),
            &[BOT],
        ))
        .await;
    let sent = posts(&stack.calls_since(before));
    assert_eq!(sent.len(), 1, "one notice per thread and day: {sent:?}");
    assert_eq!(sent[0].0, in_thread("GENERAL", Some("d1")));
    assert_eq!(
        sent[0].1,
        "helper has reached the daily limit its owner set on requests from others (1). Try \
         again after midnight UTC."
    );

    let before = stack.mock.calls().len();
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "e1", None, &[BOT]))
        .await;
    assert_eq!(
        posts(&stack.calls_since(before)).len(),
        1,
        "another thread is told too"
    );

    let before = stack.mock.calls().len();
    stack.next_turn(Turn::reply("Still mine."));
    stack
        .handle(stack.event(
            "alice",
            "GENERAL",
            ConvKind::Channel,
            "d4",
            Some("d1"),
            &[BOT],
        ))
        .await;
    assert_eq!(posts(&stack.calls_since(before))[0].1, "Still mine.");
    stack.stop().await;
}

#[tokio::test]
async fn the_owners_own_turns_leave_the_daily_cap_to_others() {
    let stack = start().await;
    let store = stack.store();
    store
        .update_agent_settings(stack.agent, |settings| settings.turns_per_day = Some(1))
        .await
        .unwrap();
    stack.next_turn(Turn::reply("Mine."));
    stack
        .handle(stack.event("alice", "GENERAL", ConvKind::Channel, "o1", None, &[BOT]))
        .await;
    stack.next_turn(Turn::reply("Bob's."));
    let before = stack.mock.calls().len();
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "o2", None, &[BOT]))
        .await;
    let sent = posts(&stack.calls_since(before));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].1, "Bob's.");
    stack.stop().await;
}

#[tokio::test]
async fn bans_and_deny_rules_refuse_a_requester_privately_once_a_day() {
    let stack = start().await;
    let store = stack.store();
    let bob = store
        .member_for_identity(&key("bob"))
        .await
        .unwrap()
        .unwrap();
    store
        .ban_member(bob, &key("root"), None, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let writer = stack.other_agent("writer", "UWRITER").await;
    for id in ["b1", "b2"] {
        stack
            .handle(stack.event(
                "bob",
                "GENERAL",
                ConvKind::Channel,
                id,
                None,
                &[BOT, "UWRITER"],
            ))
            .await;
    }
    assert!(
        posts(&stack.calls_since(0)).is_empty(),
        "nothing is said in the thread"
    );
    assert_eq!(
        stack.dms_to("bob"),
        [
            "A community admin banned you, so agents won't take your requests. Send `me` to me \
          to see why."
        ],
        "one message a day, however many agents and messages"
    );
    store.unban_member(bob).await.unwrap();

    let mut rules = agentd::policy::Rules::default();
    rules.deny(agentd::policy::Rule::Member {
        key: key("bob"),
        member: Some(bob),
        label: "@bob".into(),
    });
    for agent in [stack.agent, writer] {
        store
            .update_agent_settings(agent, |settings| rules.write(settings))
            .await
            .unwrap();
    }
    let before = stack.mock.calls().len();
    for id in ["d1", "d2"] {
        stack
            .handle(stack.event(
                "bob",
                "GENERAL",
                ConvKind::Channel,
                id,
                None,
                &[BOT, "UWRITER"],
            ))
            .await;
    }
    assert!(posts(&stack.calls_since(before)).is_empty());
    let mut told = stack.dms_to("bob")[1..].to_vec();
    told.sort();
    assert_eq!(
        told,
        [
            "helper's owner hasn't allowed you to use it where you asked it.",
            "writer's owner hasn't allowed you to use it where you asked it.",
        ],
        "once a day for each agent"
    );

    store
        .update_agent_settings(stack.agent, |settings| settings.allow_json = "[oops".into())
        .await
        .unwrap();
    let before = stack.mock.calls().len();
    stack
        .handle(stack.event("alice", "GENERAL", ConvKind::Channel, "b3", None, &[BOT]))
        .await;
    let sent = posts(&stack.calls_since(before));
    assert_eq!(
        sent[0].1, "helper can't check who may use it right now. Try again later.",
        "rules that don't read refuse, the owner too"
    );
    stack.stop().await;
}

#[tokio::test]
async fn a_ban_never_holds_back_a_community_admin() {
    let stack = start_with(Setup {
        pipeline: |settings| settings.admins = vec![key("bob")],
        ..Setup::default()
    })
    .await;
    let store = stack.store();
    let bob = store
        .member_for_identity(&key("bob"))
        .await
        .unwrap()
        .unwrap();
    store
        .ban_member(bob, &key("root"), None, OffsetDateTime::now_utc())
        .await
        .unwrap();
    stack.next_turn(Turn::reply("For the admin."));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "adm1", None, &[BOT]))
        .await;
    let sent = posts(&stack.calls_since(0));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].1, "For the admin.");
    assert!(stack.dms_to("bob").is_empty());
    stack.stop().await;
}

/// `writer`'s post `id` in GENERAL, mentioning helper, as the platform
/// delivers it, recorded as a post of a turn of its own that answered bob
/// at hop 1.
async fn writers_post_for_bob(
    stack: &Stack,
    writer: AgentId,
    bob: MemberId,
    id: &str,
) -> InboundEvent {
    let store = stack.store();
    store
        .record_message_ref(
            &store::NewMessageRef {
                session: SessionId::new_v4(),
                msg: &msg("GENERAL", id),
                thread_root: None,
                agent: Some(writer),
                turn: Some(core_types::TurnId::new_v4()),
                requester: &core_types::Requester {
                    member: Some(bob),
                    key: key("bob"),
                },
                hop: core_types::Hop(1),
                consent: None,
                hands_off: true,
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let mut hop = stack.event("UWRITER", "GENERAL", ConvKind::Channel, id, None, &[BOT]);
    hop.sender_is_bot = true;
    hop.sender_bot_user = Some(UserId::new("UWRITER"));
    hop
}

/// Bob's member id.
async fn bob(stack: &Stack) -> MemberId {
    stack
        .store()
        .member_for_identity(&key("bob"))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_hop_refused_for_its_requester_tells_no_one() {
    let stack = start().await;
    let store = stack.store();
    let bob = bob(&stack).await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let mut rules = agentd::policy::Rules::default();
    rules.deny(agentd::policy::Rule::Member {
        key: key("bob"),
        member: Some(bob),
        label: "@bob".into(),
    });
    store
        .update_agent_settings(stack.agent, |settings| rules.write(settings))
        .await
        .unwrap();
    let first = writers_post_for_bob(&stack, writer, bob, "w1").await;
    let second = writers_post_for_bob(&stack, writer, bob, "w2").await;
    stack.handle(first).await;
    store
        .ban_member(bob, &key("root"), None, OffsetDateTime::now_utc())
        .await
        .unwrap();
    stack.handle(second).await;
    assert!(
        posts(&stack.calls_since(0)).is_empty(),
        "nothing is said in the thread"
    );
    assert_eq!(
        stack.dms_to("bob"),
        Vec::<String>::new(),
        "bob never addressed the agent that refused him"
    );
    stack.stop().await;
}

#[tokio::test]
async fn a_hops_requester_without_a_link_is_asked_to_link_once() {
    let stack = start().await;
    let bob = bob(&stack).await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let first = writers_post_for_bob(&stack, writer, bob, "w1").await;
    let second = writers_post_for_bob(&stack, writer, bob, "w2").await;
    assert!(stack.store().delete_claude_link(bob).await.unwrap());
    stack.handle(first).await;
    stack.handle(second).await;
    assert_eq!(
        stack.dms_to("bob"),
        [
            "helper runs on the Claude account of whoever asks it. Link yours to use it: send \
          `login` to me here."
        ],
        "a chain of agents fanning out prompts bob once"
    );
    assert!(posts(&stack.calls_since(0)).is_empty());
    stack.stop().await;
}

#[tokio::test]
async fn a_hop_refused_because_the_rules_dont_read_can_still_run_from_another_copy() {
    let stack = start().await;
    let store = stack.store();
    let bob = bob(&stack).await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let post = writers_post_for_bob(&stack, writer, bob, "w1").await;
    let rules = store
        .update_agent_settings(stack.agent, |settings| {
            std::mem::replace(&mut settings.allow_json, "[oops".into())
        })
        .await
        .unwrap();
    for _ in 0..3 {
        stack.handle(post.clone()).await;
    }
    store
        .update_agent_settings(stack.agent, |settings| settings.allow_json = rules)
        .await
        .unwrap();
    stack.next_turn(Turn::reply("On it."));
    stack.handle(post).await;
    let texts: Vec<String> = posts(&stack.calls_since(0))
        .into_iter()
        .map(|(_, text, _)| text)
        .collect();
    assert_eq!(
        texts,
        [
            "helper can't check who may use it right now. Try again later.",
            "On it."
        ],
        "the refusal claimed nothing, so a later copy ran the hop; and it was said once in \
         the thread, however many copies met it"
    );
    stack.stop().await;
}

#[tokio::test]
async fn a_bots_message_that_cant_be_checked_gets_no_ask_to_try_again() {
    let stack = start_with(Setup {
        pipeline: |settings| settings.attribution_wait = Duration::from_secs(30),
        ..Setup::default()
    })
    .await;
    stack.other_agent("writer", "UWRITER").await;
    let reply = "@UWRITER over to you.";
    stack.next_turn(Turn::reply(reply));
    let gate = Gate::closed();
    stack.holds.posts_of(reply, &gate);
    let sink = stack.pipeline.sink(MockSurface::DEFAULT_CAPS);
    sink.send(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await
        .unwrap();
    wait_until("helper's reply waits to be posted", || gate.waiting() == 1).await;
    stack.holds.fail_next_confirm(SurfaceError::RateLimited {
        retry_after: Duration::from_secs(30),
    });
    sink.send(stack.agents_post(BOT, "m1", "c1", &["UWRITER"]))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    gate.open();
    stack.wait_for_posts(2).await;
    stack.pipeline.close();
    stack.pipeline.drain().await;
    assert_eq!(
        stack.holds.confirm_failures_left(),
        0,
        "the platform's copy was read back, and that failed"
    );
    let texts: Vec<String> = stack
        .mock
        .posts()
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    assert_eq!(
        texts,
        [reply, reply],
        "the platform's copy that couldn't be checked is dropped without a word, and \
         agentd's own copy runs the hop"
    );
    stack.stop().await;
}

#[tokio::test]
async fn the_thread_turn_cap_stops_a_thread_but_not_a_dm() {
    let stack = start_with(Setup {
        pipeline: |settings| settings.limits.thread_turns_per_hour = Some(1),
        ..Setup::default()
    })
    .await;
    stack.next_turn(Turn::reply("First."));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "t1", None, &[BOT]))
        .await;
    let before = stack.mock.calls().len();
    for id in ["t2", "t3"] {
        stack
            .handle(stack.event(
                "alice",
                "GENERAL",
                ConvKind::Channel,
                id,
                Some("t1"),
                &[BOT],
            ))
            .await;
    }
    let sent = posts(&stack.calls_since(before));
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(
        sent[0].1,
        "helper won't answer here for now: agents have reached this thread's hourly turn \
         limit (1). Try again next hour."
    );

    let before = stack.mock.calls().len();
    stack.next_turn(Turn::reply("In the DM."));
    for id in ["dm1", "dm2"] {
        stack
            .handle(stack.event("alice", "DM-ALICE", ConvKind::Dm, id, None, &[]))
            .await;
    }
    let sent = posts(&stack.calls_since(before));
    assert_eq!(
        sent.iter()
            .map(|(_, text, _)| text.as_str())
            .collect::<Vec<_>>(),
        ["In the DM.", "In the DM."],
        "a one-to-one DM isn't capped"
    );
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
        dir,
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
    stack.next_turn(Turn::reply("Too late.").with_delay(Duration::from_secs(20)));
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
}

#[tokio::test]
async fn a_turn_that_never_ran_forgets_what_it_recorded_and_its_notice_has_no_ref() {
    let stack = start().await;
    let store = stack.store();
    let first = stack.answered_root("r1", "First.").await;
    let s1 = stack.session_of(&first).await;

    let before = stack.mock.calls().len();
    let delay = Duration::from_secs(2);
    stack.mock.delay_next(Op::History, delay);
    stack.mock.delay_next(Op::History, delay);
    let handling = stack.handle(stack.event(
        "alice",
        "GENERAL",
        ConvKind::Channel,
        "r2",
        Some("r1"),
        &[BOT],
    ));
    let resetting = async {
        wait_until("the second turn starts", || {
            stack.calls_since(before).contains(&working_on("r2"))
        })
        .await;
        let s2 = store
            .reset_session(s1, OffsetDateTime::now_utc())
            .await
            .unwrap()
            .expect("a replacement")
            .id;
        wait_until("the first read of the thread ends", || {
            stack
                .calls_since(before)
                .iter()
                .any(|call| matches!(call, Call::History { .. }))
        })
        .await;
        tokio::time::sleep(delay / 2).await;
        store
            .reset_session(s2, OffsetDateTime::now_utc())
            .await
            .unwrap();
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
    stack.mock.delay_next(Op::History, Duration::from_secs(1));
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
    }
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
    let line = "helper is busy with other requests. Ask again in a few minutes.".to_owned();
    wait_until("the busy lines are posted", || {
        posts(&stack.mock.calls())
            .iter()
            .filter(|(_, text, _)| *text == line)
            .count()
            == 2
    })
    .await;
    let mut busy: Vec<_> = posts(&stack.mock.calls())
        .into_iter()
        .map(|(to, text, _)| (to, text))
        .filter(|(_, text)| *text == line)
        .collect();
    busy.sort_by_key(|(to, _)| to.thread_root.clone());
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

impl Stack {
    async fn bob(&self) -> MemberId {
        self.store()
            .member_for_identity(&key("bob"))
            .await
            .unwrap()
            .unwrap()
    }

    /// Sends bob's message `id` in GENERAL, starting a thread, mentioning
    /// `bot`, without waiting for it.
    async fn bob_asks(&self, id: &str, bot: &str) {
        self.pipeline
            .sink(MockSurface::DEFAULT_CAPS)
            .send(self.event("bob", "GENERAL", ConvKind::Channel, id, None, &[bot]))
            .await
            .unwrap();
    }

    /// Waits until `text` was posted in the thread of `root` in GENERAL.
    async fn wait_for_post(&self, root: &str, text: &str) {
        wait_until(&format!("{text:?} is posted in {root}"), || {
            self.mock
                .posts()
                .contains(&(in_thread("GENERAL", Some(root)), text.to_owned()))
        })
        .await;
    }

    fn busy_lines(&self, name: &str) -> usize {
        let line = format!("{name} is busy with other requests. Ask again in a few minutes.");
        self.mock
            .posts()
            .iter()
            .filter(|(_, text)| *text == line)
            .count()
    }
}

#[tokio::test]
async fn one_agents_flood_leaves_the_other_owners_agents_their_places() {
    let stack = start_with(Setup {
        pipeline: |settings| {
            settings.max_pending = 4;
            settings.max_pending_per_owner = 2;
        },
        ..Setup::default()
    })
    .await;
    let bob = stack.bob().await;
    stack.agent_of(bob, "writer", "UWRITER").await;
    stack.next_turn(Turn::reply("Done."));
    let held = Gate::closed();
    stack.holds.confirms_of(stack.agent, &held);
    for id in ["f1", "f2", "f3", "f4", "f5"] {
        stack.bob_asks(id, BOT).await;
    }
    stack.bob_asks("w1", "UWRITER").await;
    stack.wait_for_post("w1", "Done.").await;
    wait_until("the flood's busy lines are posted", || {
        stack.busy_lines("helper") == 3
    })
    .await;
    wait_until("helper holds its owner's two places", || {
        held.waiting() == 2
    })
    .await;
    assert_eq!(stack.busy_lines("writer"), 0);
    held.open();
    stack.wait_for_post("f1", "Done.").await;
    stack.wait_for_post("f2", "Done.").await;
    stack.stop().await;
}

#[tokio::test]
async fn one_owners_agents_together_leave_the_other_owners_agents_their_places() {
    let stack = start_with(Setup {
        pipeline: |settings| {
            settings.max_pending = 4;
            settings.max_pending_per_owner = 2;
        },
        ..Setup::default()
    })
    .await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let scribe = stack.other_agent("scribe", "USCRIBE").await;
    let bob = stack.bob().await;
    stack.agent_of(bob, "reader", "UREADER").await;
    stack.next_turn(Turn::reply("Done."));
    let held = Gate::closed();
    for agent in [stack.agent, writer, scribe] {
        stack.holds.confirms_of(agent, &held);
    }
    stack.bob_asks("f1", BOT).await;
    stack.bob_asks("f2", BOT).await;
    stack.bob_asks("w1", "UWRITER").await;
    stack.bob_asks("s1", "USCRIBE").await;
    stack.bob_asks("r1", "UREADER").await;
    stack.wait_for_post("r1", "Done.").await;
    wait_until("the busy lines of alice's other agents are posted", || {
        stack.busy_lines("writer") == 1 && stack.busy_lines("scribe") == 1
    })
    .await;
    wait_until("alice's first two messages are held", || {
        held.waiting() == 2
    })
    .await;
    assert_eq!(stack.busy_lines("reader"), 0);
    held.open();
    stack.wait_for_post("f1", "Done.").await;
    stack.wait_for_post("f2", "Done.").await;
    stack.stop().await;
}

#[tokio::test]
async fn asking_to_try_again_holds_no_place() {
    let stack = start_with(Setup {
        pipeline: |settings| settings.max_pending_per_owner = 1,
        ..Setup::default()
    })
    .await;
    stack.next_turn(Turn::reply("Done."));
    let held = Gate::closed();
    stack.holds.posts_of(UNCONFIRMED_TEXT, &held);
    stack.holds.fail_next_confirm(SurfaceError::RateLimited {
        retry_after: Duration::from_secs(30),
    });
    stack.bob_asks("u1", BOT).await;
    wait_until("the ask to try again is being posted", || {
        held.waiting() == 1
    })
    .await;
    stack.bob_asks("u2", BOT).await;
    stack.wait_for_post("u2", "Done.").await;
    assert_eq!(stack.busy_lines("helper"), 0);
    held.open();
    stack.wait_for_post("u1", UNCONFIRMED_TEXT).await;
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
    stack.mock.delay_next(Op::Post, Duration::from_secs(20));
    let event = stack.event("alice", "GENERAL", ConvKind::Channel, "c2", None, &[BOT]);
    stack
        .pipeline
        .sink(MockSurface::DEFAULT_CAPS)
        .send(event)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while stack.fake.message_requests().await.is_empty() {
        assert!(Instant::now() < deadline, "timed out waiting for the turn");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let Stack {
        pipeline,
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
                consent: None,
                hands_off: false,
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

#[tokio::test]
async fn reset_here_stops_the_warm_process_and_the_next_turn_starts_a_new_id() {
    let stack = start().await;
    let argv = stack.script.with_file_name("argv");
    let record = format!(
        "tr '\\0' ' ' < /proc/$PPID/cmdline >> {0}; echo >> {0}",
        argv.display()
    );
    stack.next_turn(Turn::reply("First.").with_command(["sh", "-c", record.as_str()]));
    stack
        .handle(stack.event("alice", "GENERAL", ConvKind::Channel, "u1", None, &[BOT]))
        .await;
    let sent = posts(&stack.calls_since(0));
    assert_eq!(sent.len(), 1);
    let old = stack.session_of(&sent[0].2).await;
    assert!(stack.turns.sessions().is_warm(old));
    let launched = std::fs::read_to_string(&argv).unwrap();
    assert!(
        launched.contains(&format!("--session-id {old}")),
        "{launched}"
    );

    let elsewhere = Origin::RocketChatChannel {
        room: "RANDOM".into(),
    };
    let commands = stack.app.commands();
    commands
        .handle_text(
            &key("bob"),
            "reset helper here",
            &Origin::RocketChatChannel {
                room: "GENERAL".into(),
            },
            &[],
        )
        .await;
    commands
        .handle_text(&key("alice"), "reset helper here", &elsewhere, &[])
        .await;
    assert!(
        stack.turns.sessions().is_warm(old),
        "neither reset reached the thread"
    );
    assert_eq!(
        stack.store().session(old).await.unwrap().unwrap().reset_at,
        None
    );

    let here = Origin::RocketChatChannel {
        room: "GENERAL".into(),
    };
    commands
        .handle_text(&key("alice"), "reset helper here", &here, &[])
        .await;
    assert!(
        !stack.turns.sessions().is_warm(old),
        "the reset stopped the warm process"
    );
    assert!(
        stack
            .store()
            .session(old)
            .await
            .unwrap()
            .unwrap()
            .reset_at
            .is_some()
    );

    let before = stack.mock.calls().len();
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
    assert_eq!(
        sent[0].1, "First.",
        "the new session's transcript starts over"
    );
    let new = stack.session_of(&sent[0].2).await;
    assert_ne!(new, old);
    let row = stack.store().session(new).await.unwrap().unwrap();
    assert_eq!(row.thread.root, Some("u1".into()), "the same thread");
    let launched = std::fs::read_to_string(&argv).unwrap();
    let last = launched.lines().last().unwrap();
    assert!(last.contains(&format!("--session-id {new}")), "{launched}");
    assert!(!last.contains("--resume"), "{launched}");
    assert_eq!(
        launched.lines().count(),
        2,
        "one process per session: {launched}"
    );
    stack.stop().await;
}

impl Stack {
    /// The bearer token of every request upstream so far, in order.
    async fn bearers(&self) -> Vec<String> {
        self.fake
            .message_requests()
            .await
            .iter()
            .map(|request| {
                request.headers["authorization"]
                    .to_str()
                    .unwrap()
                    .to_owned()
            })
            .collect()
    }

    /// The agent, requester and hop the post `msg` is recorded with, once
    /// it is: the record follows the post.
    async fn attributed(&self, msg: &MsgRef) -> (Option<AgentId>, MemberKey, u8) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(posted) = self.store().posted_message_ref(msg).await.unwrap() {
                return (posted.agent, posted.requester.key, posted.hop.0);
            }
            assert!(Instant::now() < deadline, "{msg:?} was never recorded");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// `poster`'s post `id` in GENERAL's thread `root`, mentioning
    /// `mentions`, as the platform delivers it.
    fn agents_post(&self, poster: &str, id: &str, root: &str, mentions: &[&str]) -> InboundEvent {
        let mut post = self.event(
            poster,
            "GENERAL",
            ConvKind::Channel,
            id,
            Some(root),
            mentions,
        );
        post.sender_is_bot = true;
        post.sender_bot_user = Some(UserId::new(poster));
        post
    }

    /// Waits until the mock has `count` posts, and returns them.
    async fn wait_for_posts(&self, count: usize) -> Vec<(ReplyTarget, String, MsgRef)> {
        wait_until(&format!("{count} posts"), || {
            self.mock.posts().len() >= count
        })
        .await;
        posts(&self.mock.calls())
    }
}

#[tokio::test]
async fn a_reply_that_mentions_an_agent_hands_off_until_the_hop_cap() {
    let stack = start().await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let reply = "@UBOT @UWRITER over to you.";
    stack.next_turn(Turn::reply(reply));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    let sent = stack.wait_for_posts(5).await;
    let notice = "helper won't answer: it takes part in chains of at most 3 hand-offs.";
    let texts: Vec<&str> = sent.iter().map(|(_, text, _)| text.as_str()).collect();
    assert_eq!(texts, [reply, reply, reply, reply, notice]);
    assert!(
        sent.iter()
            .all(|(to, _, _)| *to == in_thread("GENERAL", Some("c1"))),
        "{sent:?}"
    );
    let mut chain = Vec::new();
    for (_, _, posted) in &sent[..4] {
        chain.push(stack.attributed(posted).await);
    }
    let helper = Some(stack.agent);
    let writer = Some(writer);
    assert_eq!(
        chain,
        [
            (helper, key("bob"), 0),
            (writer, key("bob"), 1),
            (helper, key("bob"), 2),
            (writer, key("bob"), 3),
        ],
        "each hop inherits bob and counts one more"
    );
    assert_eq!(
        stack.bearers().await,
        vec!["Bearer token-of-bob"; 4],
        "every turn of the chain runs on bob's account"
    );
    stack.pipeline.close();
    stack.pipeline.drain().await;
    assert_eq!(stack.mock.posts().len(), 5, "nothing follows the notice");
    assert_eq!(
        stack.store().posted_message_ref(&sent[4].2).await.unwrap(),
        None,
        "the notice is agentd's own, and hands nothing off"
    );
    stack.stop().await;
}

#[tokio::test]
async fn an_agents_own_hop_limit_lowers_the_cap() {
    let stack = start().await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    stack
        .store()
        .update_agent_settings(writer, |settings| settings.max_hops = Some(1))
        .await
        .unwrap();
    let reply = "@UBOT @UWRITER over to you.";
    stack.next_turn(Turn::reply(reply));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    let sent = stack.wait_for_posts(4).await;
    let texts: Vec<&str> = sent.iter().map(|(_, text, _)| text.as_str()).collect();
    assert_eq!(
        texts,
        [
            reply,
            reply,
            reply,
            "writer won't answer: it takes part in chains of at most 1 hand-off."
        ]
    );
    stack.stop().await;
}

#[tokio::test]
async fn the_thread_token_budget_stops_a_chain() {
    let stack = start_with(Setup {
        pipeline: |settings| settings.limits.thread_tokens_per_day = Some(1),
        ..Setup::default()
    })
    .await;
    stack.other_agent("writer", "UWRITER").await;
    let reply = "@UBOT @UWRITER over to you.";
    stack.next_turn(Turn::reply(reply));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    let sent = stack.wait_for_posts(2).await;
    let texts: Vec<&str> = sent.iter().map(|(_, text, _)| text.as_str()).collect();
    assert_eq!(
        texts,
        [
            reply,
            "writer won't answer here for now: agents have used this thread's daily token \
             budget (1). Try again after midnight UTC, or in a new thread."
        ]
    );
    stack.pipeline.close();
    stack.pipeline.drain().await;
    assert_eq!(stack.mock.posts().len(), 2);
    stack.stop().await;
}

#[tokio::test]
async fn a_hop_runs_once_whichever_copy_of_the_post_arrives_first() {
    let stack = start_with(Setup {
        pipeline: |settings| settings.attribution_wait = Duration::from_secs(30),
        ..Setup::default()
    })
    .await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let reply = "@UWRITER over to you.";
    stack.next_turn(Turn::reply(reply));

    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    let sent = stack.wait_for_posts(2).await;
    assert_eq!(
        stack.attributed(&sent[1].2).await,
        (Some(writer), key("bob"), 1),
        "agentd handed helper's post to writer itself"
    );
    let platform = stack.agents_post(BOT, sent[0].2.id.as_str(), "c1", &["UWRITER"]);
    stack.handle(platform).await;
    assert_eq!(
        stack.mock.posts().len(),
        2,
        "the platform's copy, arriving second, is dropped"
    );
    stack.next_turn(Turn::reply(reply).with_command(["agentctl", "post", "--to", "here", "later"]));
    let gate = Gate::closed();
    stack.holds.posts_of(reply, &gate);
    let later = Gate::closed();
    stack.holds.posts_of("later", &later);
    let sink = stack.pipeline.sink(MockSurface::DEFAULT_CAPS);
    sink.send(stack.event("bob", "GENERAL", ConvKind::Channel, "c2", None, &[BOT]))
        .await
        .unwrap();
    wait_until("helper's reply waits to be posted", || gate.waiting() == 1).await;
    let before = stack.mock.posts().len();
    let next = format!("m{}", before + 1);
    sink.send(stack.agents_post(BOT, &next, "c2", &["UWRITER"]))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    gate.open();
    let sent = stack.wait_for_posts(before + 2).await;
    assert_eq!(sent[before].2.id.as_str(), next);
    assert!(
        later.waiting() >= 1 && !stack.mock.posts().iter().any(|(_, text)| text == "later"),
        "helper's last post is still held, so agentd hasn't handed anything off yet"
    );
    assert_eq!(
        stack.kept_hand_offs().await,
        [writer],
        "the hand-off was recorded as helper's reply was, before the turn's later posts"
    );
    assert_eq!(
        stack.attributed(&sent[before + 1].2).await,
        (Some(writer), key("bob"), 1),
        "the platform's copy, which arrived before its attribution and waited for it, ran the hop"
    );
    later.open();
    let unreacted = Call::Unreact {
        msg: msg("GENERAL", "c2"),
        emoji: "hourglass".into(),
    };
    wait_until("helper's turn is done with, hand-off and all", || {
        stack.mock.calls().contains(&unreacted)
    })
    .await;
    stack.pipeline.close();
    stack.pipeline.drain().await;
    let texts: Vec<String> = stack.mock.posts()[before..]
        .iter()
        .map(|(_, text)| text.clone())
        .collect();
    assert_eq!(
        texts,
        [reply, reply, "later", "later"],
        "agentd's own copy, arriving second, is dropped"
    );
    assert_eq!(stack.bearers().await.len(), 4);
    stack.stop().await;
}

#[tokio::test]
async fn ask_agent_posts_the_task_and_hands_it_off_with_the_turns_attribution() {
    let stack = start().await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    stack.next_turn(Turn::reply("Asked.").with_command([
        "agentctl",
        "ask-agent",
        "writer",
        "Review",
        "this",
    ]));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    let sent = stack.wait_for_posts(3).await;
    let texts: Vec<&str> = sent.iter().map(|(_, text, _)| text.as_str()).collect();
    assert_eq!(texts, ["Asked.", "@writer:\n\nReview this", "Asked."]);
    assert_eq!(
        stack.attributed(&sent[1].2).await,
        (Some(stack.agent), key("bob"), 0),
        "the task is helper's post for bob's turn"
    );
    assert_eq!(
        stack.attributed(&sent[2].2).await,
        (Some(writer), key("bob"), 1),
        "writer's turn inherits bob at the next hop"
    );
    assert_eq!(stack.bearers().await, vec!["Bearer token-of-bob"; 2]);
    stack.stop().await;
}

#[tokio::test]
async fn an_unmanaged_bots_mention_is_ignored() {
    let stack = start().await;
    let mut from_bot = stack.event(
        "UOTHERBOT",
        "GENERAL",
        ConvKind::Channel,
        "b1",
        None,
        &[BOT],
    );
    from_bot.sender_is_bot = true;
    stack.handle(from_bot.clone()).await;
    from_bot.sender_bot_user = Some(UserId::new("UOTHERBOT"));
    from_bot.message = msg("GENERAL", "b2");
    stack.handle(from_bot).await;
    assert!(
        posts(&stack.mock.calls()).is_empty(),
        "{:#?}",
        stack.mock.calls()
    );
    assert!(stack.bearers().await.is_empty(), "no turn ran");
    stack.stop().await;
}

#[tokio::test]
async fn the_hop_cap_line_is_said_once_an_hour_by_each_agent_in_a_thread() {
    let stack = start_with(Setup {
        pipeline: |settings| settings.limits.max_hops = core_types::Hop(1),
        ..Setup::default()
    })
    .await;
    stack.other_agent("writer", "UWRITER").await;
    stack.other_agent("scout", "USCOUT").await;
    stack.next_turn(Turn::reply("@UBOT @UWRITER @USCOUT next."));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    stack.wait_for_posts(5).await;
    wait_until("both hops' turns are done with", || {
        stack
            .mock
            .calls()
            .iter()
            .filter(|call| matches!(call, Call::Unreact { .. }))
            .count()
            >= 3
    })
    .await;
    stack.pipeline.close();
    stack.pipeline.drain().await;
    let mut notices: Vec<String> = stack
        .mock
        .posts()
        .into_iter()
        .map(|(_, text)| text)
        .filter(|text| text.contains("won't answer"))
        .collect();
    notices.sort();
    assert_eq!(
        notices,
        [
            "helper won't answer: it takes part in chains of at most 1 hand-off.",
            "scout won't answer: it takes part in chains of at most 1 hand-off.",
            "writer won't answer: it takes part in chains of at most 1 hand-off.",
        ],
        "four hops refused, one line from each agent"
    );
    stack.stop().await;
}

#[tokio::test]
async fn a_turn_hands_off_to_an_agent_once_however_many_of_its_posts_mention_it() {
    let stack = start().await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    stack.next_turn(Turn::reply("I asked @writer to look.").with_command([
        "agentctl",
        "ask-agent",
        "writer",
        "Review this",
    ]));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    let sent = stack.wait_for_posts(3).await;
    stack.pipeline.close();
    stack.pipeline.drain().await;
    let texts: Vec<String> = stack
        .mock
        .posts()
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    assert_eq!(
        texts,
        [
            "I asked @writer to look.",
            "@writer:\n\nReview this",
            "I asked @writer to look."
        ],
        "writer answers helper's turn once"
    );
    assert_eq!(
        stack.attributed(&sent[2].2).await,
        (Some(writer), key("bob"), 1)
    );
    assert_eq!(stack.bearers().await.len(), 2);
    let upstream = stack.fake.message_requests().await;
    let writers = String::from_utf8_lossy(&upstream.last().unwrap().body).into_owned();
    assert!(
        writers.contains("I asked @writer to look."),
        "writer's turn is on the reply, the turn's first post that mentions it: {writers}"
    );
    assert!(
        !writers.contains("Review this"),
        "the task, posted after it, isn't in writer's turn: {writers}"
    );
    stack.stop().await;
}

#[tokio::test]
async fn a_post_outside_the_turns_thread_hands_off_nothing() {
    let stack = start().await;
    stack.other_agent("writer", "UWRITER").await;
    stack.next_turn(Turn::reply("Done.").with_command([
        "agentctl",
        "post",
        "--to",
        "GENERAL",
        "@UWRITER start a thread of your own.",
    ]));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    let sent = stack.wait_for_posts(2).await;
    assert_eq!(sent[1].0, in_thread("GENERAL", None));
    let mut copy = stack.agents_post(BOT, sent[1].2.id.as_str(), "c1", &["UWRITER"]);
    copy.thread_root = None;
    copy.reply_to = None;
    stack.handle(copy).await;
    stack.pipeline.close();
    stack.pipeline.drain().await;
    assert_eq!(stack.mock.posts().len(), 2, "writer never answers");
    assert_eq!(stack.bearers().await.len(), 1);
    let posted = stack
        .store()
        .posted_message_ref(&sent[1].2)
        .await
        .unwrap()
        .unwrap();
    assert!(!posted.hands_off);
    stack.stop().await;
}

impl Stack {
    /// Another instance's pipeline on the same store, runner and surfaces.
    fn another_pipeline(&self) -> Pipeline {
        let mut settings = PipelineSettings::from_app(&self.app);
        settings.now = pinned_now;
        let replies = Replies::new(Some(Arc::new(ManagerBot::new(
            key("manager"),
            self.manager.clone(),
            Arc::new(Dms),
        ))));
        Pipeline::new(
            self.store().clone(),
            self.turns.clone(),
            Arc::clone(self.app.surfaces()),
            replies,
            settings,
        )
    }

    /// Every hand-off kept, made due now, as when whatever held it is gone.
    async fn hand_offs_due_now(&self) -> Vec<store::HandOff> {
        let now = pinned_now();
        let kept = self
            .store()
            .take_due_hand_offs(
                now + Duration::from_secs(1800),
                agentd::pipeline::HAND_OFF_LEASE,
                now - Duration::from_secs(60),
                64,
                &[],
            )
            .await
            .unwrap()
            .taken;
        let ids: Vec<i64> = kept.iter().map(|kept| kept.id).collect();
        self.store().release_hand_offs(&ids, now).await.unwrap();
        kept
    }

    /// Bob asks writer in c1's thread, which waits at writer's confirmation
    /// until `busy` opens, then asks helper in c1, whose reply hands off to
    /// writer, behind bob's message. Returns helper's post, once helper's
    /// turn is done with.
    async fn hand_off_behind_a_busy_writer(&self, writer: AgentId, busy: &Gate) -> MsgRef {
        self.next_turn(Turn::reply("@UWRITER over to you."));
        self.holds.confirms_of(writer, busy);
        let sink = self.pipeline.sink(MockSurface::DEFAULT_CAPS);
        sink.send(self.event(
            "bob",
            "GENERAL",
            ConvKind::Channel,
            "c2",
            Some("c1"),
            &["UWRITER"],
        ))
        .await
        .unwrap();
        wait_until("bob's message waits for writer", || busy.waiting() == 1).await;
        sink.send(self.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
            .await
            .unwrap();
        let unreacted = Call::Unreact {
            msg: msg("GENERAL", "c1"),
            emoji: "hourglass".into(),
        };
        wait_until("helper's turn is done with, hand-off and all", || {
            self.mock.calls().contains(&unreacted)
        })
        .await;
        posts(&self.mock.calls())[0].2.clone()
    }

    /// The hops writer's posts were recorded with, once the turns are done.
    async fn writers_hops(&self, writer: AgentId) -> Vec<u8> {
        let mut hops = Vec::new();
        for (_, _, posted) in posts(&self.mock.calls()) {
            if let Some(posted) = self.store().posted_message_ref(&posted).await.unwrap()
                && posted.agent == Some(writer)
            {
                hops.push(posted.hop.0);
            }
        }
        hops
    }
}

#[tokio::test]
async fn a_hand_off_a_shutdown_cut_is_delivered_by_the_next_instance() {
    let stack = start().await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let busy = Gate::closed();
    stack.hand_off_behind_a_busy_writer(writer, &busy).await;
    stack.pipeline.cut_short().await;
    let next = stack.another_pipeline();
    assert_eq!(
        next.replay_hand_offs().await.unwrap(),
        1,
        "the cut made the hand-off due at once"
    );
    stack.wait_for_posts(2).await;
    next.close();
    next.drain().await;
    assert_eq!(
        stack.writers_hops(writer).await,
        [1],
        "writer answered once"
    );
    assert!(stack.hand_offs_due_now().await.is_empty());
    stack.stop().await;
}

#[tokio::test]
async fn a_hand_off_is_taken_again_only_when_no_job_holds_it_and_its_hop_never_ran() {
    let stack = start().await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let busy = Gate::closed();
    let post = stack.hand_off_behind_a_busy_writer(writer, &busy).await;
    assert_eq!(stack.hand_offs_due_now().await.len(), 1);
    assert_eq!(
        stack.pipeline.replay_hand_offs().await.unwrap(),
        0,
        "the job waiting in writer's lane holds it"
    );
    let other = stack.another_pipeline();
    assert_eq!(
        other.replay_hand_offs().await.unwrap(),
        0,
        "the holder leased it again, so another instance leaves it"
    );
    let turn = stack
        .store()
        .posted_message_ref(&post)
        .await
        .unwrap()
        .unwrap()
        .turn
        .unwrap();
    stack
        .store()
        .mark_event_processed(
            "hop",
            &format!("{writer}/{turn}"),
            pinned_now(),
            store::PROCESSED_EVENT_RETENTION,
        )
        .await
        .unwrap();
    assert_eq!(stack.hand_offs_due_now().await.len(), 1);
    assert_eq!(
        other.replay_hand_offs().await.unwrap(),
        0,
        "its hop ran already"
    );
    assert!(
        stack.hand_offs_due_now().await.is_empty(),
        "a hand-off whose hop ran is done with"
    );
    busy.open();
    stack.wait_for_posts(2).await;
    stack.pipeline.close();
    stack.pipeline.drain().await;
    assert_eq!(
        stack.writers_hops(writer).await,
        [0],
        "writer answered bob alone: the hop was claimed"
    );
    stack.stop().await;
}

#[tokio::test]
async fn a_hand_off_two_instances_hold_runs_once() {
    let stack = start().await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let busy = Gate::closed();
    stack.hand_off_behind_a_busy_writer(writer, &busy).await;
    assert_eq!(stack.hand_offs_due_now().await.len(), 1);
    let other = stack.another_pipeline();
    assert_eq!(
        other.replay_hand_offs().await.unwrap(),
        1,
        "a hold this instance stopped leasing, as a stuck one, is taken"
    );
    stack.wait_for_posts(2).await;
    busy.open();
    stack.wait_for_posts(3).await;
    for pipeline in [&stack.pipeline, &other] {
        pipeline.close();
        pipeline.drain().await;
    }
    let mut hops = stack.writers_hops(writer).await;
    hops.sort_unstable();
    assert_eq!(hops, [0, 1], "writer answered bob, and the hop once");
    assert!(stack.hand_offs_due_now().await.is_empty());
    stack.stop().await;
}

#[tokio::test]
async fn a_hand_off_past_a_full_queue_keeps_its_row_and_is_taken_again() {
    let stack = start_with(Setup {
        pipeline: |settings| settings.max_pending_per_owner = 1,
        ..Setup::default()
    })
    .await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    stack.next_turn(Turn::reply("@UWRITER over to you."));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    assert_eq!(
        stack.mock.posts().len(),
        1,
        "helper's own job held alice's one place, so writer's hand-off found none"
    );
    let kept = stack.hand_offs_due_now().await;
    assert_eq!(
        kept.iter().map(|kept| kept.agent).collect::<Vec<_>>(),
        [writer],
        "the hand-off is kept"
    );
    stack
        .store()
        .add_hand_off(writer, "not an event", pinned_now(), pinned_now())
        .await
        .unwrap();
    assert_eq!(
        stack.pipeline.replay_hand_offs().await.unwrap(),
        1,
        "one job queued; the row that doesn't parse isn't"
    );
    stack.wait_for_posts(2).await;
    stack.pipeline.close();
    stack.pipeline.drain().await;
    assert_eq!(stack.writers_hops(writer).await, [1]);
    assert_eq!(
        stack.kept_hand_offs().await,
        [writer],
        "a row that doesn't parse, as one from a newer version, is left to age out"
    );
    stack.stop().await;
}

impl Stack {
    /// The agents of the `hand_offs` rows kept, read without taking them.
    async fn kept_hand_offs(&self) -> Vec<AgentId> {
        let db = self.dir.path().join("agentd.db");
        let pool = sqlx::SqlitePool::connect(&format!("sqlite://{}?mode=ro", db.display()))
            .await
            .unwrap();
        let rows: Vec<(String,)> = sqlx::query_as("SELECT agent_id FROM hand_offs ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
        pool.close().await;
        rows.into_iter()
            .map(|(agent,)| agent.parse().unwrap())
            .collect()
    }
}

#[tokio::test]
async fn a_hand_off_refused_because_the_rules_dont_read_is_kept_and_tried_again() {
    let stack = start().await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let rules = stack
        .store()
        .update_agent_settings(writer, |settings| {
            std::mem::replace(&mut settings.allow_json, "[oops".into())
        })
        .await
        .unwrap();
    let reply = "@UWRITER over to you.";
    stack.next_turn(Turn::reply(reply));
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    stack.wait_for_posts(2).await;
    stack.pipeline.close();
    stack.pipeline.drain().await;
    assert_eq!(
        stack.kept_hand_offs().await,
        [writer],
        "a refusal that may not hold later doesn't settle the hand-off"
    );
    stack
        .store()
        .update_agent_settings(writer, |settings| settings.allow_json = rules)
        .await
        .unwrap();
    stack.hand_offs_due_now().await;
    let next = stack.another_pipeline();
    assert_eq!(next.replay_hand_offs().await.unwrap(), 1);
    stack.wait_for_posts(3).await;
    next.close();
    next.drain().await;
    let texts: Vec<String> = stack
        .mock
        .posts()
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    assert_eq!(
        texts,
        [
            reply,
            "writer can't check who may use it right now. Try again later.",
            reply
        ]
    );
    assert_eq!(stack.writers_hops(writer).await, [1]);
    assert!(stack.kept_hand_offs().await.is_empty());
    stack.stop().await;
}

#[tokio::test]
async fn a_hop_whose_posting_turn_cant_be_read_waits_rather_than_risk_running_twice() {
    let stack = start().await;
    let bob = bob(&stack).await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let store = stack.store();
    store
        .record_message_ref(
            &store::NewMessageRef {
                session: SessionId::new_v4(),
                msg: &msg("GENERAL", "w1"),
                thread_root: None,
                agent: Some(writer),
                turn: None,
                requester: &core_types::Requester {
                    member: Some(bob),
                    key: key("bob"),
                },
                hop: core_types::Hop(1),
                consent: None,
                hands_off: true,
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let mut hop = stack.event("UWRITER", "GENERAL", ConvKind::Channel, "w1", None, &[BOT]);
    hop.sender_is_bot = true;
    hop.sender_bot_user = Some(UserId::new("UWRITER"));
    stack.next_turn(Turn::reply("On it."));
    stack.handle(hop).await;
    assert!(
        posts(&stack.calls_since(0)).is_empty(),
        "with no turn to claim the hop by, it doesn't run"
    );
    assert!(stack.bearers().await.is_empty());
    stack.stop().await;
}

#[tokio::test]
async fn a_failed_membership_check_answers_a_read_back_message_and_retries_a_hand_off() {
    let stack = start().await;
    let writer = stack.other_agent("writer", "UWRITER").await;
    let reply = "@UWRITER over to you.";
    stack.next_turn(Turn::reply(reply));
    stack.holds.fail_can_post(stack.agent, 1);
    stack.holds.fail_can_post(writer, 1);
    stack
        .handle(stack.event("bob", "GENERAL", ConvKind::Channel, "c1", None, &[BOT]))
        .await;
    stack.pipeline.close();
    stack.pipeline.drain().await;
    let texts: Vec<String> = stack
        .mock
        .posts()
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    assert_eq!(
        texts,
        [reply],
        "helper answered bob's message, read back with its own bot, though the check \
         failed; writer's check failed before its hop was claimed"
    );
    assert_eq!(stack.kept_hand_offs().await, [writer]);
    stack.hand_offs_due_now().await;
    let next = stack.another_pipeline();
    assert_eq!(next.replay_hand_offs().await.unwrap(), 1);
    stack.wait_for_posts(2).await;
    next.close();
    next.drain().await;
    assert_eq!(stack.writers_hops(writer).await, [1]);
    stack.stop().await;
}

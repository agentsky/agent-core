//! Requester-pays routing end to end: every turn runs on its requester's
//! own Claude account, or on the community API key an admin set when the
//! requester has none linked, never on the agent owner's. Each turn's
//! credential is checked where it lands: in the headers `fake_anthropic()`
//! recorded behind agentd's real credential proxy.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentd::commands::{ManagerBot, OpenDm, Origin, Replies};
use agentd::ctl::SurfaceLookup;
use agentd::pipeline::{
    COMMUNITY_KEY_REFUSED_TEXT, COMMUNITY_USAGE_LIMIT_TEXT, LOGIN_EXPIRED_TEXT, Pipeline,
    PipelineSettings, TurnSettings, Turns, USAGE_LIMIT_TEXT,
};
use agentd::server::{Routers, Server};
use agentd::{App, Config};
use core_types::{
    AgentId, BindingId, ConvKind, ConvRef, ConversationId, InboundEvent, MemberId, MemberKey,
    MessageId, MsgRef, ReplyTarget, ScopeKey, SessionId, Surface, SurfaceError, SurfaceKind,
    UserId, VolumeKey,
};
use runner::{PoolConfig, ProcessConfig};
use sandbox::ProcessSandbox;
use secrecy::SecretString;
use serde_json::{Value, json};
use store::{AgentCreation, NewAgent, NewClaudeLink, Store, Visibility};
use testkit::{
    Call, FakeAnthropic, MockSurface, Turn, agentctl_path, fake_anthropic, fake_claude_path,
};
use time::OffsetDateTime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{TempDir, env};

const TEAM: &str = "chat.example";
const BOT: &str = "UBOT";
const ADMIN: &str = "root";
const COMMUNITY_KEY: &str = "sk-ant-api03-community-key-5e1d";
const DEFAULT_MODEL: &str = "model-default";
const MAX_MODEL: &str = "model-max";

/// Every agent's bot acts through the one mock.
#[derive(Debug)]
struct Mocks(Arc<MockSurface>);

#[async_trait::async_trait]
impl SurfaceLookup for Mocks {
    async fn surface(&self, _agent: AgentId, _conv: &ConvRef) -> Option<Arc<dyn Surface>> {
        Some(self.0.clone())
    }
}

/// The manager bot's DM with a member is `dm-<user>`.
struct Dms;

#[async_trait::async_trait]
impl OpenDm for Dms {
    async fn open_dm(&self, member: &MemberKey) -> Result<ConversationId, SurfaceError> {
        Ok(format!("dm-{}", member.user.as_str()).into())
    }
}

struct Stack {
    app: App,
    pipeline: Pipeline,
    agents: Arc<MockSurface>,
    manager: Arc<MockSurface>,
    fake: FakeAnthropic,
    oauth: MockServer,
    script: PathBuf,
    pids: PathBuf,
    binding: BindingId,
    bob: MemberId,
    stop: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
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

async fn link(store: &Store, user: &str, plan: Option<&str>, lifetime: Duration) -> MemberId {
    let now = OffsetDateTime::now_utc();
    let member = store.ensure_member(&key(user), user, now).await.unwrap();
    store
        .put_claude_link(
            member,
            &NewClaudeLink {
                access_token: SecretString::from(format!("token-of-{user}")),
                refresh_token: SecretString::from(format!("refresh-of-{user}")),
                expires_at: now + lifetime,
                plan: plan.map(str::to_owned),
                rate_limit_tier: None,
            },
            now,
        )
        .await
        .unwrap();
    member
}

/// agentd with its listeners, the real credential proxy in front of
/// `fake_anthropic()`, a runner over a process sandbox running
/// `fake-claude`, and alice's agent `helper` (bot `UBOT`). alice, the
/// owner, is linked with no known plan; bob is linked on Claude Max, his
/// token expiring in `bobs_token_lifetime`; carol and dave are known but
/// unlinked. `root` is the community admin. Models: Claude Max gets
/// [`MAX_MODEL`], everyone else [`DEFAULT_MODEL`].
async fn start(bobs_token_lifetime: Duration) -> Stack {
    let claude = fake_claude_path();
    let agentctl = agentctl_path();
    let dir = TempDir::new();
    let fake = fake_anthropic().await;
    let oauth = MockServer::start().await;
    let text = format!(
        "{}\n[proxy]\nupstream = \"{}\"\n\
         [runner]\nworking_emoji = \"hourglass\"\n\
         [runner.models]\ndefault = \"{DEFAULT_MODEL}\"\nplans = {{ claude_max = \"{MAX_MODEL}\" }}\n\
         [community]\nadmins = [\"rocketchat:{TEAM}:{ADMIN}\"]\n\
         [claude_oauth]\ntoken_url = \"{o}/v1/oauth/token\"\n\
         revoke_url = \"{o}/v1/oauth/token/revoke\"\nprofile_url = \"{o}/api/oauth/profile\"\n",
        common::CONFIG
            .replace("/nonexistent/agentd", &dir.path().display().to_string())
            .replace(
                "sqlite::memory:",
                &format!("sqlite://{}", dir.path().join("agentd.db").display())
            ),
        fake.uri(),
        o = oauth.uri(),
    );
    let config = Config::parse(&text, env()).unwrap();
    let store = agentd::app::open_store(&config).await.unwrap();
    let agents = Arc::new(MockSurface::new());
    let app =
        App::with_surfaces(config, store.clone(), None, Arc::new(Mocks(agents.clone()))).unwrap();
    let alice = link(&store, "alice", None, Duration::from_secs(86_400)).await;
    let bob = link(&store, "bob", Some("claude_max"), bobs_token_lifetime).await;
    for user in ["carol", "dave"] {
        store
            .ensure_member(&key(user), user, OffsetDateTime::now_utc())
            .await
            .unwrap();
    }
    let team = TEAM.into();
    let AgentCreation::Created(_, binding) = store
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
    let pids = dir.path().join("pids");
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
    let manager = Arc::new(MockSurface::new());
    let replies = Replies::new(Some(Arc::new(ManagerBot::new(
        key("manager"),
        manager.clone(),
        Arc::new(Dms),
    ))));
    let pipeline = Pipeline::new(
        store.clone(),
        turns,
        Arc::clone(app.surfaces()),
        replies,
        PipelineSettings::from_app(&app),
    );
    let server = server.with_pipeline(pipeline.clone());
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(server.run(
        async {
            let _ = stopped.await;
        },
        std::future::pending(),
    ));
    let stack = Stack {
        app,
        pipeline,
        agents,
        manager,
        fake,
        oauth,
        script,
        pids,
        binding,
        bob,
        stop,
        task,
        _dir: dir,
    };
    stack.every_turn(Turn::reply("Done."));
    stack
}

/// What one request reached the upstream with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Upstream {
    bearer: Option<String>,
    api_key: Option<String>,
    model: String,
}

impl Stack {
    fn store(&self) -> &Store {
        self.app.store()
    }

    /// Makes every turn play `turn`, after writing its `claude` process's
    /// pid to [`Stack::pids`], so a restart shows as a new pid.
    fn every_turn(&self, turn: Turn) {
        let record = format!("echo $PPID >> '{}'", self.pids.display());
        let turn = turn.with_command(["sh", "-c", record.as_str()]);
        testkit::write_script(&self.script, &vec![turn; 16]).unwrap();
    }

    /// The pid of the `claude` process of every turn that ran so far.
    fn pids(&self) -> Vec<u32> {
        std::fs::read_to_string(&self.pids)
            .unwrap_or_default()
            .lines()
            .map(|line| line.parse().unwrap())
            .collect()
    }

    /// `sender`'s message `id` in `conv_id`, in the thread of `root` if
    /// given, mentioning the agent's bot.
    fn mention(&self, sender: &str, conv_id: &str, id: &str, root: Option<&str>) -> InboundEvent {
        InboundEvent {
            event_id: id.to_owned(),
            binding: self.binding,
            sender: key(sender),
            sender_is_bot: false,
            sender_bot_user: None,
            conv: conv(conv_id),
            conv_kind: ConvKind::Channel,
            thread_root: root.map(MessageId::new),
            message: msg(conv_id, id),
            text: format!("@{BOT} hello from {sender}"),
            mentions: vec![UserId::new(BOT)],
            reply_to: root.map(|root| msg(conv_id, root)),
            files: vec![],
            received_at: OffsetDateTime::now_utc(),
        }
    }

    async fn handle(&self, event: InboundEvent) {
        self.pipeline.handle(event, MockSurface::DEFAULT_CAPS).await;
    }

    /// Sends `text` as `user`'s command in their DM with the manager bot.
    async fn command(&self, user: &str, text: &str) {
        self.app
            .commands()
            .handle_text(
                &key(user),
                text,
                &Origin::RocketChatDm {
                    room: format!("dm-{user}").into(),
                },
                &[],
            )
            .await;
    }

    /// Every `/v1/messages` request the upstream got so far.
    async fn upstream(&self) -> Vec<Upstream> {
        let header = |request: &wiremock::Request, name: &str| {
            request
                .headers
                .get(name)
                .map(|value| value.to_str().unwrap().to_owned())
        };
        self.fake
            .message_requests()
            .await
            .iter()
            .map(|request| {
                let body: Value = serde_json::from_slice(&request.body).unwrap();
                Upstream {
                    bearer: header(request, "authorization")
                        .map(|value| value.strip_prefix("Bearer ").unwrap().to_owned()),
                    api_key: header(request, "x-api-key"),
                    model: body["model"].as_str().unwrap().to_owned(),
                }
            })
            .collect()
    }

    /// The agent's posts since `from`, as `(where, text, ref)`.
    fn posts_since(&self, from: usize) -> Vec<(ReplyTarget, String, MsgRef)> {
        self.agents.calls()[from..]
            .iter()
            .filter_map(|call| match call {
                Call::Post { to, text, msg } => Some((to.clone(), text.clone(), msg.clone())),
                _ => None,
            })
            .collect()
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

    async fn session_of(&self, posted: &MsgRef) -> SessionId {
        self.store()
            .posted_message_ref(posted)
            .await
            .unwrap()
            .expect("the post is attributed")
            .session
    }

    /// Runs `event` and returns its one reply, with where it went and what
    /// the upstream saw for it.
    async fn answer(&self, event: InboundEvent) -> (String, MsgRef, Upstream) {
        let calls = self.agents.calls().len();
        let requests = self.fake.message_requests().await.len();
        self.handle(event).await;
        let posts = self.posts_since(calls);
        assert_eq!(posts.len(), 1, "{posts:#?}");
        let seen = self.upstream().await;
        assert_eq!(seen.len(), requests + 1, "one upstream request: {seen:#?}");
        let (_, text, posted) = posts.into_iter().next().unwrap();
        (text, posted, seen.last().unwrap().clone())
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        self.task.await.unwrap().unwrap();
    }
}

fn bearer(token: &str, model: &str) -> Upstream {
    Upstream {
        bearer: Some(token.to_owned()),
        api_key: None,
        model: model.to_owned(),
    }
}

fn community(model: &str) -> Upstream {
    Upstream {
        bearer: None,
        api_key: Some(COMMUNITY_KEY.to_owned()),
        model: model.to_owned(),
    }
}

#[tokio::test]
async fn each_turn_in_a_thread_runs_on_its_requesters_account_or_the_community_key() {
    let stack = start(Duration::from_secs(86_400)).await;
    stack
        .command(ADMIN, &format!("admin api-key set {COMMUNITY_KEY}"))
        .await;
    assert!(stack.store().community_api_key_status().await.unwrap().set);

    let (_, bobs, seen) = stack
        .answer(stack.mention("bob", "GENERAL", "t1", None))
        .await;
    assert_eq!(
        seen,
        bearer("token-of-bob", MAX_MODEL),
        "bob, linked, runs on his own subscription, on his plan's model"
    );
    let session = stack.session_of(&bobs).await;

    let (_, carols, seen) = stack
        .answer(stack.mention("carol", "GENERAL", "t2", Some("t1")))
        .await;
    assert_eq!(
        seen,
        community(DEFAULT_MODEL),
        "carol, unlinked, runs on the community key"
    );

    let (_, alices, seen) = stack
        .answer(stack.mention("alice", "GENERAL", "t3", Some("t1")))
        .await;
    assert_eq!(
        seen,
        bearer("token-of-alice", DEFAULT_MODEL),
        "the owner runs on her own account"
    );

    let (_, bobs_again, seen) = stack
        .answer(stack.mention("bob", "GENERAL", "t4", Some("t1")))
        .await;
    assert_eq!(seen, bearer("token-of-bob", MAX_MODEL));

    let (_, alices_again, seen) = stack
        .answer(stack.mention("alice", "GENERAL", "t5", Some("t1")))
        .await;
    assert_eq!(seen, bearer("token-of-alice", DEFAULT_MODEL));

    for (posted, requester) in [
        (&bobs, "bob"),
        (&carols, "carol"),
        (&alices, "alice"),
        (&bobs_again, "bob"),
        (&alices_again, "alice"),
    ] {
        assert_eq!(
            stack.session_of(posted).await,
            session,
            "one thread, one session"
        );
        let attributed = stack
            .store()
            .posted_message_ref(posted)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(attributed.requester.key, key(requester));
    }

    let pids = stack.pids();
    assert_eq!(pids.len(), 5, "{pids:?}");
    assert_ne!(
        pids[0], pids[1],
        "a subscription turn, then a community-key turn: the process restarts"
    );
    assert_ne!(
        pids[1], pids[2],
        "a community-key turn, then a subscription turn: the process restarts"
    );
    assert_ne!(
        pids[2], pids[3],
        "another plan's model: the process restarts"
    );
    assert_ne!(pids[3], pids[4]);

    let row = stack.store().session(session).await.unwrap().unwrap();
    assert_eq!(row.scope, ScopeKey::Channel(conv("GENERAL")));
    for request in stack.fake.requests().await {
        for (name, value) in &request.headers {
            assert!(
                !value.as_bytes().starts_with(b"agentd-"),
                "a placeholder reached the upstream in {name}"
            );
        }
    }
    assert!(
        stack.dms_to("alice").is_empty(),
        "the owner is told nothing"
    );
    stack.stop().await;
}

#[tokio::test]
async fn the_same_credential_and_model_keep_the_process_and_repoint_its_placeholder() {
    let stack = start(Duration::from_secs(86_400)).await;
    let (_, _, first) = stack
        .answer(stack.mention("alice", "GENERAL", "s1", None))
        .await;
    link(stack.store(), "erin", None, Duration::from_secs(86_400)).await;
    let (_, _, second) = stack
        .answer(stack.mention("erin", "GENERAL", "s2", Some("s1")))
        .await;
    assert_eq!(first, bearer("token-of-alice", DEFAULT_MODEL));
    assert_eq!(
        second,
        bearer("token-of-erin", DEFAULT_MODEL),
        "a warm process's placeholder follows the turn's requester"
    );
    let pids = stack.pids();
    assert_eq!(pids.len(), 2);
    assert_eq!(pids[0], pids[1], "same kind, same model: the process stays");
    stack.stop().await;
}

#[tokio::test]
async fn without_a_community_key_an_unlinked_member_is_asked_to_link_and_nothing_runs() {
    let stack = start(Duration::from_secs(86_400)).await;
    let before = stack.agents.calls().len();
    stack
        .handle(stack.mention("carol", "GENERAL", "u1", None))
        .await;
    assert!(stack.posts_since(before).is_empty());
    assert!(stack.fake.message_requests().await.is_empty());
    let prompts = stack.dms_to("carol");
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(prompts[0].contains("Link yours"), "{}", prompts[0]);

    stack
        .command(ADMIN, &format!("admin api-key set {COMMUNITY_KEY}"))
        .await;
    let (_, _, seen) = stack
        .answer(stack.mention("carol", "GENERAL", "u2", None))
        .await;
    assert_eq!(seen, community(DEFAULT_MODEL));

    stack.command(ADMIN, "admin api-key clear").await;
    let requests = stack.fake.message_requests().await.len();
    stack
        .handle(stack.mention("dave", "GENERAL", "u3", None))
        .await;
    assert_eq!(stack.fake.message_requests().await.len(), requests);
    assert_eq!(stack.dms_to("dave").len(), 1, "a cleared key prompts again");

    stack
        .command("carol", &format!("admin api-key set {COMMUNITY_KEY}"))
        .await;
    assert!(
        !stack.store().community_api_key_status().await.unwrap().set,
        "only an admin sets the key"
    );
    stack.stop().await;
}

#[tokio::test]
async fn a_non_owners_dm_runs_in_its_own_dm_scope_never_the_private_one() {
    let stack = start(Duration::from_secs(86_400)).await;
    let mut dm = stack.mention("bob", "DMBOB", "d1", None);
    dm.conv_kind = ConvKind::Dm;
    dm.mentions.clear();
    let (_, posted, seen) = stack.answer(dm).await;
    assert_eq!(seen, bearer("token-of-bob", MAX_MODEL));
    let session = stack.session_of(&posted).await;
    let row = stack.store().session(session).await.unwrap().unwrap();
    assert_eq!(row.scope, ScopeKey::Dm(conv("DMBOB")));
    let agent = row.agent;
    assert!(
        stack
            .store()
            .volume(&VolumeKey {
                agent,
                scope: ScopeKey::Private,
            })
            .await
            .unwrap()
            .is_none(),
        "nothing ran on the agent's private volume"
    );
    stack.stop().await;
}

#[tokio::test]
async fn a_plan_read_at_a_token_refresh_picks_the_model_from_the_next_turn() {
    let stack = start(Duration::from_secs(60)).await;
    stack
        .store()
        .update_claude_plan(
            stack.bob,
            current_generation(&stack, stack.bob).await,
            Some("claude_pro"),
            None,
        )
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "token_type": "Bearer",
            "access_token": "token-of-bob-refreshed",
            "refresh_token": "refresh-of-bob-2",
            "expires_in": 28800,
        })))
        .expect(1)
        .mount(&stack.oauth)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/oauth/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "organization": {"organization_type": "claude_max", "rate_limit_tier": "t"},
        })))
        .mount(&stack.oauth)
        .await;

    let (_, _, seen) = stack
        .answer(stack.mention("bob", "GENERAL", "r1", None))
        .await;
    assert_eq!(
        seen,
        bearer("token-of-bob-refreshed", DEFAULT_MODEL),
        "the turn's model was picked from the plan known when it started"
    );
    wait_for_plan(&stack, stack.bob, "claude_max").await;

    let (_, _, seen) = stack
        .answer(stack.mention("bob", "GENERAL", "r2", Some("r1")))
        .await;
    assert_eq!(seen, bearer("token-of-bob-refreshed", MAX_MODEL));
    let pids = stack.pids();
    assert_ne!(
        pids[0], pids[1],
        "the model changed, so the process restarted"
    );
    stack.stop().await;
}

async fn current_generation(stack: &Stack, member: MemberId) -> i64 {
    stack
        .store()
        .get_claude_link(member)
        .await
        .unwrap()
        .unwrap()
        .generation
}

async fn wait_for_plan(stack: &Stack, member: MemberId, plan: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = stack.store().claude_link_status(member).await.unwrap();
        if status.and_then(|status| status.plan).as_deref() == Some(plan) {
            return;
        }
        assert!(Instant::now() < deadline, "the plan never became {plan}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_usage_limit_or_refusal_names_whose_account_and_tells_only_the_requester() {
    let stack = start(Duration::from_secs(86_400)).await;
    stack
        .command(ADMIN, &format!("admin api-key set {COMMUNITY_KEY}"))
        .await;

    stack.every_turn(Turn::api_error(429, "You've hit your usage limit."));
    let (text, _, _) = stack
        .answer(stack.mention("bob", "GENERAL", "l1", None))
        .await;
    assert_eq!(text, USAGE_LIMIT_TEXT);
    let told = stack.dms_to("bob");
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(
        told[0],
        "helper couldn't answer your request: your Claude account has reached its usage limit. \
         Ask again when it resets."
    );

    let (text, _, _) = stack
        .answer(stack.mention("carol", "GENERAL", "l2", Some("l1")))
        .await;
    assert_eq!(text, COMMUNITY_USAGE_LIMIT_TEXT);
    let told = stack.dms_to("carol");
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].contains("community API key"), "{}", told[0]);

    stack.every_turn(Turn::api_error(401, "Invalid bearer token"));
    let (text, _, _) = stack
        .answer(stack.mention("bob", "GENERAL", "l3", Some("l1")))
        .await;
    assert_eq!(text, LOGIN_EXPIRED_TEXT);
    let told = stack.dms_to("bob");
    assert_eq!(told.len(), 2, "{told:?}");
    assert!(told[1].contains("your Claude login"), "{}", told[1]);

    let (text, _, _) = stack
        .answer(stack.mention("carol", "GENERAL", "l4", Some("l1")))
        .await;
    assert_eq!(text, COMMUNITY_KEY_REFUSED_TEXT);
    assert_eq!(stack.dms_to("carol").len(), 2);

    stack.every_turn(Turn::api_error(429, "You've hit your usage limit."));
    let mut dm = stack.mention("bob", "DMBOB", "l5", None);
    dm.conv_kind = ConvKind::Dm;
    let (text, _, _) = stack.answer(dm).await;
    assert_eq!(text, USAGE_LIMIT_TEXT);
    assert_eq!(
        stack.dms_to("bob").len(),
        2,
        "in bob's own DM with the agent, the reply there is enough"
    );

    assert!(
        stack.dms_to("alice").is_empty(),
        "the agent's owner is never told about someone else's account"
    );
    for (_, text) in stack.agents.posts() {
        assert!(!text.contains("alice") && !text.contains("owner"), "{text}");
    }
    stack.stop().await;
}

#[tokio::test]
async fn a_login_refused_at_refresh_is_told_in_the_thread_and_left_to_the_relink_notice() {
    let stack = start(Duration::from_secs(60)).await;
    Mock::given(method("POST"))
        .and(path("/v1/oauth/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "invalid_grant",
        })))
        .mount(&stack.oauth)
        .await;
    let calls = stack.agents.calls().len();
    stack
        .handle(stack.mention("bob", "GENERAL", "x1", None))
        .await;
    let posts = stack.posts_since(calls);
    assert_eq!(posts.len(), 1, "{posts:#?}");
    assert_eq!(posts[0].1, LOGIN_EXPIRED_TEXT);
    assert!(
        stack.fake.message_requests().await.is_empty(),
        "the proxy refused the request itself"
    );
    let status = stack
        .store()
        .claude_link_status(stack.bob)
        .await
        .unwrap()
        .unwrap();
    assert!(status.broken_at.is_some());
    assert!(
        stack.dms_to("bob").is_empty(),
        "the relink notice tells bob, once, not the pipeline too"
    );
    assert!(stack.dms_to("alice").is_empty());
    stack.stop().await;
}

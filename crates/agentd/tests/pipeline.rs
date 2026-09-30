//! The turn pipeline end to end: messages from a `MockSurface`, turns in
//! `fake-claude` in a process sandbox, the real credential proxy and
//! agentctl API on agentd's listeners, and `fake_anthropic()` upstream.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentd::ctl::SurfaceLookup;
use agentd::pipeline::{Pipeline, TurnSettings, Turns, USAGE_LIMIT_TEXT};
use agentd::server::{Routers, Server};
use agentd::{App, Config};
use core_types::{
    AgentId, BindingId, Caps, ConvKind, ConvRef, InboundEvent, MemberId, MemberKey, MessageId,
    MsgRef, ReplyTarget, ScopeKey, SessionId, Surface, SurfaceKind, UserId, VolumeKey,
};
use runner::{PoolConfig, ProcessConfig};
use sandbox::ProcessSandbox;
use secrecy::SecretString;
use store::{AgentCreation, NewAgent, NewClaudeLink, Store, Visibility};
use testkit::{
    Call, FakeAnthropic, MockSurface, Turn, agentctl_path, fake_anthropic, fake_claude_path,
};
use time::OffsetDateTime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use common::{TempDir, env};

const TEAM: &str = "chat.example";
const BOT: &str = "UBOT";

/// Every agent's bot acts through the one mock.
#[derive(Debug)]
struct Mocks(Arc<MockSurface>);

#[async_trait::async_trait]
impl SurfaceLookup for Mocks {
    async fn surface(&self, _agent: AgentId, _conv: &ConvRef) -> Option<Arc<dyn Surface>> {
        Some(self.0.clone())
    }
}

struct Stack {
    app: App,
    pipeline: Pipeline,
    mock: Arc<MockSurface>,
    script: PathBuf,
    agent: AgentId,
    binding: BindingId,
    alice: MemberId,
    stop: oneshot::Sender<()>,
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

/// agentd with its listeners and a runner over a process sandbox running
/// `fake-claude`, alice and bob linked, and alice's agent `helper`, whose
/// bot is `UBOT`.
async fn start() -> Stack {
    let claude = fake_claude_path();
    let agentctl = agentctl_path();
    let dir = TempDir::new();
    let fake = fake_anthropic().await;
    let text = format!(
        "{}\n[proxy]\nupstream = \"{}\"\n[runner]\nworking_emoji = \"hourglass\"\n",
        common::CONFIG.replace("/nonexistent/agentd", &dir.path().display().to_string()),
        fake.uri()
    );
    let config = Config::parse(&text, env()).unwrap();
    let store = agentd::app::open_store(&config).await.unwrap();
    let mock = Arc::new(MockSurface::new());
    let app =
        App::with_surfaces(config, store.clone(), None, Arc::new(Mocks(mock.clone()))).unwrap();
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
    let pipeline = Pipeline::for_app(&app, turns.clone());
    let server = server.with_turns(turns);
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
        mock,
        script,
        agent: agent.id,
        binding,
        alice,
        stop,
        task,
        fake,
        _dir: dir,
    }
}

impl Stack {
    fn store(&self) -> &Store {
        self.app.store()
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

    let team = TEAM.into();
    let AgentCreation::Created(writer, writer_binding) = store
        .create_agent(
            &NewAgent {
                owner: stack.alice,
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
        panic!("created");
    };
    store
        .set_binding_bot_user(writer_binding, &UserId::new("UWRITER"), "writer")
        .await
        .unwrap();
    store
        .activate_binding(
            writer_binding,
            &SecretString::from("t"),
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    let bob = store.member_for_identity(&key("bob")).await.unwrap();
    let by_writer = msg("GENERAL", "w1");
    store
        .record_message_ref(
            &store::NewMessageRef {
                session: SessionId::new_v4(),
                msg: &by_writer,
                thread_root: None,
                agent: Some(writer.id),
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
    let before = stack.mock.calls().len();
    stack.next_turn(Turn::reply("Picking this up."));
    let mut hop = stack.event("UWRITER", "GENERAL", ConvKind::Channel, "w1", None, &[BOT]);
    hop.sender_is_bot = true;
    hop.sender_bot_user = Some(UserId::new("UWRITER"));
    stack.handle(hop).await;
    let sent = posts(&stack.calls_since(before));
    assert_eq!(sent.len(), 1);
    let attributed = store.posted_message_ref(&sent[0].2).await.unwrap().unwrap();
    assert_eq!(attributed.requester.key, key("bob"), "the hop is bob's");
    assert_eq!(attributed.hop.0, 2);
    stack.stop().await;
}

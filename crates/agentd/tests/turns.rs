//! Turns through agentd's runner end to end: `fake-claude` in a process
//! sandbox, reaching the real credential proxy and agentctl API on agentd's
//! listeners, with `fake_anthropic()` as the upstream.

mod common;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentd::pipeline::{Pipeline, TurnSettings, Turns};
use agentd::server::{Addrs, Routers, Server};
use agentd::{App, Config};
use core_types::{
    AgentId, ConvRef, CredentialRef, Hop, MemberId, MemberKey, MessageId, Requester, ScopeKey,
    Side, SurfaceKind, ThreadKey, TurnId, TurnKind,
};
use runner::{PoolConfig, ProcessConfig, SessionStart, TurnOutcome, TurnRequest};
use sandbox::{ProcessSandbox, Sandbox as _};
use secrecy::SecretString;
use store::NewClaudeLink;
use testkit::{FakeAnthropic, Turn, agentctl_path, fake_anthropic, fake_claude_path, write_script};
use time::OffsetDateTime;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use common::{TempDir, env, with_a_hung_worker};

const ACCESS_TOKEN: &str = "real-access-token";

struct Running {
    app: App,
    addrs: Addrs,
    turns: Turns,
    fake: FakeAnthropic,
    member: MemberId,
    script: std::path::PathBuf,
    sandbox: Arc<ProcessSandbox>,
    stop: oneshot::Sender<()>,
    abort: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
    dir: TempDir,
}

/// agentd with its listeners, a runner over a process sandbox whose
/// processes are `fake-claude` running `turns`, and a member with a linked
/// account.
async fn start(turns: &[Turn]) -> Running {
    start_with(turns, common::CONFIG, |routers| routers).await
}

/// [`start`] with config `config`, and the routers `change` returns.
async fn start_with(
    turns: &[Turn],
    config: &str,
    change: impl FnOnce(Routers) -> Routers,
) -> Running {
    let claude = fake_claude_path();
    let agentctl = agentctl_path();
    let dir = TempDir::new();
    let fake = fake_anthropic().await;
    let text = format!(
        "{}\n[proxy]\nupstream = \"{}\"\n",
        config.replace("/nonexistent/agentd", &dir.path().display().to_string()),
        fake.uri()
    );
    let app = App::open(Config::parse(&text, env()).unwrap())
        .await
        .unwrap();
    let now = OffsetDateTime::now_utc();
    let member = app
        .store()
        .ensure_member(&alice(), "alice", now)
        .await
        .unwrap();
    app.store()
        .put_claude_link(
            member,
            &NewClaudeLink {
                access_token: SecretString::from(ACCESS_TOKEN),
                refresh_token: SecretString::from("refresh"),
                expires_at: now + Duration::from_secs(24 * 60 * 60),
                plan: None,
                rate_limit_tier: None,
            },
            now,
        )
        .await
        .unwrap();
    let server = Server::bind(app.clone(), change(Routers::new(&app).unwrap()))
        .await
        .unwrap();
    let script = dir.path().join("script.json");
    write_script(&script, turns).unwrap();
    let settings = settings(server.addrs(), claude, agentctl, &script, dir.path());
    let sandbox = Arc::new(ProcessSandbox::new(app.store().clone(), dir.path()).unwrap());
    let turns = Turns::start(&app, Arc::clone(&sandbox) as _, settings).unwrap();
    let addrs = server.addrs();
    let server = server.with_pipeline(Pipeline::for_app(&app, turns.clone()));
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
    Running {
        app,
        addrs,
        turns,
        fake,
        member,
        script,
        sandbox,
        stop,
        abort,
        task,
        dir,
    }
}

fn settings(
    addrs: Addrs,
    claude: &Path,
    agentctl: &Path,
    script: &Path,
    data_dir: &Path,
) -> TurnSettings {
    let mut env = BTreeMap::from([
        (
            testkit::claude::SCRIPT_ENV.to_owned(),
            script.display().to_string(),
        ),
        (
            "PATH".to_owned(),
            agentctl.parent().unwrap().display().to_string(),
        ),
    ]);
    for name in ["NO_PROXY", "no_proxy"] {
        env.insert(name.to_owned(), addrs.proxy.ip().to_string());
    }
    TurnSettings {
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
        data_dir: data_dir.to_owned(),
        agentctl_url: format!("http://{}", addrs.ctl),
        env,
    }
}

fn alice() -> MemberKey {
    MemberKey {
        surface: SurfaceKind::RocketChat,
        team: "chat.example".into(),
        user: "alice".into(),
    }
}

fn thread(root: &str) -> ThreadKey {
    ThreadKey {
        conv: ConvRef {
            surface: SurfaceKind::RocketChat,
            team: "chat.example".into(),
            conversation: "GENERAL".into(),
        },
        root: Some(MessageId::new(root)),
    }
}

impl Running {
    fn request(&self, trigger: &str) -> TurnRequest {
        TurnRequest {
            turn: TurnId::new_v4(),
            message: "hello".into(),
            credential: CredentialRef::Member(self.member),
            model: None,
            requester: Requester {
                member: Some(self.member),
                key: alice(),
                outside: None,
            },
            hop: Hop::ZERO,
            side: Side::Public,
            kind: TurnKind::Normal,
            trigger: Some(MessageId::new(trigger)),
        }
    }

    async fn stop(self) {
        self.stop.send(()).unwrap();
        self.task.await.unwrap().unwrap();
        drop(self.dir);
    }

    /// Shuts agentd down, then forces it with a second signal once the
    /// listeners' and workers' drain is under way: once the ctl listener,
    /// which closes as that drain begins, after the turns', refuses
    /// connections. The forced shutdown has to return at once, well within
    /// the drain timeout.
    async fn force_during_the_drain(self) {
        self.stop.send(()).unwrap();
        refused(self.addrs.ctl).await;
        let forced = Instant::now();
        self.abort.send(()).unwrap();
        self.task.await.unwrap().unwrap();
        assert!(
            forced.elapsed() < Duration::from_secs(10),
            "the forced shutdown took {:?}",
            forced.elapsed()
        );
        drop(self.dir);
    }

    /// A session of a new agent in thread `root`, after one turn, so its
    /// sandbox is warm.
    async fn warm_session(&self, root: &str) -> core_types::SessionId {
        let agent = AgentId::new_v4();
        runner::write_persona(self.dir.path(), agent, "You are a test.\n")
            .await
            .unwrap();
        let thread = thread(root);
        let scope = ScopeKey::Channel(thread.conv.clone());
        let sessions = self.turns.sessions();
        let session = sessions
            .lookup_or_create(agent, &thread, &scope)
            .await
            .unwrap();
        let report = sessions
            .run_turn(session.id, self.request(root))
            .await
            .unwrap();
        assert!(report.outcome.is_success(), "{:?}", report.outcome);
        assert!(sessions.is_warm(session.id));
        assert_eq!(self.sandbox.list_managed().await.unwrap().len(), 1);
        session.id
    }
}

/// Waits until nothing accepts connections at `addr`.
async fn refused(addr: SocketAddr) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while tokio::net::TcpStream::connect(addr).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the listener closes");
}

#[tokio::test]
async fn a_graceful_shutdown_stops_warm_sandboxes() {
    let running = start(&[Turn::reply("warm")]).await;
    let session = running.warm_session("m1").await;
    let (turns, sandbox) = (running.turns.clone(), Arc::clone(&running.sandbox));
    running.stop().await;
    assert!(!turns.sessions().is_warm(session));
    assert_eq!(sandbox.list_managed().await.unwrap(), []);
}

#[tokio::test]
async fn a_forced_shutdown_leaves_warm_sandboxes_to_the_next_start() {
    let config = common::CONFIG.replace("drain_timeout_secs = 5", "drain_timeout_secs = 30");
    let running = start_with(&[Turn::reply("warm")], &config, with_a_hung_worker).await;
    let session = running.warm_session("m1").await;
    let (turns, sandbox) = (running.turns.clone(), Arc::clone(&running.sandbox));
    running.force_during_the_drain().await;
    assert!(turns.sessions().is_warm(session));
    assert_eq!(sandbox.list_managed().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_drain_cut_short_by_its_timeout_leaves_warm_sandboxes_to_the_next_start() {
    let config = common::CONFIG.replace("drain_timeout_secs = 5", "drain_timeout_secs = 1");
    let running = start_with(&[Turn::reply("warm")], &config, with_a_hung_worker).await;
    let session = running.warm_session("m1").await;
    let (turns, sandbox) = (running.turns.clone(), Arc::clone(&running.sandbox));
    running.stop().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(turns.sessions().is_warm(session));
    assert_eq!(sandbox.list_managed().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_turn_reaches_the_upstream_through_the_proxy_and_agentctl_through_the_api() {
    let running = start(&[
        Turn::reply("first reply")
            .with_command(["agentctl", "react", "eyes"])
            .with_command(["agentctl", "post", "--to", "here", "queued text"]),
        Turn::reply("second reply"),
    ])
    .await;
    let agent = AgentId::new_v4();
    runner::write_persona(running.dir.path(), agent, "You are a test.\n")
        .await
        .unwrap();
    let thread = thread("m1");
    let scope = ScopeKey::Channel(thread.conv.clone());
    let sessions = running.turns.sessions();
    let session = sessions
        .lookup_or_create(agent, &thread, &scope)
        .await
        .unwrap();

    let report = sessions
        .run_turn(session.id, running.request("m1"))
        .await
        .unwrap();
    assert_eq!(report.process_start, Some(SessionStart::New));
    let TurnOutcome::Finished(result) = &report.outcome else {
        panic!("{:?}", report.outcome);
    };
    assert!(!result.is_error, "{result:?}");
    assert_eq!(result.result.as_deref(), Some("first reply"));
    let outbox = report.finished.unwrap().expect("the turn's outbox");
    assert_eq!(outbox.reactions().len(), 1, "{outbox:?}");
    assert_eq!(outbox.reactions()[0].emoji, "eyes");
    assert_eq!(outbox.reactions()[0].msg.id, MessageId::new("m1"));
    assert_eq!(outbox.posts().len(), 1, "{outbox:?}");
    assert_eq!(outbox.posts()[0].text, "queued text");
    assert_eq!(outbox.posts()[0].to.thread_root, Some(MessageId::new("m1")));

    let report = sessions
        .run_turn(session.id, running.request("m3"))
        .await
        .unwrap();
    assert_eq!(report.process_start, None, "the process stays warm");
    assert!(report.outcome.is_success(), "{:?}", report.outcome);
    assert!(report.finished.unwrap().unwrap().is_empty());

    let requests = running.fake.message_requests().await;
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            request.headers.get("authorization").unwrap(),
            &format!("Bearer {ACCESS_TOKEN}")
        );
    }
    assert!(running.script.exists());

    sessions.stop(session.id).await;
    assert!(!sessions.is_warm(session.id));
    running.stop().await;
}

#[tokio::test]
async fn an_unlinked_member_gets_the_proxys_refusal_as_an_auth_error() {
    let running = start(&[Turn::reply("never sent")]).await;
    let agent = AgentId::new_v4();
    runner::write_persona(running.dir.path(), agent, "You are a test.\n")
        .await
        .unwrap();
    running
        .app
        .store()
        .delete_claude_link(running.member)
        .await
        .unwrap();
    let thread = thread("m9");
    let scope = ScopeKey::Channel(thread.conv.clone());
    let sessions = running.turns.sessions();
    let session = sessions
        .lookup_or_create(agent, &thread, &scope)
        .await
        .unwrap();
    let report = sessions
        .run_turn(session.id, running.request("m9"))
        .await
        .unwrap();
    let TurnOutcome::Finished(result) = &report.outcome else {
        panic!("{:?}", report.outcome);
    };
    assert!(result.is_error);
    assert_eq!(result.api_error_status, Some(401));
    assert_eq!(result.error_kind, Some(runner::ErrorKind::Auth));
    assert!(running.fake.message_requests().await.is_empty());
    running.stop().await;
}

//! Turns through agentd's runner end to end: `fake-claude` in a process
//! sandbox, reaching the real credential proxy and agentctl API on agentd's
//! listeners, with `fake_anthropic()` as the upstream.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentd::pipeline::{TurnSettings, Turns};
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

use common::{TempDir, env};

const ACCESS_TOKEN: &str = "real-access-token";

struct Running {
    app: App,
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
    let claude = fake_claude_path();
    let agentctl = agentctl_path();
    let dir = TempDir::new();
    let fake = fake_anthropic().await;
    let text = format!(
        "{}\n[proxy]\nupstream = \"{}\"\n",
        common::CONFIG.replace("/nonexistent/agentd", &dir.path().display().to_string()),
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
    let server = Server::bind(app.clone(), Routers::new(&app).unwrap())
        .await
        .unwrap();
    let script = dir.path().join("script.json");
    write_script(&script, turns).unwrap();
    let settings = settings(server.addrs(), claude, agentctl, &script, dir.path());
    let sandbox = Arc::new(ProcessSandbox::new(app.store().clone(), dir.path()).unwrap());
    let turns = Turns::start(&app, Arc::clone(&sandbox) as _, settings).unwrap();
    let server = server.with_turns(turns.clone());
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
            },
            hop: Hop::ZERO,
            side: Side::Public,
            kind: TurnKind::Normal,
            trigger: Some(MessageId::new(trigger)),
        }
    }

    async fn stop(self) {
        self.shut_down(false).await;
    }

    /// Shuts agentd down, forced at once by a second signal if `forced`.
    async fn shut_down(self, forced: bool) {
        if forced {
            self.abort.send(()).unwrap();
        }
        self.stop.send(()).unwrap();
        self.task.await.unwrap().unwrap();
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
    let running = start(&[Turn::reply("warm")]).await;
    let session = running.warm_session("m1").await;
    let (turns, sandbox) = (running.turns.clone(), Arc::clone(&running.sandbox));
    running.shut_down(true).await;
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

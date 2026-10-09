//! `agentctl` driven against agentd's ctl API over TCP: each subcommand
//! directly, then through a `fake-claude` script as the model would run it.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use agentd::ctl::{Ctl, CtlSettings, ProcessInfo, ProcessToken, STAGING_DIR, SurfaceLookup, Turn};
use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::IntoResponse as _;
use core_types::{
    AgentId, ConsentId, ConvRef, CtlError, CtlErrorCode, Hop, LeaseId, LockRequest, LockResponse,
    MemberKey, MessageId, Msg, Requester, ScopeKey, SessionId, Side, Surface, SurfaceKind,
    ThreadKey, TurnId, TurnKind, VolumeKey,
};
use secrecy::ExposeSecret as _;
use serde_json::Value;
use store::Store;
use testkit::claude::SCRIPT_ENV;
use testkit::{MockSurface, TempDir, fake_anthropic, fake_claude_path, write_script};
use time::OffsetDateTime;
use time::macros::datetime;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Notify;

const WAIT: Duration = Duration::from_secs(60);

/// The address [`Server`]'s second listener presents every connection as
/// coming from: another container's, since every connection the tests make
/// comes from 127.0.0.1, and a container address holds one token at a time.
const OTHER_CONTAINER: &str = "127.0.0.3";

fn uuid() -> String {
    TurnId::new_v4().to_string()
}

#[derive(Debug)]
struct Lookup(Arc<MockSurface>);

#[async_trait::async_trait]
impl SurfaceLookup for Lookup {
    async fn surface(&self, _agent: AgentId, _conv: &ConvRef) -> Option<Arc<dyn Surface>> {
        Some(self.0.clone())
    }
}

struct Server {
    ctl: Ctl,
    url: String,
    /// The second listener, whose connections come from [`OTHER_CONTAINER`].
    other_url: String,
    /// The secrets of the tokens issued at [`OTHER_CONTAINER`].
    others: Mutex<HashSet<String>>,
    surface: Arc<MockSurface>,
    dir: TempDir,
}

impl Server {
    async fn start() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(tune: impl FnOnce(&mut CtlSettings)) -> Self {
        let dir = TempDir::new("agentctl-test");
        let key = store::Sealer::generate_key().unwrap();
        let store = Store::open_in_memory(store::Sealer::from_base64(&key).unwrap())
            .await
            .unwrap();
        let mut settings = CtlSettings {
            staging_dir: dir.join(STAGING_DIR),
            attach_max_bytes: 1024,
            lease_ttl: Duration::from_secs(30),
        };
        tune(&mut settings);
        let surface = Arc::new(MockSurface::new());
        let ctl = Ctl::new(store, settings, Arc::new(Lookup(surface.clone())));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let service = ctl
            .router()
            .into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, service).await });
        let other = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let other_url = format!("http://{}", other.local_addr().unwrap());
        let peer = SocketAddr::new(OTHER_CONTAINER.parse().unwrap(), 40_000);
        let service = ctl.router().layer(axum::Extension(ConnectInfo(peer)));
        tokio::spawn(async move { axum::serve(other, service).await });
        Self {
            ctl,
            url,
            other_url,
            others: Mutex::default(),
            surface,
            dir,
        }
    }

    async fn process_at(&self, ip: &str, volume: Option<VolumeKey>) -> (ProcessInfo, ProcessToken) {
        let agent = AgentId::new_v4();
        let info = ProcessInfo {
            session: SessionId::new_v4(),
            agent,
            volume: volume.unwrap_or(VolumeKey {
                agent,
                scope: ScopeKey::Channel(conv()),
            }),
            container_ip: ip.parse().unwrap(),
        };
        let token = self.ctl.issue_process_token(info.clone()).await.unwrap();
        (info, token)
    }

    /// A process on 127.0.0.1, where the tests' connections come from, with
    /// a public turn running.
    async fn turn(&self) -> (ProcessInfo, ProcessToken) {
        let (info, token) = self.process_at("127.0.0.1", None).await;
        self.ctl
            .begin_turn(&token, turn(Side::Public))
            .await
            .unwrap();
        (info, token)
    }

    /// A process at [`OTHER_CONTAINER`] on `volume`, with a public turn
    /// running.
    async fn other(&self, volume: VolumeKey) -> ProcessToken {
        let (_, token) = self.process_at(OTHER_CONTAINER, Some(volume)).await;
        self.others
            .lock()
            .unwrap()
            .insert(token.secret().expose_secret().to_owned());
        self.ctl
            .begin_turn(&token, turn(Side::Public))
            .await
            .unwrap();
        token
    }

    /// `agentctl` with `token`, connecting from the token's container.
    fn agentctl(&self, token: &ProcessToken) -> tokio::process::Command {
        let secret = token.secret().expose_secret();
        let other = self.others.lock().unwrap().contains(secret);
        let url = if other { &self.other_url } else { &self.url };
        agentctl(url, secret, self.dir.path())
    }

    async fn run(&self, token: &ProcessToken, args: &[&str]) -> Run {
        let output = tokio::time::timeout(WAIT, self.agentctl(token).args(args).output())
            .await
            .expect("agentctl timed out")
            .unwrap();
        Run::from(output)
    }
}

/// `agentctl` against the API at `url`, run in `dir`, with its output piped.
fn agentctl(url: &str, token: &str, dir: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_agentctl"));
    command
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("AGENTCTL_URL", url)
        .env("AGENTCTL_TOKEN", token)
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("http_proxy", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    command
}

/// Waits until `path` exists.
async fn wait_for(path: &Path) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < WAIT,
            "{} never appeared",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A shell script that runs a background subshell, a grandchild of
/// agentctl, that touches `marker` and appends to `log` until it is killed
/// or `marker` is gone.
fn writer(marker: &Path, log: &Path) -> String {
    format!(
        "(echo x >> {log}; touch {marker}; \
         while [ -e {marker} ]; do echo x >> {log}; sleep 0.05; done) & wait",
        log = log.display(),
        marker = marker.display()
    )
}

/// Asserts that nothing appends to `log` any more, and returns when it was
/// last written.
async fn stopped_writing(log: &Path) -> SystemTime {
    let before = std::fs::metadata(log).unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = std::fs::metadata(log).unwrap();
    assert_eq!(
        after.len(),
        before.len(),
        "the command's children still write"
    );
    after.modified().unwrap()
}

#[cfg(unix)]
fn terminate(child: &tokio::process::Child) {
    let pid = child.id().unwrap().to_string();
    let kill = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap();
    assert!(kill.success());
}

#[derive(Debug)]
struct Run {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl From<Output> for Run {
    fn from(output: Output) -> Self {
        Self {
            code: output.status.code(),
            stdout: String::from_utf8(output.stdout).unwrap(),
            stderr: String::from_utf8(output.stderr).unwrap(),
        }
    }
}

impl Run {
    #[track_caller]
    fn ok(&self) -> &str {
        assert_eq!(self.code, Some(0), "{self:?}");
        &self.stdout
    }

    /// Asserts a refusal: status 1 and one line on standard error.
    #[track_caller]
    fn refused(&self, contains: &str) {
        assert_eq!(self.code, Some(1), "{self:?}");
        assert!(self.stdout.is_empty(), "{self:?}");
        assert_eq!(self.stderr.lines().count(), 1, "{self:?}");
        assert!(self.stderr.starts_with("agentctl: "), "{self:?}");
        assert!(self.stderr.contains(contains), "{self:?}");
    }
}

fn conv() -> ConvRef {
    ConvRef {
        surface: SurfaceKind::Slack,
        team: "T1".into(),
        conversation: "C1".into(),
    }
}

fn thread() -> ThreadKey {
    ThreadKey {
        conv: conv(),
        root: Some(MessageId::new("100.1")),
    }
}

fn turn(side: Side) -> Turn {
    Turn {
        id: TurnId::new_v4(),
        requester: Requester {
            member: None,
            key: MemberKey {
                surface: SurfaceKind::Slack,
                team: "T1".into(),
                user: "U1".into(),
            },
        },
        hop: Hop::ZERO,
        kind: TurnKind::Normal,
        side,
        thread: thread(),
        trigger: Some(MessageId::new("100.2")),
    }
}

fn msg(id: &str, text: &str) -> Msg {
    Msg {
        id: MessageId::new(id),
        sender: MemberKey {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            user: "U2".into(),
        },
        sender_is_bot: false,
        text: text.into(),
        files: vec![],
        sent_at: datetime!(2026-09-30 10:00 UTC),
    }
}

#[tokio::test]
async fn each_subcommand_works_against_the_server() {
    let server = Server::start().await;
    server
        .surface
        .set_history(thread(), vec![msg("1", "first"), msg("2", "second")]);
    let (_, token) = server.turn().await;
    std::fs::write(server.dir.join("notes.md"), "# notes\n").unwrap();

    let out = server.run(&token, &["attach", "notes.md"]).await;
    assert_eq!(
        out.ok(),
        "Staged notes.md (8 bytes). It is uploaded with this turn's reply.\n"
    );
    let out = server
        .run(&token, &["post", "--to", "here", "all", "done"])
        .await;
    assert_eq!(out.ok(), "Queued. The message is posted after this turn.\n");
    let out = server.run(&token, &["react", ":tada:"]).await;
    assert_eq!(
        out.ok(),
        "Queued :tada:. The reaction is added after this turn.\n"
    );
    let out = server.run(&token, &["history", "--before", "2"]).await;
    assert_eq!(out.ok(), "[1] U2 at 2026-09-30T10:00:00Z:\nfirst\n");
    let out = server
        .run(&token, &["lock", "--", "sh", "-c", "echo inside the lock"])
        .await;
    assert_eq!(out.ok(), "inside the lock\n");
    server
        .run(&token, &["ask-agent", "reviewer", "look", "at", "this"])
        .await
        .refused("agentctl ask-agent is not available yet");
    server
        .run(&token, &["private", "--file", "notes.md", "check", "it"])
        .await
        .refused("agentctl private is not available yet");

    let outbox = server.ctl.end_turn(&token).await.unwrap().unwrap();
    assert_eq!(outbox.attachments().len(), 1);
    assert_eq!(outbox.attachments()[0].name, "notes.md");
    assert_eq!(
        std::fs::read_to_string(&outbox.attachments()[0].path).unwrap(),
        "# notes\n"
    );
    assert_eq!(outbox.posts().len(), 1);
    assert_eq!(outbox.posts()[0].text, "all done");
    assert_eq!(outbox.posts()[0].to, thread().into());
    assert_eq!(outbox.reactions().len(), 1);
    assert_eq!(outbox.reactions()[0].emoji, "tada");
    assert_eq!(outbox.reactions()[0].msg.id, MessageId::new("100.2"));
}

#[tokio::test]
async fn refusals_exit_non_zero_with_one_line() {
    let server = Server::start().await;
    let (_, idle) = server.process_at("127.0.0.1", None).await;
    server
        .run(&idle, &["post", "--to", "here", "hi"])
        .await
        .refused("no turn is running");

    let (_, elsewhere) = server.process_at("127.0.0.2", None).await;
    server
        .ctl
        .begin_turn(&elsewhere, turn(Side::Public))
        .await
        .unwrap();
    server
        .run(&elsewhere, &["react", "eyes"])
        .await
        .refused("not valid from this container");

    let (_, token) = server.turn().await;
    server
        .run(&token, &["post", "--to", "C2", "hi"])
        .await
        .refused("may only target this conversation");
    server
        .run(&token, &["attach", "missing.txt"])
        .await
        .refused("can't read");
    server
        .run(&token, &["attach", "."])
        .await
        .refused("names no file");
    std::fs::write(server.dir.join("big.bin"), vec![b'x'; 8 << 20]).unwrap();
    server
        .run(&token, &["attach", "big.bin"])
        .await
        .refused("attachment limit");
    server.ctl.revoke_process_token(&token).await.unwrap();
    server.run(&token, &["history"]).await.refused("revoked");

    let (_, private) = server.process_at("127.0.0.1", None).await;
    let mut task = turn(Side::Owner);
    task.kind = TurnKind::PrivateTask(ConsentId::new_v4());
    server.ctl.begin_turn(&private, task).await.unwrap();
    server
        .run(&private, &["history"])
        .await
        .refused("only `agentctl attach`");
    std::fs::write(server.dir.join("result.txt"), "42").unwrap();
    server.run(&private, &["attach", "result.txt"]).await.ok();

    let output = server
        .agentctl(&token)
        .env_remove("AGENTCTL_TOKEN")
        .arg("history")
        .output()
        .await
        .unwrap();
    Run::from(output).refused("AGENTCTL_TOKEN is not set");
    let output = server
        .agentctl(&token)
        .env("AGENTCTL_URL", "http://127.0.0.1:1")
        .arg("history")
        .output()
        .await
        .unwrap();
    Run::from(output).refused("can't connect");
}

#[tokio::test]
async fn usage_errors_exit_with_status_2() {
    let server = Server::start().await;
    let (_, token) = server.turn().await;
    for args in [&["lock", "true"][..], &["post", "hi"], &["frobnicate"]] {
        let out = server.run(&token, args).await;
        assert_eq!(out.code, Some(2), "{args:?}: {out:?}");
    }
    assert!(server.run(&token, &["--help"]).await.ok().contains("lock"));
}

#[tokio::test]
async fn lock_passes_on_the_command_status() {
    let server = Server::start().await;
    let (_, token) = server.turn().await;
    let out = server
        .run(&token, &["lock", "--", "sh", "-c", "exit 3"])
        .await;
    assert_eq!(out.code, Some(3), "{out:?}");
    server
        .run(&token, &["lock", "--", "/nonexistent/cmd"])
        .await
        .refused("can't run /nonexistent/cmd");
    let out = server.run(&token, &["lock", "--", "true"]).await;
    out.ok();
}

/// Runs `agentctl lock` twice at once, each appending `start` and `end`
/// around a sleep, and returns the log. The second starts once the first
/// holds the lock, however long the first takes to start.
async fn two_locks(server: &Server, a: &ProcessToken, b: &ProcessToken) -> Vec<String> {
    let log = server.dir.join(format!("log-{}", uuid()));
    let script = |tag: &str| {
        format!(
            "echo start-{tag} >> {log}; sleep 1; echo end-{tag} >> {log}",
            log = log.display()
        )
    };
    let first = server
        .agentctl(a)
        .args(["lock", "--", "sh", "-c", &script("a")])
        .spawn()
        .unwrap();
    wait_for(&log).await;
    let second = server
        .agentctl(b)
        .args(["lock", "--", "sh", "-c", &script("b")])
        .spawn()
        .unwrap();
    let (first, second) = tokio::join!(first.wait_with_output(), second.wait_with_output());
    Run::from(first.unwrap()).ok();
    let second = Run::from(second.unwrap());
    second.ok();
    assert!(
        second.stderr.contains("waiting for the shared/ lock"),
        "{second:?}"
    );
    std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn a_second_lock_in_the_same_session_waits() {
    let server = Server::start().await;
    let (_, token) = server.turn().await;
    assert_eq!(
        two_locks(&server, &token, &token).await,
        ["start-a", "end-a", "start-b", "end-b"]
    );
}

#[tokio::test]
async fn a_second_session_waits_for_the_lock() {
    let server = Server::start().await;
    let (info, a) = server.turn().await;
    let b = server.other(info.volume).await;
    assert_eq!(
        two_locks(&server, &a, &b).await,
        ["start-a", "end-a", "start-b", "end-b"]
    );
}

#[tokio::test]
async fn lock_gives_up_after_its_timeout() {
    let server = Server::start().await;
    let (_, token) = server.turn().await;
    let marker = server.dir.join("holding");
    let holder = server
        .agentctl(&token)
        .args([
            "lock",
            "--",
            "sh",
            "-c",
            &format!("touch {}; sleep 3", marker.display()),
        ])
        .spawn()
        .unwrap();
    wait_for(&marker).await;
    let out = server
        .run(&token, &["lock", "--timeout", "1", "--", "true"])
        .await;
    assert_eq!(out.code, Some(1), "{out:?}");
    assert_eq!(
        out.stderr,
        "agentctl: waiting for the shared/ lock\n\
         agentctl: gave up waiting for the shared/ lock after the 1s timeout; another command \
         holds it\n"
    );
    Run::from(holder.wait_with_output().await.unwrap()).ok();
}

#[tokio::test]
async fn a_huge_lock_timeout_is_clamped_instead_of_panicking() {
    let server = Server::start().await;
    let (_, token) = server.turn().await;
    server
        .run(
            &token,
            &["lock", "--timeout", "18446744073709551615", "--", "true"],
        )
        .await
        .ok();
}

#[tokio::test]
async fn a_stalled_acquire_gives_up_at_the_lock_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let dir = TempDir::new("agentctl-test");
    let started = Instant::now();
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        agentctl(&url, "tok", dir.path())
            .args(["lock", "--timeout", "1", "--", "true"])
            .output(),
    )
    .await
    .expect("the acquire waited out the 30-second request timeout")
    .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(7));
    let out = Run::from(output);
    out.refused("gave up waiting for the shared/ lock after the 1s timeout; agentd at ");
    assert!(
        out.stderr.ends_with(
            ": timed out; a lease agentd granted after agentctl stopped waiting expires within \
             its TTL\n"
        ),
        "{out:?}"
    );
}

#[tokio::test]
async fn an_acquire_that_failed_or_lost_its_answer_is_retried_under_the_same_lease() {
    for hiccup in [Hiccup::Internal, Hiccup::Garbled] {
        let fake = FakeLock::start(30, 0, Renewals::Grant).await;
        fake.state.hiccups.lock().unwrap().push(hiccup);
        let dir = TempDir::new("agentctl-test");
        let out = Run::from(
            tokio::time::timeout(
                WAIT,
                agentctl(&fake.url, "tok", dir.path())
                    .args(["lock", "--", "true"])
                    .output(),
            )
            .await
            .unwrap()
            .unwrap(),
        );
        out.ok();
        assert!(fake.state.hiccups.lock().unwrap().is_empty(), "{hiccup:?}");
        assert_eq!(fake.granted().len(), 1, "{hiccup:?}");
        assert_eq!(fake.released(), fake.granted(), "{hiccup:?}");
    }
}

#[tokio::test]
async fn an_acquire_that_cant_connect_fails_at_once() {
    let url = "http://127.0.0.1:1";
    let dir = TempDir::new("agentctl-test");
    let started = Instant::now();
    let out = Run::from(
        tokio::time::timeout(
            WAIT,
            agentctl(url, "tok", dir.path())
                .args(["lock", "--", "true"])
                .output(),
        )
        .await
        .unwrap()
        .unwrap(),
    );
    assert!(started.elapsed() < Duration::from_secs(5), "{out:?}");
    out.refused("can't connect");
    assert_eq!(
        out.stderr,
        format!("agentctl: agentd at {url}: can't connect\n")
    );
}

#[tokio::test]
async fn the_lock_renews_while_the_command_runs() {
    let server = Server::with(|settings| settings.lease_ttl = Duration::from_secs(3)).await;
    let (_, token) = server.turn().await;
    let (marker, release) = (server.dir.join("holding"), server.dir.join("release"));
    let holder = server
        .agentctl(&token)
        .args([
            "lock",
            "--",
            "sh",
            "-c",
            &format!(
                "touch {marker}; while [ -e {marker} ] && [ ! -e {release} ]; do sleep 0.05; done",
                marker = marker.display(),
                release = release.display()
            ),
        ])
        .spawn()
        .unwrap();
    wait_for(&marker).await;
    tokio::time::sleep(Duration::from_millis(4_000)).await;
    server
        .run(&token, &["lock", "--timeout", "0", "--", "true"])
        .await
        .refused("gave up");
    std::fs::write(&release, "").unwrap();
    Run::from(holder.wait_with_output().await.unwrap()).ok();
    server.run(&token, &["lock", "--", "true"]).await.ok();
}

#[tokio::test]
async fn the_lock_expires_when_its_holder_dies() {
    let server = Server::with(|settings| settings.lease_ttl = Duration::from_secs(3)).await;
    let (_, token) = server.turn().await;
    let marker = server.dir.join("holding");
    let mut holder = server
        .agentctl(&token)
        .args([
            "lock",
            "--",
            "sh",
            "-c",
            &format!("touch {}; sleep 8", marker.display()),
        ])
        .spawn()
        .unwrap();
    wait_for(&marker).await;
    holder.kill().await.unwrap();
    let killed = Instant::now();
    server
        .run(&token, &["lock", "--timeout", "10", "--", "true"])
        .await
        .ok();
    assert!(
        killed.elapsed() < Duration::from_secs(7),
        "the lease expired before the orphaned command ended"
    );
}

#[tokio::test]
async fn losing_the_lease_stops_the_command_and_frees_the_lock_at_once() {
    let server = Server::with(|settings| settings.lease_ttl = Duration::from_secs(3)).await;
    let (info, token) = server.turn().await;
    let other = server.other(info.volume).await;
    let hold = |marker: &Path| {
        server
            .agentctl(&token)
            .args([
                "lock",
                "--",
                "sh",
                "-c",
                &format!("touch {}; exec sleep 20", marker.display()),
            ])
            .spawn()
            .unwrap()
    };

    let marker = server.dir.join("ended");
    let holder = hold(&marker);
    wait_for(&marker).await;
    let started = Instant::now();
    server.ctl.end_turn(&token).await.unwrap();
    server
        .run(&other, &["lock", "--timeout", "0", "--", "true"])
        .await
        .ok();
    let out = Run::from(
        tokio::time::timeout(WAIT, holder.wait_with_output())
            .await
            .unwrap()
            .unwrap(),
    );
    out.refused("lost the shared/ lock (no turn is running");
    assert!(started.elapsed() < Duration::from_secs(10), "{out:?}");

    server
        .ctl
        .begin_turn(&token, turn(Side::Public))
        .await
        .unwrap();
    let marker = server.dir.join("revoked");
    let holder = hold(&marker);
    wait_for(&marker).await;
    server.ctl.revoke_process_token(&token).await.unwrap();
    server
        .run(&other, &["lock", "--timeout", "0", "--", "true"])
        .await
        .ok();
    let out = Run::from(
        tokio::time::timeout(WAIT, holder.wait_with_output())
            .await
            .unwrap()
            .unwrap(),
    );
    out.refused("lost the shared/ lock (the agentctl token is missing, revoked");
}

/// How [`FakeLock`] answers renewals and releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Renewals {
    /// It never answers them.
    Stall,
    /// It fails the first renewal with agentd's internal error, and grants
    /// the rest.
    FailOnce,
    /// It grants renewals with more seconds left than a clock can hold.
    Endless,
    /// It grants every renewal.
    Grant,
}

/// How [`FakeLock`] answers an acquire instead of simply granting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hiccup {
    /// It fails with agentd's internal error, granting nothing.
    Internal,
    /// It grants the lease, then answers with something unreadable.
    Garbled,
}

/// A fake ctl API whose lock grants leases of `ttl` seconds, with whole
/// seconds as agentd's store has them, from a clock `skew` seconds ahead of
/// the real one.
struct FakeLock {
    url: String,
    state: Arc<FakeState>,
}

struct FakeState {
    ttl: i64,
    skew: i64,
    renewals: Renewals,
    acquire_delay: Duration,
    /// The hiccups to answer the next acquires with, last first.
    hiccups: Mutex<Vec<Hiccup>>,
    expires_at: Mutex<Option<SystemTime>>,
    granted: Mutex<Vec<LeaseId>>,
    /// The lease each acquire named, recorded as it arrives.
    asked: Mutex<Vec<LeaseId>>,
    renewed: Mutex<usize>,
    released: Mutex<Vec<LeaseId>>,
    acquiring: Notify,
    renewing: Notify,
}

impl FakeLock {
    async fn start(ttl: i64, skew: i64, renewals: Renewals) -> Self {
        Self::delayed(ttl, skew, renewals, Duration::ZERO).await
    }

    /// A fake that answers each acquire only after `acquire_delay`.
    async fn delayed(ttl: i64, skew: i64, renewals: Renewals, acquire_delay: Duration) -> Self {
        let state = Arc::new(FakeState {
            ttl,
            skew,
            renewals,
            acquire_delay,
            hiccups: Mutex::new(Vec::new()),
            expires_at: Mutex::new(None),
            granted: Mutex::new(Vec::new()),
            asked: Mutex::new(Vec::new()),
            renewed: Mutex::new(0),
            released: Mutex::new(Vec::new()),
            acquiring: Notify::new(),
            renewing: Notify::new(),
        });
        let app = axum::Router::new()
            .route("/v1/lock", axum::routing::post(fake_lock))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, state }
    }

    /// When the lease last granted or renewed really runs out.
    fn expires_at(&self) -> SystemTime {
        self.state.expires_at.lock().unwrap().unwrap()
    }

    fn granted(&self) -> Vec<LeaseId> {
        self.state.granted.lock().unwrap().clone()
    }

    fn renewed(&self) -> usize {
        *self.state.renewed.lock().unwrap()
    }

    fn released(&self) -> Vec<LeaseId> {
        self.state.released.lock().unwrap().clone()
    }
}

impl FakeState {
    fn held(&self, lease: LeaseId) -> axum::response::Response {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let expires_at = now + u64::try_from(self.ttl).unwrap();
        *self.expires_at.lock().unwrap() =
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(expires_at));
        let skewed = i64::try_from(expires_at).unwrap() + self.skew;
        Json(LockResponse::Held {
            lease,
            expires_at: OffsetDateTime::from_unix_timestamp(skewed).unwrap(),
            seconds_left: u64::try_from(self.ttl).unwrap(),
        })
        .into_response()
    }
}

/// agentd's answer when it fails, as on a busy database.
fn internal_error() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(CtlError::new(
            CtlErrorCode::Internal,
            "agentd failed; try again",
        )),
    )
        .into_response()
}

async fn fake_lock(
    State(state): State<Arc<FakeState>>,
    Json(request): Json<LockRequest>,
) -> axum::response::Response {
    match request {
        LockRequest::Acquire { lease } => {
            state.asked.lock().unwrap().push(lease);
            state.acquiring.notify_one();
            tokio::time::sleep(state.acquire_delay).await;
            let hiccup = state.hiccups.lock().unwrap().pop();
            if hiccup == Some(Hiccup::Internal) {
                return internal_error();
            }
            {
                let mut granted = state.granted.lock().unwrap();
                if !granted.contains(&lease) {
                    granted.push(lease);
                }
            }
            if hiccup == Some(Hiccup::Garbled) {
                return "garbled".into_response();
            }
            state.held(lease)
        }
        LockRequest::Renew { lease } => {
            state.renewing.notify_one();
            if state.renewals == Renewals::Stall {
                return std::future::pending().await;
            }
            if state.renewals == Renewals::Endless {
                return Json(LockResponse::Held {
                    lease,
                    expires_at: OffsetDateTime::now_utc(),
                    seconds_left: u64::MAX,
                })
                .into_response();
            }
            let first = {
                let mut renewed = state.renewed.lock().unwrap();
                *renewed += 1;
                *renewed == 1
            };
            if first && state.renewals == Renewals::FailOnce {
                return internal_error();
            }
            state.held(lease)
        }
        LockRequest::Release { lease } => {
            state.released.lock().unwrap().push(lease);
            if state.renewals == Renewals::Stall {
                return std::future::pending().await;
            }
            Json(LockResponse::Released).into_response()
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_stalled_renewal_stops_the_command_and_its_children_before_the_lease_runs_out() {
    let fake = FakeLock::start(3, 5, Renewals::Stall).await;
    let dir = TempDir::new("agentctl-test");
    let (marker, log) = (dir.join("started"), dir.join("log"));
    let output = tokio::time::timeout(
        WAIT,
        agentctl(&fake.url, "tok", dir.path())
            .args(["lock", "--", "sh", "-c", &writer(&marker, &log)])
            .output(),
    )
    .await
    .expect("agentctl, or a process holding its output, outlived the kill")
    .unwrap();
    Run::from(output).refused("lost the shared/ lock (");
    assert!(
        stopped_writing(&log).await < fake.expires_at(),
        "the command wrote after the lease expired, timed by a clock ahead of agentctl's"
    );
}

#[tokio::test]
async fn a_transient_agentd_error_is_retried_under_a_clock_behind_agentctls() {
    let fake = FakeLock::start(3, -40, Renewals::FailOnce).await;
    let dir = TempDir::new("agentctl-test");
    let out = Run::from(
        tokio::time::timeout(
            WAIT,
            agentctl(&fake.url, "tok", dir.path())
                .args(["lock", "--", "sleep", "2"])
                .output(),
        )
        .await
        .unwrap()
        .unwrap(),
    );
    out.ok();
    assert!(out.stderr.is_empty(), "{out:?}");
    assert!(fake.renewed() >= 3, "{} renewals", fake.renewed());
    assert_eq!(fake.released(), fake.granted());
}

#[tokio::test]
async fn a_renewal_too_long_for_the_clock_stops_the_command_and_says_so() {
    let fake = FakeLock::start(3, 0, Renewals::Endless).await;
    let dir = TempDir::new("agentctl-test");
    let out = Run::from(
        tokio::time::timeout(
            WAIT,
            agentctl(&fake.url, "tok", dir.path())
                .args(["lock", "--", "sleep", "30"])
                .output(),
        )
        .await
        .unwrap()
        .unwrap(),
    );
    out.refused(
        "lost the shared/ lock (agentd granted a lease longer than the clock can hold); \
         stopped the command",
    );
    assert_eq!(fake.released(), fake.granted());
}

#[tokio::test]
async fn a_lease_too_short_to_renew_is_given_back() {
    let fake = FakeLock::start(2, 0, Renewals::FailOnce).await;
    let dir = TempDir::new("agentctl-test");
    let marker = dir.join("ran");
    let out = Run::from(
        tokio::time::timeout(
            WAIT,
            agentctl(&fake.url, "tok", dir.path())
                .args(["lock", "--", "touch", marker.to_str().unwrap()])
                .output(),
        )
        .await
        .unwrap()
        .unwrap(),
    );
    out.refused("the shared/ lock's lease is too short to hold (agentd granted 2s; agentctl needs at least 3s)");
    assert!(!marker.exists());
    assert_eq!(fake.released(), fake.granted());
    assert_eq!(fake.granted().len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn a_signal_during_acquire_gives_back_the_lease_it_was_granted() {
    let fake = FakeLock::delayed(30, 0, Renewals::FailOnce, Duration::from_millis(1_500)).await;
    let dir = TempDir::new("agentctl-test");
    let marker = dir.join("ran");
    let holder = agentctl(&fake.url, "tok", dir.path())
        .args(["lock", "--", "touch", marker.to_str().unwrap()])
        .spawn()
        .unwrap();
    tokio::time::timeout(WAIT, fake.state.acquiring.notified())
        .await
        .expect("agentctl never acquired");
    terminate(&holder);
    let out = Run::from(
        tokio::time::timeout(WAIT, holder.wait_with_output())
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(out.code, Some(143), "{out:?}");
    assert!(!marker.exists(), "{out:?}");
    assert_eq!(fake.granted().len(), 1, "{out:?}");
    assert_eq!(fake.released(), fake.granted(), "{out:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_signal_during_an_unanswered_acquire_still_gives_back_its_lease() {
    let fake = FakeLock::delayed(30, 0, Renewals::Grant, Duration::from_secs(4)).await;
    let dir = TempDir::new("agentctl-test");
    let marker = dir.join("ran");
    let holder = agentctl(&fake.url, "tok", dir.path())
        .args(["lock", "--", "touch", marker.to_str().unwrap()])
        .spawn()
        .unwrap();
    tokio::time::timeout(WAIT, fake.state.acquiring.notified())
        .await
        .expect("agentctl never acquired");
    terminate(&holder);
    let out = Run::from(
        tokio::time::timeout(WAIT, holder.wait_with_output())
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(out.code, Some(143), "{out:?}");
    assert!(!marker.exists(), "{out:?}");
    let asked = fake.state.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{out:?}");
    assert_eq!(fake.released(), asked, "{out:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_signal_during_a_stalled_renewal_stops_the_lock_at_once() {
    let fake = FakeLock::start(6, 0, Renewals::Stall).await;
    let dir = TempDir::new("agentctl-test");
    let holder = agentctl(&fake.url, "tok", dir.path())
        .args(["lock", "--", "sleep", "30"])
        .spawn()
        .unwrap();
    tokio::time::timeout(WAIT, fake.state.renewing.notified())
        .await
        .expect("agentctl never renewed");
    let signalled = Instant::now();
    terminate(&holder);
    let out = Run::from(
        tokio::time::timeout(WAIT, holder.wait_with_output())
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(out.code, Some(143), "{out:?}");
    assert!(
        out.stderr.contains("releasing the shared/ lock failed"),
        "{out:?}"
    );
    assert!(signalled.elapsed() < Duration::from_secs(4), "{out:?}");
}

/// The fake CLI's stream-json output, one value per line.
fn lines(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn tool_results(lines: &[Value]) -> Vec<(bool, String)> {
    lines
        .iter()
        .filter(|line| line["type"] == "user")
        .map(|line| {
            let block = &line["message"]["content"][0];
            (
                block["is_error"].as_bool().unwrap(),
                block["content"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn fake_claude(
    server: &Server,
    token: &ProcessToken,
    dir: &Path,
    base_url: &str,
    session: &str,
) -> tokio::process::Command {
    let bin = Path::new(env!("CARGO_BIN_EXE_agentctl")).parent().unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = tokio::process::Command::new(fake_claude_path());
    command
        .args([
            "-p",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
            "--tools",
            "Bash,Read,Edit,Write,Glob,Grep,Skill",
            "--strict-mcp-config",
            "--setting-sources",
            "user",
            "--permission-mode",
            "bypassPermissions",
            "--session-id",
            session,
        ])
        .current_dir(dir.join("work"))
        .env_clear()
        .env("PATH", path)
        .env("CLAUDE_CONFIG_DIR", dir.join("claude"))
        .env("CLAUDE_CODE_PROJECT_DIR_NAME", session)
        .env("ANTHROPIC_BASE_URL", base_url)
        .env("ANTHROPIC_API_KEY", "placeholder")
        .env(SCRIPT_ENV, dir.join("script.json"))
        .env("AGENTCTL_URL", &server.url)
        .env("AGENTCTL_TOKEN", token.secret().expose_secret())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    command
}

#[tokio::test]
async fn the_model_runs_agentctl_through_its_bash_tool() {
    let server = Server::start().await;
    server
        .surface
        .set_history(thread(), vec![msg("1", "earlier in the thread")]);
    let (_, token) = server.turn().await;
    let anthropic = fake_anthropic().await;
    let dir = TempDir::new("agentctl-test");
    for sub in ["claude", "work"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    std::fs::write(dir.join("work").join("plot.txt"), "a plot").unwrap();
    let commands: [&[&str]; 8] = [
        &["agentctl", "attach", "plot.txt"],
        &["agentctl", "post", "--to", "here", "see", "the", "plot"],
        &["agentctl", "react", "eyes", "99.9"],
        &["agentctl", "history"],
        &["agentctl", "lock", "--", "sh", "-c", "echo locked"],
        &["agentctl", "post", "--to", "C2", "elsewhere"],
        &["agentctl", "ask-agent", "reviewer", "review"],
        &["agentctl", "private", "task"],
    ];
    let script = commands
        .iter()
        .fold(testkit::Turn::reply("Done."), |turn, argv| {
            turn.with_command(argv.iter().copied())
        });
    write_script(&dir.join("script.json"), &[script]).unwrap();

    let session = uuid();
    let mut child = fake_claude(&server, &token, dir.path(), &anthropic.uri(), &session)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let line = serde_json::json!({
        "type": "user",
        "message": {"role": "user", "content": "make a plot"},
    });
    stdin
        .write_all(format!("{line}\n").as_bytes())
        .await
        .unwrap();
    drop(stdin);
    let output = tokio::time::timeout(WAIT, child.wait_with_output())
        .await
        .expect("fake-claude timed out")
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let results = tool_results(&lines(&output.stdout));
    let expected: [(bool, &str); 8] = [
        (
            false,
            "Staged plot.txt (6 bytes). It is uploaded with this turn's reply.",
        ),
        (false, "Queued. The message is posted after this turn."),
        (
            false,
            "Queued :eyes:. The reaction is added after this turn.",
        ),
        (
            false,
            "[1] U2 at 2026-09-30T10:00:00Z:\nearlier in the thread",
        ),
        (false, "locked"),
        (
            true,
            "Exit code 1\nagentctl: on the public side, agentctl post may only target this \
             conversation; use --to here",
        ),
        (
            true,
            "Exit code 1\nagentctl: agentctl ask-agent is not available yet",
        ),
        (
            true,
            "Exit code 1\nagentctl: agentctl private is not available yet",
        ),
    ];
    let results: Vec<(bool, &str)> = results.iter().map(|(e, c)| (*e, c.as_str())).collect();
    assert_eq!(results, expected);

    let outbox = server.ctl.end_turn(&token).await.unwrap().unwrap();
    assert_eq!(outbox.attachments().len(), 1);
    assert_eq!(outbox.posts().len(), 1);
    assert_eq!(outbox.posts()[0].text, "see the plot");
    assert_eq!(outbox.reactions()[0].msg.id, MessageId::new("99.9"));
}

#[cfg(unix)]
#[tokio::test]
async fn a_terminated_lock_stops_its_command_and_releases_the_lease() {
    let server = Server::start().await;
    let (_, token) = server.turn().await;
    let (marker, log) = (server.dir.join("holding"), server.dir.join("log"));
    let holder = server
        .agentctl(&token)
        .args(["lock", "--", "sh", "-c", &writer(&marker, &log)])
        .spawn()
        .unwrap();
    wait_for(&marker).await;
    terminate(&holder);
    let out = Run::from(
        tokio::time::timeout(WAIT, holder.wait_with_output())
            .await
            .expect("agentctl, or a process holding its output, outlived the kill")
            .unwrap(),
    );
    assert_eq!(out.code, Some(143), "{out:?}");
    stopped_writing(&log).await;
    server
        .run(&token, &["lock", "--timeout", "0", "--", "true"])
        .await
        .ok();
}

#[cfg(unix)]
#[tokio::test]
async fn a_stop_signal_reaches_the_command_before_its_group_is_killed() {
    let server = Server::start().await;
    let (_, token) = server.turn().await;
    let (marker, cleaned) = (server.dir.join("trapping"), server.dir.join("cleaned"));
    let holder = server
        .agentctl(&token)
        .args([
            "lock",
            "--",
            "sh",
            "-c",
            &format!(
                "trap 'echo cleaned > {cleaned}; exit 0' TERM; touch {marker}; \
                 while [ -e {marker} ]; do sleep 0.1; done",
                cleaned = cleaned.display(),
                marker = marker.display()
            ),
        ])
        .spawn()
        .unwrap();
    wait_for(&marker).await;
    terminate(&holder);
    let out = Run::from(
        tokio::time::timeout(WAIT, holder.wait_with_output())
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(out.code, Some(143), "{out:?}");
    assert_eq!(std::fs::read_to_string(&cleaned).unwrap(), "cleaned\n");
    server
        .run(&token, &["lock", "--timeout", "0", "--", "true"])
        .await
        .ok();

    let marker = server.dir.join("ignoring");
    let holder = server
        .agentctl(&token)
        .args([
            "lock",
            "--",
            "sh",
            "-c",
            &format!("trap '' TERM; touch {}; exec sleep 30", marker.display()),
        ])
        .spawn()
        .unwrap();
    wait_for(&marker).await;
    let signalled = Instant::now();
    terminate(&holder);
    let out = Run::from(
        tokio::time::timeout(WAIT, holder.wait_with_output())
            .await
            .expect("a command ignoring the signal outlived the kill")
            .unwrap(),
    );
    assert_eq!(out.code, Some(143), "{out:?}");
    let took = signalled.elapsed();
    assert!(
        took >= Duration::from_millis(1_500) && took < Duration::from_secs(6),
        "{took:?}"
    );
    server
        .run(&token, &["lock", "--timeout", "0", "--", "true"])
        .await
        .ok();
}

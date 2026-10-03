use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, header};
use core_types::{
    ConsentId, Hop, MemberKey, MessageId, Msg, MsgRef, ReplyTarget, Requester, ScopeKey, Side,
    SurfaceKind, ThreadKey, TurnKind,
};
use http_body_util::BodyExt as _;
use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use testkit::{MockSurface, TempDir};
use time::macros::datetime;
use tower::ServiceExt as _;

use super::token::hash_token;
use super::*;

const CONTAINER: &str = "172.30.0.7";
const ALL_PATHS: [&str; 7] = [
    "/v1/attach?name=a.txt",
    "/v1/post",
    "/v1/react",
    "/v1/history",
    "/v1/lock",
    "/v1/ask-agent",
    "/v1/private",
];

struct Lookup(Arc<MockSurface>);

impl SurfaceLookup for Lookup {
    fn surface(&self, _agent: AgentId, conv: &ConvRef) -> Option<Arc<dyn Surface>> {
        (conv.surface == SurfaceKind::Slack).then(|| self.0.clone() as Arc<dyn Surface>)
    }
}

struct Fixture {
    ctl: Ctl,
    store: Store,
    surface: Arc<MockSurface>,
    dir: TempDir,
}

fn sealer() -> store::Sealer {
    store::Sealer::from_base64(&store::Sealer::generate_key().unwrap()).unwrap()
}

impl Fixture {
    async fn new() -> Self {
        Self::with(|_| {}).await
    }

    async fn with(tune: impl FnOnce(&mut CtlSettings)) -> Self {
        let store = Store::open_in_memory(sealer()).await.unwrap();
        Self::over(store, TempDir::new("agentd-ctl"), tune)
    }

    fn over(store: Store, dir: TempDir, tune: impl FnOnce(&mut CtlSettings)) -> Self {
        let mut settings = CtlSettings {
            staging_dir: dir.join(STAGING_DIR),
            attach_max_bytes: 1024,
            lease_ttl: DEFAULT_LEASE_TTL,
        };
        tune(&mut settings);
        let surface = Arc::new(MockSurface::new());
        let ctl = Ctl::new(store.clone(), settings, Arc::new(Lookup(surface.clone())));
        Self {
            ctl,
            store,
            surface,
            dir,
        }
    }

    async fn process(&self) -> (ProcessInfo, ProcessToken) {
        let agent = AgentId::new_v4();
        let info = ProcessInfo {
            session: SessionId::new_v4(),
            agent,
            volume: VolumeKey {
                agent,
                scope: ScopeKey::Channel(conv("C1")),
            },
            container_ip: CONTAINER.parse().unwrap(),
        };
        let token = self.ctl.issue_process_token(info.clone()).await.unwrap();
        (info, token)
    }

    /// A second process in the same volume, as another session of the same
    /// channel would be.
    async fn sibling(&self, of: &ProcessInfo) -> ProcessToken {
        self.ctl
            .issue_process_token(ProcessInfo {
                session: SessionId::new_v4(),
                ..of.clone()
            })
            .await
            .unwrap()
    }

    async fn call(&self, token: Option<&ProcessToken>, path: &str, body: Value) -> (u16, Value) {
        let body = Body::from(serde_json::to_vec(&body).unwrap());
        self.send(token.map(bearer), CONTAINER, path, body).await
    }

    async fn send(
        &self,
        authorization: Option<String>,
        peer: &str,
        path: &str,
        body: Body,
    ) -> (u16, Value) {
        let mut request = Request::post(path);
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        if !peer.is_empty() {
            let ip: IpAddr = peer.parse().unwrap();
            request = request.extension(ConnectInfo(SocketAddr::new(ip, 40_000)));
        }
        let response = self
            .ctl
            .router()
            .oneshot(request.body(body).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, value)
    }

    async fn attach(&self, token: &ProcessToken, name: &str, bytes: &[u8]) -> (u16, Value) {
        let path = format!("/v1/attach?name={name}");
        self.send(
            Some(bearer(token)),
            CONTAINER,
            &path,
            Body::from(bytes.to_vec()),
        )
        .await
    }
}

fn bearer(token: &ProcessToken) -> String {
    format!("Bearer {}", token.secret().expose_secret())
}

fn conv(id: &str) -> ConvRef {
    ConvRef {
        surface: SurfaceKind::Slack,
        team: "T1".into(),
        conversation: id.into(),
    }
}

fn thread() -> ThreadKey {
    ThreadKey {
        conv: conv("C1"),
        root: Some(MessageId::new("100.1")),
    }
}

fn turn(kind: TurnKind, side: Side) -> CtlTurn {
    CtlTurn {
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
        kind,
        side,
        thread: thread(),
        trigger: Some(MessageId::new("100.2")),
    }
}

fn public() -> CtlTurn {
    turn(TurnKind::Normal, Side::Public)
}

fn code(body: &Value) -> &str {
    body["code"].as_str().unwrap_or("")
}

fn post(to: &str) -> Value {
    json!({"to": to, "text": "hello"})
}

#[tokio::test]
async fn tokens_are_stored_only_as_their_sha256_hash() {
    let dir = TempDir::new("agentd-ctl");
    let url = dir.db_url();
    let store = Store::open(&url, sealer()).await.unwrap();
    let fixture = Fixture::over(store, dir, |_| {});
    let (info, token) = fixture.process().await;
    let secret = token.secret().expose_secret().to_owned();

    let stored = fixture
        .store
        .ctl_token(&hash_token(&secret))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.session, info.session);
    assert_eq!(stored.container_ip, info.container_ip);
    assert_eq!(stored.turn, None);

    fixture.store.close().await;
    for name in ["agentd.db", "agentd.db-wal"] {
        let path = fixture.dir.join(name);
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        assert!(
            !bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "{name} holds the token"
        );
    }
}

#[tokio::test]
async fn requests_need_a_known_bearer_token() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    let body = || Body::from(post("here").to_string());
    for authorization in [
        None,
        Some("Bearer ".to_owned()),
        Some(format!("Basic {}", token.secret().expose_secret())),
        Some("Bearer not-a-token".to_owned()),
        Some(format!("Bearer {}x", token.secret().expose_secret())),
    ] {
        let (status, value) = fixture
            .send(authorization.clone(), CONTAINER, "/v1/post", body())
            .await;
        assert_eq!(status, 401, "{authorization:?}");
        assert_eq!(code(&value), "unauthorized");
    }
    let (status, _) = fixture
        .send(Some(bearer(&token)), CONTAINER, "/v1/post", body())
        .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn requests_must_come_from_the_tokens_container() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    let body = || Body::from(post("here").to_string());
    for peer in ["172.30.0.8", "127.0.0.1", "::1", ""] {
        let (status, value) = fixture
            .send(Some(bearer(&token)), peer, "/v1/post", body())
            .await;
        assert_eq!(status, 401, "{peer:?}");
        assert_eq!(code(&value), "unauthorized");
        assert!(value["message"].as_str().unwrap().contains("container"));
    }
    for peer in [CONTAINER, "::ffff:172.30.0.7"] {
        let (status, _) = fixture
            .send(Some(bearer(&token)), peer, "/v1/post", body())
            .await;
        assert_eq!(status, 200, "{peer}");
    }
}

#[tokio::test]
async fn tokens_authorize_nothing_between_turns() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    for path in ALL_PATHS {
        let (status, value) = fixture.call(Some(&token), path, json!({})).await;
        assert_eq!(status, 409, "{path}");
        assert_eq!(code(&value), "no_turn");
    }
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    let (status, _) = fixture.call(Some(&token), "/v1/post", post("here")).await;
    assert_eq!(status, 200);
    let outbox = fixture.ctl.end_turn(&token).await.unwrap().unwrap();
    assert_eq!(outbox.posts().len(), 1);
    for path in ALL_PATHS {
        let (status, _) = fixture.call(Some(&token), path, json!({})).await;
        assert_eq!(status, 409, "{path}");
    }
    assert!(fixture.ctl.end_turn(&token).await.unwrap().is_none());
}

#[tokio::test]
async fn a_revoked_token_authorizes_nothing() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    fixture.ctl.revoke_process_token(&token).await.unwrap();
    fixture.ctl.revoke_process_token(&token).await.unwrap();
    let (status, _) = fixture.call(Some(&token), "/v1/post", post("here")).await;
    assert_eq!(status, 401);
    assert!(matches!(
        fixture.ctl.begin_turn(&token, public()).await,
        Err(HookError::UnknownToken)
    ));
    assert!(fixture.ctl.end_turn(&token).await.unwrap().is_none());
    assert!(
        std::fs::read_dir(&fixture.ctl.settings().staging_dir)
            .unwrap()
            .next()
            .is_none(),
        "no staging directory is left behind"
    );
}

#[tokio::test]
async fn a_new_process_token_replaces_the_sessions_old_one() {
    let fixture = Fixture::new().await;
    let (info, old) = fixture.process().await;
    fixture.ctl.begin_turn(&old, public()).await.unwrap();
    let new = fixture.ctl.issue_process_token(info).await.unwrap();
    let (status, _) = fixture.call(Some(&old), "/v1/post", post("here")).await;
    assert_eq!(status, 401);
    assert!(fixture.ctl.end_turn(&old).await.unwrap().is_none());
    fixture.ctl.begin_turn(&new, public()).await.unwrap();
    let (status, _) = fixture.call(Some(&new), "/v1/post", post("here")).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn startup_purge_deletes_every_token_lock_and_staged_file() {
    let fixture = Fixture::new().await;
    let (info, token) = fixture.process().await;
    let other = fixture.sibling(&info).await;
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    assert_eq!(fixture.attach(&token, "a.txt", b"hi").await.0, 200);
    let (_, held) = fixture
        .call(Some(&token), "/v1/lock", json!({"op": "acquire"}))
        .await;
    assert_eq!(held["state"], "held");

    let purged = fixture.ctl.purge().await.unwrap();
    assert_eq!(
        purged,
        CtlPurged {
            tokens: 2,
            locks: 1
        }
    );
    assert!(!fixture.ctl.settings().staging_dir.exists());
    for token in [&token, &other] {
        let (status, _) = fixture.call(Some(token), "/v1/post", post("here")).await;
        assert_eq!(status, 401);
    }
    assert!(fixture.ctl.end_turn(&token).await.unwrap().is_none());
    fixture.ctl.purge().await.unwrap();
}

#[tokio::test]
async fn only_attach_is_available_inside_a_private_task() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    let task = turn(TurnKind::PrivateTask(ConsentId::new_v4()), Side::Owner);
    fixture.ctl.begin_turn(&token, task).await.unwrap();
    for (path, body) in [
        ("/v1/post", post("here")),
        ("/v1/react", json!({"emoji": "eyes", "message": null})),
        ("/v1/history", json!({"before": null, "limit": null})),
        ("/v1/lock", json!({"op": "acquire"})),
        ("/v1/ask-agent", json!({"agent": "b", "task": "t"})),
        ("/v1/private", json!({"task": "t", "files": []})),
    ] {
        let (status, value) = fixture.call(Some(&token), path, body).await;
        assert_eq!(status, 403, "{path}");
        assert_eq!(code(&value), "refused");
        assert!(value["message"].as_str().unwrap().contains("attach"));
    }
    let (status, value) = fixture.attach(&token, "result.txt", b"done").await;
    assert_eq!(status, 200, "{value}");
    let outbox = fixture.ctl.end_turn(&token).await.unwrap().unwrap();
    assert_eq!(outbox.attachments().len(), 1);
    assert!(outbox.posts().is_empty() && outbox.reactions().is_empty());
}

#[tokio::test]
async fn public_turns_post_and_react_only_in_the_current_conversation() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    for to in ["here", "C1", "C1/99.1"] {
        let (status, value) = fixture.call(Some(&token), "/v1/post", post(to)).await;
        assert_eq!(status, 200, "{to}: {value}");
    }
    for to in ["C2", "<#C2|general>", "C2/99.1"] {
        let (status, value) = fixture.call(Some(&token), "/v1/post", post(to)).await;
        assert_eq!(status, 403, "{to}");
        assert!(value["message"].as_str().unwrap().contains("public side"));
    }
    for message in [json!(null), json!("99.9")] {
        let (status, _) = fixture
            .call(
                Some(&token),
                "/v1/react",
                json!({"emoji": ":eyes:", "message": message}),
            )
            .await;
        assert_eq!(status, 200);
    }
    let (status, _) = fixture
        .call(
            Some(&token),
            "/v1/react",
            json!({"emoji": "eyes", "message": "C2/99.9"}),
        )
        .await;
    assert_eq!(status, 403);

    let outbox = fixture.ctl.end_turn(&token).await.unwrap().unwrap();
    let targets: Vec<_> = outbox.posts().iter().map(|p| p.to.clone()).collect();
    assert_eq!(
        targets,
        [
            ReplyTarget::from(thread()),
            ReplyTarget {
                conv: conv("C1"),
                thread_root: None
            },
            ReplyTarget {
                conv: conv("C1"),
                thread_root: Some(MessageId::new("99.1"))
            },
        ]
    );
    assert_eq!(
        outbox.reactions(),
        [
            QueuedReaction {
                msg: MsgRef {
                    conv: conv("C1"),
                    id: MessageId::new("100.2")
                },
                emoji: "eyes".into(),
            },
            QueuedReaction {
                msg: MsgRef {
                    conv: conv("C1"),
                    id: MessageId::new("99.9")
                },
                emoji: "eyes".into(),
            },
        ]
    );
}

#[tokio::test]
async fn owner_turns_post_to_any_conversation_and_react_only_in_this_one() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    fixture
        .ctl
        .begin_turn(&token, turn(TurnKind::Normal, Side::Owner))
        .await
        .unwrap();
    let (status, _) = fixture.call(Some(&token), "/v1/post", post("#C9")).await;
    assert_eq!(status, 200);
    let (status, _) = fixture
        .call(
            Some(&token),
            "/v1/react",
            json!({"emoji": "eyes", "message": "C9/1.1"}),
        )
        .await;
    assert_eq!(status, 403);
    let outbox = fixture.ctl.end_turn(&token).await.unwrap().unwrap();
    assert_eq!(
        outbox.posts(),
        [QueuedPost {
            to: ReplyTarget {
                conv: conv("C9"),
                thread_root: None
            },
            text: "hello".into(),
        }]
    );
}

#[tokio::test]
async fn malformed_and_oversized_requests_are_refused() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    let cases = [
        ("/v1/post", json!({"to": "here"}), 400),
        ("/v1/post", json!({"to": "here", "text": "  "}), 400),
        (
            "/v1/post",
            json!({"to": "here", "text": "x".repeat(MAX_POST_BYTES + 1)}),
            413,
        ),
        (
            "/v1/post",
            json!({"to": "here", "text": "x".repeat(JSON_BODY_LIMIT)}),
            413,
        ),
        ("/v1/post", json!({"to": "a b", "text": "x"}), 400),
        ("/v1/react", json!({"emoji": "not an emoji"}), 400),
        ("/v1/history", json!({"limit": 0}), 400),
        ("/v1/history", json!({"limit": MAX_HISTORY_LIMIT + 1}), 400),
        ("/v1/history", json!({"before": "a b"}), 400),
        ("/v1/lock", json!({"op": "steal"}), 400),
        ("/v1/nope", json!({}), 404),
    ];
    for (path, body, expected) in cases {
        let (status, value) = fixture.call(Some(&token), path, body.clone()).await;
        assert_eq!(status, expected, "{path} {body}: {value}");
        assert!(value["message"].is_string(), "{value}");
        let message = value["message"].as_str().unwrap();
        assert!(!message.contains("xxxx"), "{message}");
    }
    let (status, value) = fixture
        .send(Some(bearer(&token)), CONTAINER, "/v1/post", Body::from("{"))
        .await;
    assert_eq!((status, code(&value)), (400, "bad_request"));
}

#[tokio::test]
async fn queues_are_capped_per_turn() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    for i in 0..=MAX_POSTS {
        let (status, _) = fixture.call(Some(&token), "/v1/post", post("here")).await;
        assert_eq!(status, if i < MAX_POSTS { 200 } else { 403 });
    }
    for i in 0..=MAX_REACTIONS {
        let (status, _) = fixture
            .call(Some(&token), "/v1/react", json!({"emoji": "eyes"}))
            .await;
        assert_eq!(status, if i < MAX_REACTIONS { 200 } else { 403 });
    }
    for i in 0..=MAX_ATTACHMENTS {
        let (status, _) = fixture.attach(&token, "f.txt", b"x").await;
        assert_eq!(status, if i < MAX_ATTACHMENTS { 200 } else { 403 });
    }
}

#[tokio::test]
async fn attach_stages_the_file_until_the_outbox_is_dropped() {
    let fixture = Fixture::with(|settings| settings.attach_max_bytes = 1 << 20).await;
    let (_, token) = fixture.process().await;
    let running = public();
    fixture
        .ctl
        .begin_turn(&token, running.clone())
        .await
        .unwrap();
    let (status, value) = fixture.attach(&token, "report%20v2.txt", b"hello").await;
    assert_eq!(status, 200, "{value}");
    assert_eq!(value, json!({"name": "report v2.txt", "size": 5}));
    let (status, _) = fixture.attach(&token, "empty", b"").await;
    assert_eq!(status, 200);
    let big = vec![b'x'; JSON_BODY_LIMIT * 2];
    let (status, value) = fixture.attach(&token, "big.bin", &big).await;
    assert_eq!(
        status, 200,
        "attach is not held to the JSON body limit: {value}"
    );

    let outbox = fixture.ctl.end_turn(&token).await.unwrap().unwrap();
    assert_eq!(outbox.turn(), running.id);
    let files = outbox.attachments().to_vec();
    assert_eq!(files[0].name, "report v2.txt");
    assert_eq!(std::fs::read(&files[0].path).unwrap(), b"hello");
    assert!(
        files[0]
            .path
            .starts_with(&fixture.ctl.settings().staging_dir)
    );
    assert_eq!(std::fs::read(&files[1].path).unwrap(), b"");
    assert_eq!(std::fs::read(&files[2].path).unwrap(), big);
    drop(outbox);
    assert!(!files[0].path.exists());
    assert!(!files[0].path.parent().unwrap().exists());
}

#[tokio::test]
async fn attach_refuses_bad_names_and_files_over_the_cap() {
    let fixture = Fixture::with(|settings| settings.attach_max_bytes = 8).await;
    let (_, token) = fixture.process().await;
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    assert_eq!(fixture.attach(&token, "ok", b"12345678").await.0, 200);
    let (status, value) = fixture.attach(&token, "big", b"123456789").await;
    assert_eq!((status, code(&value)), (413, "too_large"));
    for name in ["..", "a%2Fb", "", "a%0Ab"] {
        let (status, _) = fixture.attach(&token, name, b"x").await;
        assert_eq!(status, 400, "{name:?}");
    }
    let (status, _) = fixture
        .send(
            Some(bearer(&token)),
            CONTAINER,
            "/v1/attach",
            Body::from("x"),
        )
        .await;
    assert_eq!(status, 400);

    let chunks: Vec<Result<axum::body::Bytes, std::io::Error>> = vec![
        Ok(axum::body::Bytes::from_static(b"12345")),
        Ok(axum::body::Bytes::from_static(b"67890")),
    ];
    let (status, _) = fixture
        .send(
            Some(bearer(&token)),
            CONTAINER,
            "/v1/attach?name=streamed",
            Body::from_stream(futures::stream::iter(chunks)),
        )
        .await;
    assert_eq!(status, 413, "a stream without a length is counted");

    let outbox = fixture.ctl.end_turn(&token).await.unwrap().unwrap();
    assert_eq!(outbox.attachments().len(), 1);
    let staged: Vec<_> = std::fs::read_dir(outbox.staging()).unwrap().collect();
    assert_eq!(staged.len(), 1, "refused uploads leave no file");
}

#[tokio::test]
async fn an_upload_that_outlives_its_turn_is_discarded() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    let hash = token.hash();
    let authorized = Authorized {
        hash,
        token: fixture.store.ctl_token(&hash).await.unwrap().unwrap(),
        turn: fixture
            .store
            .ctl_token(&hash)
            .await
            .unwrap()
            .unwrap()
            .turn
            .unwrap(),
    };
    let reservation = fixture.ctl.reserve_attachment(&authorized).unwrap();
    let path = reservation.dir().join("late");
    std::fs::write(&path, b"x").unwrap();
    let outbox = fixture.ctl.end_turn(&token).await.unwrap().unwrap();
    assert!(outbox.attachments().is_empty());
    let err = fixture
        .ctl
        .commit_attachment(
            reservation,
            OutFile {
                name: "late".into(),
                path,
            },
        )
        .unwrap_err();
    assert_eq!(err.0.code, CtlErrorCode::NoTurn);
}

#[tokio::test]
async fn a_new_turn_replaces_a_turn_that_was_never_ended() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    assert_eq!(fixture.attach(&token, "old.txt", b"x").await.0, 200);
    let second = public();
    fixture
        .ctl
        .begin_turn(&token, second.clone())
        .await
        .unwrap();
    let outbox = fixture.ctl.end_turn(&token).await.unwrap().unwrap();
    assert_eq!(outbox.turn(), second.id);
    assert!(outbox.attachments().is_empty());
    let dirs = std::fs::read_dir(&fixture.ctl.settings().staging_dir)
        .unwrap()
        .count();
    assert_eq!(dirs, 1, "the first turn's staging directory is gone");
}

#[tokio::test]
async fn history_reads_the_turns_thread_through_the_surface() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    let msg = |id: &str| Msg {
        id: MessageId::new(id),
        sender: MemberKey {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            user: "U2".into(),
        },
        sender_is_bot: false,
        text: format!("message {id}"),
        files: vec![],
        sent_at: datetime!(2026-09-30 10:00 UTC),
    };
    fixture
        .surface
        .set_history(thread(), ["1", "2", "3"].map(msg).to_vec());
    fixture.ctl.begin_turn(&token, public()).await.unwrap();

    let (status, value) = fixture
        .call(
            Some(&token),
            "/v1/history",
            json!({"before": "3", "limit": 1}),
        )
        .await;
    assert_eq!(status, 200, "{value}");
    let response: core_types::HistoryResponse = serde_json::from_value(value).unwrap();
    assert_eq!(response.messages, vec![msg("2")]);

    let (status, value) = fixture
        .call(Some(&token), "/v1/history", json!({"before": "9"}))
        .await;
    assert_eq!((status, code(&value)), (404, "not_found"));

    let mut elsewhere = public();
    elsewhere.thread.conv.surface = SurfaceKind::RocketChat;
    fixture.ctl.begin_turn(&token, elsewhere).await.unwrap();
    let (status, value) = fixture.call(Some(&token), "/v1/history", json!({})).await;
    assert_eq!((status, code(&value)), (501, "not_available"));
}

#[tokio::test]
async fn ask_agent_and_private_are_not_available_yet() {
    let fixture = Fixture::new().await;
    let (_, token) = fixture.process().await;
    fixture.ctl.begin_turn(&token, public()).await.unwrap();
    for path in ["/v1/ask-agent", "/v1/private"] {
        let (status, value) = fixture.call(Some(&token), path, json!({})).await;
        assert_eq!(status, 501, "{path}");
        assert_eq!(code(&value), "not_available");
        assert!(
            value["message"]
                .as_str()
                .unwrap()
                .contains("not available yet")
        );
    }
}

async fn lock(fixture: &Fixture, token: &ProcessToken, body: Value) -> Value {
    let (status, value) = fixture.call(Some(token), "/v1/lock", body).await;
    assert_eq!(status, 200, "{value}");
    value
}

#[tokio::test]
async fn the_lock_is_exclusive_across_and_within_sessions() {
    let fixture = Fixture::new().await;
    let (info, a) = fixture.process().await;
    let b = fixture.sibling(&info).await;
    for token in [&a, &b] {
        fixture.ctl.begin_turn(token, public()).await.unwrap();
    }
    let held = lock(&fixture, &a, json!({"op": "acquire"})).await;
    assert_eq!(held["state"], "held");
    assert_eq!(held["seconds_left"], DEFAULT_LEASE_TTL.as_secs());
    let lease = held["lease"].clone();
    assert_eq!(
        lock(&fixture, &b, json!({"op": "acquire"})).await,
        json!({"state": "busy"}),
        "a second session waits"
    );
    assert_eq!(
        lock(&fixture, &a, json!({"op": "acquire"})).await,
        json!({"state": "busy"}),
        "a second lock in the same session waits too"
    );
    let renewed = lock(&fixture, &a, json!({"op": "renew", "lease": lease})).await;
    assert_eq!(renewed["state"], "held");
    assert_eq!(renewed["lease"], lease);
    assert_eq!(renewed["seconds_left"], DEFAULT_LEASE_TTL.as_secs());
    assert_eq!(
        lock(&fixture, &b, json!({"op": "release", "lease": lease})).await,
        json!({"state": "released"})
    );
    assert_eq!(
        lock(&fixture, &b, json!({"op": "acquire"})).await,
        json!({"state": "busy"}),
        "another session can't release the lease"
    );
    assert_eq!(
        lock(&fixture, &a, json!({"op": "release", "lease": lease})).await,
        json!({"state": "released"})
    );
    let next = lock(&fixture, &b, json!({"op": "acquire"})).await;
    assert_eq!(next["state"], "held");
    assert_ne!(next["lease"], lease);
    assert_eq!(
        lock(&fixture, &a, json!({"op": "renew", "lease": lease})).await,
        json!({"state": "released"}),
        "renewing an earlier lease leaves the current one alone"
    );
    assert_eq!(
        lock(&fixture, &a, json!({"op": "release", "lease": lease})).await,
        json!({"state": "released"})
    );
    assert_eq!(
        lock(&fixture, &a, json!({"op": "acquire"})).await,
        json!({"state": "busy"})
    );
}

#[tokio::test]
async fn a_lease_expires_when_its_holder_stops_renewing() {
    let fixture = Fixture::with(|settings| settings.lease_ttl = Duration::from_secs(1)).await;
    let (info, a) = fixture.process().await;
    let b = fixture.sibling(&info).await;
    for token in [&a, &b] {
        fixture.ctl.begin_turn(token, public()).await.unwrap();
    }
    let held = lock(&fixture, &a, json!({"op": "acquire"})).await;
    assert_eq!(held["state"], "held");
    tokio::time::sleep(Duration::from_millis(2_100)).await;
    let taken = lock(&fixture, &b, json!({"op": "acquire"})).await;
    assert_eq!(taken["state"], "held");
    assert_ne!(taken["lease"], held["lease"]);
}

#[tokio::test]
async fn a_lease_ends_with_its_turn_and_its_token() {
    let fixture = Fixture::new().await;
    let (info, a) = fixture.process().await;
    let b = fixture.sibling(&info).await;
    for token in [&a, &b] {
        fixture.ctl.begin_turn(token, public()).await.unwrap();
    }
    let acquire = || json!({"op": "acquire"});
    assert_eq!(lock(&fixture, &a, acquire()).await["state"], "held");
    assert_eq!(lock(&fixture, &b, acquire()).await["state"], "busy");
    fixture.ctl.end_turn(&a).await.unwrap();
    assert_eq!(
        lock(&fixture, &b, acquire()).await["state"],
        "held",
        "the lock is free as soon as the holder's turn ends"
    );
    fixture.ctl.begin_turn(&a, public()).await.unwrap();
    assert_eq!(lock(&fixture, &a, acquire()).await["state"], "busy");
    fixture.ctl.begin_turn(&b, public()).await.unwrap();
    assert_eq!(
        lock(&fixture, &a, acquire()).await["state"],
        "held",
        "a turn that replaces the holder's turn frees it too"
    );
    fixture.ctl.revoke_process_token(&a).await.unwrap();
    assert_eq!(
        lock(&fixture, &b, acquire()).await["state"],
        "held",
        "the lock is free as soon as the holder's token is revoked"
    );
    let c = fixture.sibling(&info).await;
    fixture.ctl.begin_turn(&c, public()).await.unwrap();
    assert_eq!(
        lock(&fixture, &c, acquire()).await["state"],
        "busy",
        "a new token for another session leaves the lease alone"
    );
    let holder = fixture.store.ctl_token(&b.hash()).await.unwrap().unwrap();
    fixture
        .ctl
        .issue_process_token(ProcessInfo {
            session: holder.session,
            ..info
        })
        .await
        .unwrap();
    assert_eq!(
        lock(&fixture, &c, acquire()).await["state"],
        "held",
        "a new token for the holder's session frees it"
    );
}

#[tokio::test]
async fn locks_are_per_volume() {
    let fixture = Fixture::new().await;
    let (_, a) = fixture.process().await;
    let (_, b) = fixture.process().await;
    for token in [&a, &b] {
        fixture.ctl.begin_turn(token, public()).await.unwrap();
        assert_eq!(
            lock(&fixture, token, json!({"op": "acquire"})).await["state"],
            "held"
        );
    }
}

#[tokio::test]
async fn settings_come_from_the_config() {
    let config =
        crate::Config::parse(crate::config::tests::MINIMAL, crate::config::tests::env()).unwrap();
    let settings = CtlSettings::from_config(&config);
    assert_eq!(
        settings.staging_dir,
        PathBuf::from("/nonexistent/agentd/ctl-outbox")
    );
    assert_eq!(settings.attach_max_bytes, 50 * 1024 * 1024);
    assert_eq!(settings.lease_ttl, DEFAULT_LEASE_TTL);
    let store = Store::open_in_memory(sealer()).await.unwrap();
    let ctl = Ctl::new(store, settings, Arc::new(NoSurfaces));
    assert!(format!("{ctl:?}").starts_with("Ctl"));
}

#[test]
fn no_surfaces_has_no_surface() {
    assert!(NoSurfaces.surface(AgentId::new_v4(), &conv("C1")).is_none());
}

#[test]
fn hook_errors_name_what_failed() {
    assert_eq!(
        HookError::UnknownToken.to_string(),
        "the agentctl token is unknown or revoked"
    );
    assert_eq!(
        HookError::Random.to_string(),
        "the system random number generator failed"
    );
}

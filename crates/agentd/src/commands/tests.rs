//! Command dispatch against a `MockSurface` manager bot and wiremock OAuth
//! endpoints.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use auth::{Auth, AuthError, OAuthConfig, TokenSource};
use core_types::{
    Binding, BindingId, ConvKind, ConvRef, ConversationId, InboundEvent, MemberKey, MsgRef,
    ReplyTarget, SurfaceError, SurfaceKind, TeamId, UserId,
};
use secrecy::SecretString;
use serde_json::json;
use store::{NewClaudeLink, Sealer, Store};
use testkit::{Held, MockSurface, Op};
use time::OffsetDateTime;
use tokio::sync::watch;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use super::relink::{
    RELINK_BACKOFF_INITIAL, RELINK_BACKOFF_MAX, RELINK_LEASE, RELINK_MAX_ATTEMPTS, RelinkNotifier,
    relink_notice,
};
use super::rocketchat::{CommandIntake, command_in, listen};
use super::*;
use crate::telemetry::tests::Captured;
use crate::telemetry::{LogFormat, subscriber};

const TEAM: &str = "chat.example.org";
const TOKEN_PATH: &str = "/v1/oauth/token";
const REVOKE_PATH: &str = "/v1/oauth/token/revoke";
const PROFILE_PATH: &str = "/api/oauth/profile";
const CODE: &str = "SECRETCODE-4f2a9c";
const API_KEY: &str = "sk-ant-api03-SECRETKEY-77b1";

fn key(user: &str) -> MemberKey {
    MemberKey {
        surface: SurfaceKind::RocketChat,
        team: TeamId::new(TEAM),
        user: UserId::new(user),
    }
}

fn conv(room: &str) -> ConvRef {
    ConvRef {
        surface: SurfaceKind::RocketChat,
        team: TeamId::new(TEAM),
        conversation: room.into(),
    }
}

fn dm_room(user: &str) -> String {
    format!("dm-{user}")
}

/// Opens `dm-<user>` for every member.
struct Dms;

#[async_trait]
impl OpenDm for Dms {
    async fn open_dm(&self, member: &MemberKey) -> Result<ConversationId, SurfaceError> {
        Ok(dm_room(member.user.as_str()).into())
    }
}

struct Harness {
    store: Store,
    auth: Arc<Auth>,
    commands: Commands,
    mock: Arc<MockSurface>,
    oauth: MockServer,
    manager: Binding,
}

async fn harness() -> Harness {
    let oauth = MockServer::start().await;
    let store =
        Store::open_in_memory(Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap())
            .await
            .unwrap();
    let config = OAuthConfig {
        token_url: format!("{}{TOKEN_PATH}", oauth.uri()),
        revoke_url: format!("{}{REVOKE_PATH}", oauth.uri()),
        profile_url: format!("{}{PROFILE_PATH}", oauth.uri()),
        ..OAuthConfig::default()
    };
    let auth = Arc::new(Auth::new(config, store.clone()).unwrap());
    let mock = Arc::new(MockSurface::new());
    let manager = Binding {
        id: BindingId::new_v4(),
        agent: None,
        bot: key("manager"),
    };
    let bot = Arc::new(ManagerBot::new(
        manager.bot.clone(),
        mock.clone(),
        Arc::new(Dms),
    ));
    let commands = Commands::new(
        store.clone(),
        Arc::clone(&auth),
        Replies::new(Some(bot)),
        None,
    );
    Harness {
        store,
        auth,
        commands,
        mock,
        oauth,
        manager,
    }
}

impl Harness {
    fn event(&self, sender: &str, kind: ConvKind, room: &str, text: &str) -> InboundEvent {
        InboundEvent {
            event_id: format!("ev-{}", uuid::Uuid::new_v4()),
            binding: self.manager.id,
            sender: key(sender),
            sender_is_bot: false,
            sender_bot_user: None,
            conv: conv(room),
            conv_kind: kind,
            thread_root: None,
            message: MsgRef {
                conv: conv(room),
                id: "m1".into(),
            },
            text: text.to_owned(),
            mentions: Vec::new(),
            reply_to: None,
            files: Vec::new(),
            received_at: OffsetDateTime::now_utc(),
        }
    }

    /// Sends `text` as `user` in their DM with the manager bot.
    async fn dm(&self, user: &str, text: &str) {
        let origin = Origin::RocketChatDm {
            room: dm_room(user).into(),
        };
        self.commands
            .handle_text(&key(user), text, &origin, &[])
            .await;
    }

    /// Sends `!agent <text>` as `user` in a channel.
    async fn channel(&self, user: &str, text: &str) {
        let origin = Origin::RocketChatChannel {
            room: "GENERAL".into(),
        };
        self.commands
            .handle_text(&key(user), text, &origin, &[])
            .await;
    }

    /// Every text posted in `user`'s DM with the manager bot.
    fn replies_to(&self, user: &str) -> Vec<String> {
        let to = ReplyTarget {
            conv: conv(&dm_room(user)),
            thread_root: None,
        };
        self.mock
            .posts()
            .into_iter()
            .filter(|(target, _)| *target == to)
            .map(|(_, text)| text)
            .collect()
    }

    fn last_reply(&self, user: &str) -> String {
        self.replies_to(user).pop().expect("no reply")
    }

    async fn wait_for_replies(&self, user: &str, count: usize) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let replies = self.replies_to(user);
            if replies.len() >= count {
                return replies;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "only {} replies: {replies:?}",
                replies.len()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn member(&self, user: &str) -> Option<core_types::MemberId> {
        self.store.member_for_identity(&key(user)).await.unwrap()
    }

    async fn linked_member(&self, user: &str, plan: &str) -> (core_types::MemberId, i64) {
        let member = self
            .store
            .ensure_member(&key(user), user, OffsetDateTime::now_utc())
            .await
            .unwrap();
        let generation = self
            .store
            .put_claude_link(
                member,
                &NewClaudeLink {
                    access_token: SecretString::from("access"),
                    refresh_token: SecretString::from("refresh"),
                    expires_at: OffsetDateTime::now_utc() + time::Duration::hours(8),
                    plan: Some(plan.to_owned()),
                    rate_limit_tier: None,
                },
                OffsetDateTime::now_utc(),
            )
            .await
            .unwrap();
        (member, generation)
    }

    async fn oauth_requests(&self) -> usize {
        self.oauth
            .received_requests()
            .await
            .unwrap_or_default()
            .len()
    }
}

/// Runs the manager bot's connection to the mock surface, feeding a new
/// intake, as the server does. Completes once both have finished.
fn serve(
    h: &Harness,
    stopping: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<Result<(), SurfaceError>> {
    let (intake, feed) = CommandIntake::new(h.commands.clone(), h.manager.clone());
    let connection = listen(
        h.mock.clone(),
        h.manager.clone(),
        feed.into_sender(None),
        stopping,
    );
    tokio::spawn(async move {
        let (ended, ()) = tokio::join!(connection, intake.run());
        ended
    })
}

fn state_of(reply: &str) -> String {
    let at = reply.find("state=").expect("no state in the link") + "state=".len();
    reply[at..]
        .split(|c: char| c == '&' || c.is_whitespace())
        .next()
        .unwrap()
        .to_owned()
}

fn capture_logs() -> (Captured, tracing::subscriber::DefaultGuard) {
    let captured = Captured::default();
    let logs = subscriber(
        LogFormat::Json,
        tracing_subscriber::EnvFilter::new("trace"),
        captured.clone(),
    );
    (captured, tracing::subscriber::set_default(logs))
}

fn exchanged() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "token_type": "Bearer",
        "access_token": "new-access",
        "refresh_token": "new-refresh",
        "expires_in": 28800,
    }))
}

async fn mount_exchange(oauth: &MockServer, state: &str, exchange: impl Respond + 'static) {
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .and(body_partial_json(json!({
            "grant_type": "authorization_code",
            "code": CODE,
            "state": state,
        })))
        .respond_with(exchange)
        .expect(1)
        .mount(oauth)
        .await;
    Mock::given(method("GET"))
        .and(path(PROFILE_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "organization": {"organization_type": "claude_max", "rate_limit_tier": "t"},
        })))
        .mount(oauth)
        .await;
}

#[tokio::test]
async fn full_login_flow_from_a_dm_links_the_account_without_logging_the_code() {
    let h = harness().await;
    let (logs, _guard) = capture_logs();
    let (stop, stopping) = watch::channel(false);
    let task = serve(&h, stopping);

    h.mock
        .inject(h.event("alice", ConvKind::Dm, &dm_room("alice"), "login"));
    let started = h.wait_for_replies("alice", 1).await.remove(0);
    assert!(
        started.contains("https://claude.com/cai/oauth/authorize?"),
        "{started}"
    );
    assert!(started.contains("`login <code>`"), "{started}");
    let state = state_of(&started);
    mount_exchange(&h.oauth, &state, exchanged()).await;

    let pasted = format!("login {CODE}#{state}");
    h.mock
        .inject(h.event("alice", ConvKind::Dm, &dm_room("alice"), &pasted));
    let replies = h.wait_for_replies("alice", 2).await;
    assert_eq!(
        replies[1],
        "Your Claude account is linked. Plan: Claude Max."
    );

    let member = h.member("alice").await.unwrap();
    let status = h.auth.status(member).await.unwrap();
    assert!(status.linked && !status.broken);
    stop.send_replace(true);
    task.await.unwrap().unwrap();

    let out = logs.text();
    assert!(out.contains("running a command"), "{out}");
    assert!(out.contains("\"command\":\"login\""), "{out}");
    assert!(
        !out.contains(CODE),
        "the login code reached the log:\n{out}"
    );
    assert!(!out.contains(&pasted), "{out}");
}

#[tokio::test]
async fn one_members_commands_run_in_order_without_holding_up_others() {
    let h = harness().await;
    h.dm("alice", "login").await;
    let state = state_of(&h.last_reply("alice"));
    let (held, mut hold) = Held::new(exchanged());
    mount_exchange(&h.oauth, &state, held).await;
    let (_stop, stopping) = watch::channel(false);
    let task = serve(&h, stopping);
    let dm = dm_room("alice");
    h.mock
        .inject(h.event("alice", ConvKind::Dm, &dm, &format!("login {CODE}#{state}")));
    h.mock.inject(h.event("alice", ConvKind::Dm, &dm, "me"));
    h.mock
        .inject(h.event("bob", ConvKind::Dm, &dm_room("bob"), "me"));
    h.mock.close_events(h.manager.id);
    hold.arrived().await;
    h.wait_for_replies("bob", 1).await;
    hold.release();
    task.await.unwrap().unwrap();

    let texts: Vec<String> = h.mock.posts().into_iter().map(|(_, text)| text).collect();
    assert_eq!(texts.len(), 4, "{texts:?}");
    assert!(
        texts[1].starts_with("Claude account: not linked."),
        "{texts:?}"
    );
    assert_eq!(texts[2], "Your Claude account is linked. Plan: Claude Max.");
    assert_eq!(texts[3], "Claude account: linked. Plan: Claude Max.");
    assert_eq!(h.replies_to("bob").len(), 1);
}

#[tokio::test]
async fn a_login_code_in_a_channel_is_refused_and_invalidates_the_pending_login() {
    let h = harness().await;
    let (logs, _guard) = capture_logs();
    h.dm("alice", "login").await;
    let state = state_of(&h.last_reply("alice"));
    let member = h.member("alice").await.unwrap();

    h.channel("alice", &format!("login {CODE}#{state}")).await;

    let reply = h.last_reply("alice");
    assert!(
        reply.starts_with("You posted a secret in a room others can read."),
        "{reply}"
    );
    assert!(reply.contains("cancelled your pending login"), "{reply}");
    assert!(reply.contains("Start again with `login`"), "{reply}");
    assert!(h.store.take_pending_login(&state).await.unwrap().is_none());
    assert_eq!(h.oauth_requests().await, 0);
    assert!(!h.auth.status(member).await.unwrap().linked);
    assert!(
        h.mock
            .posts()
            .iter()
            .all(|(to, _)| to.conv != conv("GENERAL"))
    );
    let out = logs.text();
    assert!(!out.contains(CODE), "{out}");
}

#[tokio::test]
async fn a_public_login_code_says_it_cancelled_a_login_only_when_one_was_pending() {
    let h = harness().await;
    h.channel("alice", &format!("login {CODE}#no-such-state"))
        .await;
    let reply = h.last_reply("alice");
    assert!(reply.contains("so I didn't use it."), "{reply}");
    assert!(!reply.contains("cancelled"), "{reply}");

    h.dm("bob", "login").await;
    let state = state_of(&h.last_reply("bob"));
    h.channel("bob", &format!("login {CODE}#{state}")).await;
    assert!(h.last_reply("bob").contains("cancelled your pending login"));
    h.channel("bob", &format!("login {CODE}#{state}")).await;
    let reply = h.last_reply("bob");
    assert!(reply.contains("so I didn't use it."), "{reply}");
    assert!(!reply.contains("cancelled"), "{reply}");
}

#[tokio::test]
async fn a_public_secret_is_still_reported_when_the_store_fails() {
    let h = harness().await;
    h.dm("alice", "login").await;
    let state = state_of(&h.last_reply("alice"));
    h.store.close().await;

    h.channel("alice", &format!("login {CODE}#{state}")).await;
    let reply = h.last_reply("alice");
    assert!(
        reply.starts_with("You posted a secret in a room others can read."),
        "{reply}"
    );
    assert!(reply.contains("so I didn't use it."), "{reply}");
    assert!(!reply.contains("cancelled"), "{reply}");

    h.channel("alice", &format!("logn {CODE}#{state}")).await;
    let reply = h.last_reply("alice");
    assert!(
        reply.starts_with("Your message looked like it held a secret"),
        "{reply}"
    );
    assert!(!reply.contains("cancelled"), "{reply}");
    assert_eq!(h.oauth_requests().await, 0);
}

#[tokio::test]
async fn a_login_code_posted_publicly_by_someone_else_cancels_the_login_it_names() {
    let h = harness().await;
    h.dm("alice", "login").await;
    let state = state_of(&h.last_reply("alice"));
    h.dm("bob", "login").await;
    let bobs = state_of(&h.last_reply("bob"));

    h.channel("bob", &format!("login {CODE}#{state}")).await;

    assert!(h.last_reply("bob").contains("cancelled your pending login"));
    assert!(h.store.take_pending_login(&state).await.unwrap().is_none());
    assert!(h.store.take_pending_login(&bobs).await.unwrap().is_none());
    assert_eq!(h.oauth_requests().await, 0);
}

#[tokio::test]
async fn an_api_key_in_a_channel_is_refused_with_revoke_advice() {
    let h = harness().await;
    let (logs, _guard) = capture_logs();
    h.channel("root", &format!("admin api-key set {API_KEY}"))
        .await;
    let reply = h.last_reply("root");
    assert!(reply.contains("I didn't store it"), "{reply}");
    assert!(
        reply.contains("Revoke it in the Anthropic Console"),
        "{reply}"
    );
    assert!(!reply.contains(API_KEY));
    let out = logs.text();
    assert!(out.contains("\"command\":\"admin api-key set\""), "{out}");
    assert!(!out.contains(API_KEY), "{out}");
}

#[tokio::test]
async fn a_slack_token_in_a_channel_is_refused_with_revoke_advice() {
    let h = harness().await;
    h.channel("alice", "slack-token xoxe.xoxp-1-abc xoxe-1-def")
        .await;
    let reply = h.last_reply("alice");
    assert!(reply.contains("Revoke it at api.slack.com"), "{reply}");
}

#[tokio::test]
async fn secret_looking_text_that_fails_to_parse_in_a_channel_cancels_logins() {
    let h = harness().await;
    let (logs, _guard) = capture_logs();
    h.dm("alice", "login").await;
    let state = state_of(&h.last_reply("alice"));

    h.channel("alice", &format!("login {CODE} extra")).await;

    let reply = h.last_reply("alice");
    assert!(reply.contains("looked like it held a secret"), "{reply}");
    assert!(reply.contains("Usage: `login [code]`"), "{reply}");
    assert!(h.store.take_pending_login(&state).await.unwrap().is_none());
    assert!(!logs.text().contains(CODE));
}

#[tokio::test]
async fn text_that_fails_to_parse_privately_gets_the_parser_message() {
    let h = harness().await;
    h.dm("alice", "frobnicate").await;
    assert!(h.last_reply("alice").starts_with("Unknown command."));
    h.channel("alice", "login a b").await;
    let reply = h.last_reply("alice");
    assert!(reply.contains("looked like it held a secret"), "{reply}");
    h.dm("alice", "login a b").await;
    assert!(!h.last_reply("alice").contains("secret"));
}

#[tokio::test]
async fn logout_deletes_the_link_and_revokes_the_token() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(REVOKE_PATH))
        .and(body_partial_json(json!({"token": "refresh"})))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&h.oauth)
        .await;
    let (member, _) = h.linked_member("alice", "claude_pro").await;
    h.store
        .put_pending_login(
            "stale-state",
            member,
            &SecretString::from("v"),
            OffsetDateTime::now_utc() + time::Duration::minutes(5),
        )
        .await
        .unwrap();

    h.dm("alice", "logout").await;

    assert_eq!(
        h.last_reply("alice"),
        "Your Claude account is unlinked. Send `login` to link one again."
    );
    assert!(!h.auth.status(member).await.unwrap().linked);
    assert!(
        h.store
            .take_pending_login("stale-state")
            .await
            .unwrap()
            .is_none()
    );

    h.dm("alice", "logout").await;
    assert_eq!(h.last_reply("alice"), "No Claude account is linked.");
    h.dm("bob", "logout").await;
    assert_eq!(h.last_reply("bob"), "No Claude account is linked.");
    assert_eq!(h.member("bob").await, None);
}

#[tokio::test]
async fn me_reports_linked_unlinked_and_broken_members() {
    let h = harness().await;
    h.dm("stranger", "me").await;
    assert_eq!(
        h.last_reply("stranger"),
        "Claude account: not linked. Send `login` to link one."
    );
    assert_eq!(h.member("stranger").await, None);

    let (member, generation) = h.linked_member("alice", "claude_pro").await;
    h.channel("alice", "me").await;
    assert_eq!(
        h.last_reply("alice"),
        "Claude account: linked. Plan: Claude Pro."
    );

    h.linked_member("carol", "claude_galaxy").await;
    h.dm("carol", "me").await;
    assert_eq!(
        h.last_reply("carol"),
        "Claude account: linked. Plan: `claude_galaxy`."
    );

    h.store
        .mark_claude_link_broken(member, generation, OffsetDateTime::now_utc())
        .await
        .unwrap();
    h.dm("alice", "me").await;
    assert_eq!(
        h.last_reply("alice"),
        "Claude account: linked, but it stopped working. Send `login` to link it again."
    );
}

#[tokio::test]
async fn login_code_replies_for_each_failure() {
    let h = harness().await;
    h.dm("alice", &format!("login {CODE}#nostate")).await;
    assert_eq!(
        h.last_reply("alice"),
        "No login is waiting for a code. Start again with `login`."
    );
    h.dm("alice", "login").await;
    let state = state_of(&h.last_reply("alice"));
    h.dm("alice", "login no-hash-here").await;
    assert!(
        h.last_reply("alice")
            .starts_with("That isn't a login code.")
    );
    h.dm("alice", &format!("login {CODE}#wrong-state")).await;
    assert_eq!(
        h.last_reply("alice"),
        "No login is waiting for that code. Start again with `login`."
    );
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
        .mount(&h.oauth)
        .await;
    h.dm("alice", &format!("login {CODE}#{state}")).await;
    assert_eq!(
        h.last_reply("alice"),
        "Anthropic didn't accept that code. Start again with `login`."
    );
}

#[tokio::test]
async fn a_failed_exchange_and_a_failed_store_get_generic_replies() {
    let h = harness().await;
    h.dm("alice", "login").await;
    let state = state_of(&h.last_reply("alice"));
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.oauth)
        .await;
    h.dm("alice", &format!("login {CODE}#{state}")).await;
    assert_eq!(
        h.last_reply("alice"),
        "I couldn't finish linking your account. Start again with `login`."
    );
    h.store.close().await;
    h.dm("alice", "me").await;
    assert_eq!(h.last_reply("alice"), FAILED);
}

#[tokio::test]
async fn commands_that_come_later_say_so() {
    let h = harness().await;
    h.dm("alice", "skill rm helper tool").await;
    assert_eq!(h.last_reply("alice"), "`skill rm` isn't available yet.");
    h.dm("root", &format!("admin api-key set {API_KEY}")).await;
    assert_eq!(
        h.last_reply("root"),
        "`admin api-key set` isn't available yet."
    );
}

#[tokio::test]
async fn which_rocketchat_messages_are_commands() {
    let h = harness().await;
    let dm = h.event("alice", ConvKind::Dm, "dm-alice", "!agent login");
    let (origin, text) = command_in(&dm, &h.manager).unwrap();
    assert!(matches!(origin, Origin::RocketChatDm { room } if room.as_str() == "dm-alice"));
    assert_eq!(text, "login");
    let dm = h.event("alice", ConvKind::Dm, "dm-alice", "me");
    assert_eq!(command_in(&dm, &h.manager).unwrap().1, "me");

    let channel = h.event("alice", ConvKind::Channel, "GENERAL", "!agent me");
    let (origin, text) = command_in(&channel, &h.manager).unwrap();
    assert!(matches!(origin, Origin::RocketChatChannel { room } if room.as_str() == "GENERAL"));
    assert_eq!(text, "me");
    for kind in [ConvKind::Channel, ConvKind::GroupDm] {
        assert!(command_in(&h.event("alice", kind, "GENERAL", "me"), &h.manager).is_none());
    }

    let mut agent_dm = h.event("alice", ConvKind::Dm, "dm-helper", "!agent login x#y");
    agent_dm.binding = BindingId::new_v4();
    let (origin, _) = command_in(&agent_dm, &h.manager).unwrap();
    assert!(!origin.is_private());
    agent_dm.text = "login x#y".into();
    assert!(command_in(&agent_dm, &h.manager).is_none());

    let own = h.event("manager", ConvKind::Dm, "dm-alice", "Unknown command.");
    assert!(command_in(&own, &h.manager).is_none());
    let mut bot = h.event("helper", ConvKind::Channel, "GENERAL", "!agent delete x");
    bot.sender_is_bot = true;
    assert!(command_in(&bot, &h.manager).is_none());
    bot.sender_is_bot = false;
    bot.sender_bot_user = Some("helper".into());
    assert!(command_in(&bot, &h.manager).is_none());
}

#[tokio::test]
async fn the_intake_ignores_non_commands_and_stops_when_the_connection_ends() {
    let h = harness().await;
    let (_stop, stopping) = watch::channel(false);
    let task = serve(&h, stopping);
    h.mock
        .inject(h.event("alice", ConvKind::Channel, "GENERAL", "hello there"));
    h.mock
        .inject(h.event("manager", ConvKind::Dm, &dm_room("alice"), "me"));
    h.mock
        .inject(h.event("alice", ConvKind::Channel, "GENERAL", "!agent me"));
    h.mock.close_events(h.manager.id);
    task.await.unwrap().unwrap();
    assert_eq!(h.mock.posts().len(), 1);
    assert!(
        h.last_reply("alice")
            .starts_with("Claude account: not linked.")
    );
}

#[tokio::test]
async fn slack_and_unknown_workspaces_have_no_private_replies_yet() {
    let h = harness().await;
    let replies = h.commands.replies();
    let slack = MemberKey {
        surface: SurfaceKind::Slack,
        team: "T1".into(),
        user: "U1".into(),
    };
    let origin = Origin::SlackSlash {
        response_url: SecretString::from("https://hooks.slack.com/commands/secret"),
    };
    assert!(origin.is_private());
    assert_eq!(format!("{origin:?}"), "SlackSlash");
    let err = replies
        .reply_private(&slack, &origin, "hi")
        .await
        .unwrap_err();
    assert!(matches!(err, ReplyError::SlackUnavailable), "{err}");
    assert!(matches!(
        replies.dm(&slack, "hi").await,
        Err(ReplyError::SlackUnavailable)
    ));
    let elsewhere = MemberKey {
        team: "other.example.org".into(),
        ..key("alice")
    };
    assert!(!replies.can_dm(&elsewhere));
    assert!(matches!(
        replies.dm(&elsewhere, "hi").await,
        Err(ReplyError::NoManagerBot(SurfaceKind::RocketChat))
    ));
    assert!(!Replies::default().can_dm(&key("alice")));
    h.commands.dispatch(&slack, Command::Me, &origin, &[]).await;
    assert!(h.mock.posts().is_empty());
}

#[tokio::test]
async fn a_relink_notice_is_sent_once_per_break_across_instances() {
    let h = harness().await;
    let (member, generation) = h.linked_member("alice", "claude_max").await;
    h.store
        .mark_claude_link_broken(member, generation, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let first = RelinkNotifier::new(h.store.clone(), h.commands.replies().clone());
    let second = RelinkNotifier::new(h.store.clone(), h.commands.replies().clone());

    let (a, b) = tokio::join!(first.send_pending(), second.send_pending());
    assert_eq!(a.unwrap() + b.unwrap(), 1);
    assert_eq!(first.send_pending().await.unwrap(), 0);
    assert_eq!(h.replies_to("alice"), [relink_notice(&key("alice"))]);
    assert!(h.last_reply("alice").contains("Send `login`"));

    let (_, generation) = h.linked_member("alice", "claude_max").await;
    h.store
        .mark_claude_link_broken(member, generation, OffsetDateTime::now_utc())
        .await
        .unwrap();
    assert_eq!(second.send_pending().await.unwrap(), 1);
    assert_eq!(h.replies_to("alice").len(), 2);
}

/// Opens no DM: every open fails, and each try is counted.
#[derive(Default)]
struct DeadDms(std::sync::atomic::AtomicUsize);

#[async_trait]
impl OpenDm for DeadDms {
    async fn open_dm(&self, _: &MemberKey) -> Result<ConversationId, SurfaceError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(SurfaceError::Transport("down".into()))
    }
}

async fn broken_member(h: &Harness, user: &str) -> core_types::MemberId {
    let (member, generation) = h.linked_member(user, "claude_max").await;
    h.store
        .mark_claude_link_broken(member, generation, OffsetDateTime::now_utc())
        .await
        .unwrap();
    member
}

fn clock(at: OffsetDateTime) -> impl Fn() -> OffsetDateTime {
    move || at
}

#[tokio::test]
async fn a_relink_notice_that_fails_to_send_is_retried_after_a_backoff() {
    let h = harness().await;
    broken_member(&h, "alice").await;
    let notifier = RelinkNotifier::new(h.store.clone(), h.commands.replies().clone());
    let start = OffsetDateTime::now_utc();
    h.mock
        .fail_next(Op::Post, SurfaceError::Transport("down".into()));
    assert_eq!(notifier.send_pending_at(clock(start)).await.unwrap(), 0);
    let later = start + Duration::from_secs(59);
    assert!(
        h.store
            .pending_relink_notices(later, u32::MAX)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(notifier.send_pending_at(clock(later)).await.unwrap(), 0);
    assert_eq!(h.mock.posts().len(), 0);
    let after = start + Duration::from_secs(60);
    assert_eq!(notifier.send_pending_at(clock(after)).await.unwrap(), 1);
    assert_eq!(h.replies_to("alice"), [relink_notice(&key("alice"))]);
    let much_later = after + Duration::from_secs(86_400);
    assert_eq!(
        notifier.send_pending_at(clock(much_later)).await.unwrap(),
        0
    );
    assert_eq!(h.replies_to("alice").len(), 1);
}

#[tokio::test]
async fn an_unreachable_member_is_retried_with_growing_waits_then_given_up_once() {
    let h = harness().await;
    let (logs, _guard) = capture_logs();
    broken_member(&h, "alice").await;
    let dms = Arc::new(DeadDms::default());
    let bot = Arc::new(ManagerBot::new(
        h.manager.bot.clone(),
        h.mock.clone(),
        dms.clone(),
    ));
    let notifier = RelinkNotifier::new(h.store.clone(), Replies::new(Some(bot)));
    let tries = || dms.0.load(std::sync::atomic::Ordering::SeqCst);
    let mut now = OffsetDateTime::now_utc();
    let mut waits = Vec::new();
    for _ in 0..RELINK_MAX_ATTEMPTS {
        let before = tries();
        let mut wait = Duration::ZERO;
        while tries() == before {
            assert!(wait <= RELINK_BACKOFF_MAX, "no attempt after {wait:?}");
            notifier.send_pending_at(clock(now)).await.unwrap();
            if tries() == before {
                now += Duration::from_secs(60);
                wait += Duration::from_secs(60);
            }
        }
        waits.push(wait);
    }
    assert_eq!(waits[0], Duration::ZERO);
    assert_eq!(waits[1], RELINK_BACKOFF_INITIAL);
    assert_eq!(waits[2], RELINK_BACKOFF_INITIAL * 2);
    assert_eq!(waits[3], RELINK_BACKOFF_INITIAL * 4);
    assert_eq!(waits.last(), Some(&RELINK_BACKOFF_MAX));
    for _ in 0..3 {
        now += RELINK_BACKOFF_MAX * 2;
        notifier.send_pending_at(clock(now)).await.unwrap();
    }
    assert_eq!(tries(), usize::try_from(RELINK_MAX_ATTEMPTS).unwrap());
    let out = logs.text();
    assert_eq!(
        out.matches("giving up on the relink notice").count(),
        1,
        "{out}"
    );
}

#[tokio::test]
async fn a_notice_claimed_by_an_instance_that_died_is_sent_when_the_lease_ends() {
    let h = harness().await;
    let member = broken_member(&h, "alice").await;
    let start = OffsetDateTime::now_utc();
    let notice = h.store.pending_relink_notices(start, 1).await.unwrap()[0];
    let claimed = h
        .store
        .claim_relink_notice(member, notice.generation, start, start + RELINK_LEASE, 1)
        .await
        .unwrap();
    assert_eq!(claimed, Some(1));
    let notifier = RelinkNotifier::new(h.store.clone(), h.commands.replies().clone());
    let before_end = start + RELINK_LEASE - Duration::from_secs(1);
    assert_eq!(
        notifier.send_pending_at(clock(before_end)).await.unwrap(),
        0
    );
    assert!(h.replies_to("alice").is_empty());
    let lease_end = start + RELINK_LEASE;
    assert_eq!(notifier.send_pending_at(clock(lease_end)).await.unwrap(), 1);
    assert_eq!(h.replies_to("alice"), [relink_notice(&key("alice"))]);
    let much_later = lease_end + Duration::from_secs(86_400);
    assert_eq!(
        notifier.send_pending_at(clock(much_later)).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn a_relink_notice_waits_while_no_manager_bot_reaches_the_member() {
    let h = harness().await;
    let (member, generation) = h.linked_member("alice", "claude_max").await;
    h.store
        .mark_claude_link_broken(member, generation, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let unreachable = RelinkNotifier::new(h.store.clone(), Replies::default());
    assert_eq!(unreachable.send_pending().await.unwrap(), 0);
    let pending = h
        .store
        .pending_relink_notices(OffsetDateTime::now_utc(), RELINK_MAX_ATTEMPTS)
        .await
        .unwrap();
    assert_eq!(pending.len(), 1);
    h.store.close().await;
    assert!(unreachable.send_pending().await.is_err());
}

#[tokio::test]
async fn the_notifier_dms_a_member_when_a_refresh_breaks_their_link() {
    let h = harness().await;
    let member = h
        .store
        .ensure_member(&key("alice"), "alice", OffsetDateTime::now_utc())
        .await
        .unwrap();
    h.store
        .put_claude_link(
            member,
            &NewClaudeLink {
                access_token: SecretString::from("access"),
                refresh_token: SecretString::from("dead-refresh"),
                expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(10),
                plan: None,
                rate_limit_tier: None,
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({"error": "invalid_grant"})))
        .mount(&h.oauth)
        .await;
    let notifier = RelinkNotifier::new(h.store.clone(), h.commands.replies().clone());
    let (stop, stopping) = watch::channel(false);
    let task = tokio::spawn(notifier.run(
        h.auth.take_relink_notices(),
        Duration::from_secs(3600),
        stopping,
    ));
    tokio::task::yield_now().await;

    let err = h.auth.access_token(member).await.unwrap_err();
    assert!(matches!(err, AuthError::RelinkRequired), "{err}");
    let err = h.auth.access_token(member).await.unwrap_err();
    assert!(matches!(err, AuthError::RelinkRequired), "{err}");

    let replies = h.wait_for_replies("alice", 1).await;
    assert_eq!(replies, [relink_notice(&key("alice"))]);
    stop.send_replace(true);
    task.await.unwrap();
    assert_eq!(h.replies_to("alice").len(), 1);
}

#[tokio::test]
async fn the_notifier_stops_when_its_sender_is_dropped() {
    let h = harness().await;
    let notifier = RelinkNotifier::new(h.store.clone(), h.commands.replies().clone());
    let (stop, stopping) = watch::channel(false);
    let task = tokio::spawn(notifier.run(None, Duration::from_millis(5), stopping));
    tokio::time::sleep(Duration::from_millis(20)).await;
    drop(stop);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn agent_commands_without_rocketchat_agents() {
    let h = harness().await;
    h.dm("alice", "create helper").await;
    assert_eq!(
        h.last_reply("alice"),
        "Creating agents here isn't available yet."
    );
    h.dm("alice", "list").await;
    assert_eq!(
        h.last_reply("alice"),
        "There are no agents yet. Create one with `create <name>`."
    );
    h.dm("alice", "list @bob").await;
    assert_eq!(h.last_reply("alice"), "I don't know that member.");
    h.dm("alice", "list <@U1>").await;
    assert_eq!(h.last_reply("alice"), "That member has no agents.");
    for command in [
        "pause helper",
        "resume helper",
        "delete helper",
        "persona helper x",
    ] {
        h.dm("alice", command).await;
        assert_eq!(
            h.last_reply("alice"),
            "You have no agent named `helper`. Only an agent's owner can change it.",
            "{command}"
        );
    }

    let (alice, _) = h.linked_member("alice", "claude_pro").await;
    let team = TeamId::new(TEAM);
    let new = store::NewAgent {
        owner: alice,
        name: "helper",
        persona: "p",
        visibility: store::Visibility::Private,
        surface: SurfaceKind::RocketChat,
        team: &team,
    };
    assert!(matches!(
        h.store
            .create_agent(&new, 10, OffsetDateTime::now_utc())
            .await
            .unwrap(),
        store::AgentCreation::Created(..)
    ));
    h.dm("bob", "list").await;
    assert_eq!(
        h.last_reply("bob"),
        "There are no agents yet. Create one with `create <name>`."
    );
    h.dm("alice", "list").await;
    assert_eq!(
        h.last_reply("alice"),
        "Agents:\n- `helper` (no bot here), owned by alice"
    );
    h.channel("alice", "persona helper").await;
    assert!(
        h.last_reply("alice")
            .starts_with("Put the persona after the name, or attach it"),
        "{}",
        h.last_reply("alice")
    );
    h.dm("alice", "persona helper   ").await;
    assert!(
        h.last_reply("alice")
            .starts_with("Put the persona after the name"),
        "{}",
        h.last_reply("alice")
    );
    h.dm("alice", "delete helper").await;
    assert_eq!(h.last_reply("alice"), "Deleted `helper`.");
}

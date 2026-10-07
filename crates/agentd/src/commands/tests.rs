//! Command dispatch against a `MockSurface` manager bot and wiremock OAuth
//! endpoints.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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

use super::intake::CommandIntake;
use super::relink::{
    RELINK_BACKOFF_INITIAL, RELINK_BACKOFF_MAX, RELINK_LEASE, RELINK_MAX_ATTEMPTS, RelinkNotifier,
    relink_notice,
};
use super::rocketchat::{CommandFeed, command_in, listen};
use super::*;
use crate::telemetry::tests::global_logs;

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

/// Opens `dm-<user>` for every member, and counts the opens.
#[derive(Default)]
struct Dms(AtomicUsize);

#[async_trait]
impl OpenDm for Dms {
    async fn open_dm(&self, member: &MemberKey) -> Result<ConversationId, SurfaceError> {
        self.0.fetch_add(1, Ordering::SeqCst);
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
    dms: Arc<Dms>,
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
    let dms = Arc::new(Dms::default());
    let bot = Arc::new(ManagerBot::new(
        manager.bot.clone(),
        mock.clone(),
        dms.clone(),
    ));
    let commands = Commands::new(
        store.clone(),
        Arc::clone(&auth),
        Replies::new(Some(bot)),
        None,
        None,
    );
    Harness {
        store,
        auth,
        commands,
        mock,
        oauth,
        manager,
        dms,
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
    let (intake, submitter) = CommandIntake::new(h.commands.clone());
    let feed = CommandFeed::new(submitter, h.manager.clone());
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
    let logs = global_logs().tag();
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

    logs.snapshot()
        .assert_has("running a command")
        .assert_has("\"command\":\"login\"");
    global_logs()
        .snapshot()
        .assert_lacks(CODE)
        .assert_lacks(&pasted);
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
    let logs = global_logs().tag();
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
    logs.snapshot()
        .assert_has("\"command\":\"login\"")
        .assert_has("cancelled pending logins after a public secret");
    global_logs().snapshot().assert_lacks(CODE);
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
    let logs = global_logs().tag();
    h.channel("root", &format!("admin api-key set {API_KEY}"))
        .await;
    let reply = h.last_reply("root");
    assert!(reply.contains("I didn't store it"), "{reply}");
    assert!(
        reply.contains("Revoke it in the Anthropic Console"),
        "{reply}"
    );
    assert!(!reply.contains(API_KEY));
    logs.snapshot()
        .assert_has("\"command\":\"admin api-key set\"");
    global_logs().snapshot().assert_lacks(API_KEY);
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
    let logs = global_logs().tag();
    h.dm("alice", "login").await;
    let state = state_of(&h.last_reply("alice"));

    h.channel("alice", &format!("login {CODE} extra")).await;

    let reply = h.last_reply("alice");
    assert!(reply.contains("looked like it held a secret"), "{reply}");
    assert!(reply.contains("Usage: `login [code]`"), "{reply}");
    assert!(h.store.take_pending_login(&state).await.unwrap().is_none());
    logs.snapshot()
        .assert_has("command text didn't parse")
        .assert_has("cancelled pending logins after a public secret");
    global_logs().snapshot().assert_lacks(CODE);
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
async fn without_the_slack_manager_app_slack_and_unknown_workspaces_get_no_replies() {
    let h = harness().await;
    let replies = h.commands.replies();
    let slack = MemberKey {
        surface: SurfaceKind::Slack,
        team: "T1".into(),
        user: "U1".into(),
    };
    let origin = Origin::SlackSlash {
        response_url: SecretString::from("https://hooks.slack.com/commands/secret"),
        conv: ConvRef {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            conversation: "C1".into(),
        },
    };
    assert!(origin.is_private());
    let debug = format!("{origin:?}");
    assert!(debug.starts_with("SlackSlash { conv: "), "{debug}");
    assert!(!debug.contains("hooks.slack.com"), "{debug}");
    let err = replies
        .reply_private(&slack, &origin, "hi")
        .await
        .unwrap_err();
    assert!(
        matches!(err, ReplyError::NoManagerBot(SurfaceKind::Slack)),
        "{err}"
    );
    assert!(matches!(
        replies.dm(&slack, "hi").await,
        Err(ReplyError::NoManagerBot(SurfaceKind::Slack))
    ));
    assert!(!replies.can_dm(&slack));
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
struct DeadDms(AtomicUsize);

#[async_trait]
impl OpenDm for DeadDms {
    async fn open_dm(&self, _: &MemberKey) -> Result<ConversationId, SurfaceError> {
        self.0.fetch_add(1, Ordering::SeqCst);
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
    let logs = global_logs().tag();
    broken_member(&h, "alice").await;
    let dms = Arc::new(DeadDms::default());
    let bot = Arc::new(ManagerBot::new(
        h.manager.bot.clone(),
        h.mock.clone(),
        dms.clone(),
    ));
    let notifier = RelinkNotifier::new(h.store.clone(), Replies::new(Some(bot)));
    let tries = || dms.0.load(Ordering::SeqCst);
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
    let out = logs.snapshot();
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

/// A runner for the session commands: warm sessions are listed, and a
/// reset holds its session's lock, wakes itself and yields once, which
/// makes a `FuturesUnordered` stop polling the others after two, then goes
/// to the store once the gate lets it through, except for sessions whose
/// sandbox won't stop.
struct FakeRunner {
    store: Store,
    mock: Arc<MockSurface>,
    warm: std::sync::Mutex<std::collections::HashSet<core_types::SessionId>>,
    stuck: std::sync::Mutex<std::collections::HashSet<core_types::SessionId>>,
    slots: std::sync::Mutex<
        std::collections::HashMap<core_types::SessionId, Arc<tokio::sync::Mutex<()>>>,
    >,
    resets: std::sync::Mutex<Vec<(core_types::SessionId, usize)>>,
    gate: tokio::sync::Semaphore,
}

impl FakeRunner {
    fn new(h: &Harness) -> Arc<Self> {
        Self::with_gate(h, tokio::sync::Semaphore::MAX_PERMITS)
    }

    /// A runner whose resets each wait for a permit of its gate, which
    /// starts closed.
    fn gated(h: &Harness) -> Arc<Self> {
        Self::with_gate(h, 0)
    }

    fn with_gate(h: &Harness, permits: usize) -> Arc<Self> {
        Arc::new(Self {
            store: h.store.clone(),
            mock: Arc::clone(&h.mock),
            warm: std::sync::Mutex::default(),
            stuck: std::sync::Mutex::default(),
            slots: std::sync::Mutex::default(),
            resets: std::sync::Mutex::default(),
            gate: tokio::sync::Semaphore::new(permits),
        })
    }

    /// The sessions whose reset holds their lock, sorted.
    fn resets(&self) -> Vec<core_types::SessionId> {
        let mut resets: Vec<_> = self
            .resets
            .lock()
            .unwrap()
            .iter()
            .map(|(session, _)| *session)
            .collect();
        resets.sort();
        resets
    }

    /// How many resets held their session's lock before the manager bot
    /// posted anything.
    fn held_before_any_post(&self) -> usize {
        self.resets
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, posts)| *posts == 0)
            .count()
    }
}

#[async_trait]
impl SessionControl for FakeRunner {
    async fn reset(
        &self,
        session: core_types::SessionId,
    ) -> Result<Option<store::Session>, runner::RunnerError> {
        let slot = Arc::clone(self.slots.lock().unwrap().entry(session).or_default());
        let _held = slot.lock_owned().await;
        let posts = self.mock.posts().len();
        self.resets.lock().unwrap().push((session, posts));
        let mut yielded = false;
        std::future::poll_fn(|cx| {
            if yielded {
                return std::task::Poll::Ready(());
            }
            yielded = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        })
        .await;
        self.gate.acquire().await.unwrap().forget();
        if self.stuck.lock().unwrap().contains(&session) {
            return Err(sandbox::SandboxError::NotFound.into());
        }
        self.warm.lock().unwrap().remove(&session);
        Ok(self.store.reset_session(session, at(10_000)).await?)
    }

    fn warm_sessions(&self) -> Vec<core_types::SessionId> {
        self.warm.lock().unwrap().iter().copied().collect()
    }
}

fn at(offset: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000 + offset).unwrap()
}

/// The sessions of alice's agent `helper`, one of each kind.
struct Sessions {
    agent: core_types::AgentId,
    own_dm: store::Session,
    thread: store::Session,
    other_thread: store::Session,
    elsewhere: store::Session,
    their_dm: store::Session,
    task: store::Session,
    unused: store::Session,
    first_turn: store::Session,
}

fn thread_in(room: &str, root: Option<&str>) -> core_types::ThreadKey {
    core_types::ThreadKey {
        conv: conv(room),
        root: root.map(core_types::MessageId::new),
    }
}

impl Harness {
    /// alice's agent `helper`, and its sessions: alice's DM with it, two
    /// threads in `GENERAL` and one in `OTHER`, bob's DM with it, a private
    /// task posting to `GENERAL`, a session that has had no turn, and one
    /// whose first turn is running. bob owns an agent `helper` too, with a
    /// session in `GENERAL`.
    async fn sessions(&self) -> Sessions {
        let owner = self
            .store
            .ensure_member(&key("alice"), "alice", at(0))
            .await
            .unwrap();
        let agent = self.agent(owner).await;
        let bob = self
            .store
            .ensure_member(&key("bob"), "bob", at(0))
            .await
            .unwrap();
        let bobs = self.agent(bob).await;
        let channel =
            |room: &str| core_types::ScopeKey::for_conversation(ConvKind::Channel, conv(room));
        let session = async |thread: core_types::ThreadKey,
                             scope: core_types::ScopeKey,
                             created: i64,
                             turn: Option<i64>| {
            let session = self
                .store
                .session_for_thread(agent, &thread, &scope, at(created))
                .await
                .unwrap()
                .session;
            if let Some(turn) = turn {
                self.store
                    .record_session_turn(session.id, true, at(turn))
                    .await
                    .unwrap();
            }
            self.store.session(session.id).await.unwrap().unwrap()
        };
        let own_dm = session(
            thread_in("dm-helper-alice", None),
            core_types::ScopeKey::Private,
            1,
            Some(600),
        )
        .await;
        let thread = session(
            thread_in("GENERAL", Some("R1")),
            channel("GENERAL"),
            1,
            Some(60),
        )
        .await;
        let other_thread = session(
            thread_in("GENERAL", Some("R2")),
            channel("GENERAL"),
            1,
            Some(30),
        )
        .await;
        let elsewhere = session(
            thread_in("OTHER", Some("R3")),
            channel("OTHER"),
            1,
            Some(20),
        )
        .await;
        let their_dm = session(
            thread_in("dm-helper-bob", None),
            core_types::ScopeKey::for_conversation(ConvKind::Dm, conv("dm-helper-bob")),
            1,
            Some(10),
        )
        .await;
        let unused = session(
            thread_in("GENERAL", Some("R4")),
            channel("GENERAL"),
            5,
            None,
        )
        .await;
        let first_turn = session(
            thread_in("GENERAL", Some("R5")),
            channel("GENERAL"),
            4,
            None,
        )
        .await;
        assert!(
            self.store
                .mark_session_turn_pending(first_turn.id)
                .await
                .unwrap()
        );
        let first_turn = self.store.session(first_turn.id).await.unwrap().unwrap();
        let task = self
            .store
            .create_private_session(
                agent,
                core_types::ConsentId::new_v4(),
                &thread_in("GENERAL", Some("R1")),
                at(2),
            )
            .await
            .unwrap();
        self.store
            .record_session_turn(task.id, true, at(3_600))
            .await
            .unwrap();
        let task = self.store.session(task.id).await.unwrap().unwrap();
        let bobs = self
            .store
            .session_for_thread(
                bobs,
                &thread_in("GENERAL", Some("R1")),
                &channel("GENERAL"),
                at(1),
            )
            .await
            .unwrap()
            .session;
        self.store
            .record_session_turn(bobs.id, true, at(1))
            .await
            .unwrap();
        Sessions {
            agent,
            own_dm,
            thread,
            other_thread,
            elsewhere,
            their_dm,
            task,
            unused,
            first_turn,
        }
    }

    async fn agent(&self, owner: core_types::MemberId) -> core_types::AgentId {
        let team = TeamId::new(TEAM);
        let new = store::NewAgent {
            owner,
            name: "helper",
            persona: "p",
            visibility: store::Visibility::Public,
            surface: SurfaceKind::RocketChat,
            team: &team,
        };
        let store::AgentCreation::Created(agent, _) =
            self.store.create_agent(&new, 10, at(0)).await.unwrap()
        else {
            panic!("the agent was created");
        };
        agent.id
    }

    /// The sessions of `agent` in use, with none warm.
    async fn in_use(&self, agent: core_types::AgentId) -> Vec<core_types::SessionId> {
        let mut in_use: Vec<_> = self
            .store
            .sessions_in_use(agent, &[], None)
            .await
            .unwrap()
            .into_iter()
            .map(|session| session.id)
            .collect();
        in_use.sort();
        in_use
    }

    async fn is_reset(&self, session: &store::Session) -> bool {
        self.store
            .session(session.id)
            .await
            .unwrap()
            .unwrap()
            .reset_at
            .is_some()
    }
}

fn sorted<const N: usize>(ids: [core_types::SessionId; N]) -> Vec<core_types::SessionId> {
    let mut ids = ids.to_vec();
    ids.sort();
    ids
}

#[tokio::test]
async fn sessions_lists_the_sessions_in_use_most_recent_first() {
    let h = harness().await;
    let s = h.sessions().await;
    let runner = FakeRunner::new(&h);
    runner
        .warm
        .lock()
        .unwrap()
        .extend([s.thread.id, s.unused.id]);
    let control: Arc<dyn SessionControl> = runner.clone();
    h.commands.use_sessions(Arc::downgrade(&control));

    h.dm("alice", "sessions helper").await;
    assert_eq!(
        h.last_reply("alice"),
        "`helper`'s 8 sessions, most recent first:\n\
         - A private task, last turn 2026-09-21 15:13 UTC, cold\n\
         - Your DM with it, last turn 2026-09-21 14:23 UTC, cold\n\
         - A thread in a channel, last turn 2026-09-21 14:14 UTC, warm\n\
         - A thread in a channel, last turn 2026-09-21 14:13 UTC, cold\n\
         - A thread in a channel, last turn 2026-09-21 14:13 UTC, cold\n\
         - Another member's DM with it, last turn 2026-09-21 14:13 UTC, cold\n\
         - A thread in a channel, no turn finished yet, warm\n\
         - A thread in a channel, no turn finished yet, cold\n\n\
         Start them all over with `reset helper`, or one conversation's with \
         `!agent reset helper here` there."
    );
    assert!(runner.resets().is_empty());
}

#[tokio::test]
async fn reset_without_here_resets_every_session_in_use_of_the_senders_agent() {
    let h = harness().await;
    let s = h.sessions().await;
    let runner = FakeRunner::new(&h);
    let control: Arc<dyn SessionControl> = runner.clone();
    h.commands.use_sessions(Arc::downgrade(&control));

    h.dm("carol", "reset helper").await;
    assert_eq!(
        h.last_reply("carol"),
        "You have no agent named `helper`. Only an agent's owner can change it."
    );
    h.dm("carol", "sessions helper").await;
    assert_eq!(
        h.last_reply("carol"),
        "You have no agent named `helper`. Only an agent's owner can change it."
    );
    h.dm("bob", "reset helper").await;
    assert_eq!(
        h.last_reply("bob"),
        "Resetting `helper`'s session: the next message in it starts a new conversation. If \
         it is running a turn, it resets once that turn ends. If it can't be reset, I'll tell \
         you in a direct message."
    );
    assert_eq!(runner.resets().len(), 1);
    assert!(
        !h.is_reset(&s.thread).await,
        "bob's reset reached alice's agent"
    );
    runner.resets.lock().unwrap().clear();

    h.channel("alice", "reset helper").await;
    assert_eq!(
        h.last_reply("alice"),
        "Resetting `helper`'s 7 sessions: the next message in each starts a new conversation. \
         A session running a turn resets once that turn ends. If any can't be reset, I'll tell \
         you in a direct message."
    );
    assert_eq!(
        runner.resets(),
        sorted([
            s.own_dm.id,
            s.thread.id,
            s.other_thread.id,
            s.elsewhere.id,
            s.their_dm.id,
            s.task.id,
            s.first_turn.id,
        ])
    );
    assert!(!h.is_reset(&s.unused).await);
    for old in [&s.own_dm, &s.thread, &s.task] {
        assert!(h.is_reset(old).await);
    }
    assert!(
        h.in_use(s.agent).await.is_empty(),
        "the replacements have had no turn"
    );
    h.dm("alice", "sessions helper").await;
    assert_eq!(h.last_reply("alice"), "`helper` has no sessions yet.");
    h.dm("alice", "reset helper").await;
    assert_eq!(h.last_reply("alice"), "`helper` has no sessions to reset.");
}

#[tokio::test]
async fn reset_here_resets_only_the_conversations_sessions() {
    let h = harness().await;
    let s = h.sessions().await;
    let runner = FakeRunner::new(&h);
    let control: Arc<dyn SessionControl> = runner.clone();
    h.commands.use_sessions(Arc::downgrade(&control));

    h.dm("alice", "reset helper here").await;
    assert_eq!(
        h.last_reply("alice"),
        "`reset helper here` resets the conversation it is sent in, and no agent answers in \
         this one. Send `!agent reset helper here` in the conversation to reset, or \
         `reset helper` here to reset them all."
    );
    assert!(runner.resets().is_empty());

    h.channel("alice", "reset helper here").await;
    assert_eq!(
        h.last_reply("alice"),
        "Resetting `helper`'s 3 sessions here: the next message in each starts a new \
         conversation. A session running a turn resets once that turn ends. If any can't be \
         reset, I'll tell you in a direct message."
    );
    assert_eq!(
        runner.resets(),
        sorted([s.thread.id, s.other_thread.id, s.first_turn.id])
    );
    for untouched in [&s.own_dm, &s.elsewhere, &s.their_dm, &s.task, &s.unused] {
        assert!(!h.is_reset(untouched).await);
    }
    h.channel("alice", "reset helper here").await;
    assert_eq!(
        h.last_reply("alice"),
        "`helper` has no session here to reset."
    );

    let in_dm = Origin::RocketChatChannel {
        room: "dm-helper-alice".into(),
    };
    h.commands
        .handle_text(&key("alice"), "reset helper HERE", &in_dm, &[])
        .await;
    assert_eq!(
        h.last_reply("alice"),
        "Resetting `helper`'s session here: the next message in it starts a new conversation. \
         If it is running a turn, it resets once that turn ends. If it can't be reset, I'll \
         tell you in a direct message."
    );
    assert!(h.is_reset(&s.own_dm).await);
}

#[tokio::test]
async fn reset_here_in_a_rocketchat_room_without_a_session_opens_only_the_replys_dm() {
    let h = harness().await;
    h.sessions().await;
    let opened = h.dms.0.load(Ordering::SeqCst);
    let quiet = Origin::RocketChatChannel {
        room: "QUIET".into(),
    };
    h.commands
        .handle_text(&key("alice"), "reset helper here", &quiet, &[])
        .await;
    assert_eq!(
        h.last_reply("alice"),
        "`helper` has no session here to reset."
    );
    assert_eq!(
        h.dms.0.load(Ordering::SeqCst) - opened,
        1,
        "the room isn't compared with the manager bot's DM"
    );
}

#[tokio::test]
async fn a_session_whose_sandbox_wont_stop_is_not_reset() {
    let h = harness().await;
    let s = h.sessions().await;
    let runner = FakeRunner::new(&h);
    runner
        .stuck
        .lock()
        .unwrap()
        .extend([s.thread.id, s.elsewhere.id]);
    let control: Arc<dyn SessionControl> = runner.clone();
    h.commands.use_sessions(Arc::downgrade(&control));

    h.channel("alice", "reset helper here").await;
    let replies = h.replies_to("alice");
    assert_eq!(replies.len(), 2, "{replies:?}");
    assert!(replies[0].starts_with("Resetting `helper`'s 3 sessions here"));
    assert_eq!(
        replies[1],
        "`!agent reset helper here` couldn't reset 1 of `helper`'s 3 sessions; the other 2 are \
         reset. Please send it again in a minute."
    );
    assert!(!h.is_reset(&s.thread).await);
    assert!(h.is_reset(&s.other_thread).await);

    let other = Origin::RocketChatChannel {
        room: "OTHER".into(),
    };
    h.commands
        .handle_text(&key("alice"), "reset helper here", &other, &[])
        .await;
    assert_eq!(
        h.last_reply("alice"),
        "`!agent reset helper here` couldn't reset `helper`'s session. Please send it again in \
         a minute."
    );
    assert!(!h.is_reset(&s.elsewhere).await);

    runner
        .stuck
        .lock()
        .unwrap()
        .extend([s.own_dm.id, s.their_dm.id, s.task.id]);
    h.dm("alice", "reset helper").await;
    assert_eq!(
        h.last_reply("alice"),
        "`reset helper` couldn't reset any of `helper`'s 5 sessions. Please send it again in a \
         minute."
    );
}

#[tokio::test]
async fn without_a_runner_sessions_are_reset_in_the_store_and_none_is_warm() {
    let h = harness().await;
    let s = h.sessions().await;
    h.dm("alice", "sessions helper").await;
    let listed = h.last_reply("alice");
    assert!(listed.starts_with("`helper`'s 7 sessions"), "{listed}");
    assert!(!listed.contains("warm"), "{listed}");

    let runner = FakeRunner::new(&h);
    runner.warm.lock().unwrap().insert(s.thread.id);
    let control: Arc<dyn SessionControl> = runner.clone();
    h.commands.use_sessions(Arc::downgrade(&control));
    drop(control);
    drop(runner);

    let other = Origin::RocketChatChannel {
        room: "OTHER".into(),
    };
    h.commands
        .handle_text(&key("alice"), "reset helper here", &other, &[])
        .await;
    assert!(
        h.last_reply("alice")
            .starts_with("Resetting `helper`'s session here:")
    );
    assert!(h.is_reset(&s.elsewhere).await);
    h.dm("alice", "sessions helper").await;
    let listed = h.last_reply("alice");
    assert!(listed.starts_with("`helper`'s 6 sessions"), "{listed}");
    assert!(!listed.contains("warm"), "{listed}");
}

#[tokio::test]
async fn sessions_lists_at_most_the_most_recent_ones() {
    let h = harness().await;
    let owner = h
        .store
        .ensure_member(&key("alice"), "alice", at(0))
        .await
        .unwrap();
    let agent = h.agent(owner).await;
    let channel = core_types::ScopeKey::for_conversation(ConvKind::Channel, conv("GENERAL"));
    for n in 0..=MAX_LISTED {
        let root = format!("R{n}");
        let session = h
            .store
            .session_for_thread(agent, &thread_in("GENERAL", Some(&root)), &channel, at(0))
            .await
            .unwrap()
            .session;
        let turn = i64::try_from(n).unwrap() * 60;
        h.store
            .record_session_turn(session.id, true, at(turn))
            .await
            .unwrap();
    }
    h.dm("alice", "sessions helper").await;
    let listed = h.last_reply("alice");
    let lines: Vec<&str> = listed.lines().collect();
    assert_eq!(
        lines[0],
        format!(
            "`helper`'s {MAX_LISTED} most recent sessions of {}:",
            MAX_LISTED + 1
        )
    );
    assert_eq!(
        lines[1],
        "- A thread in a channel, last turn 2026-09-21 14:33 UTC, cold"
    );
    assert_eq!(
        lines.iter().filter(|line| line.starts_with("- ")).count(),
        MAX_LISTED
    );
    assert!(
        !listed.contains("14:13 UTC"),
        "the oldest is left out: {listed}"
    );
}

#[tokio::test]
async fn every_reset_is_queued_before_the_reply_and_later_commands_dont_wait_for_them() {
    const COUNT: usize = 200;
    let h = harness().await;
    let owner = h
        .store
        .ensure_member(&key("alice"), "alice", at(0))
        .await
        .unwrap();
    let agent = h.agent(owner).await;
    let channel = core_types::ScopeKey::for_conversation(ConvKind::Channel, conv("GENERAL"));
    let mut sessions = Vec::new();
    for n in 0..COUNT {
        let root = format!("R{n}");
        let session = h
            .store
            .session_for_thread(agent, &thread_in("GENERAL", Some(&root)), &channel, at(0))
            .await
            .unwrap()
            .session;
        h.store
            .record_session_turn(session.id, true, at(i64::try_from(n).unwrap()))
            .await
            .unwrap();
        sessions.push(session.id);
    }
    sessions.sort();
    let runner = FakeRunner::gated(&h);
    runner.stuck.lock().unwrap().insert(sessions[4]);
    let control: Arc<dyn SessionControl> = runner.clone();
    h.commands.use_sessions(Arc::downgrade(&control));
    let (intake, submitter) = CommandIntake::new(h.commands.clone());
    let intake = tokio::spawn(intake.run());
    let in_dm = || Origin::RocketChatDm {
        room: dm_room("alice").into(),
    };

    submitter
        .submit(key("alice"), "reset helper".to_owned(), in_dm(), Vec::new())
        .await
        .unwrap();
    let replies = h.wait_for_replies("alice", 1).await;
    assert_eq!(
        replies[0],
        format!(
            "Resetting `helper`'s {COUNT} sessions: the next message in each starts a new \
             conversation. A session running a turn resets once that turn ends. If any can't be \
             reset, I'll tell you in a direct message."
        )
    );
    assert_eq!(
        runner.held_before_any_post(),
        COUNT,
        "every reset held its session before the reply was posted"
    );
    assert_eq!(runner.resets(), sessions);

    submitter
        .submit(
            key("alice"),
            "sessions helper".to_owned(),
            in_dm(),
            Vec::new(),
        )
        .await
        .unwrap();
    let replies = h.wait_for_replies("alice", 2).await;
    assert!(
        replies[1].starts_with(&format!(
            "`helper`'s {MAX_LISTED} most recent sessions of {COUNT}:"
        )),
        "the owner's next command ran while the resets waited: {}",
        replies[1]
    );
    for id in &sessions {
        assert_eq!(h.store.session(*id).await.unwrap().unwrap().reset_at, None);
    }

    runner.gate.add_permits(COUNT);
    let replies = h.wait_for_replies("alice", 3).await;
    assert_eq!(
        replies[2],
        format!(
            "`reset helper` couldn't reset 1 of `helper`'s {COUNT} sessions; the other {} are \
             reset. Please send it again in a minute.",
            COUNT - 1
        )
    );
    for (n, id) in sessions.iter().enumerate() {
        let reset = h.store.session(*id).await.unwrap().unwrap().reset_at;
        assert_eq!(reset.is_some(), n != 4, "session {n}");
    }
    drop(submitter);
    tokio::time::timeout(Duration::from_secs(10), intake)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn the_intake_waits_for_a_queued_reset_when_it_stops() {
    let h = harness().await;
    let s = h.sessions().await;
    let runner = FakeRunner::gated(&h);
    let control: Arc<dyn SessionControl> = runner.clone();
    h.commands.use_sessions(Arc::downgrade(&control));
    let (intake, submitter) = CommandIntake::new(h.commands.clone());
    let intake = tokio::spawn(intake.run());
    submitter
        .submit(
            key("alice"),
            "reset helper".to_owned(),
            Origin::RocketChatDm {
                room: dm_room("alice").into(),
            },
            Vec::new(),
        )
        .await
        .unwrap();
    h.wait_for_replies("alice", 1).await;
    drop(submitter);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!intake.is_finished(), "the reset is still waiting");
    runner.gate.add_permits(100);
    tokio::time::timeout(Duration::from_secs(10), intake)
        .await
        .unwrap()
        .unwrap();
    assert!(h.is_reset(&s.thread).await);
    assert_eq!(h.replies_to("alice").len(), 1, "no reset failed");
}

//! Commands on Slack against a wiremock Slack: slash commands answered
//! through `response_url`, manager DMs, `/agent slack-token`, the
//! configuration token rotator, and members who leave.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use auth::{Auth, OAuthConfig};
use core_types::{
    BindingId, ConvKind, ConvRef, InboundEvent, MemberId, MemberKey, MsgRef, Sender, SurfaceKind,
    TeamId, UserId,
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use store::{NewClaudeLink, NewSlackConfigToken, Sealer, Store};
use surface_slack::{
    BindingRef, InFlight, Interaction, SlackClient, SlackEvent, SlackInbound, SlashCommand,
};
use testkit::TempDir;
use time::OffsetDateTime;
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use super::intake::CommandIntake;
use super::relink::RelinkNotifier;
use super::slack::{consent_action, dm_command, member_who_left, slash_command};
use super::slack_tokens::{
    ConfigTokenRotator, NOTICE_LEASE, NOTICE_MAX_ATTEMPTS, ROTATION_LEASE, RotationPass,
    STORE_ATTEMPTS, broken_token_notice,
};
use super::*;
use crate::consents::ConsentSettings;
use crate::slack::Inbound;
use crate::slack::manager::{ManagerIdentity, SlackManager};
use crate::telemetry::tests::global_logs;

const TEAM: &str = "T0TEAM001";
const BOT_TOKEN: &str = "xoxb-manager-SECRET-bot";
const GIVEN_TOKEN: &str = "xoxe.xoxp-1-GIVEN-SECRET-token";
const GIVEN_REFRESH: &str = "xoxe-1-GIVEN-SECRET-refresh";
const HOOK: &str = "hook-SECRET-path";

pub(super) fn slack_key(user: &str) -> MemberKey {
    MemberKey {
        surface: SurfaceKind::Slack,
        team: TeamId::new(TEAM),
        user: UserId::new(user),
    }
}

pub(super) fn slack_channel(id: &str) -> ConvRef {
    ConvRef {
        surface: SurfaceKind::Slack,
        team: TeamId::new(TEAM),
        conversation: id.into(),
    }
}

pub(super) fn identity() -> ManagerIdentity {
    ManagerIdentity {
        team: TeamId::new(TEAM),
        bot_user: UserId::new("U0MANAGER"),
        bot_id: "B0MANAGER".to_owned(),
        app_id: "A0MANAGER".to_owned(),
        app_name: Some("agent-core".to_owned()),
        enterprise: None,
    }
}

fn ok(body: Value) -> ResponseTemplate {
    let mut body = body;
    body["ok"] = json!(true);
    ResponseTemplate::new(200).set_body_json(body)
}

pub(super) struct SlackHarness {
    pub(super) store: Store,
    data: TempDir,
    pub(super) commands: Commands,
    consents: Consents,
    pub(super) slack: MockServer,
    manager: SlackManager,
    hooks: std::sync::atomic::AtomicUsize,
}

pub(super) async fn slack_harness() -> SlackHarness {
    let store =
        Store::open_in_memory(Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap())
            .await
            .unwrap();
    slack_harness_on(store).await
}

pub(super) async fn slack_harness_on(store: Store) -> SlackHarness {
    let oauth = OAuthConfig {
        token_url: "http://127.0.0.1:9/token".to_owned(),
        revoke_url: "http://127.0.0.1:9/revoke".to_owned(),
        profile_url: "http://127.0.0.1:9/profile".to_owned(),
        ..OAuthConfig::default()
    };
    let auth = Arc::new(Auth::new(oauth, store.clone()).unwrap());
    let slack = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.postMessage"))
        .respond_with(ok(json!({"ts": "1727700000.000100"})))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.open"))
        .respond_with(ok(json!({"channel": {"id": "D0DM00001"}})))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.list"))
        .respond_with(ok(json!({"members": []})))
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/users.info"))
        .respond_with(home_member)
        .mount(&slack)
        .await;
    Mock::given(method("POST"))
        .and(path_regex("^/hooks/"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&slack)
        .await;
    let client = SlackClient::new(&format!("{}/api/", slack.uri()))
        .unwrap()
        .with_max_retry_wait(Duration::from_secs(5));
    let manager = SlackManager::with_identity(
        client.clone(),
        client.bot(SecretString::from(BOT_TOKEN)),
        identity(),
    );
    let replies = Replies::new(None).with_slack(&manager);
    let data = TempDir::new("agentd-slack");
    let git = crate::skills::Git::new(cred_proxy::EgressPolicy::new(Vec::new(), Vec::new()));
    let skills = crate::skills::Skills::new(store.clone(), data.path().to_owned(), git);
    let consents = Consents::new(store.clone(), ConsentSettings::in_data_dir(data.path()));
    let commands = Commands::new(
        store.clone(),
        Arc::clone(&auth),
        replies,
        None,
        Some(manager.clone()),
        skills,
    )
    .with_consents(consents.clone());
    SlackHarness {
        store,
        data,
        commands,
        consents,
        slack,
        manager,
        hooks: std::sync::atomic::AtomicUsize::new(0),
    }
}

impl SlackHarness {
    /// A new `response_url`, and its path.
    pub(super) fn response_url(&self) -> (SecretString, String) {
        let n = self.hooks.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = format!("/hooks/{n}/{HOOK}");
        let url = SecretString::from(format!("{}{path}", self.slack.uri()));
        (url, path)
    }

    /// Runs `/agent <text>` as `user` in `C0CHAN001` and returns the
    /// replies sent to its `response_url`.
    pub(super) async fn slash(&self, user: &str, text: &str) -> Vec<String> {
        self.slash_in(user, &slack_channel("C0CHAN001"), text).await
    }

    /// Runs `/agent <text>` as `user` in `conv` and returns the replies
    /// sent to its `response_url`.
    async fn slash_in(&self, user: &str, conv: &ConvRef, text: &str) -> Vec<String> {
        let (response_url, hook) = self.response_url();
        let origin = Origin::SlackSlash {
            response_url,
            conv: conv.clone(),
        };
        self.commands
            .handle_text(&slack_key(user), text, &origin, &[])
            .await;
        self.requests()
            .await
            .into_iter()
            .filter(|request| request.url.path() == hook)
            .map(|request| json_body(&request)["text"].as_str().unwrap().to_owned())
            .collect()
    }

    pub(super) async fn requests(&self) -> Vec<Request> {
        self.slack.received_requests().await.unwrap_or_default()
    }

    async fn calls(&self, method_name: &str) -> Vec<Request> {
        let wanted = format!("/api/{method_name}");
        self.requests()
            .await
            .into_iter()
            .filter(|request| request.url.path() == wanted)
            .collect()
    }

    /// Every text the manager posted, with the channel.
    pub(super) async fn posts(&self) -> Vec<(String, String)> {
        self.calls("chat.postMessage")
            .await
            .iter()
            .map(|request| {
                let body = json_body(request);
                (
                    body["channel"].as_str().unwrap().to_owned(),
                    body["text"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    pub(super) async fn linked(&self, user: &str) -> MemberId {
        self.linked_as(&slack_key(user)).await
    }

    async fn linked_as(&self, key: &MemberKey) -> MemberId {
        let member = self
            .store
            .ensure_member(key, key.user.as_str(), OffsetDateTime::now_utc())
            .await
            .unwrap();
        link(&self.store, member).await;
        member
    }

    fn rotator(&self) -> ConfigTokenRotator {
        ConfigTokenRotator::new(
            self.store.clone(),
            self.manager.client().clone(),
            self.commands.replies().clone(),
        )
    }

    async fn stored(&self, member: MemberId) -> Option<(String, String)> {
        self.store
            .slack_config_token(member, &TeamId::new(TEAM))
            .await
            .unwrap()
            .map(|token| {
                (
                    token.token.expose_secret().to_owned(),
                    token.refresh_token.expose_secret().to_owned(),
                )
            })
    }
}

pub(super) fn json_body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap()
}

/// Answers a rotation of `refresh` with `token` and `new_refresh`, for
/// `user`, expiring at `exp`, exactly once.
async fn mount_rotation(
    slack: &MockServer,
    refresh: &str,
    token: &str,
    new_refresh: &str,
    user: &str,
    exp: i64,
) {
    Mock::given(method("POST"))
        .and(path("/api/tooling.tokens.rotate"))
        .and(body_string_contains(format!("refresh_token={refresh}")))
        .respond_with(ok(json!({
            "token": token,
            "refresh_token": new_refresh,
            "team_id": TEAM,
            "user_id": user,
            "iat": exp - 43_200,
            "exp": exp,
        })))
        .expect(1)
        .mount(slack)
        .await;
}

fn clock(at: OffsetDateTime) -> impl Fn() -> OffsetDateTime {
    move || at
}

fn in_hours(hours: i64) -> i64 {
    (OffsetDateTime::now_utc() + time::Duration::hours(hours)).unix_timestamp()
}

#[tokio::test]
async fn slack_token_rotates_at_once_and_stores_the_new_pair_without_logging_either() {
    let h = slack_harness().await;
    let logs = global_logs().tag();
    let alice = h.linked("U0HUMAN01").await;
    let exp = in_hours(12);
    mount_rotation(
        &h.slack,
        GIVEN_REFRESH,
        "xoxe.xoxp-1-NEW-SECRET-token",
        "xoxe-1-NEW-SECRET-refresh",
        "U0HUMAN01",
        exp,
    )
    .await;

    let replies = h
        .slash(
            "U0HUMAN01",
            &format!("slack-token {GIVEN_TOKEN} {GIVEN_REFRESH}"),
        )
        .await;
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert!(
        replies[0].starts_with("Your Slack configuration token is registered."),
        "{replies:?}"
    );
    assert_eq!(
        h.stored(alice).await,
        Some((
            "xoxe.xoxp-1-NEW-SECRET-token".to_owned(),
            "xoxe-1-NEW-SECRET-refresh".to_owned()
        ))
    );
    let status = h
        .store
        .slack_config_token_status(alice, &TeamId::new(TEAM))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status.expires_at.unix_timestamp(), exp);

    let rotations = h.calls("tooling.tokens.rotate").await;
    assert!(rotations[0].headers.get("authorization").is_none());

    logs.snapshot()
        .assert_has("registered a Slack configuration token");
    let everything = global_logs().snapshot();
    for secret in [
        GIVEN_TOKEN,
        GIVEN_REFRESH,
        "NEW-SECRET",
        HOOK,
        BOT_TOKEN,
        "SECRET",
    ] {
        everything.assert_lacks(secret);
    }
    for reply in &replies {
        assert!(!reply.contains("SECRET"), "{reply}");
    }
}

#[tokio::test]
async fn a_refused_refresh_token_stores_nothing_and_says_so() {
    let h = slack_harness().await;
    let logs = global_logs().tag();
    let alice = h.linked("U0HUMAN01").await;
    Mock::given(method("POST"))
        .and(path("/api/tooling.tokens.rotate"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "invalid_refresh_token"})),
        )
        .expect(1)
        .mount(&h.slack)
        .await;
    let replies = h
        .slash(
            "U0HUMAN01",
            &format!("slack-token {GIVEN_TOKEN} {GIVEN_REFRESH}"),
        )
        .await;
    assert!(
        replies[0].starts_with("Slack didn't accept that refresh token."),
        "{replies:?}"
    );
    assert_eq!(h.stored(alice).await, None);
    logs.snapshot()
        .assert_has("Slack refused a configuration refresh token");
    global_logs().snapshot().assert_lacks("SECRET");
}

#[tokio::test]
async fn an_unreachable_slack_stores_nothing() {
    let h = slack_harness().await;
    let alice = h.linked("U0HUMAN01").await;
    Mock::given(method("POST"))
        .and(path("/api/tooling.tokens.rotate"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.slack)
        .await;
    let replies = h
        .slash(
            "U0HUMAN01",
            &format!("slack-token {GIVEN_TOKEN} {GIVEN_REFRESH}"),
        )
        .await;
    assert!(
        replies[0].starts_with("I couldn't reach Slack"),
        "{replies:?}"
    );
    assert_eq!(h.stored(alice).await, None);
}

#[tokio::test]
async fn a_token_of_another_member_or_workspace_is_refused() {
    for (user, team) in [("U0OTHER01", TEAM), ("U0HUMAN01", "T0ELSEWHERE")] {
        let h = slack_harness().await;
        let alice = h.linked("U0HUMAN01").await;
        Mock::given(method("POST"))
            .and(path("/api/tooling.tokens.rotate"))
            .respond_with(ok(json!({
                "token": "xoxe.xoxp-1-NEW-SECRET-token",
                "refresh_token": "xoxe-1-NEW-SECRET-refresh",
                "team_id": team,
                "user_id": user,
                "exp": in_hours(12),
            })))
            .mount(&h.slack)
            .await;
        let replies = h
            .slash(
                "U0HUMAN01",
                &format!("slack-token {GIVEN_TOKEN} {GIVEN_REFRESH}"),
            )
            .await;
        assert!(
            replies[0].starts_with("That configuration token belongs to another member"),
            "{replies:?}"
        );
        assert_eq!(h.stored(alice).await, None);
    }
}

#[tokio::test]
async fn slack_token_needs_a_linked_member_on_slack() {
    let h = slack_harness().await;
    let replies = h
        .slash(
            "U0HUMAN01",
            &format!("slack-token {GIVEN_TOKEN} {GIVEN_REFRESH}"),
        )
        .await;
    assert!(
        replies[0].starts_with("Link your Claude account first with `/agent login`"),
        "{replies:?}"
    );
    h.store
        .ensure_member(&slack_key("U0HUMAN02"), "bob", OffsetDateTime::now_utc())
        .await
        .unwrap();
    let replies = h
        .slash(
            "U0HUMAN02",
            &format!("slack-token {GIVEN_TOKEN} {GIVEN_REFRESH}"),
        )
        .await;
    assert!(replies[0].starts_with("Link your Claude account first"));
    assert!(h.calls("tooling.tokens.rotate").await.is_empty());

    let rocketchat = MemberKey {
        surface: SurfaceKind::RocketChat,
        team: TeamId::new("chat.example.org"),
        user: UserId::new("alice"),
    };
    let (reply, _) = h
        .commands
        .run(
            &rocketchat,
            commands::parse(&format!("slack-token {GIVEN_TOKEN} {GIVEN_REFRESH}")).unwrap(),
            &Origin::RocketChatDm {
                room: "dm-alice".into(),
            },
            &[],
            "",
        )
        .await;
    assert!(reply.contains("send it on Slack"), "{reply}");
    assert!(h.calls("tooling.tokens.rotate").await.is_empty());
}

#[tokio::test]
async fn without_the_slack_manager_app_slack_token_is_unavailable() {
    let store =
        Store::open_in_memory(Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap())
            .await
            .unwrap();
    let auth = Arc::new(Auth::new(OAuthConfig::default(), store.clone()).unwrap());
    let git = crate::skills::Git::new(cred_proxy::EgressPolicy::new(Vec::new(), Vec::new()));
    let skills = crate::skills::Skills::new(store.clone(), "/nonexistent/agentd".into(), git);
    let commands = Commands::new(store, auth, Replies::default(), None, None, skills);
    let (reply, _) = commands
        .run(
            &slack_key("U0HUMAN01"),
            commands::parse(&format!("slack-token {GIVEN_TOKEN} {GIVEN_REFRESH}")).unwrap(),
            &Origin::SlackDm {
                channel: "D0DM00001".into(),
            },
            &[],
            "",
        )
        .await;
    assert_eq!(reply, "Slack isn't set up on this agentd.");
}

#[tokio::test]
async fn me_on_slack_names_the_manager_app_and_the_token_state() {
    let h = slack_harness().await;
    let replies = h.slash("U0HUMAN01", "me").await;
    let reply = &replies[0];
    assert!(reply.starts_with("Claude account: not linked."), "{reply}");
    assert!(
        reply.contains("Slack configuration token: not registered."),
        "{reply}"
    );
    assert!(reply.contains("`agent-core` (`A0MANAGER`)"), "{reply}");

    let alice = h.linked("U0HUMAN01").await;
    let row = h
        .store
        .put_slack_config_token(
            alice,
            &TeamId::new(TEAM),
            &NewSlackConfigToken {
                token: SecretString::from("t"),
                refresh_token: SecretString::from("r"),
                expires_at: OffsetDateTime::now_utc() + time::Duration::hours(12),
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap()
        .unwrap();
    let reply = h.slash("U0HUMAN01", "me").await.remove(0);
    assert!(reply.contains("Plan: Claude Max."), "{reply}");
    assert!(
        reply.contains("Slack configuration token: registered, renewed automatically."),
        "{reply}"
    );
    h.store
        .mark_slack_config_token_broken(&row, OffsetDateTime::now_utc())
        .await
        .unwrap();
    let reply = h.slash("U0HUMAN01", "me").await.remove(0);
    assert!(reply.contains("Slack refused to renew it."), "{reply}");
}

#[tokio::test]
async fn logout_deletes_the_configuration_tokens() {
    let h = slack_harness().await;
    let alice = h.linked("U0HUMAN01").await;
    for team in [TEAM, "T0ELSEWHERE"] {
        h.store
            .put_slack_config_token(
                alice,
                &TeamId::new(team),
                &NewSlackConfigToken {
                    token: SecretString::from("t"),
                    refresh_token: SecretString::from("r"),
                    expires_at: OffsetDateTime::now_utc(),
                },
                OffsetDateTime::now_utc(),
            )
            .await
            .unwrap()
            .unwrap();
    }
    let reply = h.slash("U0HUMAN01", "logout").await.remove(0);
    assert!(
        reply.contains("I also deleted your Slack configuration token"),
        "{reply}"
    );
    assert_eq!(h.stored(alice).await, None);
    assert_eq!(h.store.delete_slack_config_tokens(alice).await.unwrap(), 0);
}

#[tokio::test]
async fn the_rotator_renews_each_token_before_it_expires() {
    let h = slack_harness().await;
    let alice = h.linked("U0HUMAN01").await;
    let start = OffsetDateTime::now_utc();
    h.store
        .put_slack_config_token(
            alice,
            &TeamId::new(TEAM),
            &NewSlackConfigToken {
                token: SecretString::from("xoxe.xoxp-1-T0"),
                refresh_token: SecretString::from("xoxe-1-R0"),
                expires_at: start + time::Duration::hours(1),
            },
            start,
        )
        .await
        .unwrap()
        .unwrap();
    let first_exp = (start + time::Duration::hours(12)).unix_timestamp();
    let second_exp = (start + time::Duration::hours(22)).unix_timestamp();
    mount_rotation(
        &h.slack,
        "xoxe-1-R0",
        "xoxe.xoxp-1-T1",
        "xoxe-1-R1",
        "U0HUMAN01",
        first_exp,
    )
    .await;
    mount_rotation(
        &h.slack,
        "xoxe-1-R1",
        "xoxe.xoxp-1-T2",
        "xoxe-1-R2",
        "U0HUMAN01",
        second_exp,
    )
    .await;
    let rotator = h.rotator();

    let pass = rotator.pass_at(clock(start)).await.unwrap();
    assert_eq!(
        pass,
        RotationPass {
            renewed: 1,
            ..RotationPass::default()
        }
    );
    assert_eq!(
        h.stored(alice).await,
        Some(("xoxe.xoxp-1-T1".to_owned(), "xoxe-1-R1".to_owned()))
    );

    let not_yet = start + time::Duration::hours(9);
    assert_eq!(
        rotator.pass_at(clock(not_yet)).await.unwrap(),
        RotationPass::default()
    );

    let due = start + time::Duration::hours(10) + time::Duration::minutes(1);
    let pass = rotator.pass_at(clock(due)).await.unwrap();
    assert_eq!(pass.renewed, 1);
    assert_eq!(
        h.stored(alice).await,
        Some(("xoxe.xoxp-1-T2".to_owned(), "xoxe-1-R2".to_owned()))
    );
    let rotations = h.calls("tooling.tokens.rotate").await;
    assert_eq!(rotations.len(), 2);
    assert!(String::from_utf8_lossy(&rotations[0].body).contains("xoxe-1-R0"));
    assert!(String::from_utf8_lossy(&rotations[1].body).contains("xoxe-1-R1"));
}

#[tokio::test]
async fn a_refused_renewal_breaks_the_token_and_dms_the_member_once() {
    let h = slack_harness().await;
    let logs = global_logs().tag();
    let alice = h.linked("U0HUMAN01").await;
    let start = OffsetDateTime::now_utc();
    h.store
        .put_slack_config_token(
            alice,
            &TeamId::new(TEAM),
            &NewSlackConfigToken {
                token: SecretString::from("xoxe.xoxp-1-SECRET-T0"),
                refresh_token: SecretString::from("xoxe-1-SECRET-R0"),
                expires_at: start + time::Duration::minutes(30),
            },
            start,
        )
        .await
        .unwrap()
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/api/tooling.tokens.rotate"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "invalid_refresh_token"})),
        )
        .expect(1)
        .mount(&h.slack)
        .await;
    let rotator = h.rotator();
    let pass = rotator.pass_at(clock(start)).await.unwrap();
    assert_eq!(
        pass,
        RotationPass {
            renewed: 0,
            broken: 1,
            notified: 1
        }
    );
    let notice = h
        .manager
        .manager_bot()
        .render(&broken_token_notice())
        .remove(0);
    assert_eq!(h.posts().await, [("D0DM00001".to_owned(), notice)]);
    let opened = h.calls("conversations.open").await;
    assert_eq!(String::from_utf8_lossy(&opened[0].body), "users=U0HUMAN01");
    let later = start + NOTICE_LEASE + time::Duration::hours(1);
    assert_eq!(
        rotator.pass_at(clock(later)).await.unwrap(),
        RotationPass::default()
    );
    assert_eq!(h.posts().await.len(), 1);
    logs.snapshot()
        .assert_has("Slack refused to renew a configuration token")
        .assert_has("sent the configuration token notice");
    global_logs().snapshot().assert_lacks("SECRET");
}

#[tokio::test]
async fn a_failed_renewal_is_tried_again_after_the_lease() {
    let h = slack_harness().await;
    let alice = h.linked("U0HUMAN01").await;
    let start = OffsetDateTime::now_utc();
    h.store
        .put_slack_config_token(
            alice,
            &TeamId::new(TEAM),
            &NewSlackConfigToken {
                token: SecretString::from("xoxe.xoxp-1-T0"),
                refresh_token: SecretString::from("xoxe-1-R0"),
                expires_at: start + time::Duration::minutes(30),
            },
            start,
        )
        .await
        .unwrap()
        .unwrap();
    Mock::given(method("POST"))
        .and(path("/api/tooling.tokens.rotate"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(1)
        .mount(&h.slack)
        .await;
    let rotator = h.rotator();
    assert_eq!(
        rotator.pass_at(clock(start)).await.unwrap(),
        RotationPass::default()
    );
    let soon = start + time::Duration::minutes(1);
    assert_eq!(
        rotator.pass_at(clock(soon)).await.unwrap(),
        RotationPass::default()
    );
    assert_eq!(h.calls("tooling.tokens.rotate").await.len(), 1);

    mount_rotation(
        &h.slack,
        "xoxe-1-R0",
        "xoxe.xoxp-1-T1",
        "xoxe-1-R1",
        "U0HUMAN01",
        in_hours(12),
    )
    .await;
    let after_lease = start + ROTATION_LEASE;
    assert_eq!(
        rotator.pass_at(clock(after_lease)).await.unwrap().renewed,
        1
    );
    assert!(h.posts().await.is_empty());
}

#[tokio::test]
async fn a_notice_nobody_can_send_is_tried_a_bounded_number_of_times() {
    let h = slack_harness().await;
    let alice = h.linked("U0HUMAN01").await;
    let start = OffsetDateTime::now_utc();
    let row = h
        .store
        .put_slack_config_token(
            alice,
            &TeamId::new(TEAM),
            &NewSlackConfigToken {
                token: SecretString::from("t"),
                refresh_token: SecretString::from("r"),
                expires_at: start,
            },
            start,
        )
        .await
        .unwrap()
        .unwrap();
    h.store
        .mark_slack_config_token_broken(&row, start)
        .await
        .unwrap();
    h.slack.reset().await;
    Mock::given(method("POST"))
        .and(path("/api/users.info"))
        .respond_with(home_member)
        .mount(&h.slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/conversations.open"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.slack)
        .await;
    let rotator = h.rotator();
    let mut now = start;
    for _ in 0..NOTICE_MAX_ATTEMPTS + 2 {
        assert_eq!(rotator.pass_at(clock(now)).await.unwrap().notified, 0);
        now += NOTICE_LEASE;
    }
    assert_eq!(
        h.calls("conversations.open").await.len(),
        usize::try_from(NOTICE_MAX_ATTEMPTS).unwrap()
    );
}

/// Collects what the Slack queue would hand on, through [`Inbound`], into a
/// running intake.
pub(super) struct Running {
    inbound: Sender<SlackInbound>,
    intake: tokio::task::JoinHandle<()>,
}

impl Running {
    pub(super) fn start(h: &SlackHarness) -> Self {
        let (intake, submitter) = CommandIntake::new(h.commands.clone());
        let inbound = Inbound::new(h.store.clone(), Some(identity()), submitter);
        Self {
            inbound: Sender::new(inbound),
            intake: tokio::spawn(intake.run()),
        }
    }

    pub(super) async fn send(&self, item: SlackInbound) {
        self.inbound.send(item).await.unwrap();
    }

    pub(super) async fn stop(self) {
        drop(self.inbound);
        tokio::time::timeout(Duration::from_secs(20), self.intake)
            .await
            .unwrap()
            .unwrap();
    }
}

pub(super) fn dm_event(sender: &str, text: &str) -> InboundEvent {
    let conv = ConvRef {
        surface: SurfaceKind::Slack,
        team: TeamId::new(TEAM),
        conversation: "D0DM00001".into(),
    };
    InboundEvent {
        event_id: "Ev0IM000001".to_owned(),
        binding: BindingRef::MANAGER_ID,
        sender: slack_key(sender),
        sender_is_bot: false,
        sender_bot_user: None,
        conv: conv.clone(),
        conv_kind: ConvKind::Dm,
        thread_root: None,
        message: MsgRef {
            conv,
            id: "1727697900.000500".into(),
        },
        text: text.to_owned(),
        mentions: Vec::new(),
        reply_to: None,
        files: Vec::new(),
        received_at: OffsetDateTime::now_utc(),
        outside: None,
    }
}

pub(super) fn slash(text: &str, response_url: SecretString) -> SlashCommand {
    SlashCommand {
        binding: BindingRef::MANAGER_ID,
        sender: slack_key("U0HUMAN01"),
        conv: slack_channel("C0CHAN001"),
        command: "/agent".to_owned(),
        text: text.to_owned(),
        response_url,
        trigger_id: None,
        received_at: OffsetDateTime::now_utc(),
    }
}

async fn wait_for<T>(mut probe: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = probe().await {
            return found;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_command_sent_as_a_manager_dm_is_answered_in_that_dm() {
    let h = slack_harness().await;
    let running = Running::start(&h);
    running
        .send(SlackInbound::Message(
            Box::new(dm_event("U0HUMAN01", "me")),
            InFlight::untracked(),
        ))
        .await;
    let (channel, text) = wait_for(async || h.posts().await.pop()).await;
    assert_eq!(channel, "D0DM00001");
    assert!(text.starts_with("Claude account: not linked."), "{text}");
    assert!(text.contains("Send `login` to link one."), "{text}");
    running.stop().await;
    assert_eq!(h.posts().await.len(), 1);
    assert!(h.calls("conversations.open").await.is_empty());
}

#[tokio::test]
async fn a_slash_command_is_answered_through_its_response_url() {
    let h = slack_harness().await;
    let running = Running::start(&h);
    let (response_url, hook) = h.response_url();
    running
        .send(SlackInbound::Command(slash("me", response_url)))
        .await;
    let reply = wait_for(async || {
        h.requests()
            .await
            .into_iter()
            .find(|request| request.url.path() == hook)
    })
    .await;
    let body = json_body(&reply);
    assert_eq!(body["response_type"], "ephemeral");
    assert!(
        body["text"]
            .as_str()
            .unwrap()
            .starts_with("Claude account: not linked. Send `/agent login`")
    );
    running.stop().await;
    assert!(h.posts().await.is_empty());
}

#[tokio::test]
async fn the_managers_own_and_other_bots_messages_are_not_commands() {
    let h = slack_harness().await;
    let running = Running::start(&h);
    let mut own = dm_event("U0MANAGER", "me");
    own.sender_is_bot = true;
    running
        .send(SlackInbound::Message(Box::new(own), InFlight::untracked()))
        .await;
    let mut unflagged_own = dm_event("U0MANAGER", "me");
    unflagged_own.sender_is_bot = false;
    running
        .send(SlackInbound::Message(
            Box::new(unflagged_own),
            InFlight::untracked(),
        ))
        .await;
    let mut bot = dm_event("U0BOT0001", "me");
    bot.sender_bot_user = Some(UserId::new("U0BOT0001"));
    running
        .send(SlackInbound::Message(Box::new(bot), InFlight::untracked()))
        .await;
    let mut channel = dm_event("U0HUMAN01", "me");
    channel.conv_kind = ConvKind::Channel;
    running
        .send(SlackInbound::Message(
            Box::new(channel),
            InFlight::untracked(),
        ))
        .await;
    let mut agent = dm_event("U0HUMAN01", "me");
    agent.binding = BindingId::new_v4();
    running
        .send(SlackInbound::Message(
            Box::new(agent),
            InFlight::untracked(),
        ))
        .await;
    running.stop().await;
    assert!(h.posts().await.is_empty());
}

#[tokio::test]
async fn a_deleted_user_loses_their_configuration_token() {
    let h = slack_harness().await;
    let grace = h.linked("U0HUMAN02").await;
    let alice = h.linked("U0HUMAN01").await;
    for member in [grace, alice] {
        h.store
            .put_slack_config_token(
                member,
                &TeamId::new(TEAM),
                &NewSlackConfigToken {
                    token: SecretString::from("t"),
                    refresh_token: SecretString::from("r"),
                    expires_at: OffsetDateTime::now_utc(),
                },
                OffsetDateTime::now_utc(),
            )
            .await
            .unwrap()
            .unwrap();
    }
    let envelope: Value = serde_json::from_str(testkit::slack::USER_CHANGE).unwrap();
    let event = |value: Value| SlackEvent {
        binding: BindingRef::MANAGER_ID,
        team: TeamId::new(TEAM),
        event_id: "Ev0USERCHG1".to_owned(),
        event_type: "user_change".to_owned(),
        event: value,
        received_at: OffsetDateTime::now_utc(),
    };
    let running = Running::start(&h);
    let mut active = envelope["event"].clone();
    active["user"]["deleted"] = json!(false);
    active["user"]["id"] = json!("U0HUMAN01");
    running.send(SlackInbound::Event(event(active))).await;
    running
        .send(SlackInbound::Event(event(envelope["event"].clone())))
        .await;
    running.stop().await;
    assert_eq!(h.stored(grace).await, None);
    assert!(h.stored(alice).await.is_some());
    assert!(h.posts().await.is_empty());
}

#[test]
fn only_agent_slash_commands_are_taken_and_their_text_is_decoded() {
    let url = SecretString::from("https://hooks.slack.com/commands/x");
    let (member, text, origin) =
        slash_command(slash("persona helper a &lt;b&gt; &amp;amp; c", url.clone())).unwrap();
    assert_eq!(member, slack_key("U0HUMAN01"));
    assert_eq!(text, "persona helper a &lt;b&gt; &amp;amp; c");
    assert_eq!(origin.decoded(&text), "persona helper a <b> &amp; c");
    let Origin::SlackSlash { conv, .. } = &origin else {
        panic!("{origin:?}");
    };
    assert_eq!(*conv, slack_channel("C0CHAN001"));
    assert_eq!(
        origin.conversation(&member),
        Some(slack_channel("C0CHAN001"))
    );
    let mut other = slash("me", url);
    other.command = "/other".to_owned();
    assert!(slash_command(other).is_none());
}

#[test]
fn a_manager_dm_is_parsed_whole_or_after_a_prefix() {
    let mut event = dm_event("U0HUMAN01", "persona helper &lt;b&gt;");
    let file = core_types::InFile {
        id: "F1".into(),
        name: "persona.md".into(),
        mime_type: None,
        size: Some(3),
        url: "https://files.slack.com/files-pri/T0TEAM-F1/download/persona.md".into(),
    };
    event.files = vec![file.clone()];
    let (member, text, origin, files) = dm_command(&event, &identity()).unwrap();
    assert_eq!(files, [file]);
    assert_eq!(member, slack_key("U0HUMAN01"));
    assert_eq!(text, "persona helper &lt;b&gt;");
    assert_eq!(origin.decoded(&text), "persona helper <b>");
    assert_eq!(
        format!("{origin:?}"),
        r#"SlackDm { channel: ConversationId("D0DM00001") }"#
    );
    assert!(origin.is_private());
    let (_, text, _, files) = dm_command(&dm_event("U0HUMAN01", "!agent me"), &identity()).unwrap();
    assert!(files.is_empty());
    assert_eq!(text, "me");
}

async fn slack_agent(h: &SlackHarness, owner: MemberId) -> store::Agent {
    let team = TeamId::new(TEAM);
    let store::AgentCreation::Created(agent, _) = h
        .store
        .create_agent(
            &store::NewAgent {
                owner,
                name: "helper",
                persona: "p",
                visibility: store::Visibility::Public,
                surface: SurfaceKind::Slack,
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
    agent
}

#[tokio::test]
async fn the_slack_inbound_passes_a_dms_files_to_the_intake() {
    let h = slack_harness().await;
    let alice = h.linked("U0HUMAN01").await;
    let agent = slack_agent(&h, alice).await;
    Mock::given(method("GET"))
        .and(path("/files-pri/T0TEAM001-F9/download/persona.md"))
        .and(wiremock::matchers::header(
            "authorization",
            format!("Bearer {BOT_TOKEN}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string("Via the DM.\n"))
        .mount(&h.slack)
        .await;
    let file = |size: u64| core_types::InFile {
        id: "F9".into(),
        name: "persona.md".into(),
        mime_type: None,
        size: Some(size),
        url: format!(
            "{}/files-pri/T0TEAM001-F9/download/persona.md",
            h.slack.uri()
        ),
    };
    let running = Running::start(&h);
    let mut too_big = dm_event("U0HUMAN01", "persona helper");
    too_big.files = vec![file(64 * 1024 + 1)];
    running
        .send(SlackInbound::Message(
            Box::new(too_big),
            InFlight::untracked(),
        ))
        .await;
    let mut event = dm_event("U0HUMAN01", "persona helper");
    event.files = vec![file(12)];
    running
        .send(SlackInbound::Message(
            Box::new(event),
            InFlight::untracked(),
        ))
        .await;
    running.stop().await;
    let row = h.store.agent(agent.id).await.unwrap().unwrap();
    assert_eq!(row.persona, "Via the DM.\n");
    let posts: Vec<String> = h.posts().await.into_iter().map(|(_, text)| text).collect();
    assert_eq!(posts.len(), 2, "{posts:?}");
    assert_eq!(posts[0], "That file is over the 64 KB limit.");
    assert!(
        posts[1].starts_with("Replaced `helper`'s persona."),
        "{posts:?}"
    );
}

#[tokio::test]
async fn a_persona_sent_in_a_manager_dm_arrives_decoded() {
    let h = slack_harness().await;
    let alice = h.linked("U0HUMAN01").await;
    let agent = slack_agent(&h, alice).await;
    let running = Running::start(&h);
    running
        .send(SlackInbound::Message(
            Box::new(dm_event("U0HUMAN01", "persona helper You &amp; me &lt;3")),
            InFlight::untracked(),
        ))
        .await;
    running.stop().await;
    let row = h.store.agent(agent.id).await.unwrap().unwrap();
    assert_eq!(row.persona, "You & me <3");
}

#[test]
fn only_a_deleted_user_in_a_user_change_has_left() {
    let envelope: Value = serde_json::from_str(testkit::slack::USER_CHANGE).unwrap();
    let event = |event_type: &str, value: Value| SlackEvent {
        binding: BindingRef::MANAGER_ID,
        team: TeamId::new(TEAM),
        event_id: "Ev1".to_owned(),
        event_type: event_type.to_owned(),
        event: value,
        received_at: OffsetDateTime::now_utc(),
    };
    let left_of = |value: Value| member_who_left(&event("user_change", value), None);
    let left = envelope["event"].clone();
    assert_eq!(left_of(left.clone()), Some(slack_key("U0HUMAN02")));
    assert_eq!(
        member_who_left(&event("team_join", left.clone()), None),
        None
    );
    let mut active = left.clone();
    active["user"]["deleted"] = json!(false);
    assert_eq!(left_of(active), None);
    let mut nameless = left.clone();
    nameless["user"]["id"] = json!("");
    assert_eq!(left_of(nameless), None);
    assert_eq!(left_of(json!({})), None);
    for team in [
        json!("T0THEIRS1"),
        json!("E0HOMEORG"),
        json!(null),
        json!(7),
    ] {
        let mut elsewhere = left.clone();
        elsewhere["user"]["team_id"] = team.clone();
        assert_eq!(left_of(elsewhere), None, "deactivated in {team}, not here");
    }
    let mut teamless = left.clone();
    teamless["user"].as_object_mut().unwrap().remove("team_id");
    assert_eq!(left_of(teamless), None);
}

#[test]
fn a_grid_member_of_the_workspace_homed_in_a_sibling_has_left() {
    let envelope: Value = serde_json::from_str(testkit::slack::USER_CHANGE).unwrap();
    let org = TeamId::new("E0HOMEORG");
    let left_of = |grid: Value, home_org: Option<&TeamId>| {
        let mut value = envelope["event"].clone();
        value["user"]["team_id"] = json!("T0SIBLING");
        value["user"]["enterprise_user"] = grid;
        let event = SlackEvent {
            binding: BindingRef::MANAGER_ID,
            team: TeamId::new(TEAM),
            event_id: "Ev1".to_owned(),
            event_type: "user_change".to_owned(),
            event: value,
            received_at: OffsetDateTime::now_utc(),
        };
        member_who_left(&event, home_org)
    };
    let member = json!({"enterprise_id": "E0HOMEORG", "teams": ["T0SIBLING", TEAM]});
    assert_eq!(
        left_of(member.clone(), Some(&org)),
        Some(slack_key("U0HUMAN02"))
    );
    assert_eq!(left_of(member, None), None, "no organization at home");
    let sibling_only = json!({"enterprise_id": "E0HOMEORG", "teams": ["T0SIBLING"]});
    assert_eq!(left_of(sibling_only, Some(&org)), None);
    let other_org = json!({"enterprise_id": "E0THEIRS1", "teams": ["T0SIBLING", TEAM]});
    assert_eq!(left_of(other_org, Some(&org)), None);
}

#[tokio::test]
async fn a_token_that_fails_to_decrypt_does_not_hold_up_the_others() {
    let (old_key, url, _dir) = file_store().await;
    let sealer = || Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap();
    let start = OffsetDateTime::now_utc();
    let token = |refresh: &str, minutes: i64| NewSlackConfigToken {
        token: SecretString::from("xoxe.xoxp-1-T0"),
        refresh_token: SecretString::from(refresh),
        expires_at: start + time::Duration::minutes(minutes),
    };
    let bob = old_key
        .ensure_member(&slack_key("U0HUMAN02"), "bob", start)
        .await
        .unwrap();
    link(&old_key, bob).await;
    old_key
        .put_slack_config_token(bob, &TeamId::new(TEAM), &token("xoxe-1-BOB", 10), start)
        .await
        .unwrap()
        .unwrap();
    old_key.close().await;

    let h = slack_harness().await;
    let store = Store::open(&url, sealer()).await.unwrap();
    let alice = store
        .ensure_member(&slack_key("U0HUMAN01"), "alice", start)
        .await
        .unwrap();
    link(&store, alice).await;
    store
        .put_slack_config_token(alice, &TeamId::new(TEAM), &token("xoxe-1-R0", 20), start)
        .await
        .unwrap()
        .unwrap();
    mount_rotation(
        &h.slack,
        "xoxe-1-R0",
        "xoxe.xoxp-1-T1",
        "xoxe-1-R1",
        "U0HUMAN01",
        in_hours(12),
    )
    .await;
    let rotator = ConfigTokenRotator::new(
        store.clone(),
        h.manager.client().clone(),
        h.commands.replies().clone(),
    );
    let pass = rotator.pass_at(clock(start)).await.unwrap();
    assert_eq!(pass.renewed, 1);
    assert_eq!(
        store
            .slack_config_token(alice, &TeamId::new(TEAM))
            .await
            .unwrap()
            .unwrap()
            .refresh_token
            .expose_secret(),
        "xoxe-1-R1"
    );
}

#[tokio::test]
async fn me_says_when_a_token_expired_because_renewing_it_keeps_failing() {
    let h = slack_harness().await;
    let alice = h.linked("U0HUMAN01").await;
    h.store
        .put_slack_config_token(
            alice,
            &TeamId::new(TEAM),
            &NewSlackConfigToken {
                token: SecretString::from("t"),
                refresh_token: SecretString::from("r"),
                expires_at: OffsetDateTime::now_utc() - time::Duration::minutes(1),
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap()
        .unwrap();
    let reply = h.slash("U0HUMAN01", "me").await.remove(0);
    assert!(
        reply.contains(
            "Slack configuration token: expired, because renewing it keeps failing. I keep \
             trying; if this lasts, send a new one with `/agent slack-token"
        ),
        "{reply}"
    );
    assert!(!reply.contains("renewed automatically"), "{reply}");
}

#[tokio::test]
async fn requests_from_another_workspace_are_dropped() {
    let h = slack_harness().await;
    let other = TeamId::new("T0OTHER01");
    let outsider = MemberKey {
        surface: SurfaceKind::Slack,
        team: other.clone(),
        user: UserId::new("U0HUMAN02"),
    };
    let grace_there = h.linked_as(&outsider).await;
    let grace_here = h.linked("U0HUMAN02").await;
    for (member, team) in [
        (grace_there, other.clone()),
        (grace_here, TeamId::new(TEAM)),
    ] {
        h.store
            .put_slack_config_token(
                member,
                &team,
                &NewSlackConfigToken {
                    token: SecretString::from("t"),
                    refresh_token: SecretString::from("r"),
                    expires_at: OffsetDateTime::now_utc() + time::Duration::hours(12),
                },
                OffsetDateTime::now_utc(),
            )
            .await
            .unwrap()
            .unwrap();
    }
    let running = Running::start(&h);

    let (response_url, _) = h.response_url();
    let mut command = slash(
        &format!("slack-token {GIVEN_TOKEN} {GIVEN_REFRESH}"),
        response_url,
    );
    command.sender = outsider.clone();
    command.conv.team = other.clone();
    running.send(SlackInbound::Command(command)).await;

    let mut dm = dm_event("U0HUMAN02", "me");
    dm.sender = outsider.clone();
    dm.conv.team = other.clone();
    dm.message.conv.team = other.clone();
    running
        .send(SlackInbound::Message(Box::new(dm), InFlight::untracked()))
        .await;

    let envelope: Value = serde_json::from_str(testkit::slack::USER_CHANGE).unwrap();
    running
        .send(SlackInbound::Event(SlackEvent {
            binding: BindingRef::MANAGER_ID,
            team: other.clone(),
            event_id: "Ev0USERCHG1".to_owned(),
            event_type: "user_change".to_owned(),
            event: envelope["event"].clone(),
            received_at: OffsetDateTime::now_utc(),
        }))
        .await;
    running.stop().await;

    assert!(h.requests().await.is_empty(), "nothing reached Slack");
    assert!(
        h.store
            .slack_config_token_status(grace_there, &other)
            .await
            .unwrap()
            .is_some()
    );
    assert!(h.stored(grace_here).await.is_some());
}

/// A store in a new SQLite file, its URL, and the file's directory.
pub(super) async fn file_store() -> (Store, String, TempDir) {
    let dir = TempDir::new("agentd-slack");
    let url = dir.db_url();
    let store = Store::open(
        &url,
        Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap(),
    )
    .await
    .unwrap();
    (store, url, dir)
}

/// Runs `statements` on the SQLite database at `url`.
pub(super) async fn sql(url: &str, statements: &str) {
    use sqlx::Connection as _;
    let mut db = sqlx::SqliteConnection::connect(url).await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(statements.to_owned()))
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
}

const FAIL_TOKEN_WRITES: &str = "\
    CREATE TABLE token_write_failures (remaining INTEGER NOT NULL); \
    INSERT INTO token_write_failures VALUES (0); \
    CREATE TRIGGER fail_token_insert BEFORE INSERT ON slack_config_tokens \
    WHEN (SELECT remaining FROM token_write_failures) > 0 BEGIN \
    UPDATE token_write_failures SET remaining = remaining - 1; \
    SELECT RAISE(FAIL, 'injected write failure'); END; \
    CREATE TRIGGER fail_token_update BEFORE UPDATE OF token_enc ON slack_config_tokens \
    WHEN (SELECT remaining FROM token_write_failures) > 0 BEGIN \
    UPDATE token_write_failures SET remaining = remaining - 1; \
    SELECT RAISE(FAIL, 'injected write failure'); END;";

/// Links `member`'s Claude account in `store`, as `login` does.
pub(super) async fn link(store: &Store, member: MemberId) {
    store
        .put_claude_link(
            member,
            &NewClaudeLink {
                access_token: SecretString::from("access"),
                refresh_token: SecretString::from("refresh"),
                expires_at: OffsetDateTime::now_utc() + time::Duration::hours(8),
                plan: Some("claude_max".to_owned()),
                rate_limit_tier: None,
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
}

/// Makes the next `failures` writes of configuration tokens to the store
/// at `url` fail: inserts, and updates that set the tokens.
async fn fail_token_writes(url: &str, failures: i64) {
    use sqlx::Connection as _;
    let mut db = sqlx::SqliteConnection::connect(url).await.unwrap();
    sqlx::raw_sql(FAIL_TOKEN_WRITES)
        .execute(&mut db)
        .await
        .unwrap();
    sqlx::query("UPDATE token_write_failures SET remaining = ?")
        .bind(failures)
        .execute(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
}

/// How many of the failures [`fail_token_writes`] set up are left.
async fn failures_left(url: &str) -> i64 {
    use sqlx::Connection as _;
    let mut db = sqlx::SqliteConnection::connect(url).await.unwrap();
    let left = sqlx::query_scalar("SELECT remaining FROM token_write_failures")
        .fetch_one(&mut db)
        .await
        .unwrap();
    db.close().await.unwrap();
    left
}

#[tokio::test]
async fn a_renewed_pair_is_stored_although_the_first_writes_fail() {
    let (store, url, _dir) = file_store().await;
    let h = slack_harness_on(store).await;
    let alice = h.linked("U0HUMAN01").await;
    let start = OffsetDateTime::now_utc();
    h.store
        .put_slack_config_token(
            alice,
            &TeamId::new(TEAM),
            &NewSlackConfigToken {
                token: SecretString::from("xoxe.xoxp-1-T0"),
                refresh_token: SecretString::from("xoxe-1-R0"),
                expires_at: start + time::Duration::minutes(30),
            },
            start,
        )
        .await
        .unwrap()
        .unwrap();
    mount_rotation(
        &h.slack,
        "xoxe-1-R0",
        "xoxe.xoxp-1-T1",
        "xoxe-1-R1",
        "U0HUMAN01",
        in_hours(12),
    )
    .await;
    fail_token_writes(&url, 2).await;

    assert_eq!(h.rotator().pass_at(clock(start)).await.unwrap().renewed, 1);
    assert_eq!(failures_left(&url).await, 0);
    assert_eq!(
        h.stored(alice).await,
        Some(("xoxe.xoxp-1-T1".to_owned(), "xoxe-1-R1".to_owned()))
    );
}

#[tokio::test]
async fn a_checked_pair_is_stored_although_the_first_write_fails() {
    let (store, url, _dir) = file_store().await;
    let h = slack_harness_on(store).await;
    let alice = h.linked("U0HUMAN01").await;
    mount_rotation(
        &h.slack,
        GIVEN_REFRESH,
        "xoxe.xoxp-1-NEW-SECRET-token",
        "xoxe-1-NEW-SECRET-refresh",
        "U0HUMAN01",
        in_hours(12),
    )
    .await;
    fail_token_writes(&url, 1).await;

    let replies = h
        .slash(
            "U0HUMAN01",
            &format!("slack-token {GIVEN_TOKEN} {GIVEN_REFRESH}"),
        )
        .await;
    assert!(
        replies[0].starts_with("Your Slack configuration token is registered."),
        "{replies:?}"
    );
    assert_eq!(failures_left(&url).await, 0);
    assert_eq!(
        h.stored(alice).await,
        Some((
            "xoxe.xoxp-1-NEW-SECRET-token".to_owned(),
            "xoxe-1-NEW-SECRET-refresh".to_owned()
        ))
    );
}

#[tokio::test]
async fn a_checked_pair_the_store_keeps_refusing_is_reported_lost() {
    let (store, url, _dir) = file_store().await;
    let h = slack_harness_on(store).await;
    let logs = global_logs().tag();
    let alice = h.linked("U0HUMAN01").await;
    mount_rotation(
        &h.slack,
        GIVEN_REFRESH,
        "xoxe.xoxp-1-NEW-SECRET-token",
        "xoxe-1-NEW-SECRET-refresh",
        "U0HUMAN01",
        in_hours(12),
    )
    .await;
    fail_token_writes(&url, 10).await;

    let replies = h
        .slash(
            "U0HUMAN01",
            &format!("slack-token {GIVEN_TOKEN} {GIVEN_REFRESH}"),
        )
        .await;
    assert!(
        replies[0].starts_with(
            "I couldn't save that configuration token, and checking it used up its refresh token."
        ),
        "{replies:?}"
    );
    assert_eq!(failures_left(&url).await, 10 - i64::from(STORE_ATTEMPTS));
    assert_eq!(h.stored(alice).await, None);
    logs.snapshot()
        .assert_has("couldn't store a checked configuration token");
    global_logs().snapshot().assert_lacks("SECRET");
}

#[tokio::test]
async fn files_in_the_manager_dm_feed_skill_add_and_persona() {
    let h = slack_harness().await;
    let data = &h.data;
    let commands = h.commands.clone();
    let alice = h.linked("U0HUMAN01").await;
    let team = TeamId::new(TEAM);
    let store::AgentCreation::Created(agent, _) = h
        .store
        .create_agent(
            &store::NewAgent {
                owner: alice,
                name: "helper",
                persona: "p",
                visibility: store::Visibility::Public,
                surface: SurfaceKind::Slack,
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
    let skill = "---\nname: notes\ndescription: Keep notes.\n---\nWrite them down.\n";
    Mock::given(method("GET"))
        .and(path("/files-pri/T0TEAM001-F1/download/SKILL.md"))
        .and(wiremock::matchers::header(
            "authorization",
            format!("Bearer {BOT_TOKEN}").as_str(),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(skill))
        .mount(&h.slack)
        .await;
    Mock::given(method("GET"))
        .and(path("/files-pri/T0TEAM001-F2/download/persona.md"))
        .respond_with(ResponseTemplate::new(200).set_body_string("You are brief.\n"))
        .mount(&h.slack)
        .await;
    let file = |id: &str, name: &str, size: usize| core_types::InFile {
        id: id.into(),
        name: name.into(),
        mime_type: None,
        size: Some(u64::try_from(size).unwrap()),
        url: format!("{}/files-pri/T0TEAM001-{id}/download/{name}", h.slack.uri()),
    };
    let dm = Origin::SlackDm {
        channel: "D0DM00001".into(),
    };
    let (reply, _) = commands
        .run(
            &slack_key("U0HUMAN01"),
            commands::parse("skill add helper").unwrap(),
            &dm,
            &[file("F1", "SKILL.md", skill.len())],
            "",
        )
        .await;
    assert_eq!(
        reply,
        "Added the skill `notes` to `helper`. Its files are in its sandboxes now, though a \
         conversation already running may not use it until it next starts."
    );
    let installed = runner::skills_dir(data.path(), agent.id).join("notes/SKILL.md");
    assert_eq!(std::fs::read_to_string(installed).unwrap(), skill);

    let too_big = file("F3", "SKILL.md", 10 * 1024 * 1024);
    let (reply, _) = commands
        .run(
            &slack_key("U0HUMAN01"),
            commands::parse("skill add helper").unwrap(),
            &dm,
            &[too_big],
            "",
        )
        .await;
    assert_eq!(reply, "That file is over the 256 KB limit.");

    let (reply, _) = commands
        .run(
            &slack_key("U0HUMAN01"),
            commands::parse("persona helper").unwrap(),
            &dm,
            &[file("F2", "persona.md", 15)],
            "",
        )
        .await;
    assert!(reply.starts_with("Replaced `helper`'s persona."), "{reply}");
    let row = h.store.agent(agent.id).await.unwrap().unwrap();
    assert_eq!(row.persona, "You are brief.\n");

    let (response_url, _) = h.response_url();
    let (reply, _) = commands
        .run(
            &slack_key("U0HUMAN01"),
            commands::parse("skill add helper").unwrap(),
            &Origin::SlackSlash {
                response_url,
                conv: slack_channel("C0CHAN001"),
            },
            &[file("F1", "SKILL.md", skill.len())],
            "",
        )
        .await;
    assert!(
        reply.starts_with("Give the skill's https:// Git URL"),
        "a slash command carries no files: {reply}"
    );
}

#[tokio::test]
async fn slack_session_commands_link_threads_and_reset_the_slash_commands_channel() {
    let h = slack_harness().await;
    let alice = h.linked("U0HUMAN01").await;
    let team = TeamId::new(TEAM);
    let store::AgentCreation::Created(agent, _) = h
        .store
        .create_agent(
            &store::NewAgent {
                owner: alice,
                name: "helper",
                persona: "p",
                visibility: store::Visibility::Public,
                surface: SurfaceKind::Slack,
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
    let mut sessions = Vec::new();
    for (channel, root) in [
        ("C0CHAN001", "1727700000.000100"),
        ("C0OTHER01", "1727700001.000200"),
    ] {
        let thread = core_types::ThreadKey {
            conv: slack_channel(channel),
            root: Some(root.into()),
        };
        let scope =
            core_types::ScopeKey::for_conversation(ConvKind::Channel, slack_channel(channel));
        let session = h
            .store
            .session_for_thread(agent.id, &thread, &scope, OffsetDateTime::now_utc())
            .await
            .unwrap()
            .session;
        h.store
            .record_session_turn(session.id, true, OffsetDateTime::now_utc())
            .await
            .unwrap();
        sessions.push(session.id);
    }
    for thread in [
        core_types::ThreadKey {
            conv: slack_channel("D0BOBSDM1"),
            root: None,
        },
        core_types::ThreadKey {
            conv: slack_channel("C0CHAN001"),
            root: Some("1727700002.000300".into()),
        },
    ] {
        let task = h
            .store
            .create_private_session(
                agent.id,
                core_types::ConsentId::new_v4(),
                &thread,
                OffsetDateTime::now_utc(),
            )
            .await
            .unwrap();
        h.store
            .record_session_turn(task.id, true, OffsetDateTime::now_utc())
            .await
            .unwrap();
    }

    let listed = h.slash("U0HUMAN01", "sessions helper").await.remove(0);
    for link in [
        "https://app.slack.com/client/T0TEAM001/C0CHAN001/thread/C0CHAN001-1727700000.000100",
        "https://app.slack.com/client/T0TEAM001/C0OTHER01/thread/C0OTHER01-1727700001.000200",
    ] {
        assert!(listed.contains(link), "{listed}");
    }
    assert!(listed.starts_with("`helper`'s 4 sessions"), "{listed}");
    let tasks: Vec<&str> = listed
        .lines()
        .filter(|line| line.contains("A private task"))
        .collect();
    assert_eq!(tasks.len(), 2, "{listed}");
    for task in tasks {
        assert!(
            !task.contains("https://"),
            "a private task has no link: {task}"
        );
    }
    assert!(!listed.contains("D0BOBSDM1"), "{listed}");
    assert!(
        listed.ends_with(
            "Start them all over with `/agent reset helper`, or one conversation's with \
             `/agent reset helper here` there."
        ),
        "{listed}"
    );

    let (in_dm, _) = h
        .commands
        .run(
            &slack_key("U0HUMAN01"),
            commands::parse("reset helper here").unwrap(),
            &Origin::SlackDm {
                channel: "D0DM00001".into(),
            },
            &[],
            "",
        )
        .await;
    assert_eq!(
        in_dm,
        "`reset helper here` resets the conversation it is sent in, and no agent answers in \
         this one. Send `/agent reset helper here` in the conversation to reset, or \
         `reset helper` here to reset them all."
    );
    let slashed_in_dm = h
        .slash_in(
            "U0HUMAN01",
            &slack_channel("D0DM00001"),
            "reset helper here",
        )
        .await;
    assert_eq!(
        slashed_in_dm,
        [
            "`reset helper here` resets the conversation it is sent in, and no agent answers in \
          this one. Send `/agent reset helper here` in the conversation to reset, or \
          `/agent reset helper` here to reset them all."
        ]
    );
    let in_agents_dm = h
        .slash_in(
            "U0HUMAN01",
            &slack_channel("D0AGENTS1"),
            "reset helper here",
        )
        .await;
    assert_eq!(in_agents_dm, ["`helper` has no session here to reset."]);

    let replies = h
        .slash_in(
            "U0HUMAN01",
            &slack_channel("C0CHAN001"),
            "reset helper here",
        )
        .await;
    assert_eq!(
        replies,
        [
            "Resetting `helper`'s session here: the next message in it starts a new \
          conversation. If it is running a turn, it resets once that turn ends. If it can't be \
          reset, I'll tell you in a direct message."
        ]
    );
    let reset_at = async |id| h.store.session(id).await.unwrap().unwrap().reset_at;
    assert!(reset_at(sessions[0]).await.is_some());
    assert!(reset_at(sessions[1]).await.is_none());
}

#[tokio::test]
async fn a_slack_channel_token_is_a_rule_shown_by_its_name() {
    let h = slack_harness().await;
    let alice = h.linked("U0HUMAN01").await;
    let team = TeamId::new(TEAM);
    h.store
        .create_agent(
            &store::NewAgent {
                owner: alice,
                name: "helper",
                persona: "p",
                visibility: store::Visibility::Public,
                surface: SurfaceKind::Slack,
                team: &team,
            },
            10,
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
    assert_eq!(
        h.slash("U0HUMAN01", "allow helper <#C0CHAN002|general>")
            .await,
        ["Only you and `#general` may use `helper`."]
    );
    assert_eq!(
        h.slash("U0HUMAN01", "deny helper <#C0CHAN003>").await,
        ["Only you and `#general` may use `helper`, except `#C0CHAN003`."]
    );
}

/// A click by `user` on the consent card button `action` of consent
/// `value`, answered through `response_url`, from the fixture's payload.
fn click(user: &str, action: &str, value: &str, response_url: SecretString) -> Interaction {
    let mut payload: Value = serde_json::from_str(testkit::slack::BLOCK_ACTIONS).unwrap();
    payload["user"]["id"] = json!(user);
    payload["actions"][0]["action_id"] = json!(action);
    payload["actions"][0]["value"] = json!(value);
    let Value::Object(payload) = payload else {
        unreachable!()
    };
    Interaction {
        binding: BindingRef::MANAGER_ID,
        kind: "block_actions".to_owned(),
        sender: Some(slack_key(user)),
        sender_team: Some(TeamId::new(TEAM)),
        response_url: Some(response_url),
        payload,
        received_at: OffsetDateTime::now_utc(),
    }
}

/// A consent of `helper`, owned by U0OWNER, asked for by U0BOB, whose card
/// was sent and which the owner then approved.
async fn approved_with_a_card(h: &SlackHarness) -> core_types::ConsentId {
    let owner = h.linked("U0OWNER").await;
    let bob = h.linked("U0BOB").await;
    let team = TeamId::new(TEAM);
    let store::AgentCreation::Created(agent, _) = h
        .store
        .create_agent(
            &store::NewAgent {
                owner,
                name: "helper",
                persona: "p",
                visibility: store::Visibility::Public,
                surface: SurfaceKind::Slack,
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
    let id = core_types::ConsentId::new_v4();
    let now = OffsetDateTime::now_utc();
    h.store
        .create_consent(
            &store::NewConsent {
                id,
                agent: agent.id,
                requester: &core_types::Requester {
                    member: Some(bob),
                    key: slack_key("U0BOB"),
                    outside: None,
                },
                hop: core_types::Hop::ZERO,
                task: "Read my notes",
                attachments_json: "[]",
                thread: &core_types::ThreadKey {
                    conv: slack_channel("C0CHAN001"),
                    root: Some("1727697600.000100".into()),
                },
                origin_session: core_types::SessionId::new_v4(),
                expires_at: now + time::Duration::hours(1),
                approved_by_owner: None,
            },
            store::OpenLimits {
                per_requester: 1,
                per_agent: 1,
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        h.consents.send_cards(h.commands.replies()).await.unwrap(),
        1
    );
    h.store
        .decide_consent(id, true, &slack_key("U0OWNER"), now)
        .await
        .unwrap()
        .unwrap();
    id
}

#[tokio::test]
async fn a_card_update_that_fails_on_the_way_is_tried_again() {
    let h = slack_harness().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&h.slack)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(ok(json!({"ts": "1727700000.000100"})))
        .mount(&h.slack)
        .await;
    approved_with_a_card(&h).await;
    let close = async || h.consents.close_cards(h.commands.replies()).await.unwrap();
    assert_eq!(close().await, 0, "the first update fails");
    assert_eq!(close().await, 1, "and the next pass closes the card");
    assert_eq!(close().await, 0, "once");
    assert_eq!(h.calls("chat.update").await.len(), 2);
}

#[tokio::test]
async fn a_claim_that_cannot_be_released_does_not_stop_the_pass() {
    use sqlx::Connection as _;
    let (store, url, _dir) = file_store().await;
    let h = slack_harness_on(store).await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.slack)
        .await;
    approved_with_a_card(&h).await;
    let mut db = sqlx::SqliteConnection::connect(&url).await.unwrap();
    sqlx::raw_sql(
        "CREATE TRIGGER fail_card_release BEFORE UPDATE OF card_closed_at ON consents \
         WHEN NEW.card_closed_at IS NULL BEGIN SELECT RAISE(FAIL, 'injected write failure'); END;",
    )
    .execute(&mut db)
    .await
    .unwrap();
    db.close().await.unwrap();
    for _ in 0..2 {
        assert_eq!(
            h.consents.close_cards(h.commands.replies()).await.unwrap(),
            0
        );
    }
    assert_eq!(
        h.calls("chat.update").await.len(),
        1,
        "the claim stays held"
    );
}

#[tokio::test]
async fn a_card_the_platform_refuses_to_update_is_not_tried_again() {
    let h = slack_harness().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"ok": false, "error": "cant_update_message"})),
        )
        .mount(&h.slack)
        .await;
    approved_with_a_card(&h).await;
    for _ in 0..2 {
        assert_eq!(
            h.consents.close_cards(h.commands.replies()).await.unwrap(),
            0
        );
    }
    assert_eq!(h.calls("chat.update").await.len(), 1);
}

#[test]
fn only_a_consent_cards_buttons_are_commands() {
    let (url, _) = (SecretString::from("https://hooks.slack.com/actions/x"), ());
    let id = core_types::ConsentId::new_v4();
    let (member, text, origin) = consent_action(click(
        "U0OWNER",
        crate::consents::card::DECLINE_ACTION,
        &id.to_string(),
        url.clone(),
    ))
    .unwrap();
    assert_eq!(member, slack_key("U0OWNER"));
    assert_eq!(text, format!("decline {id}"));
    assert!(matches!(
        origin,
        Origin::SlackSlash { ref conv, .. } if conv.conversation.as_str() == "D0MGRDM01"
    ));
    let mut other = click("U0OWNER", "something_else", &id.to_string(), url.clone());
    assert!(consent_action(other).is_none());
    other = click("U0OWNER", "consent_approve", "not-an-id", url.clone());
    assert!(consent_action(other).is_none());
    other = click("U0OWNER", "consent_approve", &id.to_string(), url.clone());
    other.kind = "view_submission".to_owned();
    assert!(consent_action(other).is_none());
    other = click("U0OWNER", "consent_approve", &id.to_string(), url.clone());
    other.payload["actions"][0]["block_id"] = json!("elsewhere");
    assert!(consent_action(other).is_none());
    other = click("U0OWNER", "consent_approve", &id.to_string(), url.clone());
    other.sender = None;
    assert!(consent_action(other).is_none());
    other = click("U0OWNER", "consent_approve", &id.to_string(), url);
    other.response_url = None;
    assert!(consent_action(other).is_none());
}

#[tokio::test]
async fn only_owner_can_decide() {
    let h = slack_harness().await;
    Mock::given(method("POST"))
        .and(path("/api/chat.update"))
        .respond_with(ok(json!({"ts": "1727700000.000100"})))
        .mount(&h.slack)
        .await;
    let owner = h.linked("U0OWNER").await;
    let bob = h.linked("U0BOB").await;
    let team = TeamId::new(TEAM);
    let store::AgentCreation::Created(agent, _) = h
        .store
        .create_agent(
            &store::NewAgent {
                owner,
                name: "helper",
                persona: "p",
                visibility: store::Visibility::Public,
                surface: SurfaceKind::Slack,
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
    let id = core_types::ConsentId::new_v4();
    let now = OffsetDateTime::now_utc();
    h.store
        .create_consent(
            &store::NewConsent {
                id,
                agent: agent.id,
                requester: &core_types::Requester {
                    member: Some(bob),
                    key: slack_key("U0BOB"),
                    outside: None,
                },
                hop: core_types::Hop::ZERO,
                task: "Read my *notes* <!channel>",
                attachments_json: "[]",
                thread: &core_types::ThreadKey {
                    conv: slack_channel("C0CHAN001"),
                    root: Some("1727697600.000100".into()),
                },
                origin_session: core_types::SessionId::new_v4(),
                expires_at: now + time::Duration::hours(1),
                approved_by_owner: None,
            },
            store::OpenLimits {
                per_requester: 1,
                per_agent: 1,
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();

    assert_eq!(
        h.consents.send_cards(h.commands.replies()).await.unwrap(),
        1
    );
    assert_eq!(
        h.consents.send_cards(h.commands.replies()).await.unwrap(),
        0
    );
    let posted = h.calls("chat.postMessage").await;
    assert_eq!(posted.len(), 1);
    let card = json_body(&posted[0]);
    assert_eq!(
        card["channel"], "D0DM00001",
        "the owner's DM with the manager app"
    );
    assert_eq!(
        card["blocks"][2]["elements"][0]["elements"][0],
        json!({"type": "text", "text": "Read my *notes* <!channel>"})
    );
    assert!(
        card["blocks"][0]["text"]["text"]
            .as_str()
            .unwrap()
            .contains("<@U0BOB>")
    );
    let actions = card["blocks"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(actions["elements"][0]["value"], id.to_string());
    let opened = h.calls("conversations.open").await;
    assert!(String::from_utf8_lossy(&opened[0].body).contains("U0OWNER"));
    let recorded = h.store.consent(id).await.unwrap().unwrap().card.unwrap();
    assert_eq!(recorded.conv.conversation.as_str(), "D0DM00001");
    assert_eq!(recorded.id.as_str(), "1727700000.000100");

    let running = Running::start(&h);
    let answer = async |user: &str, action: &str, value: &str| {
        let (url, hook) = h.response_url();
        running
            .send(SlackInbound::Interaction(click(user, action, value, url)))
            .await;
        wait_for(async || {
            h.requests()
                .await
                .into_iter()
                .find(|request| request.url.path() == hook)
                .map(|request| json_body(&request)["text"].as_str().unwrap().to_owned())
        })
        .await
    };
    for user in ["U0BOB", "U0EVE"] {
        let reply = answer(user, "consent_approve", &id.to_string()).await;
        assert!(reply.contains("is waiting for you"), "{user}: {reply}");
        let reply = answer(user, "consent_decline", &id.to_string()).await;
        assert!(reply.contains("only the owner"), "{user}: {reply}");
    }
    h.commands
        .handle_text(
            &slack_key("U0BOB"),
            &format!("approve {id}"),
            &Origin::SlackDm {
                channel: "D0DM00002".into(),
            },
            &[],
        )
        .await;
    assert_eq!(
        h.store.consent(id).await.unwrap().unwrap().state,
        store::ConsentState::Pending,
        "nobody but the owner decided"
    );
    let other = core_types::ConsentId::new_v4().to_string();
    let reply = answer("U0OWNER", "consent_approve", &other).await;
    assert!(reply.contains("is waiting for you"), "{reply}");

    let reply = answer("U0OWNER", "consent_approve", &id.to_string()).await;
    assert!(reply.starts_with("Approved private task"), "{reply}");
    let decided = h.store.consent(id).await.unwrap().unwrap();
    assert_eq!(decided.state, store::ConsentState::Approved);
    assert_eq!(decided.decided_by, Some(slack_key("U0OWNER")));
    let reply = answer("U0OWNER", "consent_decline", &id.to_string()).await;
    assert!(reply.contains("was already approved"), "{reply}");
    running.stop().await;

    assert_eq!(
        h.consents.close_cards(h.commands.replies()).await.unwrap(),
        1
    );
    assert_eq!(
        h.consents.close_cards(h.commands.replies()).await.unwrap(),
        0
    );
    let updated = h.calls("chat.update").await;
    assert_eq!(updated.len(), 1);
    let update = json_body(&updated[0]);
    assert_eq!(update["channel"], "D0DM00001");
    assert_eq!(update["ts"], "1727700000.000100");
    let text = update["blocks"].to_string();
    assert!(text.contains("Approved by <@U0OWNER>."), "{text}");
    assert!(
        !text.contains("consent_approve"),
        "the buttons are gone: {text}"
    );
}

const OUTSIDE_TEAM: &str = "T0THEIRS1";

/// `users.info`'s answer that whoever was asked about is a member of the
/// workspace.
fn home_member(request: &Request) -> ResponseTemplate {
    let form: HashMap<String, String> =
        serde_urlencoded::from_bytes(&request.body).unwrap_or_default();
    let user = form.get("user").cloned().unwrap_or_default();
    ok(json!({"user": {"id": user, "team_id": TEAM}}))
}

/// Answers `users.info` for `user` alone with `response`, ahead of the
/// harness's answer for everyone else.
async fn mount_user_info(h: &SlackHarness, user: &str, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/api/users.info"))
        .and(body_string_contains(format!("user={user}").as_str()))
        .respond_with(response)
        .with_priority(1)
        .mount(&h.slack)
        .await;
}

fn user_in(user: &str, team: &str) -> ResponseTemplate {
    ok(json!({"user": {"id": user, "team_id": team}}))
}

fn slack_refused(code: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"ok": false, "error": code}))
}

/// `users.info` calls about `user`.
async fn looked_up(h: &SlackHarness, user: &str) -> usize {
    h.calls("users.info")
        .await
        .iter()
        .filter(|request| String::from_utf8_lossy(&request.body).contains(&format!("user={user}")))
        .count()
}

fn dm_in(sender: &str, channel: &str, text: &str) -> SlackInbound {
    let mut event = dm_event(sender, text);
    event.conv.conversation = channel.into();
    event.message.conv.conversation = channel.into();
    SlackInbound::Message(Box::new(event), InFlight::untracked())
}

/// Sends a DM from a home member and waits for its answer, which the
/// intake runs after whatever it received before.
async fn answered_marker(h: &SlackHarness, running: &Running) {
    running.send(dm_in("U0HUMAN01", "D0MARKER1", "me")).await;
    wait_for(async || {
        h.posts()
            .await
            .into_iter()
            .find(|(channel, _)| channel == "D0MARKER1")
    })
    .await;
}

#[tokio::test]
async fn a_teamless_manager_dm_from_outside_never_runs_a_command() {
    let logs = global_logs().tag();
    let h = slack_harness().await;
    mount_user_info(&h, "U0OUTSID1", user_in("U0OUTSID1", OUTSIDE_TEAM)).await;
    mount_user_info(&h, "U0NOTEAM1", ok(json!({"user": {"id": "U0NOTEAM1"}}))).await;
    mount_user_info(&h, "U0GONE001", slack_refused("user_not_found")).await;
    mount_user_info(&h, "U0SCOPE01", slack_refused("missing_scope")).await;
    mount_user_info(&h, "U0SCOPE02", slack_refused("missing_scope")).await;
    let running = Running::start(&h);
    for user in [
        "U0OUTSID1",
        "U0NOTEAM1",
        "U0GONE001",
        "U0SCOPE01",
        "U0SCOPE02",
    ] {
        let dm = dm_event(user, "me");
        assert_eq!(dm.outside, None, "no field says where they are from");
        running
            .send(SlackInbound::Message(Box::new(dm), InFlight::untracked()))
            .await;
    }
    answered_marker(&h, &running).await;
    running.stop().await;
    let posts = h.posts().await;
    assert_eq!(
        posts.len(),
        1,
        "only the home member's command ran: {posts:?}"
    );
    for user in [
        "U0OUTSID1",
        "U0NOTEAM1",
        "U0GONE001",
        "U0SCOPE01",
        "U0SCOPE02",
    ] {
        assert_eq!(looked_up(&h, user).await, 1, "{user}");
        assert!(
            h.store
                .member_for_identity(&slack_key(user))
                .await
                .unwrap()
                .is_none()
        );
    }
    assert!(h.calls("conversations.open").await.is_empty());
    let failed = logs
        .snapshot()
        .matching("couldn't ask Slack whether a user is home");
    assert_eq!(
        failed
            .to_string()
            .lines()
            .filter(|line| line.contains("WARN"))
            .count(),
        1,
        "warned once a minute at most:\n{failed}"
    );
}

#[tokio::test]
async fn a_rate_limited_manager_dm_lookup_asks_the_dm_to_try_again() {
    let h = slack_harness().await;
    mount_user_info(&h, "U0DOWN001", ResponseTemplate::new(503)).await;
    mount_user_info(
        &h,
        "U0BUSY001",
        ResponseTemplate::new(429).insert_header("retry-after", "30"),
    )
    .await;
    let running = Running::start(&h);
    running.send(dm_in("U0DOWN001", "D0DOWN001", "me")).await;
    let (_, text) = wait_for(async || {
        h.posts()
            .await
            .into_iter()
            .find(|(channel, _)| channel == "D0DOWN001")
    })
    .await;
    assert_eq!(text, crate::pipeline::UNCONFIRMED_TEXT);
    let started = std::time::Instant::now();
    running.send(dm_in("U0BUSY001", "D0BUSY001", "me")).await;
    let (_, text) = wait_for(async || {
        h.posts()
            .await
            .into_iter()
            .find(|(channel, _)| channel == "D0BUSY001")
    })
    .await;
    assert_eq!(text, crate::pipeline::UNCONFIRMED_TEXT);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "never waits for the quota"
    );
    running.stop().await;
    assert_eq!(h.posts().await.len(), 2, "no command ran");
    assert!(
        h.calls("conversations.open").await.is_empty(),
        "the try-again line goes into the DM the event named"
    );
}

#[tokio::test]
async fn a_manager_dm_lookup_never_holds_up_the_slack_queue() {
    let h = slack_harness().await;
    mount_user_info(
        &h,
        "U0SLOW001",
        user_in("U0SLOW001", TEAM).set_delay(Duration::from_secs(7)),
    )
    .await;
    let running = Running::start(&h);
    let started = std::time::Instant::now();
    running.send(dm_in("U0SLOW001", "D0SLOW001", "me")).await;
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "the queue handed the DM on without looking anyone up"
    );
    answered_marker(&h, &running).await;
    assert!(
        started.elapsed() < Duration::from_millis(3500),
        "another member's command didn't wait for the slow lookup"
    );
    assert!(
        !h.posts()
            .await
            .iter()
            .any(|(channel, _)| channel == "D0SLOW001")
    );
    running.stop().await;
    assert!(
        h.posts()
            .await
            .iter()
            .any(|(channel, _)| channel == "D0SLOW001"),
        "the slow member's command ran once Slack answered"
    );
}

#[tokio::test]
async fn an_interaction_without_user_team_is_dropped() {
    let h = slack_harness().await;
    let running = Running::start(&h);
    let id = core_types::ConsentId::new_v4().to_string();
    let mut hooks = Vec::new();
    for sender_team in [None, Some(TeamId::new(OUTSIDE_TEAM))] {
        let (url, hook) = h.response_url();
        let mut interaction = click("U0OWNER", crate::consents::card::APPROVE_ACTION, &id, url);
        interaction.sender_team = sender_team;
        running.send(SlackInbound::Interaction(interaction)).await;
        hooks.push(hook);
    }
    let (url, answered) = h.response_url();
    running
        .send(SlackInbound::Interaction(click(
            "U0OWNER",
            crate::consents::card::APPROVE_ACTION,
            &id,
            url,
        )))
        .await;
    wait_for(async || {
        h.requests()
            .await
            .into_iter()
            .find(|request| request.url.path() == answered)
    })
    .await;
    running.stop().await;
    for hook in hooks {
        assert!(
            !h.requests()
                .await
                .iter()
                .any(|request| request.url.path() == hook),
            "a click from no or another team ran nothing"
        );
    }
}

#[tokio::test]
async fn no_dm_is_opened_with_an_outside_user() {
    let h = slack_harness().await;
    mount_user_info(&h, "U0OUTSID1", user_in("U0OUTSID1", OUTSIDE_TEAM)).await;
    mount_user_info(&h, "U0GONE001", slack_refused("user_not_found")).await;
    mount_user_info(&h, "U0DOWN001", ResponseTemplate::new(503)).await;
    mount_user_info(&h, "U0SCOPE01", slack_refused("missing_scope")).await;
    let replies = h.commands.replies();
    for user in ["U0OUTSID1", "U0GONE001"] {
        let err = replies.dm(&slack_key(user), "hello").await.unwrap_err();
        assert!(
            matches!(err, ReplyError::Surface(SurfaceError::Forbidden(_))),
            "{user}: {err:?}"
        );
        let err = replies.dm_room(&slack_key(user)).await.unwrap_err();
        assert!(matches!(
            err,
            ReplyError::Surface(SurfaceError::Forbidden(_))
        ));
        let err = h
            .manager
            .manager_bot()
            .dm(&slack_key(user), "hello")
            .await
            .unwrap_err();
        assert!(matches!(err, SurfaceError::Forbidden(_)), "{err:?}");
    }
    let err = replies
        .dm(&slack_key("U0DOWN001"), "hello")
        .await
        .unwrap_err();
    assert!(
        matches!(err, ReplyError::Surface(SurfaceError::Transport(_))),
        "a failed lookup is passed on as it came: {err:?}"
    );
    let err = replies
        .dm(&slack_key("U0SCOPE01"), "hello")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ReplyError::Surface(SurfaceError::Forbidden(code)) if code == "missing_scope"),
        "Slack's own refusal, as it came: {err:?}"
    );
    replies
        .dm(&slack_key("U0DOWN001"), "hello")
        .await
        .unwrap_err();
    assert_eq!(
        looked_up(&h, "U0DOWN001").await,
        2,
        "a failed lookup isn't kept"
    );
    assert!(h.calls("conversations.open").await.is_empty());
    assert!(h.posts().await.is_empty());

    replies.dm(&slack_key("U0HUMAN01"), "hello").await.unwrap();
    assert_eq!(h.calls("conversations.open").await.len(), 1);
}

#[tokio::test]
async fn outside_commands_dms_and_clicks_never_run() {
    let h = slack_harness().await;
    let running = Running::start(&h);
    let mut dm = dm_event("U0OUTSID1", "me");
    dm.outside = Some(core_types::Outside {
        team: Some(TeamId::new(OUTSIDE_TEAM)),
    });
    running
        .send(SlackInbound::Message(Box::new(dm), InFlight::untracked()))
        .await;
    let (url, click_hook) = h.response_url();
    let mut outside_click = click(
        "U0OUTSID1",
        crate::consents::card::APPROVE_ACTION,
        &core_types::ConsentId::new_v4().to_string(),
        url,
    );
    outside_click.sender_team = Some(TeamId::new(OUTSIDE_TEAM));
    running.send(SlackInbound::Interaction(outside_click)).await;
    let (url, slash_hook) = h.response_url();
    let mut command = slash("me", url);
    command.sender = MemberKey {
        team: TeamId::new(OUTSIDE_TEAM),
        ..slack_key("U0OUTSID1")
    };
    command.conv.team = TeamId::new(OUTSIDE_TEAM);
    running.send(SlackInbound::Command(command)).await;
    running.stop().await;
    let requests = h.requests().await;
    assert!(
        requests.is_empty(),
        "nothing reached Slack, not even a lookup: {:?}",
        requests
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect::<Vec<_>>()
    );
    assert!(
        h.store
            .member_for_identity(&slack_key("U0OUTSID1"))
            .await
            .unwrap()
            .is_none()
    );

    let running = Running::start(&h);
    let (url, home_hook) = h.response_url();
    running.send(SlackInbound::Command(slash("me", url))).await;
    running.stop().await;
    assert!(
        h.requests()
            .await
            .iter()
            .any(|request| request.url.path() == home_hook),
        "a slash command naming this workspace runs"
    );
    let requests = h.requests().await;
    assert!(
        !requests
            .iter()
            .any(|request| [click_hook.as_str(), slash_hook.as_str()].contains(&request.url.path())),
        "nothing answered the outsider's click or command"
    );
    assert!(
        h.calls("users.info").await.is_empty(),
        "a slash command carries no sender team and costs no lookup: its guard is \
         Slack's own rule that only the installing workspace's members run an app's \
         commands, and its team_id must be this workspace"
    );
}

#[tokio::test]
async fn slack_command_text_is_decoded_once_before_it_is_parsed() {
    let h = slack_harness().await;
    let alice = h.linked("U0HUMAN01").await;
    let team = TeamId::new(TEAM);
    let store::AgentCreation::Created(agent, _) = h
        .store
        .create_agent(
            &store::NewAgent {
                owner: alice,
                name: "helper",
                persona: "p",
                visibility: store::Visibility::Public,
                surface: SurfaceKind::Slack,
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
    let reply = h
        .slash("U0HUMAN01", "persona helper a &lt;b&gt; &amp;amp; c")
        .await;
    assert!(
        reply[0].starts_with("Replaced `helper`'s persona."),
        "{reply:?}"
    );
    let row = h.store.agent(agent.id).await.unwrap().unwrap();
    assert_eq!(row.persona, "a <b> &amp; c");

    let (intake, submitter) = CommandIntake::new(h.commands.clone());
    let inbound = Sender::new(Inbound::new(h.store.clone(), Some(identity()), submitter));
    let running = tokio::spawn(intake.run());
    inbound
        .send(SlackInbound::Message(
            Box::new(dm_event(
                "U0HUMAN01",
                "persona helper x &lt;y&gt; &amp;amp; z",
            )),
            InFlight::untracked(),
        ))
        .await
        .unwrap();
    drop(inbound);
    tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .unwrap()
        .unwrap();
    let row = h.store.agent(agent.id).await.unwrap().unwrap();
    assert_eq!(row.persona, "x <y> &amp; z");
}

#[tokio::test]
async fn a_relink_notice_the_store_fails_on_leaves_the_others() {
    let (store, url, _dir) = file_store().await;
    let h = slack_harness_on(store).await;
    let alice = h.linked("U0HUMAN01").await;
    let grace = h.linked("U0HUMAN02").await;
    let now = OffsetDateTime::now_utc();
    for (member, broken_at) in [(alice, now - time::Duration::minutes(1)), (grace, now)] {
        let generation = h
            .store
            .get_claude_link(member)
            .await
            .unwrap()
            .unwrap()
            .generation;
        h.store
            .mark_claude_link_broken(member, generation, broken_at)
            .await
            .unwrap();
    }
    sql(
        &url,
        &format!(
            "CREATE TRIGGER no_claim BEFORE UPDATE OF relink_attempts ON claude_links \
             WHEN OLD.member_id = '{alice}' BEGIN SELECT RAISE(FAIL, 'injected'); END;"
        ),
    )
    .await;
    let notifier = RelinkNotifier::new(h.store.clone(), h.commands.replies().clone());
    assert!(notifier.send_pending().await.is_err());
    assert_eq!(h.posts().await.len(), 1, "grace was told");
    sql(&url, "DROP TRIGGER no_claim;").await;
    assert_eq!(notifier.send_pending().await.unwrap(), 1);
    assert_eq!(h.posts().await.len(), 2, "alice was told after");
}

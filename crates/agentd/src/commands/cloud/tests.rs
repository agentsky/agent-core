//! `cloud` commands against a `MockSurface` manager bot or a wiremock Slack,
//! with a wiremock routine endpoint.

use core_types::{ConvKind, MemberId, RoutineId, SurfaceKind, TeamId, UserId};
use secrecy::SecretString;
use serde_json::{Value, json};
use store::{CloudHandoffState, CloudUnknownReason, NewCloudHandoff, RecentCloudHandoff, Store};
use surface_slack::{BindingRef, InFlight, SlackEvent, SlackInbound};
use testkit::Held;
use time::OffsetDateTime;
use tokio::sync::watch;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::cloud::FireClient;
use crate::commands::rocketchat::command_in;
use crate::commands::slack::dm_command;
use crate::commands::slack_tests::{
    Running, SlackHarness, dm_event, file_store, identity, json_body, slack_channel, slack_harness,
    slack_harness_on, slack_key, sql,
};
use crate::commands::tests::{Harness, conv, dm_room, harness, key, serve};
use crate::config::CloudConfig;
use crate::telemetry::tests::global_logs;

const TOKEN: &str = "sk-ant-oat01-ROUTINE-SECRET-token";
const NEW_TOKEN: &str = "sk-ant-oat01-ROUTINE-SECRET-renewed";
const ROUTINE: &str = "trig_01ABCdef";
const SESSION: &str = "session_01XYZabc";
const LABEL: &str = "agent-core";

fn fire_path() -> String {
    format!("/v1/claude_code/routines/{ROUTINE}/fire")
}

fn session_url() -> String {
    format!("{SESSION_URL_PREFIX}{SESSION}")
}

fn fire_client(endpoint: &MockServer) -> FireClient {
    fire_client_capped(endpoint, CloudConfig::default().handoffs_per_hour)
}

fn fire_client_capped(endpoint: &MockServer, handoffs_per_hour: u32) -> FireClient {
    FireClient::new(&CloudConfig {
        base_url: endpoint.uri(),
        handoffs_per_hour,
        ..CloudConfig::default()
    })
    .unwrap()
}

fn started() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "type": "routine_fire",
        "claude_code_session_id": SESSION,
        "claude_code_session_url": session_url(),
    }))
}

async fn mount_fire(endpoint: &MockServer, response: ResponseTemplate, times: u64) {
    Mock::given(method("POST"))
        .and(path(fire_path()))
        .respond_with(response)
        .expect(times)
        .mount(endpoint)
        .await;
}

async fn fires(endpoint: &MockServer) -> Vec<wiremock::Request> {
    endpoint
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == fire_path())
        .collect()
}

fn routine_url(endpoint: &MockServer) -> String {
    format!("{}{}", endpoint.uri(), fire_path())
}

/// A Rocket.Chat harness whose commands fire routines at `endpoint`.
struct Cloud {
    h: Harness,
    endpoint: MockServer,
}

async fn cloud() -> Cloud {
    let endpoint = MockServer::start().await;
    let mut h = harness().await;
    h.commands = h.commands.clone().with_cloud(fire_client(&endpoint));
    Cloud { h, endpoint }
}

impl Cloud {
    async fn linked(&self, user: &str) -> MemberId {
        self.h.linked_member(user, "claude_max").await.0
    }

    async fn add(&self, user: &str) -> String {
        self.h
            .dm(
                user,
                &format!("cloud add {LABEL} {} {TOKEN}", routine_url(&self.endpoint)),
            )
            .await;
        self.h.last_reply(user)
    }

    async fn run(&self, user: &str, task: &str) -> String {
        self.h.dm(user, &format!("cloud run {LABEL} {task}")).await;
        self.h.last_reply(user)
    }

    fn notifier(&self) -> CloudNotifier {
        CloudNotifier::new(
            self.h.store.clone(),
            self.h.commands.replies().clone(),
            None,
        )
    }
}

async fn handoffs(store: &Store, member: MemberId) -> Vec<RecentCloudHandoff> {
    store.recent_cloud_handoffs(member, 100).await.unwrap()
}

fn clock(at: OffsetDateTime) -> impl Fn() -> OffsetDateTime {
    move || at
}

/// Now, to the whole second, as the store keeps times.
fn second() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp()).unwrap()
}

async fn put_routine(store: &Store, member: MemberId, url_origin: &str, added_by: &MemberKey) {
    let token = RoutineToken::parse(SecretString::from(TOKEN)).unwrap();
    let routine_id: RoutineId = ROUTINE.parse().unwrap();
    store
        .put_cloud_routine(
            &NewCloudRoutine {
                member,
                label: LABEL,
                routine_id: &routine_id,
                url_origin,
                token: &token,
                added_by,
            },
            OffsetDateTime::now_utc(),
        )
        .await
        .unwrap();
}

async fn begin(
    store: &Store,
    member: MemberId,
    by: &MemberKey,
    at: OffsetDateTime,
) -> core_types::CloudHandoffId {
    if store.cloud_routine(member, LABEL).await.unwrap().is_none() {
        put_routine(store, member, "https://api.anthropic.com", by).await;
    }
    let routine_id: RoutineId = ROUTINE.parse().unwrap();
    let begun = store
        .begin_cloud_handoff(
            &NewCloudHandoff {
                member,
                routine_label: LABEL,
                routine_id: &routine_id,
                requested_by: by,
                origin: CloudOrigin::RocketChatDm,
                task: "Fix the flaky test",
            },
            u32::MAX,
            at,
        )
        .await
        .unwrap();
    let CloudBegun::Begun(id) = begun else {
        panic!("{begun:?}");
    };
    id
}

#[tokio::test]
async fn cloud_run_fires_once_and_replies_privately_with_the_link() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    let logs = global_logs().tag();
    Mock::given(method("POST"))
        .and(path(fire_path()))
        .and(header("authorization", format!("Bearer {TOKEN}").as_str()))
        .and(body_json(
            json!({"text": "Fix the flaky test\nthen open a PR"}),
        ))
        .respond_with(started())
        .expect(1)
        .mount(&c.endpoint)
        .await;

    let added = c.add("alice").await;
    assert!(
        added.contains(&format!("Registered routine `{LABEL}` (`{ROUTINE}`)")),
        "{added}"
    );
    let reply = c.run("alice", "Fix the flaky test\nthen open a PR").await;
    assert!(
        reply.starts_with(&format!(
            "Started a cloud session from routine `{LABEL}`: {}",
            session_url()
        )),
        "{reply}"
    );
    assert!(
        reply.contains(&format!("`claude --teleport {SESSION}`")),
        "{reply}"
    );
    assert!(reply.contains("I won't follow it"), "{reply}");

    let recorded = handoffs(&c.h.store, alice).await;
    assert_eq!(recorded.len(), 1);
    let handoff = &recorded[0].handoff;
    assert_eq!(handoff.state, CloudHandoffState::Fired);
    assert_eq!(handoff.session_id.as_deref(), Some(SESSION));
    assert_eq!(handoff.session_url, Some(session_url()));
    assert_eq!(handoff.origin, CloudOrigin::RocketChatDm);
    assert_eq!(handoff.requested_by, key("alice"));
    assert!(handoff.notified_at.is_some(), "the reply told alice");

    let dm = conv(&dm_room("alice"));
    assert!(
        c.h.mock.posts().iter().all(|(target, _)| target.conv == dm),
        "every reply is in alice's DM: {:?}",
        c.h.mock.posts()
    );
    logs.snapshot()
        .assert_has("\"command\":\"cloud run\"")
        .assert_has("recorded a cloud hand-off")
        .assert_has(SESSION);
    global_logs()
        .snapshot()
        .assert_lacks(TOKEN)
        .assert_lacks("Fix the flaky test")
        .assert_lacks(&routine_url(&c.endpoint));
}

#[tokio::test]
async fn cloud_commands_are_refused_in_a_rocketchat_room() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    mount_fire(&c.endpoint, started(), 0).await;
    put_routine(&c.h.store, alice, &c.endpoint.uri(), &key("alice")).await;
    for command in [
        format!("cloud run {LABEL} Fix the flaky test"),
        "cloud list".to_owned(),
        format!("cloud rm {LABEL}"),
    ] {
        c.h.channel("alice", &command).await;
        let reply = c.h.last_reply("alice");
        assert!(
            reply.contains("runs only where no one else reads it, so I didn't run it"),
            "{command}: {reply}"
        );
        assert!(reply.contains("here, in this direct message"), "{reply}");
    }
    assert_eq!(c.h.store.cloud_routines(alice).await.unwrap().len(), 1);
    assert!(handoffs(&c.h.store, alice).await.is_empty());
}

#[tokio::test]
async fn cloud_add_in_a_room_gets_the_secret_refusal_and_stores_nothing() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    c.h.channel(
        "alice",
        &format!("cloud add {LABEL} {} {TOKEN}", routine_url(&c.endpoint)),
    )
    .await;
    let reply = c.h.last_reply("alice");
    assert!(reply.contains("I didn't store it"), "{reply}");
    assert!(
        reply.contains("**Regenerate** or **Revoke**") && reply.contains("claude.ai/code/routines"),
        "{reply}"
    );
    assert!(!reply.contains(TOKEN), "{reply}");
    assert!(c.h.store.cloud_routines(alice).await.unwrap().is_empty());
}

#[tokio::test]
async fn cloud_add_and_run_are_refused_without_cloud_config() {
    let h = harness().await;
    let endpoint = MockServer::start().await;
    mount_fire(&endpoint, started(), 0).await;
    let (alice, _) = h.linked_member("alice", "claude_max").await;

    h.dm(
        "alice",
        &format!("cloud add {LABEL} {} {TOKEN}", routine_url(&endpoint)),
    )
    .await;
    assert_eq!(
        h.last_reply("alice"),
        "Cloud hand-off is off on this agentd, so I didn't store the routine. `cloud list` and \
         `cloud rm` still work."
    );
    assert!(h.store.cloud_routines(alice).await.unwrap().is_empty());

    put_routine(&h.store, alice, &endpoint.uri(), &key("alice")).await;
    h.dm("alice", &format!("cloud run {LABEL} Fix the flaky test"))
        .await;
    assert_eq!(
        h.last_reply("alice"),
        "Cloud hand-off is off on this agentd, so I didn't start anything. `cloud list` and \
         `cloud rm` still work."
    );
    assert!(handoffs(&h.store, alice).await.is_empty());

    h.dm("alice", "cloud list").await;
    let listed = h.last_reply("alice");
    assert!(
        listed.contains(&format!("- `{LABEL}`: `{ROUTINE}`")),
        "{listed}"
    );
    assert!(listed.contains("Cloud hand-off is off"), "{listed}");
    h.dm("alice", &format!("cloud rm {LABEL}")).await;
    assert!(
        h.last_reply("alice")
            .starts_with(&format!("Forgot routine `{LABEL}`")),
    );
    assert!(h.store.cloud_routines(alice).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_unlinked_member_cannot_add_or_run() {
    let c = cloud().await;
    mount_fire(&c.endpoint, started(), 0).await;
    let alice =
        c.h.store
            .ensure_member(&key("alice"), "alice", OffsetDateTime::now_utc())
            .await
            .unwrap();
    let added = c.add("alice").await;
    assert!(
        added.starts_with("Link your Claude account first: send `login`."),
        "{added}"
    );
    assert!(c.h.store.cloud_routines(alice).await.unwrap().is_empty());

    put_routine(&c.h.store, alice, &c.endpoint.uri(), &key("alice")).await;
    let ran = c.run("alice", "Fix the flaky test").await;
    assert!(ran.starts_with("Link your Claude account first"), "{ran}");
    assert!(handoffs(&c.h.store, alice).await.is_empty());

    let ran = c.run("bob", "Fix the flaky test").await;
    assert!(ran.starts_with("Link your Claude account first"), "{ran}");
    assert!(
        c.h.store
            .member_for_identity(&key("bob"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_banned_member_can_only_rm() {
    let c = cloud().await;
    mount_fire(&c.endpoint, started(), 0).await;
    let alice = c.linked("alice").await;
    put_routine(&c.h.store, alice, &c.endpoint.uri(), &key("alice")).await;
    c.h.store
        .ban_member(alice, &key("root"), None, OffsetDateTime::now_utc())
        .await
        .unwrap();

    assert_eq!(c.add("alice").await, BANNED_REPLY);
    assert_eq!(c.run("alice", "Fix the flaky test").await, BANNED_REPLY);
    c.h.dm("alice", "cloud list").await;
    assert_eq!(c.h.last_reply("alice"), BANNED_REPLY);
    assert!(BANNED_REPLY.contains("`cloud rm`"));
    assert!(handoffs(&c.h.store, alice).await.is_empty());

    c.h.dm("alice", &format!("cloud rm {LABEL}")).await;
    assert!(
        c.h.last_reply("alice")
            .starts_with(&format!("Forgot routine `{LABEL}` and its token.")),
    );
    assert!(c.h.store.cloud_routines(alice).await.unwrap().is_empty());
    c.h.dm("alice", "me").await;
    assert!(c.h.last_reply("alice").contains("`cloud rm`"));
}

const BANNED_REPLY: &str = super::super::BANNED;

#[tokio::test]
async fn a_task_with_invisible_characters_is_refused() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    c.add("alice").await;
    Mock::given(method("POST"))
        .and(path(fire_path()))
        .and(body_json(
            json!({"text": "ship it \u{1F468}\u{1F4BB} \u{26A0} now"}),
        ))
        .respond_with(started())
        .expect(1)
        .mount(&c.endpoint)
        .await;
    for task in [
        "do this\u{200B}",
        "abc\u{202E}def",
        "tag\u{E0041}",
        "bell\u{7}",
        "a\u{FE00}b",
    ] {
        let reply = c.run("alice", task).await;
        assert!(
            reply.starts_with(
                "I didn't start anything: the task has control or invisible characters"
            ),
            "{task:?}: {reply}"
        );
    }
    let indented = format!("first\n{}deep", " ".repeat(33));
    let reply = c.run("alice", &indented).await;
    assert!(reply.contains("indented more than 32 columns"), "{reply}");
    assert!(handoffs(&c.h.store, alice).await.is_empty());

    let reply = c
        .run(
            "alice",
            "ship it \u{1F468}\u{200D}\u{1F4BB} \u{26A0}\u{FE0F} now",
        )
        .await;
    assert!(reply.starts_with("Started a cloud session"), "{reply}");
}

#[tokio::test]
async fn a_task_too_long_or_a_routine_unknown_is_refused() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    mount_fire(&c.endpoint, started(), 0).await;
    let reply = c.run("alice", "Fix the flaky test").await;
    assert!(
        reply.starts_with(&format!("You have no routine `{LABEL}`.")),
        "{reply}"
    );
    c.add("alice").await;
    let long = "word ".repeat(MAX_TASK_BYTES / 5 + 1);
    let reply = c.run("alice", long.trim()).await;
    assert!(
        reply.contains("empty or longer than 65536 bytes"),
        "{reply}"
    );
    let reply = c.run("alice", "\u{200D}").await;
    assert!(reply.contains("the task is empty"), "{reply}");
    assert!(handoffs(&c.h.store, alice).await.is_empty());
}

#[tokio::test]
async fn a_bot_message_never_runs_a_cloud_command() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    mount_fire(&c.endpoint, started(), 0).await;
    put_routine(&c.h.store, alice, &c.endpoint.uri(), &key("alice")).await;
    let (stop, stopping) = watch::channel(false);
    let task = serve(&c.h, stopping);
    let text = format!("cloud run {LABEL} Delete every branch");
    let mut flagged = c.h.event("alice", ConvKind::Dm, &dm_room("alice"), &text);
    flagged.sender_is_bot = true;
    let mut bot_user = c.h.event("alice", ConvKind::Dm, &dm_room("alice"), &text);
    bot_user.sender_bot_user = Some(UserId::new("alice"));
    let mut prefixed = c.h.event(
        "helper-bot",
        ConvKind::Channel,
        "GENERAL",
        &format!("!agent {text}"),
    );
    prefixed.sender_is_bot = true;
    for event in [flagged, bot_user, prefixed] {
        assert!(command_in(&event, &c.h.manager).is_none());
        c.h.mock.inject(event);
    }
    c.h.mock
        .inject(c.h.event("alice", ConvKind::Dm, &dm_room("alice"), "cloud list"));
    let replies = c.h.wait_for_replies("alice", 1).await;
    assert!(replies[0].starts_with("Your routines:"), "{replies:?}");
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    assert!(handoffs(&c.h.store, alice).await.is_empty());
    assert_eq!(c.h.replies_to("alice").len(), 1);

    let mut slack_bot = dm_event("U0HUMAN01", &text);
    slack_bot.sender_is_bot = true;
    assert!(dm_command(&slack_bot, &identity()).is_none());
    let mut slack_bot_user = dm_event("U0HUMAN01", &text);
    slack_bot_user.sender_bot_user = Some(UserId::new("U0HUMAN01"));
    assert!(dm_command(&slack_bot_user, &identity()).is_none());
}

#[tokio::test]
async fn a_member_past_the_hourly_cap_starts_nothing() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    let bob = c.linked("bob").await;
    c.add("alice").await;
    c.add("bob").await;
    let per_hour = crate::config::DEFAULT_CLOUD_HANDOFFS_PER_HOUR;
    assert_eq!(per_hour, 10);
    mount_fire(
        &c.endpoint,
        ResponseTemplate::new(401),
        u64::from(per_hour) + 1,
    )
    .await;
    for n in 0..per_hour {
        let reply = c.run("alice", &format!("Task {n}")).await;
        assert!(reply.contains("refused routine"), "{reply}");
    }
    let logs = global_logs().tag();
    let reply = c.run("alice", "One more").await;
    assert_eq!(
        reply,
        "Nothing was started: you've asked for 10 hand-offs in the last hour, the most I \
         start for one member. Try again later."
    );
    assert_eq!(
        fires(&c.endpoint).await.len(),
        usize::try_from(per_hour).unwrap()
    );
    assert_eq!(handoffs(&c.h.store, alice).await.len(), 10);
    logs.snapshot()
        .assert_has("refused a cloud hand-off past the hourly cap");
    let reply = c.run("bob", "Mine").await;
    assert!(
        reply.contains("refused routine"),
        "the cap is each member's: {reply}"
    );
    assert_eq!(handoffs(&c.h.store, bob).await.len(), 1);
}

#[tokio::test]
async fn an_unknown_outcome_is_never_retried() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    c.add("alice").await;
    mount_fire(&c.endpoint, ResponseTemplate::new(500), 1).await;
    let reply = c.run("alice", "Fix the flaky test").await;
    assert!(
        reply.starts_with("I can't tell whether a cloud session started"),
        "{reply}"
    );
    assert!(reply.contains("Check claude.ai/code before running it again"));
    assert_eq!(fires(&c.endpoint).await.len(), 1);
    let recorded = handoffs(&c.h.store, alice).await;
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].handoff.state, CloudHandoffState::Unknown);
    assert_eq!(
        recorded[0].handoff.unknown_reason,
        Some(CloudUnknownReason::ServerError)
    );
    assert_eq!(recorded[0].handoff.http_status, Some(500));
}

#[tokio::test]
async fn an_unknown_outcome_in_the_reply_gets_no_second_notice() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    c.add("alice").await;
    mount_fire(&c.endpoint, ResponseTemplate::new(503), 1).await;
    c.run("alice", "Fix the flaky test").await;
    let replies = c.h.replies_to("alice").len();
    for minutes in [2, 30, 120] {
        let pass = c
            .notifier()
            .pass_at(clock(second() + time::Duration::minutes(minutes)))
            .await
            .unwrap();
        assert_eq!((pass.marked, pass.told), (0, 0));
    }
    assert_eq!(c.h.replies_to("alice").len(), replies);
    let handoff = &handoffs(&c.h.store, alice).await[0].handoff;
    assert_eq!(handoff.state, CloudHandoffState::Unknown);
    assert!(handoff.notified_at.is_some());
}

#[tokio::test]
async fn a_stale_sending_handoff_is_reported_once() {
    let h = harness().await;
    let (alice, _) = h.linked_member("alice", "claude_max").await;
    let t0 = second();
    begin(&h.store, alice, &key("alice"), t0).await;
    let config = CloudConfig {
        timeout_secs: 10,
        connect_timeout_secs: 5,
        ..CloudConfig::default()
    };
    let notifier = CloudNotifier::new(h.store.clone(), h.commands.replies().clone(), Some(&config));
    let early = notifier
        .pass_at(clock(t0 + time::Duration::seconds(20)))
        .await
        .unwrap();
    assert_eq!(early, CloudPass::default());
    let pass = notifier
        .pass_at(clock(t0 + time::Duration::seconds(21)))
        .await
        .unwrap();
    assert_eq!((pass.marked, pass.told), (1, 1));
    for minutes in [1, 20, 90] {
        let again = notifier
            .pass_at(clock(t0 + time::Duration::minutes(minutes)))
            .await
            .unwrap();
        assert_eq!((again.marked, again.told), (0, 0));
    }
    let notices = h.replies_to("alice");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(
        notices[0].starts_with(&format!(
            "Your cloud hand-off to routine `{LABEL}`, asked at"
        )),
        "{}",
        notices[0]
    );
    assert!(notices[0].contains("may have started"), "{}", notices[0]);
    assert!(notices[0].contains("check claude.ai/code before running it again"));
    assert!(notices[0].contains("`cloud list`"));
    let handoff = &handoffs(&h.store, alice).await[0].handoff;
    assert_eq!(handoff.state, CloudHandoffState::Unknown);
    assert_eq!(handoff.unknown_reason, Some(CloudUnknownReason::NoAnswer));
    assert!(handoff.notified_at.is_some());
}

#[tokio::test]
async fn a_notice_no_manager_bot_reaches_waits_and_a_failed_one_is_deferred() {
    let h = harness().await;
    let slack = MemberKey {
        surface: SurfaceKind::Slack,
        team: TeamId::new("T0TEAM001"),
        user: UserId::new("U0HUMAN01"),
    };
    let member = h
        .store
        .ensure_member(&slack, "U0HUMAN01", OffsetDateTime::now_utc())
        .await
        .unwrap();
    let t0 = second();
    begin(&h.store, member, &slack, t0).await;
    let notifier = CloudNotifier::new(h.store.clone(), h.commands.replies().clone(), None);
    let pass = notifier
        .pass_at(clock(t0 + time::Duration::minutes(2)))
        .await
        .unwrap();
    assert_eq!((pass.marked, pass.told), (1, 0));
    let handoff = &handoffs(&h.store, member).await[0].handoff;
    assert_eq!(
        handoff.notice_attempts, 0,
        "nobody could send it, so no claim"
    );

    let (alice, _) = h.linked_member("alice", "claude_max").await;
    begin(&h.store, alice, &key("alice"), t0).await;
    h.mock.fail_next(
        testkit::Op::Post,
        core_types::SurfaceError::Transport("down".to_owned()),
    );
    let pass = notifier
        .pass_at(clock(t0 + time::Duration::minutes(3)))
        .await
        .unwrap();
    assert_eq!((pass.marked, pass.told), (1, 0));
    let pass = notifier
        .pass_at(clock(
            t0 + time::Duration::minutes(3) + time::Duration::seconds(59),
        ))
        .await
        .unwrap();
    assert_eq!(pass.told, 0, "backing off a minute");
    let pass = notifier
        .pass_at(clock(t0 + time::Duration::minutes(4)))
        .await
        .unwrap();
    assert_eq!(pass.told, 1);
    assert_eq!(h.replies_to("alice").len(), 1);
}

#[tokio::test]
async fn a_late_answer_after_the_pass_is_recorded_as_fired() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    c.add("alice").await;
    let (held, mut hold) = Held::new(started());
    Mock::given(method("POST"))
        .and(path(fire_path()))
        .respond_with(held)
        .mount(&c.endpoint)
        .await;
    let commands = c.h.commands.clone();
    let origin = Origin::RocketChatDm {
        room: dm_room("alice").into(),
    };
    let running = tokio::spawn(async move {
        commands
            .handle_text(
                &key("alice"),
                &format!("cloud run {LABEL} Fix the flaky test"),
                &origin,
                &[],
            )
            .await;
    });
    hold.arrived().await;
    let pass = c
        .notifier()
        .pass_at(clock(second() + time::Duration::minutes(5)))
        .await
        .unwrap();
    assert_eq!((pass.marked, pass.told), (1, 1));
    hold.release();
    running.await.unwrap();

    let replies = c.h.replies_to("alice");
    assert!(replies[replies.len() - 2].contains("may have started"));
    assert!(
        replies[replies.len() - 1].contains(&session_url()),
        "{replies:?}"
    );
    let handoff = &handoffs(&c.h.store, alice).await[0].handoff;
    assert_eq!(handoff.state, CloudHandoffState::Fired);
    assert_eq!(handoff.session_id.as_deref(), Some(SESSION));
    assert_eq!(handoff.unknown_reason, None);
    c.h.dm("alice", "cloud list").await;
    let listed = c.h.last_reply("alice");
    assert!(
        listed.contains(&format!("started {}", session_url())),
        "{listed}"
    );
    let again = c
        .notifier()
        .pass_at(clock(second() + time::Duration::minutes(10)))
        .await
        .unwrap();
    assert_eq!((again.marked, again.told), (0, 0));
}

#[tokio::test]
async fn logout_drops_routines_and_handoffs() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    c.add("alice").await;
    mount_fire(&c.endpoint, started(), 1).await;
    c.run("alice", "Fix the flaky test").await;
    c.h.dm("alice", "logout").await;
    let reply = c.h.last_reply("alice");
    assert!(
        reply.contains("I also forgot your 1 routine and 1 hand-off."),
        "{reply}"
    );
    assert!(
        reply.contains(
            "revoke each with **Revoke** on the routine's API trigger at \
                        claude.ai/code/routines"
        ),
        "{reply}"
    );
    assert!(c.h.store.cloud_routines(alice).await.unwrap().is_empty());
    assert!(handoffs(&c.h.store, alice).await.is_empty());
    c.h.dm("alice", "logout").await;
    assert!(!c.h.last_reply("alice").contains("routine"));

    let bob = c.linked("bob").await;
    c.add("bob").await;
    c.h.dm("bob", "logout").await;
    let reply = c.h.last_reply("bob");
    assert!(reply.contains("I also forgot your 1 routine."), "{reply}");
    assert!(!reply.contains("hand-off"), "{reply}");
    assert!(c.h.store.cloud_routines(bob).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_logout_that_failed_still_says_to_revoke_when_sent_again() {
    let endpoint = MockServer::start().await;
    let (h, url, dir) = slack_cloud_on_file(&endpoint).await;
    let alice = h
        .store
        .member_for_identity(&slack_key("U0HUMAN01"))
        .await
        .unwrap()
        .unwrap();
    sql(
        &url,
        "CREATE TRIGGER no_unlink BEFORE DELETE ON claude_links \
         BEGIN SELECT RAISE(FAIL, 'injected'); END;",
    )
    .await;
    assert_eq!(h.slash("U0HUMAN01", "logout").await, [FAILED.to_owned()]);
    assert_eq!(
        h.store.cloud_routines(alice).await.unwrap().len(),
        1,
        "nothing is forgotten while the link stays"
    );
    sql(&url, "DROP TRIGGER no_unlink;").await;
    let reply = h.slash("U0HUMAN01", "logout").await.remove(0);
    assert!(
        reply.starts_with("Your Claude account is unlinked."),
        "{reply}"
    );
    assert!(reply.contains("I also forgot your 1 routine."), "{reply}");
    assert!(
        reply.contains("I can't revoke a routine's token: revoke each"),
        "{reply}"
    );
    assert!(h.store.cloud_routines(alice).await.unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_member_slack_reports_deleted_loses_routines_from_every_surface() {
    let h = slack_harness().await;
    let grace = h.linked("U0HUMAN02").await;
    let alice = h.linked("U0HUMAN01").await;
    let on_rocketchat = MemberKey {
        surface: SurfaceKind::RocketChat,
        team: TeamId::new("chat.example.org"),
        user: UserId::new("grace"),
    };
    for (member, by) in [
        (grace, on_rocketchat.clone()),
        (alice, slack_key("U0HUMAN01")),
    ] {
        put_routine(&h.store, member, "https://api.anthropic.com", &by).await;
        begin(&h.store, member, &by, OffsetDateTime::now_utc()).await;
    }
    let envelope: Value = serde_json::from_str(testkit::slack::USER_CHANGE).unwrap();
    let running = Running::start(&h);
    running
        .send(SlackInbound::Event(SlackEvent {
            binding: BindingRef::MANAGER_ID,
            team: TeamId::new("T0TEAM001"),
            event_id: "Ev0USERCHG1".to_owned(),
            event_type: "user_change".to_owned(),
            event: envelope["event"].clone(),
            received_at: OffsetDateTime::now_utc(),
        }))
        .await;
    running.stop().await;
    assert!(h.store.cloud_routines(grace).await.unwrap().is_empty());
    assert!(handoffs(&h.store, grace).await.is_empty());
    assert_eq!(h.store.cloud_routines(alice).await.unwrap().len(), 1);
    assert_eq!(handoffs(&h.store, alice).await.len(), 1);
    assert!(h.posts().await.is_empty());
    assert!(
        h.requests()
            .await
            .iter()
            .all(|request| !request.url.path().starts_with("/hooks/")),
        "nothing is sent to a member Slack reports deleted"
    );
}

/// A Slack harness whose commands fire routines at `endpoint`, with
/// `U0HUMAN01` linked and the routine registered.
async fn slack_cloud() -> (SlackHarness, MockServer, MemberId) {
    let endpoint = MockServer::start().await;
    let mut h = slack_harness().await;
    h.commands = h.commands.clone().with_cloud(fire_client(&endpoint));
    let alice = h.linked("U0HUMAN01").await;
    let added = h
        .slash(
            "U0HUMAN01",
            &format!("cloud add {LABEL} <{}> {TOKEN}", routine_url(&endpoint)),
        )
        .await;
    assert!(added[0].starts_with("Registered routine"), "{added:?}");
    (h, endpoint, alice)
}

async fn fired_text(endpoint: &MockServer) -> String {
    let requests = fires(endpoint).await;
    let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    body["text"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn slack_tokens_in_a_task_become_what_slack_showed() {
    let (h, endpoint, alice) = slack_cloud().await;
    mount_fire(&endpoint, started(), 1).await;
    let reply = h
        .slash(
            "U0HUMAN01",
            "cloud run agent-core Ask <@U0HUMAN02|grace> in <#C0CHAN001|general> about \
             <https://example.com/a?b=1&amp;c=2> and <https://example.com/x|https://example.com/x>; \
             keep &lt;div&gt; &amp; &lt;T&gt; as typed",
        )
        .await;
    assert!(reply[0].starts_with("Started a cloud session"), "{reply:?}");
    assert_eq!(
        fired_text(&endpoint).await,
        "Ask @grace in #general about https://example.com/a?b=1&c=2 and https://example.com/x; \
         keep <div> & <T> as typed"
    );
    let recorded = handoffs(&h.store, alice).await;
    assert_eq!(recorded[0].handoff.origin, CloudOrigin::SlackSlash);

    for refused in [
        "cloud run agent-core Tell <!here> about it",
        "cloud run agent-core Ask <!subteam^S0TEAM|@devs> to look",
        "cloud run agent-core Ask <@U0HUMAN02> to look",
        "cloud run agent-core On <!date^1727700000^{date}|Sep 30>",
        "cloud run agent-core Unclosed <https://example.com",
        "cloud run agent-core Odd <not a link>",
    ] {
        let reply = h.slash("U0HUMAN01", refused).await;
        assert!(
            reply[0].starts_with("Slack sent part of the task as something other than"),
            "{refused}: {reply:?}"
        );
    }
    assert_eq!(fires(&endpoint).await.len(), 1);
    assert_eq!(handoffs(&h.store, alice).await.len(), 1);
}

#[tokio::test]
async fn a_link_label_other_than_its_url_is_shown_with_the_url() {
    let (h, endpoint, _) = slack_cloud().await;
    mount_fire(&endpoint, started(), 2).await;
    h.slash(
        "U0HUMAN01",
        "cloud run agent-core Read <https://evil.example/x|https://good.example/x> first",
    )
    .await;
    assert_eq!(
        fired_text(&endpoint).await,
        "Read https://good.example/x (https://evil.example/x) first"
    );
    h.slash(
        "U0HUMAN01",
        "cloud run agent-core Mail <mailto:a@example.com|a@example.com> about <https://x.example|docs &amp; notes>",
    )
    .await;
    assert_eq!(
        fired_text(&endpoint).await,
        "Mail a@example.com (mailto:a@example.com) about docs & notes (https://x.example)"
    );
}

#[test]
fn slack_tokens_are_rewritten_only_where_slack_put_them() {
    assert_eq!(slack_task("plain &amp; simple").unwrap(), "plain & simple");
    assert_eq!(
        slack_task("<@U1|a&amp;b> <#C1|c> <https://x.example|> <tel:+1555|call>").unwrap(),
        "@a&b #c https://x.example call (tel:+1555)"
    );
    assert_eq!(slack_task("a &gt; b &lt; c").unwrap(), "a > b < c");
    assert_eq!(slack_task("&lt;@U1|x&gt;").unwrap(), "<@U1|x>");
    for refused in [
        "<!channel>",
        "<@U1|>",
        "<#C1>",
        "<x>",
        "<https://a b>",
        "<:x>",
        "a <b",
    ] {
        assert_eq!(slack_task(refused), Err(SLACK_TOKEN_REFUSED), "{refused}");
    }
}

#[tokio::test]
async fn cloud_notifier_uses_the_defaults_without_cloud_config() {
    let h = harness().await;
    let (alice, _) = h.linked_member("alice", "claude_max").await;
    let t0 = second();
    begin(&h.store, alice, &key("alice"), t0).await;
    let notifier = CloudNotifier::new(h.store.clone(), h.commands.replies().clone(), None);
    let pass = notifier
        .pass_at(clock(t0 + time::Duration::seconds(60)))
        .await
        .unwrap();
    assert_eq!(pass.marked, 0, "twice the default 30 seconds hasn't passed");
    let pass = notifier
        .pass_at(clock(t0 + time::Duration::seconds(61)))
        .await
        .unwrap();
    assert_eq!((pass.marked, pass.told), (1, 1));
    let pass = notifier
        .pass_at(clock(t0 + time::Duration::days(90)))
        .await
        .unwrap();
    assert_eq!(pass.purged, 0, "kept for the default 90 days");
    let pass = notifier
        .pass_at(clock(
            t0 + time::Duration::days(90) + time::Duration::seconds(1),
        ))
        .await
        .unwrap();
    assert_eq!(pass.purged, 1);
    assert!(handoffs(&h.store, alice).await.is_empty());

    let config = CloudConfig {
        retention_days: 1,
        ..CloudConfig::default()
    };
    begin(&h.store, alice, &key("alice"), t0).await;
    let short = CloudNotifier::new(h.store.clone(), h.commands.replies().clone(), Some(&config));
    let pass = short
        .pass_at(clock(t0 + time::Duration::days(2)))
        .await
        .unwrap();
    assert_eq!(
        (pass.marked, pass.told, pass.purged),
        (1, 1, 1),
        "the configured retention, once the notice went out"
    );
}

#[tokio::test]
async fn a_routine_url_on_another_origin_is_refused() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    let other = MockServer::start().await;
    for url in [
        format!("https://api.anthropic.com{}", fire_path()),
        routine_url(&other),
    ] {
        c.h.dm("alice", &format!("cloud add {LABEL} {url} {TOKEN}"))
            .await;
        let reply = c.h.last_reply("alice");
        assert!(
            reply.starts_with(&format!(
                "That URL isn't on the routine endpoint this agentd fires, `{}`",
                c.endpoint.uri()
            )),
            "{reply}"
        );
    }
    assert!(c.h.store.cloud_routines(alice).await.unwrap().is_empty());
    let added = c.add("alice").await;
    assert!(added.starts_with("Registered routine"), "{added}");
}

#[tokio::test]
async fn a_routine_registered_for_another_origin_is_refused_before_it_is_written() {
    let c = cloud().await;
    let alice = c.linked("alice").await;
    mount_fire(&c.endpoint, started(), 0).await;
    put_routine(
        &c.h.store,
        alice,
        "https://api.anthropic.com",
        &key("alice"),
    )
    .await;
    let reply = c.run("alice", "Fix the flaky test").await;
    assert!(
        reply.starts_with(&format!(
            "Routine `{LABEL}` was registered for another routine endpoint than the one this \
             agentd fires now (`[cloud] base_url` changed), so I sent its token nowhere"
        )),
        "{reply}"
    );
    assert!(
        reply.contains(&format!("`cloud add {LABEL} <url> <token>`")),
        "{reply}"
    );
    assert!(handoffs(&c.h.store, alice).await.is_empty());
    c.h.dm("alice", "cloud list").await;
    assert!(
        c.h.last_reply("alice")
            .contains("registered for another routine endpoint than this agentd fires"),
    );

    put_routine(
        &c.h.store,
        alice,
        &format!("{}/", c.endpoint.uri()),
        &key("alice"),
    )
    .await;
    c.endpoint.verify().await;
    c.endpoint.reset().await;
    mount_fire(&c.endpoint, started(), 1).await;
    let reply = c.run("alice", "Fix the flaky test").await;
    assert!(
        reply.starts_with("Started"),
        "an origin compares parsed: {reply}"
    );
}

#[tokio::test]
async fn the_link_is_never_posted_outside_the_private_reply() {
    let logs = global_logs().tag();
    let (h, endpoint, _) = slack_cloud().await;
    mount_fire(&endpoint, started(), 2).await;
    let (response_url, hook) = h.response_url();
    let origin = Origin::SlackSlash {
        response_url,
        conv: slack_channel("C0CHAN001"),
    };
    h.commands
        .handle_text(
            &slack_key("U0HUMAN01"),
            "cloud run agent-core Fix the flaky test",
            &origin,
            &[],
        )
        .await;
    let link = session_url();
    let carrying: Vec<String> = h
        .requests()
        .await
        .into_iter()
        .filter(|request| String::from_utf8_lossy(&request.body).contains(&link))
        .map(|request| request.url.path().to_owned())
        .collect();
    assert_eq!(carrying, std::slice::from_ref(&hook));
    let hooked = h
        .requests()
        .await
        .into_iter()
        .find(|request| request.url.path() == hook)
        .unwrap();
    assert_eq!(json_body(&hooked)["response_type"], "ephemeral");
    assert!(h.posts().await.is_empty());

    let running = Running::start(&h);
    running
        .send(SlackInbound::Message(
            Box::new(dm_event("U0HUMAN01", "cloud run agent-core Fix it again")),
            InFlight::untracked(),
        ))
        .await;
    running.stop().await;
    let posts = h.posts().await;
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert_eq!(posts[0].0, "D0DM00001");
    assert!(posts[0].1.contains(&link), "{posts:?}");
    logs.snapshot().assert_lacks(&link);
}

#[tokio::test]
async fn each_refusal_gets_its_line_and_never_the_endpoints_words() {
    let c = cloud().await;
    c.linked("alice").await;
    c.add("alice").await;
    let error = |status: u16, kind: &str| {
        ResponseTemplate::new(status).set_body_json(json!({
            "type": "error",
            "error": {"type": kind, "message": "the endpoint's own words"},
        }))
    };
    let cases = [
        (error(400, "invalid_request_error"), "It may be paused"),
        (
            error(401, "authentication_error"),
            "Generate a new token on its API trigger",
        ),
        (
            error(403, "permission_error"),
            "the account can't fire routines, or something in between refused the request",
        ),
        (
            error(404, "not_found_error"),
            "It may have been deleted; if so, remove it with `cloud rm agent-core`",
        ),
        (
            error(429, "rate_limit_error").insert_header("retry-after", "125"),
            "It resets in about 3 minutes; I don't retry on my own",
        ),
        (error(429, "rate_limit_error"), "Try again later"),
    ];
    for (response, line) in cases {
        c.endpoint.reset().await;
        mount_fire(&c.endpoint, response, 1).await;
        let reply = c.run("alice", "Fix the flaky test").await;
        assert!(reply.starts_with("Nothing was started"), "{reply}");
        assert!(reply.contains(line), "{line}: {reply}");
        assert!(!reply.contains("_error"), "{reply}");
        assert!(!reply.contains("own words"), "{reply}");
    }
}

#[tokio::test]
async fn a_connection_that_failed_first_says_nothing_started() {
    let h = harness().await;
    let listener = tokio::net::TcpSocket::new_v4().unwrap();
    listener.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let fire = FireClient::new(&CloudConfig {
        base_url: base.clone(),
        ..CloudConfig::default()
    })
    .unwrap();
    let commands = h.commands.clone().with_cloud(fire);
    let (alice, _) = h.linked_member("alice", "claude_max").await;
    put_routine(&h.store, alice, &base, &key("alice")).await;
    let origin = Origin::RocketChatDm {
        room: dm_room("alice").into(),
    };
    commands
        .handle_text(&key("alice"), "cloud run agent-core x", &origin, &[])
        .await;
    assert!(
        h.last_reply("alice")
            .starts_with("Nothing was started: the request couldn't be sent"),
    );
    let handoff = &handoffs(&h.store, alice).await[0].handoff;
    assert_eq!(handoff.state, CloudHandoffState::Rejected);
    assert_eq!(handoff.http_status, None);
}

#[tokio::test]
async fn a_new_token_replaces_the_routine_and_a_routine_is_registered_once() {
    let logs = global_logs().tag();
    let c = cloud().await;
    let alice = c.linked("alice").await;
    c.add("alice").await;
    let url = routine_url(&c.endpoint);
    c.h.dm("alice", &format!("cloud add {LABEL} {url} {NEW_TOKEN}"))
        .await;
    assert!(c.h.last_reply("alice").starts_with(&format!(
        "Replaced routine `{LABEL}`: it now fires `{ROUTINE}`"
    )),);
    c.h.dm("alice", &format!("cloud add other {url} {NEW_TOKEN}"))
        .await;
    assert!(
        c.h.last_reply("alice")
            .starts_with(&format!("You registered that routine as `{LABEL}` already")),
    );
    for n in 1..store::MAX_CLOUD_ROUTINES {
        let url = url.replace(ROUTINE, &format!("trig_{n}"));
        c.h.dm("alice", &format!("cloud add r{n} {url} {NEW_TOKEN}"))
            .await;
    }
    c.h.dm(
        "alice",
        &format!(
            "cloud add full {} {NEW_TOKEN}",
            url.replace(ROUTINE, "trig_full")
        ),
    )
    .await;
    let reply = c.h.last_reply("alice");
    assert!(
        reply.starts_with("You have 20 routines, the most one member may hold"),
        "{reply}"
    );
    assert_eq!(c.h.store.cloud_routines(alice).await.unwrap().len(), 20);
    let token =
        c.h.store
            .cloud_routine(alice, LABEL)
            .await
            .unwrap()
            .unwrap()
            .token;
    assert_eq!(token.expose_secret(), NEW_TOKEN);
    logs.snapshot()
        .assert_has("registered a cloud routine")
        .assert_lacks(NEW_TOKEN)
        .assert_lacks(TOKEN);
}

#[tokio::test]
async fn cloud_list_shows_routines_and_the_last_ten_handoffs() {
    let mut c = cloud().await;
    c.h.commands =
        c.h.commands
            .clone()
            .with_cloud(fire_client_capped(&c.endpoint, 100));
    let alice = c.linked("alice").await;
    c.h.dm("alice", "cloud list").await;
    let empty = c.h.last_reply("alice");
    assert!(
        empty.starts_with("You have no routines. Register one with `cloud add"),
        "{empty}"
    );
    assert!(empty.ends_with("I don't follow sessions. Follow yours at claude.ai/code."));
    c.h.dm("bob", "cloud list").await;
    assert!(c.h.last_reply("bob").starts_with("You have no routines."));

    c.add("alice").await;
    mount_fire(&c.endpoint, started(), 11).await;
    for n in 0..11 {
        c.run("alice", &format!("Task {n}")).await;
    }
    c.endpoint.reset().await;
    mount_fire(&c.endpoint, ResponseTemplate::new(401), 1).await;
    let long = format!("Use `x` here {}\nsecond line", "y".repeat(80));
    c.run("alice", &long).await;
    c.h.dm("alice", "cloud list").await;
    let listed = c.h.last_reply("alice");
    assert!(
        listed.starts_with(&format!("Your routines:\n- `{LABEL}`: `{ROUTINE}`\n\n")),
        "{listed}"
    );
    let lines: Vec<&str> = listed
        .lines()
        .skip_while(|line| !line.starts_with("Your last hand-offs"))
        .skip(1)
        .take_while(|line| line.starts_with("- "))
        .collect();
    assert_eq!(lines.len(), 10, "{listed}");
    assert!(
        lines[0].ends_with(&format!(
            "`{LABEL}`: refused (HTTP 401), nothing started. Task: `Use x here {}…`",
            "y".repeat(49)
        )),
        "{}",
        lines[0]
    );
    assert!(
        lines[1].ends_with(&format!(
            "`{LABEL}`: started {}. Task: `Task 10`",
            session_url()
        )),
        "{}",
        lines[1]
    );
    assert!(!listed.contains("Task 1`"), "the oldest isn't listed");
    assert!(!listed.contains("second line"));
    assert_eq!(handoffs(&c.h.store, alice).await.len(), 12);
}

#[test]
fn a_tasks_first_line_is_cut_and_shown_as_code() {
    assert_eq!(task_line("short"), "`short`");
    assert_eq!(task_line("a`b`c\nrest"), "`abc`");
    assert_eq!(task_line("``"), "(nothing to show)");
    let long: String = "é".repeat(61);
    assert_eq!(task_line(&long), format!("`{}…`", "é".repeat(60)));
    assert_eq!(task_line(&"é".repeat(60)), format!("`{}`", "é".repeat(60)));
}

#[test]
fn a_retry_after_reads_in_whole_minutes_or_hours() {
    assert_eq!(wait_in_words(0), "a minute");
    assert_eq!(wait_in_words(59), "a minute");
    assert_eq!(wait_in_words(61), "2 minutes");
    assert_eq!(wait_in_words(3600), "60 minutes");
    assert_eq!(wait_in_words(3601), "61 minutes");
    assert_eq!(wait_in_words(7200), "120 minutes");
    assert_eq!(wait_in_words(7201), "2 hours");
    assert_eq!(wait_in_words(9_000), "3 hours");
    assert_eq!(wait_in_words(86_400), "24 hours");
    assert_eq!(wait_in_words(u32::MAX), "1193046 hours");
}

#[test]
fn a_session_without_its_link_is_shown_by_id() {
    let origin = Origin::RocketChatDm { room: "d".into() };
    let label: RoutineLabel = LABEL.parse().unwrap();
    let reply = outcome_reply(
        &label,
        &CloudOutcome::Fired {
            session_id: SESSION.to_owned(),
            session_url: None,
        },
        &origin,
    );
    assert!(
        reply.starts_with(&format!(
            "Started cloud session `{SESSION}` from routine `{LABEL}`. Find it at \
             https://claude.ai/code\n"
        )),
        "{reply}"
    );
    let reply = outcome_reply(
        &label,
        &CloudOutcome::Rejected {
            status: Some(418),
            error_type: Some("teapot_error".to_owned()),
            retry_after_secs: None,
        },
        &origin,
    );
    assert_eq!(
        reply,
        "Nothing was started: the routine endpoint refused the request (HTTP 418)."
    );
}

/// Runs `statements` on the SQLite database at `url`.
/// A Slack harness on a database file, whose commands fire routines at
/// `endpoint`, with `U0HUMAN01` linked and the routine registered; the
/// database's URL and directory.
async fn slack_cloud_on_file(endpoint: &MockServer) -> (SlackHarness, String, std::path::PathBuf) {
    let (store, url, dir) = file_store().await;
    let mut h = slack_harness_on(store).await;
    h.commands = h.commands.clone().with_cloud(fire_client(endpoint));
    h.linked("U0HUMAN01").await;
    let added = h
        .slash(
            "U0HUMAN01",
            &format!("cloud add {LABEL} {} {TOKEN}", routine_url(endpoint)),
        )
        .await;
    assert!(added[0].starts_with("Registered routine"), "{added:?}");
    (h, url, dir)
}

#[tokio::test]
async fn a_stored_token_that_no_longer_opens_asks_for_cloud_add_again() {
    let endpoint = MockServer::start().await;
    let (h, url, dir) = slack_cloud_on_file(&endpoint).await;
    sql(
        &url,
        "UPDATE cloud_routines SET url_origin = 'https://moved.example'",
    )
    .await;
    let reply = h
        .slash("U0HUMAN01", "cloud run agent-core Fix the flaky test")
        .await;
    assert_eq!(
        reply,
        [format!(
            "Routine `{LABEL}`'s stored token can't be used any more, so I didn't start \
             anything. Register it again with `/agent cloud add {LABEL} &lt;url&gt; \
             &lt;token&gt;`."
        )]
    );
    assert!(fires(&endpoint).await.is_empty());
    let replaced = h
        .slash(
            "U0HUMAN01",
            &format!("cloud add {LABEL} {} {NEW_TOKEN}", routine_url(&endpoint)),
        )
        .await;
    assert!(replaced[0].starts_with("Replaced routine"), "{replaced:?}");
    mount_fire(&endpoint, started(), 1).await;
    let reply = h
        .slash("U0HUMAN01", "cloud run agent-core Fix the flaky test")
        .await;
    assert!(reply[0].starts_with("Started"), "{reply:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_store_failing_around_the_request_never_hides_the_link() {
    let endpoint = MockServer::start().await;
    let (h, url, dir) = slack_cloud_on_file(&endpoint).await;
    let alice = h
        .store
        .member_for_identity(&slack_key("U0HUMAN01"))
        .await
        .unwrap()
        .unwrap();
    sql(
        &url,
        "CREATE TRIGGER no_handoffs BEFORE INSERT ON cloud_handoffs \
         BEGIN SELECT RAISE(FAIL, 'injected'); END;",
    )
    .await;
    let reply = h
        .slash("U0HUMAN01", "cloud run agent-core Fix the flaky test")
        .await;
    assert_eq!(
        reply,
        ["Nothing was started: I couldn't record the hand-off. Try again in a minute."]
    );
    assert!(fires(&endpoint).await.is_empty());

    sql(
        &url,
        "DROP TRIGGER no_handoffs; CREATE TRIGGER no_outcomes BEFORE UPDATE OF state ON \
         cloud_handoffs BEGIN SELECT RAISE(FAIL, 'injected'); END;",
    )
    .await;
    mount_fire(&endpoint, started(), 1).await;
    let reply = h
        .slash("U0HUMAN01", "cloud run agent-core Fix the flaky test")
        .await;
    assert!(reply[0].contains(&session_url()), "{reply:?}");
    let recorded = handoffs(&h.store, alice).await;
    assert_eq!(recorded[0].handoff.state, CloudHandoffState::Sending);

    sql(&url, "DROP TRIGGER no_outcomes;").await;
    let notifier = CloudNotifier::new(h.store.clone(), h.commands.replies().clone(), None);
    let pass = notifier
        .pass_at(clock(second() + time::Duration::minutes(2)))
        .await
        .unwrap();
    assert_eq!((pass.marked, pass.told), (1, 1));
    let posts = h.posts().await;
    assert!(posts[0].1.contains("`/agent cloud list`"), "{posts:?}");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_notice_the_store_fails_on_leaves_the_others_and_the_purge() {
    let endpoint = MockServer::start().await;
    let (h, url, dir) = slack_cloud_on_file(&endpoint).await;
    let alice = h
        .store
        .member_for_identity(&slack_key("U0HUMAN01"))
        .await
        .unwrap()
        .unwrap();
    let grace = h.linked("U0HUMAN02").await;
    let t0 = second();
    begin(&h.store, alice, &slack_key("U0HUMAN01"), t0).await;
    begin(&h.store, grace, &slack_key("U0HUMAN02"), t0).await;
    let long_ago = t0 - time::Duration::days(91);
    let old = begin(&h.store, grace, &slack_key("U0HUMAN02"), long_ago).await;
    h.store
        .finish_cloud_handoff(
            old,
            &CloudOutcome::Rejected {
                status: Some(404),
                error_type: None,
                retry_after_secs: None,
            },
            long_ago,
        )
        .await
        .unwrap();
    sql(
        &url,
        &format!(
            "CREATE TRIGGER no_claim BEFORE UPDATE OF notice_attempts ON cloud_handoffs \
             WHEN OLD.member_id = '{alice}' BEGIN SELECT RAISE(FAIL, 'injected'); END;"
        ),
    )
    .await;
    let notifier = CloudNotifier::new(h.store.clone(), h.commands.replies().clone(), None);
    let failed = notifier
        .pass_at(clock(t0 + time::Duration::seconds(61)))
        .await;
    assert!(failed.is_err(), "{failed:?}");
    let posts = h.posts().await;
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert!(posts[0].1.starts_with("Your cloud hand-off"), "{posts:?}");
    let left: Vec<_> = handoffs(&h.store, grace)
        .await
        .into_iter()
        .map(|recent| recent.handoff.id)
        .collect();
    assert!(!left.contains(&old), "the purge ran");
    assert!(
        handoffs(&h.store, alice).await[0]
            .handoff
            .notified_at
            .is_none()
    );

    sql(&url, "DROP TRIGGER no_claim;").await;
    let pass = notifier
        .pass_at(clock(t0 + time::Duration::seconds(62)))
        .await
        .unwrap();
    assert_eq!((pass.marked, pass.told), (0, 1));
    assert!(
        handoffs(&h.store, alice).await[0]
            .handoff
            .notified_at
            .is_some()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn a_task_line_in_cloud_list_on_slack_formats_and_pings_nothing() {
    let (h, endpoint, _) = slack_cloud().await;
    mount_fire(&endpoint, started(), 1).await;
    h.slash(
        "U0HUMAN01",
        "cloud run agent-core &lt;!here&gt; *loud* _x_ @grace &lt;https://evil.example|docs&gt; `y`",
    )
    .await;
    assert_eq!(
        fired_text(&endpoint).await,
        "<!here> *loud* _x_ @grace <https://evil.example|docs> `y`"
    );
    let listed = h.slash("U0HUMAN01", "cloud list").await.remove(0);
    let line = listed
        .lines()
        .find(|line| line.contains("Task: "))
        .unwrap_or_else(|| panic!("{listed}"));
    assert!(
        line.ends_with(
            "Task: `&lt;!here&gt; *loud* _x_ @grace &lt;https://evil.example|docs&gt; y`"
        ),
        "{line}"
    );
    assert!(!listed.contains("<!here>"), "{listed}");
    assert!(!listed.contains("<https://evil"), "{listed}");
}

#[tokio::test]
async fn a_slack_dm_points_to_the_slash_command_to_add_a_routine() {
    let endpoint = MockServer::start().await;
    let mut h = slack_harness().await;
    h.commands = h.commands.clone().with_cloud(fire_client(&endpoint));
    h.linked("U0HUMAN01").await;
    let running = Running::start(&h);
    for text in ["cloud list", "cloud run agent-core Fix it"] {
        running
            .send(SlackInbound::Message(
                Box::new(dm_event("U0HUMAN01", text)),
                InFlight::untracked(),
            ))
            .await;
    }
    running.stop().await;
    let posts = h.posts().await;
    assert_eq!(posts.len(), 2, "{posts:?}");
    assert!(
        posts[1]
            .1
            .starts_with("You have no routine `agent-core`. `cloud list` shows yours"),
        "{posts:?}"
    );
    for (_, text) in &posts {
        assert!(text.contains("`/agent cloud add "), "{text}");
        assert!(!text.contains("`cloud add"), "{text}");
    }
}

#[tokio::test]
async fn a_logout_that_failed_after_unlinking_forgets_the_routines_when_sent_again() {
    let endpoint = MockServer::start().await;
    let (h, url, dir) = slack_cloud_on_file(&endpoint).await;
    let alice = h
        .store
        .member_for_identity(&slack_key("U0HUMAN01"))
        .await
        .unwrap()
        .unwrap();
    sql(
        &url,
        "CREATE TRIGGER no_forgetting BEFORE DELETE ON cloud_routines \
         BEGIN SELECT RAISE(FAIL, 'injected'); END;",
    )
    .await;
    assert_eq!(h.slash("U0HUMAN01", "logout").await, [FAILED.to_owned()]);
    assert!(h.store.get_claude_link(alice).await.unwrap().is_none());
    assert_eq!(h.store.cloud_routines(alice).await.unwrap().len(), 1);
    sql(&url, "DROP TRIGGER no_forgetting;").await;
    let reply = h.slash("U0HUMAN01", "logout").await.remove(0);
    assert!(reply.starts_with("No Claude account is linked."), "{reply}");
    assert!(reply.contains("I also forgot your 1 routine."), "{reply}");
    assert!(
        reply.contains("I can't revoke a routine's token: revoke each"),
        "{reply}"
    );
    assert!(h.store.cloud_routines(alice).await.unwrap().is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

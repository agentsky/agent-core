use core_types::{SurfaceKind, TeamId, UserId};
use secrecy::ExposeSecret;

use super::*;
use crate::SealError;
use crate::claude_links::tests::new_link;
use crate::test_util::*;

/// A member with a Claude link, as `cloud add` needs.
async fn member(store: &Store, user: &str) -> MemberId {
    let member = unlinked_member(store, user).await;
    store
        .put_claude_link(member, &new_link("a", "r"), at(1))
        .await
        .unwrap();
    member
}

async fn unlinked_member(store: &Store, user: &str) -> MemberId {
    store
        .ensure_member(&member_key(user), user, at(1))
        .await
        .unwrap()
}

const API: &str = "https://api.anthropic.com";

fn routine(id: &str) -> RoutineId {
    id.parse().unwrap()
}

fn token(text: &str) -> RoutineToken {
    RoutineToken::parse(SecretString::from(format!("sk-ant-{text}"))).unwrap()
}

fn shown(token: &RoutineToken) -> &str {
    token.expose_secret().strip_prefix("sk-ant-").unwrap()
}

fn slack_key(user: &str) -> MemberKey {
    MemberKey {
        surface: SurfaceKind::Slack,
        team: TeamId::new("T1"),
        user: UserId::new(user),
    }
}

async fn put(
    store: &Store,
    member: MemberId,
    label: &str,
    id: &str,
    secret: &str,
    now: i64,
) -> CloudRoutinePut {
    store
        .put_cloud_routine(
            &NewCloudRoutine {
                member,
                label,
                routine_id: &routine(id),
                url_origin: API,
                token: &token(secret),
                added_by: &member_key("ada"),
            },
            at(now),
        )
        .await
        .unwrap()
}

async fn opened(store: &Store, member: MemberId, label: &str) -> Option<(RoutineId, String)> {
    store
        .cloud_routine(member, label)
        .await
        .unwrap()
        .map(|found| (found.routine_id, shown(&found.token).to_owned()))
}

/// The registration of `member`'s routine `label` stored now, or none.
async fn registration(store: &Store, member: MemberId, label: &str) -> CloudRoutineVersion {
    store
        .cloud_routine(member, label)
        .await
        .unwrap()
        .map(|found| found.version)
        .unwrap_or_default()
}

async fn begin(store: &Store, member: MemberId, task: &str, now: i64) -> CloudHandoffId {
    let routine_id = routine("trig_1");
    let held = store
        .cloud_routines(member)
        .await
        .unwrap()
        .into_iter()
        .find(|held| held.routine_id == routine_id);
    let label = match held {
        Some(held) => held.label,
        None => {
            put(store, member, "agent-core", "trig_1", "sk", now).await;
            "agent-core".to_owned()
        }
    };
    let requested_by = member_key("ada");
    let registration = registration(store, member, &label).await;
    let begun = store
        .begin_cloud_handoff(
            &NewCloudHandoff {
                member,
                routine_label: &label,
                routine_id: &routine_id,
                registration: &registration,
                requested_by: &requested_by,
                origin: CloudOrigin::RocketChatDm,
                task,
            },
            u32::MAX,
            at(now),
        )
        .await
        .unwrap();
    let CloudBegun::Begun(id) = begun else {
        panic!("{begun:?}");
    };
    id
}

async fn handoff(store: &Store, member: MemberId, id: CloudHandoffId) -> CloudHandoff {
    store
        .recent_cloud_handoffs(member, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|recent| recent.handoff)
        .find(|handoff| handoff.id == id)
        .unwrap()
}

fn fired(session: &str) -> CloudOutcome {
    CloudOutcome::Fired {
        session_id: session.to_owned(),
        session_url: Some(format!("https://claude.ai/code/{session}")),
    }
}

fn rejected(status: u16) -> CloudOutcome {
    CloudOutcome::Rejected {
        status: Some(status),
        error_type: Some("rate_limit_error".to_owned()),
        retry_after_secs: Some(120),
    }
}

async fn finish(store: &Store, id: CloudHandoffId, outcome: &CloudOutcome, now: i64) -> bool {
    store
        .finish_cloud_handoff(id, outcome, at(now))
        .await
        .unwrap()
        == CloudFinished::Recorded
}

async fn due(store: &Store, now: i64) -> Vec<CloudHandoffId> {
    store
        .due_cloud_handoff_notices(at(now))
        .await
        .unwrap()
        .into_iter()
        .map(|handoff| handoff.id)
        .collect()
}

async fn claim(store: &Store, id: CloudHandoffId, now: i64) -> Option<u32> {
    store.claim_cloud_handoff_notice(id, at(now)).await.unwrap()
}

/// A hand-off asked at 100 that a pass marked `unknown` at `answered`.
async fn unknown(store: &Store, member: MemberId, answered: i64) -> CloudHandoffId {
    let id = begin(store, member, "task", 100).await;
    let stale = store
        .stale_cloud_handoffs(at(101), at(answered))
        .await
        .unwrap();
    assert!(stale.iter().any(|handoff| handoff.id == id));
    id
}

#[tokio::test]
async fn a_routine_is_registered_listed_and_opened() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let CloudRoutinePut::Added(id) = put(&store, ada, "agent-core", "trig_1", "sk-1", 10).await
    else {
        panic!()
    };
    put(&store, ada, "Docs.site/x", "trig_2", "sk-2", 11).await;
    let found = store
        .cloud_routine(ada, "agent-core")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, id);
    assert_eq!(found.routine_id, routine("trig_1"));
    assert_eq!(shown(&found.token), "sk-1");
    assert!(!format!("{found:?}").contains("sk-1"));
    assert_eq!(
        store.cloud_routines(ada).await.unwrap(),
        [
            CloudRoutine {
                id: store
                    .cloud_routine(ada, "Docs.site/x")
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
                label: "Docs.site/x".to_owned(),
                routine_id: routine("trig_2"),
                url_origin: API.to_owned(),
                added_by: member_key("ada"),
                added_at: at(11),
            },
            CloudRoutine {
                id,
                label: "agent-core".to_owned(),
                routine_id: routine("trig_1"),
                url_origin: API.to_owned(),
                added_by: member_key("ada"),
                added_at: at(10),
            },
        ]
    );
    assert!(store.cloud_routine(ada, "nope").await.unwrap().is_none());
    let bob = member(&store, "bob").await;
    assert!(
        store
            .cloud_routine(bob, "agent-core")
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.cloud_routines(bob).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_routine_label_is_replaced_in_place() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let CloudRoutinePut::Added(id) = put(&store, ada, "agent-core", "trig_1", "old", 10).await
    else {
        panic!()
    };
    assert_eq!(
        store
            .put_cloud_routine(
                &NewCloudRoutine {
                    member: ada,
                    label: "agent-core",
                    routine_id: &routine("trig_1"),
                    url_origin: API,
                    token: &token("new"),
                    added_by: &slack_key("U1")
                },
                at(20),
            )
            .await
            .unwrap(),
        CloudRoutinePut::Replaced(id)
    );
    assert_eq!(
        opened(&store, ada, "agent-core").await,
        Some((routine("trig_1"), "new".to_owned()))
    );
    let listed = store.cloud_routines(ada).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, id);
    assert_eq!(listed[0].added_by, slack_key("U1"));
    assert_eq!(listed[0].added_at, at(20));

    assert_eq!(
        put(&store, ada, "agent-core", "trig_9", "newer", 30).await,
        CloudRoutinePut::Replaced(id),
        "a label may move to another routine"
    );
    assert_eq!(
        opened(&store, ada, "agent-core").await,
        Some((routine("trig_9"), "newer".to_owned()))
    );
}

#[tokio::test]
async fn a_routine_id_is_registered_once_per_member() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    put(&store, ada, "one", "trig_1", "a", 10).await;
    put(&store, ada, "two", "trig_2", "b", 10).await;
    assert_eq!(
        put(&store, ada, "three", "trig_1", "c", 11).await,
        CloudRoutinePut::RoutineTaken {
            label: "one".to_owned()
        }
    );
    assert_eq!(
        put(&store, ada, "two", "trig_1", "c", 11).await,
        CloudRoutinePut::RoutineTaken {
            label: "one".to_owned()
        },
        "a label replaced in place can't take another label's routine"
    );
    assert!(store.cloud_routine(ada, "three").await.unwrap().is_none());
    assert_eq!(
        opened(&store, ada, "two").await,
        Some((routine("trig_2"), "b".to_owned()))
    );
    assert_eq!(
        opened(&store, ada, "one").await,
        Some((routine("trig_1"), "a".to_owned()))
    );

    let bob = member(&store, "bob").await;
    assert!(matches!(
        put(&store, bob, "theirs", "trig_1", "d", 12).await,
        CloudRoutinePut::Added(_)
    ));
}

#[tokio::test]
async fn the_twenty_first_routine_is_refused() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    for n in 0..MAX_CLOUD_ROUTINES {
        assert!(matches!(
            put(&store, ada, &format!("r{n}"), &format!("trig_{n}"), "t", 10).await,
            CloudRoutinePut::Added(_)
        ));
    }
    assert_eq!(
        put(&store, ada, "r20", "trig_20", "t", 11).await,
        CloudRoutinePut::Full
    );
    assert!(store.cloud_routine(ada, "r20").await.unwrap().is_none());
    assert!(matches!(
        put(&store, ada, "r3", "trig_99", "t", 12).await,
        CloudRoutinePut::Replaced(_)
    ));
    let bob = member(&store, "bob").await;
    assert!(matches!(
        put(&store, bob, "r20", "trig_20", "t", 12).await,
        CloudRoutinePut::Added(_)
    ));
    assert!(store.delete_cloud_routine(ada, "r0").await.unwrap());
    assert!(matches!(
        put(&store, ada, "r20", "trig_20", "t", 13).await,
        CloudRoutinePut::Added(_)
    ));
}

#[tokio::test]
async fn a_routine_is_refused_without_a_claude_link() {
    let store = memory_store().await;
    let ada = unlinked_member(&store, "ada").await;
    assert_eq!(
        put(&store, ada, "agent-core", "trig_1", "t", 10).await,
        CloudRoutinePut::Unlinked
    );
    let bob = member(&store, "bob").await;
    put(&store, bob, "agent-core", "trig_1", "t", 10).await;
    assert!(store.delete_claude_link(bob).await.unwrap());
    assert_eq!(
        put(&store, bob, "agent-core", "trig_2", "u", 11).await,
        CloudRoutinePut::Unlinked
    );
    assert_eq!(
        put(&store, bob, "other", "trig_3", "v", 11).await,
        CloudRoutinePut::Unlinked
    );
    assert!(store.cloud_routines(ada).await.unwrap().is_empty());
    assert_eq!(
        opened(&store, bob, "agent-core").await,
        Some((routine("trig_1"), "t".to_owned()))
    );
    assert_eq!(store.cloud_routines(bob).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_handoff_is_refused_without_a_claude_link() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    put(&store, ada, "agent-core", "trig_1", "sk", 10).await;
    let registration = registration(&store, ada, "agent-core").await;
    assert!(store.delete_claude_link(ada).await.unwrap());
    let begun = store
        .begin_cloud_handoff(
            &NewCloudHandoff {
                member: ada,
                routine_label: "agent-core",
                routine_id: &routine("trig_1"),
                registration: &registration,
                requested_by: &member_key("ada"),
                origin: CloudOrigin::RocketChatDm,
                task: "Fix the flaky test",
            },
            u32::MAX,
            at(20),
        )
        .await
        .unwrap();
    assert_eq!(begun, CloudBegun::Unlinked);
    assert!(
        store
            .recent_cloud_handoffs(ada, 100)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn concurrent_registrations_never_pass_the_cap_together() {
    let dir = TempDir::new("store-test");
    let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
    let ada = member(&store, "ada").await;
    let mut tasks = tokio::task::JoinSet::new();
    for n in 0..(MAX_CLOUD_ROUTINES + 10) {
        let store = store.clone();
        tasks.spawn(async move {
            store
                .put_cloud_routine(
                    &NewCloudRoutine {
                        member: ada,
                        label: &format!("r{n}"),
                        routine_id: &routine(&format!("trig_{n}")),
                        url_origin: API,
                        token: &token("t"),
                        added_by: &member_key("ada"),
                    },
                    at(10),
                )
                .await
                .unwrap()
        });
    }
    let mut added = 0;
    while let Some(put) = tasks.join_next().await {
        match put.unwrap() {
            CloudRoutinePut::Added(_) => added += 1,
            CloudRoutinePut::Full => {}
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(added, MAX_CLOUD_ROUTINES);
    assert_eq!(
        store.cloud_routines(ada).await.unwrap().len(),
        usize::try_from(MAX_CLOUD_ROUTINES).unwrap()
    );
}

#[tokio::test]
async fn a_routine_token_is_sealed_to_its_row() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    put(&store, ada, "one", "trig_1", "sk-ant-oat01-ONE", 10).await;
    put(&store, ada, "two", "trig_2", "sk-ant-oat01-TWO", 10).await;
    let raw: Vec<Vec<u8>> = sqlx::query_scalar("SELECT token_enc FROM cloud_routines")
        .fetch_all(&store.pool)
        .await
        .unwrap();
    for sealed in raw {
        assert!(!String::from_utf8_lossy(&sealed).contains("sk-ant"));
    }
    sqlx::query(
        "UPDATE cloud_routines SET token_enc = \
         (SELECT token_enc FROM cloud_routines WHERE label = 'one') WHERE label = 'two'",
    )
    .execute(&store.pool)
    .await
    .unwrap();
    let err = store.cloud_routine(ada, "two").await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Seal {
                table: "cloud_routines",
                column: "token_enc",
                source: SealError::Decrypt,
            }
        ),
        "{err:?}"
    );
    assert!(!format!("{err} {err:?}").contains("ONE"));
    assert_eq!(
        opened(&store, ada, "one").await,
        Some((routine("trig_1"), "sk-ant-oat01-ONE".to_owned()))
    );
    assert_eq!(store.cloud_routines(ada).await.unwrap().len(), 2);
}

#[tokio::test]
async fn a_handoff_task_is_sealed_to_its_row() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let first = begin(&store, ada, "Fix the TASK-ONE bug.\nThen open a PR.", 100).await;
    let second = begin(&store, ada, "TASK-TWO", 101).await;
    let raw: Vec<Vec<u8>> = sqlx::query_scalar("SELECT task_enc FROM cloud_handoffs")
        .fetch_all(&store.pool)
        .await
        .unwrap();
    for sealed in raw {
        assert!(!String::from_utf8_lossy(&sealed).contains("TASK"));
    }
    let recent = store.recent_cloud_handoffs(ada, 10).await.unwrap();
    let tasks: Vec<(CloudHandoffId, &str)> = recent
        .iter()
        .map(|recent| (recent.handoff.id, recent.task.expose_secret()))
        .collect();
    assert_eq!(
        tasks,
        [
            (second, "TASK-TWO"),
            (first, "Fix the TASK-ONE bug.\nThen open a PR.")
        ]
    );
    assert!(!format!("{recent:?}").contains("TASK"));

    sqlx::query(
        "UPDATE cloud_handoffs SET task_enc = \
         (SELECT task_enc FROM cloud_handoffs WHERE id = ?) WHERE id = ?",
    )
    .bind(first.to_string())
    .bind(second.to_string())
    .execute(&store.pool)
    .await
    .unwrap();
    let err = store.recent_cloud_handoffs(ada, 10).await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Seal {
                table: "cloud_handoffs",
                column: "task_enc",
                source: SealError::Decrypt,
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_new_handoff_is_sending_with_what_was_asked() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let routine_id = routine("trig_7");
    let requested_by = slack_key("U7");
    put(&store, ada, "docs", "trig_7", "sk", 10).await;
    let registration = registration(&store, ada, "docs").await;
    let begun = store
        .begin_cloud_handoff(
            &NewCloudHandoff {
                member: ada,
                routine_label: "docs",
                routine_id: &routine_id,
                registration: &registration,
                requested_by: &requested_by,
                origin: CloudOrigin::SlackSlash,
                task: "t",
            },
            1,
            at(500),
        )
        .await
        .unwrap();
    let CloudBegun::Begun(id) = begun else {
        panic!("{begun:?}");
    };
    assert_eq!(
        handoff(&store, ada, id).await,
        CloudHandoff {
            id,
            member: ada,
            routine_label: "docs".to_owned(),
            routine_id,
            requested_by,
            origin: CloudOrigin::SlackSlash,
            state: CloudHandoffState::Sending,
            http_status: None,
            error_type: None,
            retry_after_secs: None,
            session_id: None,
            session_url: None,
            created_at: at(500),
            answered_at: None,
            notice_attempts: 0,
            notified_at: None,
            unknown_reason: None,
        }
    );
}

async fn begin_capped(
    store: &Store,
    member: MemberId,
    label: &str,
    id: &str,
    per_hour: u32,
    now: i64,
) -> CloudBegun {
    let registration = registration(store, member, label).await;
    store
        .begin_cloud_handoff(
            &NewCloudHandoff {
                member,
                routine_label: label,
                routine_id: &routine(id),
                registration: &registration,
                requested_by: &member_key("ada"),
                origin: CloudOrigin::RocketChatDm,
                task: "t",
            },
            per_hour,
            at(now),
        )
        .await
        .unwrap()
}

async fn handoffs_held(store: &Store) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM cloud_handoffs")
        .fetch_one(&store.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_member_past_the_hourly_handoff_cap_records_nothing() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let bob = member(&store, "bob").await;
    put(&store, ada, "r", "trig_1", "sk", 10).await;
    put(&store, ada, "s", "trig_2", "sk", 10).await;
    put(&store, bob, "r", "trig_1", "sk", 10).await;
    let window = i64::try_from(CLOUD_HANDOFF_WINDOW.as_secs()).unwrap();
    assert_eq!(window, 3_600);

    assert!(matches!(
        begin_capped(&store, ada, "r", "trig_1", 2, 1_000).await,
        CloudBegun::Begun(_)
    ));
    assert!(matches!(
        begin_capped(&store, ada, "s", "trig_2", 2, 1_001).await,
        CloudBegun::Begun(_)
    ));
    assert_eq!(
        begin_capped(&store, ada, "r", "trig_1", 2, 1_002).await,
        CloudBegun::TooMany,
        "the cap is the member's, over every routine"
    );
    assert_eq!(
        begin_capped(&store, ada, "r", "trig_1", 2, 1_000 + window - 1).await,
        CloudBegun::TooMany
    );
    assert_eq!(handoffs_held(&store).await, 2);
    assert!(matches!(
        begin_capped(&store, bob, "r", "trig_1", 2, 1_002).await,
        CloudBegun::Begun(_)
    ));
    assert!(
        matches!(
            begin_capped(&store, ada, "r", "trig_1", 2, 1_000 + window).await,
            CloudBegun::Begun(_)
        ),
        "a hand-off an hour old leaves the count"
    );
    assert_eq!(
        begin_capped(&store, ada, "r", "trig_1", 0, 1_000).await,
        CloudBegun::TooMany
    );
}

#[tokio::test]
async fn concurrent_handoffs_never_pass_the_hourly_cap_together() {
    let dir = TempDir::new("store-test");
    let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
    let ada = member(&store, "ada").await;
    put(&store, ada, "r", "trig_1", "sk", 10).await;
    let per_hour = 5;
    for n in 0..per_hour - 1 {
        assert!(matches!(
            begin_capped(&store, ada, "r", "trig_1", per_hour, 100 + i64::from(n)).await,
            CloudBegun::Begun(_)
        ));
    }
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let store = store.clone();
        tasks.spawn(async move { begin_capped(&store, ada, "r", "trig_1", per_hour, 200).await });
    }
    let mut begun = 0;
    while let Some(outcome) = tasks.join_next().await {
        match outcome.unwrap() {
            CloudBegun::Begun(_) => begun += 1,
            CloudBegun::TooMany => {}
            other @ (CloudBegun::RoutineGone | CloudBegun::Unlinked) => panic!("{other:?}"),
        }
    }
    assert_eq!(begun, 1, "one place was left");
    assert_eq!(handoffs_held(&store).await, i64::from(per_hour));
}

#[tokio::test]
async fn a_token_replaced_after_it_was_read_records_nothing() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    put(&store, ada, "r", "trig_1", "old", 10).await;
    let read = store.cloud_routine(ada, "r").await.unwrap().unwrap();
    put(&store, ada, "r", "trig_1", "new", 10).await;
    let begin = |registration: CloudRoutineVersion| {
        let store = store.clone();
        async move {
            store
                .begin_cloud_handoff(
                    &NewCloudHandoff {
                        member: ada,
                        routine_label: "r",
                        routine_id: &routine("trig_1"),
                        registration: &registration,
                        requested_by: &member_key("ada"),
                        origin: CloudOrigin::RocketChatDm,
                        task: "t",
                    },
                    10,
                    at(10),
                )
                .await
                .unwrap()
        }
    };
    assert_eq!(
        begin(read.version).await,
        CloudBegun::RoutineGone,
        "the same label, routine id and second, but another token"
    );
    assert_eq!(handoffs_held(&store).await, 0);
    let current = store.cloud_routine(ada, "r").await.unwrap().unwrap();
    assert!(matches!(begin(current.version).await, CloudBegun::Begun(_)));
}

#[tokio::test]
async fn a_handoff_whose_routine_is_gone_records_nothing() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    assert_eq!(
        begin_capped(&store, ada, "r", "trig_1", 10, 100).await,
        CloudBegun::RoutineGone
    );
    put(&store, ada, "r", "trig_1", "sk", 10).await;
    put(&store, ada, "r", "trig_2", "sk", 11).await;
    assert_eq!(
        begin_capped(&store, ada, "r", "trig_1", 10, 100).await,
        CloudBegun::RoutineGone,
        "replaced under its label"
    );
    let CloudBegun::Begun(id) = begin_capped(&store, ada, "r", "trig_2", 10, 100).await else {
        panic!("not begun");
    };
    store.delete_cloud_routines_of(ada).await.unwrap();
    assert_eq!(
        begin_capped(&store, ada, "r", "trig_2", 10, 101).await,
        CloudBegun::RoutineGone,
        "logged out"
    );
    assert_eq!(handoffs_held(&store).await, 0);
    assert_eq!(
        store
            .finish_cloud_handoff(id, &fired("session_1"), at(102))
            .await
            .unwrap(),
        CloudFinished::Gone,
        "deleted while its request was out"
    );
}

#[tokio::test]
async fn recent_handoffs_are_the_members_newest_first() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let bob = member(&store, "bob").await;
    let mut ids = Vec::new();
    for n in 0..12 {
        ids.push(begin(&store, ada, &format!("task {n}"), 100 + n / 2).await);
    }
    begin(&store, bob, "theirs", 200).await;
    let recent = store.recent_cloud_handoffs(ada, 10).await.unwrap();
    let got: Vec<CloudHandoffId> = recent.iter().map(|recent| recent.handoff.id).collect();
    let want: Vec<CloudHandoffId> = ids.iter().rev().take(10).copied().collect();
    assert_eq!(got, want);
    assert_eq!(recent[0].task.expose_secret(), "task 11");
    assert!(
        store
            .recent_cloud_handoffs(ada, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn routines_of_a_member_are_deleted_by_member_id() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let bob = member(&store, "bob").await;
    put(&store, ada, "rc", "trig_1", "a", 10).await;
    store
        .put_cloud_routine(
            &NewCloudRoutine {
                member: ada,
                label: "slack",
                routine_id: &routine("trig_2"),
                url_origin: API,
                token: &token("b"),
                added_by: &slack_key("U1"),
            },
            at(10),
        )
        .await
        .unwrap();
    put(&store, bob, "bobs", "trig_1", "c", 10).await;
    begin(&store, ada, "one", 100).await;
    begin(&store, ada, "two", 101).await;
    let bobs = begin(&store, bob, "three", 102).await;

    assert!(store.delete_cloud_routine(ada, "rc").await.unwrap());
    assert!(!store.delete_cloud_routine(ada, "rc").await.unwrap());
    assert_eq!(
        store.recent_cloud_handoffs(ada, 10).await.unwrap().len(),
        2,
        "cloud rm keeps the hand-offs"
    );

    assert_eq!(
        store.delete_cloud_routines_of(ada).await.unwrap(),
        CloudDeleted {
            routines: 1,
            handoffs: 2
        }
    );
    assert!(store.cloud_routines(ada).await.unwrap().is_empty());
    assert!(
        store
            .recent_cloud_handoffs(ada, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.cloud_routines(bob).await.unwrap().len(), 1);
    assert_eq!(handoff(&store, bob, bobs).await.id, bobs);
    assert_eq!(
        store.delete_cloud_routines_of(ada).await.unwrap(),
        CloudDeleted::default()
    );
}

#[tokio::test]
async fn a_member_deleted_takes_their_routines_and_handoffs() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    put(&store, ada, "rc", "trig_1", "a", 10).await;
    begin(&store, ada, "one", 100).await;
    sqlx::query("DELETE FROM members WHERE id = ?")
        .bind(ada.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    let left: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM cloud_routines), (SELECT COUNT(*) FROM cloud_handoffs)",
    )
    .fetch_one(&store.pool)
    .await
    .unwrap();
    assert_eq!(left, (0, 0));
}

#[tokio::test]
async fn a_handoff_finishes_from_sending_and_late_from_unknown() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;

    let id = begin(&store, ada, "t", 100).await;
    assert!(finish(&store, id, &fired("session_01A"), 110).await);
    let row = handoff(&store, ada, id).await;
    assert_eq!(row.state, CloudHandoffState::Fired);
    assert_eq!(row.session_id.as_deref(), Some("session_01A"));
    assert_eq!(
        row.session_url.as_deref(),
        Some("https://claude.ai/code/session_01A")
    );
    assert_eq!(row.answered_at, Some(at(110)));
    for outcome in [
        fired("session_02"),
        rejected(429),
        CloudOutcome::Unknown {
            status: None,
            reason: CloudUnknownReason::ServerError,
        },
    ] {
        assert!(
            !finish(&store, id, &outcome, 120).await,
            "{outcome:?} after fired"
        );
    }
    assert_eq!(
        handoff(&store, ada, id).await.session_id.as_deref(),
        Some("session_01A")
    );

    let id = begin(&store, ada, "t", 100).await;
    assert!(finish(&store, id, &rejected(429), 110).await);
    let row = handoff(&store, ada, id).await;
    assert_eq!(row.state, CloudHandoffState::Rejected);
    assert_eq!(row.http_status, Some(429));
    assert_eq!(row.error_type.as_deref(), Some("rate_limit_error"));
    assert_eq!(row.retry_after_secs, Some(120));
    assert_eq!(row.session_id, None);
    assert!(!finish(&store, id, &fired("session_x"), 120).await);

    let id = begin(&store, ada, "t", 100).await;
    let refused = CloudOutcome::Rejected {
        status: None,
        error_type: None,
        retry_after_secs: None,
    };
    assert!(finish(&store, id, &refused, 110).await);
    assert_eq!(handoff(&store, ada, id).await.http_status, None);

    let id = begin(&store, ada, "t", 100).await;
    assert!(
        finish(
            &store,
            id,
            &CloudOutcome::Unknown {
                status: Some(503),
                reason: CloudUnknownReason::ServerError
            },
            110
        )
        .await
    );
    let row = handoff(&store, ada, id).await;
    assert_eq!(row.state, CloudHandoffState::Unknown);
    assert_eq!(row.http_status, Some(503));
    assert!(
        !finish(&store, id, &fired("session_late"), 120).await,
        "only a row the pass marked takes a late answer"
    );
    assert_eq!(
        handoff(&store, ada, id).await.state,
        CloudHandoffState::Unknown
    );

    for (late, state) in [
        (fired("session_03"), CloudHandoffState::Fired),
        (rejected(404), CloudHandoffState::Rejected),
    ] {
        let id = unknown(&store, ada, 200).await;
        assert!(finish(&store, id, &late, 210).await);
        let row = handoff(&store, ada, id).await;
        assert_eq!(row.state, state);
        assert_eq!(row.unknown_reason, None);
        assert_eq!(row.answered_at, Some(at(210)));
        assert!(!finish(&store, id, &late, 220).await);
    }

    let told = unknown(&store, ada, 300).await;
    let attempt = claim(&store, told, 300).await.unwrap();
    assert!(
        store
            .mark_cloud_handoff_notified(told, attempt, at(301))
            .await
            .unwrap()
    );
    assert!(
        finish(&store, told, &fired("session_after_notice"), 310).await,
        "a late answer is recorded after the notice too"
    );
    let row = handoff(&store, ada, told).await;
    assert_eq!(row.state, CloudHandoffState::Fired);
    assert_eq!(row.session_id.as_deref(), Some("session_after_notice"));
    assert_eq!(row.notified_at, Some(at(301)), "the notice's time is kept");
    assert!(!finish(&store, told, &fired("session_again"), 320).await);

    assert_eq!(
        store
            .finish_cloud_handoff(told, &fired("session_again"), at(330))
            .await
            .unwrap(),
        CloudFinished::Kept
    );
    assert_eq!(
        store
            .finish_cloud_handoff(CloudHandoffId::new_v4(), &fired("session_9"), at(1))
            .await
            .unwrap(),
        CloudFinished::Gone
    );
}

#[tokio::test]
async fn only_the_pass_records_no_answer() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let no_answer = CloudOutcome::Unknown {
        status: None,
        reason: CloudUnknownReason::NoAnswer,
    };
    let id = begin(&store, ada, "t", 100).await;
    let err = store
        .finish_cloud_handoff(id, &no_answer, at(110))
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Refused { .. }), "{err:?}");
    let row = handoff(&store, ada, id).await;
    assert_eq!(row.state, CloudHandoffState::Sending);
    assert!(finish(&store, id, &fired("session_1"), 120).await);

    let pass_marked = unknown(&store, ada, 200).await;
    assert!(
        store
            .finish_cloud_handoff(pass_marked, &no_answer, at(210))
            .await
            .is_err()
    );
    assert_eq!(
        handoff(&store, ada, pass_marked).await.notified_at,
        None,
        "the notice is still owed"
    );
}

#[tokio::test]
async fn a_late_unknown_answer_marks_the_notice_done() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let id = unknown(&store, ada, 200).await;
    assert_eq!(due(&store, 205).await, [id]);
    assert!(
        finish(
            &store,
            id,
            &CloudOutcome::Unknown {
                status: Some(503),
                reason: CloudUnknownReason::Timeout
            },
            210
        )
        .await
    );
    let row = handoff(&store, ada, id).await;
    assert_eq!(row.state, CloudHandoffState::Unknown);
    assert_eq!(row.http_status, Some(503));
    assert_eq!(row.answered_at, Some(at(200)));
    assert_eq!(row.notified_at, Some(at(210)));
    assert_eq!(row.unknown_reason, Some(CloudUnknownReason::Timeout));
    assert!(due(&store, 220).await.is_empty(), "no second notice");
    assert_eq!(claim(&store, id, 220).await, None);
    assert!(
        !finish(
            &store,
            id,
            &CloudOutcome::Unknown {
                status: None,
                reason: CloudUnknownReason::ServerError
            },
            230
        )
        .await
    );
    assert!(!finish(&store, id, &fired("session_1"), 230).await);
}

#[tokio::test]
async fn recording_an_outcome_marks_its_notice_done() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;

    for outcome in [
        fired("session_1"),
        rejected(401),
        CloudOutcome::Unknown {
            status: Some(500),
            reason: CloudUnknownReason::ServerError,
        },
    ] {
        let id = begin(&store, ada, "t", 100).await;
        assert!(finish(&store, id, &outcome, 110).await);
        assert_eq!(
            handoff(&store, ada, id).await.notified_at,
            Some(at(110)),
            "{outcome:?}"
        );
        assert!(due(&store, 120).await.is_empty(), "{outcome:?}");
        assert_eq!(claim(&store, id, 120).await, None);
    }

    let unclaimed = unknown(&store, ada, 200).await;
    assert_eq!(due(&store, 210).await, [unclaimed]);
    assert!(finish(&store, unclaimed, &fired("session_2"), 210).await);
    assert_eq!(
        handoff(&store, ada, unclaimed).await.notified_at,
        Some(at(210))
    );
    assert!(due(&store, 211).await.is_empty());

    let leased = unknown(&store, ada, 300).await;
    let attempt = claim(&store, leased, 300).await.unwrap();
    assert!(finish(&store, leased, &rejected(403), 310).await);
    assert_eq!(
        handoff(&store, ada, leased).await.notified_at,
        Some(at(310)),
        "the reply tells the member, whatever a claim is doing"
    );
    assert!(due(&store, 2_000).await.is_empty());
    assert!(
        !store
            .mark_cloud_handoff_notified(leased, attempt, at(320))
            .await
            .unwrap()
    );
    assert_eq!(
        handoff(&store, ada, leased).await.notified_at,
        Some(at(310))
    );

    let deferred = unknown(&store, ada, 400).await;
    let attempt = claim(&store, deferred, 400).await.unwrap();
    assert!(
        store
            .defer_cloud_handoff_notice(deferred, attempt, at(401))
            .await
            .unwrap()
    );
    assert!(finish(&store, deferred, &fired("session_3"), 405).await);
    assert_eq!(
        handoff(&store, ada, deferred).await.notified_at,
        Some(at(405)),
        "a claim that ended no longer holds it"
    );

    let lapsed = unknown(&store, ada, 500).await;
    claim(&store, lapsed, 500).await.unwrap();
    let after_lease = 500 + 600;
    assert!(finish(&store, lapsed, &fired("session_4"), after_lease).await);
    assert_eq!(
        handoff(&store, ada, lapsed).await.notified_at,
        Some(at(after_lease))
    );
}

#[tokio::test]
async fn stale_sending_handoffs_become_unknown() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let old = begin(&store, ada, "t", 100).await;
    let older = begin(&store, ada, "t", 90).await;
    let fresh = begin(&store, ada, "t", 160).await;
    let done = begin(&store, ada, "t", 50).await;
    assert!(finish(&store, done, &fired("session_1"), 55).await);

    let stale = store.stale_cloud_handoffs(at(160), at(170)).await.unwrap();
    let ids: Vec<CloudHandoffId> = stale.iter().map(|handoff| handoff.id).collect();
    assert_eq!(ids, [older, old]);
    for handoff in &stale {
        assert_eq!(handoff.state, CloudHandoffState::Unknown);
        assert_eq!(handoff.answered_at, Some(at(170)));
        assert_eq!(handoff.unknown_reason, Some(CloudUnknownReason::NoAnswer));
        assert_eq!(handoff.notified_at, None);
        assert_eq!(handoff.http_status, None);
    }
    assert_eq!(
        handoff(&store, ada, fresh).await.state,
        CloudHandoffState::Sending
    );
    assert_eq!(
        handoff(&store, ada, done).await.state,
        CloudHandoffState::Fired
    );
    assert!(
        store
            .stale_cloud_handoffs(at(160), at(180))
            .await
            .unwrap()
            .is_empty(),
        "a row is marked once"
    );
    assert_eq!(due(&store, 170).await, [older, old]);
}

#[tokio::test]
async fn a_handoff_notice_is_claimed_once_and_backs_off() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let id = unknown(&store, ada, 1_000).await;
    let sending = begin(&store, ada, "t", 1_000).await;
    assert_eq!(
        claim(&store, sending, 1_000).await,
        None,
        "nothing owed yet"
    );

    assert_eq!(claim(&store, id, 1_000).await, Some(1));
    assert_eq!(claim(&store, id, 1_001).await, None, "the lease holds it");
    assert!(due(&store, 1_599).await.is_empty());
    assert_eq!(due(&store, 1_600).await, [id], "the lease ran out");
    assert_eq!(claim(&store, id, 1_600).await, Some(2));
    assert!(
        !store
            .defer_cloud_handoff_notice(id, 1, at(1_601))
            .await
            .unwrap(),
        "a stale claim can't shorten the latest one's lease"
    );
    assert!(due(&store, 1_700).await.is_empty());

    let mut now = 1_610;
    for (claimed, backoff) in [(2, 120), (3, 240), (4, 480), (5, 960), (6, 1_920)] {
        assert!(
            store
                .defer_cloud_handoff_notice(id, claimed, at(now))
                .await
                .unwrap()
        );
        assert!(due(&store, now + backoff - 1).await.is_empty(), "{claimed}");
        now += backoff;
        assert_eq!(due(&store, now).await, [id], "{claimed}");
        assert_eq!(claim(&store, id, now).await, Some(claimed + 1));
    }
    for claimed in 7..9 {
        assert!(
            store
                .defer_cloud_handoff_notice(id, claimed, at(now))
                .await
                .unwrap()
        );
        assert!(due(&store, now + 3_599).await.is_empty());
        now += 3_600;
        assert_eq!(claim(&store, id, now).await, Some(claimed + 1));
    }

    assert!(
        !store
            .mark_cloud_handoff_notified(id, 10, at(now))
            .await
            .unwrap(),
        "only a claim that was made marks it"
    );
    assert!(
        store
            .mark_cloud_handoff_notified(id, 8, at(now))
            .await
            .unwrap(),
        "an older claim whose notice was sent marks it too"
    );
    assert_eq!(handoff(&store, ada, id).await.notified_at, Some(at(now)));
    assert!(
        !store
            .mark_cloud_handoff_notified(id, 9, at(now))
            .await
            .unwrap()
    );
    assert!(
        !store
            .defer_cloud_handoff_notice(id, 9, at(now))
            .await
            .unwrap()
    );
    assert!(due(&store, now + 10_000).await.is_empty());
    assert_eq!(handoff(&store, ada, id).await.notice_attempts, 9);
}

#[tokio::test]
async fn claim_zero_neither_defers_nor_marks_a_notice() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let id = unknown(&store, ada, 1_000).await;
    assert!(
        !store
            .defer_cloud_handoff_notice(id, 0, at(1_000))
            .await
            .unwrap(),
        "a never-claimed notice isn't pushed back"
    );
    assert!(
        !store
            .mark_cloud_handoff_notified(id, 0, at(1_000))
            .await
            .unwrap()
    );
    assert_eq!(due(&store, 1_000).await, [id], "still due at once");
}

#[tokio::test]
async fn a_handoff_notice_is_given_up_after_a_day() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let id = unknown(&store, ada, 1_000).await;
    let day = 24 * 60 * 60;
    assert_eq!(due(&store, 1_000 + day - 1).await, [id]);
    assert!(due(&store, 1_000 + day).await.is_empty());
    assert_eq!(claim(&store, id, 1_000 + day).await, None);
    assert_eq!(claim(&store, id, 1_000 + day - 1).await, Some(1));
    assert!(
        store
            .defer_cloud_handoff_notice(id, 1, at(1_000 + day - 1))
            .await
            .unwrap()
    );
    assert!(due(&store, 1_000 + day + 60).await.is_empty());
    assert_eq!(handoff(&store, ada, id).await.notified_at, None);
}

#[tokio::test]
async fn old_handoffs_are_purged() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let old = begin(&store, ada, "t", 100).await;
    assert!(finish(&store, old, &fired("session_1"), 110).await);
    let rejected_early = begin(&store, ada, "t", 50).await;
    assert!(finish(&store, rejected_early, &rejected(404), 60).await);
    let kept = begin(&store, ada, "t", 200).await;
    assert_eq!(
        store.purge_cloud_handoffs(at(200), at(300)).await.unwrap(),
        2
    );
    let left: Vec<CloudHandoffId> = store
        .recent_cloud_handoffs(ada, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|recent| recent.handoff.id)
        .collect();
    assert_eq!(left, [kept]);
    assert_eq!(
        store.purge_cloud_handoffs(at(200), at(300)).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn a_routine_keeps_the_origin_it_was_registered_with() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    store
        .put_cloud_routine(
            &NewCloudRoutine {
                member: ada,
                label: "r",
                routine_id: &routine("trig_1"),
                url_origin: "http://127.0.0.1:8080",
                token: &token("sk-1"),
                added_by: &member_key("ada"),
            },
            at(10),
        )
        .await
        .unwrap();
    let found = store.cloud_routine(ada, "r").await.unwrap().unwrap();
    assert_eq!(found.url_origin, "http://127.0.0.1:8080");
    assert_eq!(
        store.cloud_routines(ada).await.unwrap()[0].url_origin,
        "http://127.0.0.1:8080"
    );
    put(&store, ada, "r", "trig_1", "sk-2", 20).await;
    let found = store.cloud_routine(ada, "r").await.unwrap().unwrap();
    assert_eq!(found.url_origin, API, "a replacement takes the new origin");
    assert_eq!(shown(&found.token), "sk-2");

    sqlx::query("UPDATE cloud_routines SET url_origin = 'https://evil.example'")
        .execute(&store.pool)
        .await
        .unwrap();
    let err = store.cloud_routine(ada, "r").await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Seal {
                table: "cloud_routines",
                column: "token_enc",
                source: SealError::Decrypt,
            }
        ),
        "a token is bound to its origin: {err:?}"
    );
}

#[tokio::test]
async fn a_token_is_bound_to_its_label() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    put(&store, ada, "deploy", "trig_1", "DEPLOY", 10).await;
    put(&store, ada, "prod", "trig_2", "PROD", 10).await;
    for (from, to) in [("deploy", "swap"), ("prod", "deploy"), ("swap", "prod")] {
        sqlx::query("UPDATE cloud_routines SET label = ? WHERE label = ?")
            .bind(to)
            .bind(from)
            .execute(&store.pool)
            .await
            .unwrap();
    }
    let err = store.cloud_routine(ada, "deploy").await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Seal {
                source: SealError::Decrypt,
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn the_stale_pass_reads_only_sending_rows() {
    let store = memory_store().await;
    let plan: Vec<(i64, i64, i64, String)> =
        sqlx::query_as(concat!("EXPLAIN QUERY PLAN ", stale_update!()))
            .bind(1)
            .bind(1)
            .fetch_all(&store.pool)
            .await
            .unwrap();
    assert!(
        plan.iter()
            .any(|row| row.3.contains("cloud_handoffs_sending")),
        "{plan:?}"
    );
}

#[tokio::test]
async fn a_purge_keeps_a_notice_still_owed() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let sending = begin(&store, ada, "t", 50).await;
    assert_eq!(
        store.purge_cloud_handoffs(at(500), at(500)).await.unwrap(),
        0,
        "a row the pass hasn't marked yet is kept"
    );
    assert!(finish(&store, sending, &fired("session_0"), 60).await);
    assert_eq!(
        store.purge_cloud_handoffs(at(500), at(500)).await.unwrap(),
        1
    );
    let owed = unknown(&store, ada, 1_000).await;
    let day = 24 * 60 * 60;
    assert_eq!(
        store
            .purge_cloud_handoffs(at(500), at(1_000 + day - 1))
            .await
            .unwrap(),
        0
    );
    assert_eq!(handoff(&store, ada, owed).await.id, owed);
    assert_eq!(
        store
            .purge_cloud_handoffs(at(500), at(1_000 + day))
            .await
            .unwrap(),
        1,
        "given up, it goes"
    );

    let told = unknown(&store, ada, 1_000).await;
    let attempt = claim(&store, told, 1_000).await.unwrap();
    assert!(
        store
            .mark_cloud_handoff_notified(told, attempt, at(1_001))
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .purge_cloud_handoffs(at(500), at(1_002))
            .await
            .unwrap(),
        1,
        "sent, it goes"
    );
}

#[tokio::test]
async fn the_schema_refuses_rows_that_contradict_their_state() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let id = begin(&store, ada, "t", 100).await;
    for sql in [
        "UPDATE cloud_handoffs SET state = 'fired' WHERE id = ?",
        "UPDATE cloud_handoffs SET state = 'done', answered_at = 1 WHERE id = ?",
        "UPDATE cloud_handoffs SET origin = 'rocketchat_channel' WHERE id = ?",
        "UPDATE cloud_handoffs SET answered_at = 1 WHERE id = ?",
        "UPDATE cloud_handoffs SET notified_at = 1 WHERE id = ?",
        "UPDATE cloud_handoffs SET session_url = 'x' WHERE id = ?",
        "UPDATE cloud_handoffs SET http_status = 42 WHERE id = ?",
        "UPDATE cloud_handoffs SET state = 'rejected', answered_at = 1, session_id = 'x' \
         WHERE id = ?",
        "UPDATE cloud_handoffs SET state = 'fired', answered_at = 1, session_id = '' \
         WHERE id = ?",
        "UPDATE cloud_handoffs SET state = 'unknown', answered_at = 1, error_type = 'x', \
         unknown_reason = 'timeout' WHERE id = ?",
        "UPDATE cloud_handoffs SET state = 'unknown', answered_at = 1 WHERE id = ?",
        "UPDATE cloud_handoffs SET state = 'unknown', answered_at = 1, \
         unknown_reason = 'because' WHERE id = ?",
        "UPDATE cloud_handoffs SET unknown_reason = 'timeout' WHERE id = ?",
        "UPDATE cloud_handoffs SET state = 'fired', answered_at = 1, session_id = 's', \
         retry_after_secs = 1 WHERE id = ?",
        "UPDATE cloud_handoffs SET routine_id = 'routine_1' WHERE id = ?",
        "UPDATE cloud_handoffs SET state = 'fired', answered_at = 1, session_id = 's' \
         WHERE id = ?",
        "UPDATE cloud_handoffs SET state = 'rejected', answered_at = 1 WHERE id = ?",
        "UPDATE cloud_handoffs SET routine_id = 'trig_' WHERE id = ?",
    ] {
        let err = sqlx::query(sql)
            .bind(id.to_string())
            .execute(&store.pool)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{sql}: {err}");
    }
}

#[tokio::test]
async fn the_schema_refuses_a_routine_id_without_trig() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    put(&store, ada, "r", "trig_1", "t", 10).await;
    for bad in ["1", "trig_", "TRIG_1"] {
        let err = sqlx::query("UPDATE cloud_routines SET routine_id = ?")
            .bind(bad)
            .execute(&store.pool)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{bad}: {err}");
    }
    let err = sqlx::query("UPDATE cloud_routines SET label = 'a:b'")
        .execute(&store.pool)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("CHECK"), "{err}");
}

#[tokio::test]
async fn a_routine_moved_to_another_member_fails_to_open() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let mallory = member(&store, "mallory").await;
    put(&store, ada, "r", "trig_1", "sk-ant-oat01-ADA", 10).await;
    sqlx::query("UPDATE cloud_routines SET member_id = ?")
        .bind(mallory.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    let err = store.cloud_routine(mallory, "r").await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Seal {
                table: "cloud_routines",
                column: "token_enc",
                source: SealError::Decrypt,
            }
        ),
        "{err:?}"
    );

    put(&store, ada, "s", "trig_2", "sk-ant-oat01-ADA2", 10).await;
    sqlx::query("UPDATE cloud_routines SET routine_id = 'trig_3' WHERE label = 's'")
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(
        store.cloud_routine(ada, "s").await.is_err(),
        "a token is bound to its routine id"
    );
}

#[tokio::test]
async fn a_handoff_moved_to_another_member_fails_to_open() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let mallory = member(&store, "mallory").await;
    begin(&store, ada, "ADA-TASK", 100).await;
    sqlx::query("UPDATE cloud_handoffs SET member_id = ?")
        .bind(mallory.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    let err = store.recent_cloud_handoffs(mallory, 10).await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Seal {
                table: "cloud_handoffs",
                column: "task_enc",
                source: SealError::Decrypt,
            }
        ),
        "{err:?}"
    );
}

#[test]
fn a_new_handoff_leaves_its_task_out_of_debug() {
    let member = MemberId::new_v4();
    let routine_id = routine("trig_1");
    let requested_by = member_key("ada");
    let new = NewCloudHandoff {
        member,
        routine_label: "r",
        routine_id: &routine_id,
        registration: &CloudRoutineVersion::default(),
        requested_by: &requested_by,
        origin: CloudOrigin::SlackDm,
        task: "TASK-TEXT",
    };
    let debug = format!("{new:?} {new:#?}");
    assert!(!debug.contains("TASK-TEXT"), "{debug}");
    assert!(debug.contains("trig_1"), "{debug}");
}

#[tokio::test]
async fn corrupt_rows_are_reported() {
    let store = memory_store().await;
    let ada = member(&store, "ada").await;
    let id = begin(&store, ada, "t", 100).await;
    sqlx::query("UPDATE cloud_handoffs SET routine_id = 'trig_a/b' WHERE id = ?")
        .bind(id.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    let err = store.recent_cloud_handoffs(ada, 10).await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Corrupt {
                table: "cloud_handoffs",
                column: "routine_id"
            }
        ),
        "{err:?}"
    );
    put(&store, ada, "x", "trig_1", "t", 10).await;
    sqlx::query("UPDATE cloud_routines SET added_by = 'nope'")
        .execute(&store.pool)
        .await
        .unwrap();
    let err = store.cloud_routines(ada).await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Corrupt {
                table: "cloud_routines",
                column: "added_by"
            }
        ),
        "{err:?}"
    );
}

#[test]
fn origins_and_states_have_their_stored_forms() {
    for origin in [
        CloudOrigin::SlackSlash,
        CloudOrigin::SlackDm,
        CloudOrigin::RocketChatDm,
    ] {
        assert_eq!(CloudOrigin::parse(origin.as_str()).unwrap(), origin);
    }
    assert!(CloudOrigin::parse("rocketchat_channel").is_err());
    for state in [
        CloudHandoffState::Sending,
        CloudHandoffState::Fired,
        CloudHandoffState::Rejected,
        CloudHandoffState::Unknown,
    ] {
        assert_eq!(CloudHandoffState::parse(state.as_str()).unwrap(), state);
    }
    assert!(CloudHandoffState::parse("done").is_err());
    for reason in [
        CloudUnknownReason::ServerError,
        CloudUnknownReason::OtherStatus,
        CloudUnknownReason::Timeout,
        CloudUnknownReason::ConnectionLost,
        CloudUnknownReason::Redirect,
        CloudUnknownReason::UnreadableAnswer,
        CloudUnknownReason::NoAnswer,
    ] {
        assert_eq!(CloudUnknownReason::parse(reason.as_str()).unwrap(), reason);
    }
    assert!(CloudUnknownReason::parse("because").is_err());
}

#[test]
fn the_notice_backoff_doubles_from_a_minute_to_an_hour() {
    let secs: Vec<u64> = [0, 1, 2, 3, 6, 7, 8, 40, u32::MAX]
        .into_iter()
        .map(|claim| notice_backoff(claim).as_secs())
        .collect();
    assert_eq!(secs, [60, 60, 120, 240, 1_920, 3_600, 3_600, 3_600, 3_600]);
}

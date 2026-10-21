use secrecy::ExposeSecret;

use super::*;
use crate::test_util::*;

const TEAM: &str = "chat.example.org";

/// A per-owner limit no test reaches unless it means to.
const MAX: u32 = 100;

fn team() -> TeamId {
    TeamId::new(TEAM)
}

fn bot(user: &str) -> MemberKey {
    MemberKey {
        surface: SurfaceKind::RocketChat,
        team: team(),
        user: UserId::new(user),
    }
}

async fn owner(store: &Store, user: &str) -> MemberId {
    store
        .ensure_member(&member_key(user), user, at(1_000))
        .await
        .unwrap()
}

async fn create(store: &Store, owner: MemberId, name: &str, at_secs: i64) -> (Agent, BindingId) {
    let team = team();
    let new = NewAgent {
        owner,
        name,
        persona: "You help.",
        visibility: Visibility::Public,
        surface: SurfaceKind::RocketChat,
        team: &team,
    };
    match store.create_agent(&new, MAX, at(at_secs)).await.unwrap() {
        AgentCreation::Created(agent, binding) => (agent, binding),
        other => panic!("the name is free: {other:?}"),
    }
}

/// Creates an agent whose binding is active as bot user `user`.
async fn active(store: &Store, owner: MemberId, name: &str, user: &str) -> (Agent, BindingId) {
    let (agent, binding) = create(store, owner, name, 1_000).await;
    assert_eq!(
        store
            .set_binding_bot_user(binding, &UserId::new(user), user)
            .await
            .unwrap(),
        Some(true)
    );
    let token = SecretString::from(format!("token-of-{user}"));
    assert!(
        store
            .activate_binding(binding, &token, at(1_010))
            .await
            .unwrap()
    );
    (agent, binding)
}

#[tokio::test]
async fn create_agent_stores_the_agent_and_a_creating_binding() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (agent, binding) = create(&store, ada, "helper", 1_000).await;
    assert_eq!(agent.state, AgentState::Active);
    assert_eq!(store.agent(agent.id).await.unwrap(), Some(agent.clone()));
    assert_eq!(
        store.agent_by_name(ada, "helper").await.unwrap(),
        Some(agent.clone())
    );
    let row = store.binding(binding).await.unwrap().unwrap();
    assert_eq!(row.agent, agent.id);
    assert_eq!(row.state, BindingState::Creating);
    assert_eq!(row.surface, SurfaceKind::RocketChat);
    assert_eq!(row.team, team());
    assert_eq!(row.bot_user, None);
    assert_eq!(row.state_changed_at, at(1_000));
    assert_eq!(store.bindings_of(agent.id).await.unwrap(), [row]);
}

#[tokio::test]
async fn names_are_unique_per_owner_until_deleted() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let bob = owner(&store, "bob").await;
    let (first, _) = create(&store, ada, "helper", 1_000).await;
    let team = team();
    let again = NewAgent {
        owner: ada,
        name: "helper",
        persona: "",
        visibility: Visibility::Public,
        surface: SurfaceKind::RocketChat,
        team: &team,
    };
    assert_eq!(
        store.create_agent(&again, MAX, at(1_001)).await.unwrap(),
        AgentCreation::NameTaken
    );
    create(&store, bob, "helper", 1_002).await;

    assert!(store.delete_agent(first.id, at(1_003)).await.unwrap());
    assert_eq!(store.agent_by_name(ada, "helper").await.unwrap(), None);
    let AgentCreation::Created(second, _) =
        store.create_agent(&again, MAX, at(1_004)).await.unwrap()
    else {
        panic!("the name is free again");
    };
    assert_ne!(second.id, first.id);
    assert_eq!(
        store.agent(first.id).await.unwrap().unwrap().state,
        AgentState::Deleted
    );
}

#[tokio::test]
async fn an_unknown_owner_is_a_database_error() {
    let store = memory_store().await;
    let team = team();
    let new = NewAgent {
        owner: MemberId::new_v4(),
        name: "helper",
        persona: "",
        visibility: Visibility::Public,
        surface: SurfaceKind::RocketChat,
        team: &team,
    };
    let err = store.create_agent(&new, MAX, at(1_000)).await.unwrap_err();
    assert!(matches!(err, StoreError::Database(_)), "{err:?}");
}

#[tokio::test]
async fn activation_needs_a_creating_binding_with_a_bot_user() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (_, binding) = create(&store, ada, "helper", 1_000).await;
    let token = SecretString::from("t");
    assert!(
        !store
            .activate_binding(binding, &token, at(1_001))
            .await
            .unwrap(),
        "no bot user yet"
    );
    assert_eq!(
        store
            .set_binding_bot_user(binding, &UserId::new("bot1"), "helper")
            .await
            .unwrap(),
        Some(true)
    );
    assert!(
        store
            .activate_binding(binding, &token, at(1_002))
            .await
            .unwrap()
    );
    let row = store.binding(binding).await.unwrap().unwrap();
    assert_eq!(row.state, BindingState::Active);
    assert_eq!(row.state_changed_at, at(1_002));
    assert_eq!(row.bot_user, Some(UserId::new("bot1")));
    assert_eq!(row.bot_username.as_deref(), Some("helper"));
    assert!(
        !store
            .activate_binding(binding, &token, at(1_003))
            .await
            .unwrap(),
        "already active"
    );
    assert_eq!(
        store
            .set_binding_bot_user(binding, &UserId::new("bot2"), "other")
            .await
            .unwrap(),
        None,
        "it has a bot user"
    );
    assert_eq!(
        store
            .set_binding_bot_user(BindingId::new_v4(), &UserId::new("bot3"), "x")
            .await
            .unwrap(),
        None,
        "no such binding"
    );
}

#[tokio::test]
async fn an_owner_has_at_most_the_limit_of_live_agents() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let bob = owner(&store, "bob").await;
    let team = team();
    let named = |name| NewAgent {
        owner: ada,
        name,
        persona: "p",
        visibility: Visibility::Public,
        surface: SurfaceKind::RocketChat,
        team: &team,
    };
    let AgentCreation::Created(first, _) = store
        .create_agent(&named("one"), 2, at(1_000))
        .await
        .unwrap()
    else {
        panic!("under the limit");
    };
    assert!(matches!(
        store
            .create_agent(&named("two"), 2, at(1_001))
            .await
            .unwrap(),
        AgentCreation::Created(..)
    ));
    assert_eq!(
        store
            .create_agent(&named("three"), 2, at(1_002))
            .await
            .unwrap(),
        AgentCreation::LimitReached
    );
    assert_eq!(store.agent_by_name(ada, "three").await.unwrap(), None);
    let bobs = NewAgent {
        owner: bob,
        ..named("three")
    };
    assert!(matches!(
        store.create_agent(&bobs, 2, at(1_003)).await.unwrap(),
        AgentCreation::Created(..)
    ));
    assert!(store.delete_agent(first.id, at(1_004)).await.unwrap());
    assert!(matches!(
        store
            .create_agent(&named("three"), 2, at(1_005))
            .await
            .unwrap(),
        AgentCreation::Created(..)
    ));
}

#[tokio::test]
async fn a_bot_user_is_recorded_on_an_abandoned_binding_and_then_owes_retirement() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (_, binding) = create(&store, ada, "helper", 1_000).await;
    assert!(
        store
            .set_binding_bot_username(binding, "helper")
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .binding(binding)
            .await
            .unwrap()
            .unwrap()
            .bot_username
            .as_deref(),
        Some("helper")
    );
    assert!(
        store
            .abandon_creation(binding, at(1_000), at(1_001))
            .await
            .unwrap()
    );
    assert!(
        !store
            .set_binding_bot_username(binding, "ada.helper")
            .await
            .unwrap(),
        "not creating any more"
    );
    assert_eq!(
        store
            .set_binding_bot_user(binding, &UserId::new("bot1"), "helper")
            .await
            .unwrap(),
        Some(false)
    );
    let pending = store
        .pending_retirements(SurfaceKind::RocketChat, &team(), at(1_002), 3)
        .await
        .unwrap();
    assert_eq!(
        pending,
        [PendingRetirement {
            binding,
            bot_user: Some(UserId::new("bot1")),
        }]
    );
}

#[tokio::test]
async fn one_bot_user_backs_one_binding() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    active(&store, ada, "helper", "bot1").await;
    let (_, second) = create(&store, ada, "other", 1_000).await;
    let err = store
        .set_binding_bot_user(second, &UserId::new("bot1"), "helper")
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::Database(_)), "{err:?}");
}

#[tokio::test]
async fn active_bots_are_the_active_bindings_of_live_agents() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (helper, helper_binding) = active(&store, ada, "helper", "bot1").await;
    let (paused, _) = active(&store, ada, "paused", "bot2").await;
    let (gone, _) = active(&store, ada, "gone", "bot3").await;
    create(&store, ada, "creating", 1_000).await;
    assert!(store.set_agent_paused(paused.id, true).await.unwrap());
    assert!(store.delete_agent(gone.id, at(1_020)).await.unwrap());

    let bots = store
        .active_bots(SurfaceKind::RocketChat, &team())
        .await
        .unwrap();
    let found: Vec<(AgentId, String, String)> = bots
        .iter()
        .map(|b| {
            (
                b.agent,
                b.bot.user.to_string(),
                b.token.expose_secret().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        found,
        [
            (helper.id, "bot1".to_owned(), "token-of-bot1".to_owned()),
            (paused.id, "bot2".to_owned(), "token-of-bot2".to_owned()),
        ]
    );
    assert_eq!(bots[0].binding, helper_binding);
    assert_eq!(bots[0].bot, bot("bot1"));
    sqlx::query("UPDATE agent_bindings SET bot_token_enc = x'00' WHERE id = ?")
        .bind(helper_binding.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    let readable = store
        .active_bots(SurfaceKind::RocketChat, &team())
        .await
        .unwrap();
    assert_eq!(
        readable.iter().map(|b| b.agent).collect::<Vec<_>>(),
        [paused.id],
        "a row whose token doesn't decrypt is left out"
    );
    assert!(
        store
            .active_bots(SurfaceKind::Slack, &team())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .active_bots(SurfaceKind::RocketChat, &TeamId::new("other"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn bots_and_bindings_lead_to_their_agent() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (helper, binding) = active(&store, ada, "helper", "bot1").await;
    let (found, found_binding) = store.agent_for_bot(&bot("bot1")).await.unwrap().unwrap();
    assert_eq!((found.id, found_binding), (helper.id, binding));
    assert_eq!(store.agent_for_bot(&bot("nobody")).await.unwrap(), None);
    let elsewhere = MemberKey {
        team: TeamId::new("other"),
        ..bot("bot1")
    };
    assert_eq!(store.agent_for_bot(&elsewhere).await.unwrap(), None);
    assert_eq!(
        store
            .agent_for_binding(binding)
            .await
            .unwrap()
            .map(|a| a.id),
        Some(helper.id)
    );
    assert_eq!(
        store
            .bot_token(binding)
            .await
            .unwrap()
            .map(|t| t.expose_secret().to_owned()),
        Some("token-of-bot1".to_owned())
    );

    assert_eq!(
        store.agent_of_bot_user(&bot("bot1")).await.unwrap(),
        Some(helper.id)
    );
    assert_eq!(store.agent_of_bot_user(&elsewhere).await.unwrap(), None);

    assert!(store.delete_agent(helper.id, at(1_100)).await.unwrap());
    assert_eq!(store.agent_for_bot(&bot("bot1")).await.unwrap(), None);
    assert_eq!(store.agent_for_binding(binding).await.unwrap(), None);
    assert!(store.bot_token(binding).await.unwrap().is_none());
    assert_eq!(
        store.agent_of_bot_user(&bot("bot1")).await.unwrap(),
        Some(helper.id),
        "a deleted agent's bot is still its"
    );
}

#[tokio::test]
async fn a_token_moved_to_another_binding_fails_to_decrypt() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (_, first) = active(&store, ada, "helper", "bot1").await;
    let (_, second) = active(&store, ada, "other", "bot2").await;
    sqlx::query(
        "UPDATE agent_bindings SET bot_token_enc = \
         (SELECT bot_token_enc FROM agent_bindings WHERE id = ?) WHERE id = ?",
    )
    .bind(first.to_string())
    .bind(second.to_string())
    .execute(&store.pool)
    .await
    .unwrap();
    let err = store.bot_token(second).await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Seal {
                table: "agent_bindings",
                column: "bot_token_enc",
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn pause_and_resume_change_only_the_matching_state() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (agent, _) = create(&store, ada, "helper", 1_000).await;
    assert!(!store.set_agent_paused(agent.id, false).await.unwrap());
    assert!(store.set_agent_paused(agent.id, true).await.unwrap());
    assert!(!store.set_agent_paused(agent.id, true).await.unwrap());
    assert_eq!(
        store.agent(agent.id).await.unwrap().unwrap().state,
        AgentState::Paused
    );
    assert!(store.set_agent_paused(agent.id, false).await.unwrap());
    assert!(store.delete_agent(agent.id, at(1_001)).await.unwrap());
    assert!(!store.set_agent_paused(agent.id, true).await.unwrap());
    assert!(!store.set_agent_paused(agent.id, false).await.unwrap());
}

#[tokio::test]
async fn persona_changes_until_the_agent_is_deleted() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (agent, _) = create(&store, ada, "helper", 1_000).await;
    assert!(
        store
            .set_agent_persona(agent.id, "Be terse.")
            .await
            .unwrap()
    );
    assert_eq!(
        store.agent(agent.id).await.unwrap().unwrap().persona,
        "Be terse."
    );
    assert!(store.delete_agent(agent.id, at(1_001)).await.unwrap());
    assert!(!store.set_agent_persona(agent.id, "Again.").await.unwrap());
    assert!(
        !store
            .set_agent_persona(AgentId::new_v4(), "x")
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn delete_disables_every_binding_once() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (agent, binding) = active(&store, ada, "helper", "bot1").await;
    assert!(store.delete_agent(agent.id, at(1_100)).await.unwrap());
    assert!(!store.delete_agent(agent.id, at(1_200)).await.unwrap());
    assert!(
        !store
            .delete_agent(AgentId::new_v4(), at(1_200))
            .await
            .unwrap()
    );
    let row = store.binding(binding).await.unwrap().unwrap();
    assert_eq!(row.state, BindingState::Disabled);
    assert_eq!(row.state_changed_at, at(1_100));
    assert_eq!(row.retired_at, None);
    let token: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT bot_token_enc FROM agent_bindings WHERE id = ?")
            .bind(binding.to_string())
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert_eq!(token, None, "the token is forgotten");
}

#[tokio::test]
async fn directory_lists_live_agents_with_their_owner_and_bot() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let bob = owner(&store, "bob").await;
    active(&store, bob, "writer", "bot1").await;
    let (creating, _) = create(&store, ada, "coder", 1_000).await;
    let (gone, _) = active(&store, ada, "gone", "bot2").await;
    assert!(store.delete_agent(gone.id, at(1_100)).await.unwrap());
    assert!(store.set_member_display_name(ada, "Ada L").await.unwrap());

    let all = store
        .directory(SurfaceKind::RocketChat, &team(), None)
        .await
        .unwrap();
    let summary: Vec<(&str, &str, Option<&str>)> = all
        .iter()
        .map(|e| {
            (
                e.agent.name.as_str(),
                e.owner_name.as_str(),
                e.bot_username.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [("coder", "Ada L", None), ("writer", "bob", Some("bot1"))]
    );
    let adas = store
        .directory(SurfaceKind::RocketChat, &team(), Some(ada))
        .await
        .unwrap();
    assert_eq!(adas.len(), 1);
    assert_eq!(adas[0].agent.id, creating.id);
    let slack = store
        .directory(SurfaceKind::Slack, &TeamId::new("T1"), None)
        .await
        .unwrap();
    assert!(slack.iter().all(|e| e.bot_username.is_none()));
}

#[tokio::test]
async fn a_stale_creation_is_abandoned_once_and_frees_the_name() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (old, old_binding) = create(&store, ada, "old", 1_000).await;
    let (_, fresh) = create(&store, ada, "fresh", 1_500).await;
    active(&store, ada, "done", "bot9").await;
    assert_eq!(
        store
            .set_binding_bot_user(old_binding, &UserId::new("bot1"), "old")
            .await
            .unwrap(),
        Some(true)
    );

    let stale = store
        .stale_creations(SurfaceKind::RocketChat, &team(), at(1_200))
        .await
        .unwrap();
    assert_eq!(stale, [old_binding]);
    assert!(
        !store
            .abandon_creation(fresh, at(1_200), at(1_300))
            .await
            .unwrap(),
        "started after the cut-off"
    );
    assert!(
        store
            .abandon_creation(old_binding, at(1_200), at(1_300))
            .await
            .unwrap()
    );
    assert!(
        !store
            .abandon_creation(old_binding, at(1_200), at(1_300))
            .await
            .unwrap()
    );
    assert_eq!(
        store.agent(old.id).await.unwrap().unwrap().state,
        AgentState::Deleted
    );
    assert_eq!(
        store.binding(old_binding).await.unwrap().unwrap().state,
        BindingState::Disabled
    );
    assert!(
        !store
            .activate_binding(old_binding, &SecretString::from("t"), at(1_400))
            .await
            .unwrap(),
        "an abandoned creation can't finish"
    );
    create(&store, ada, "old", 1_500).await;
    let pending = store
        .pending_retirements(SurfaceKind::RocketChat, &team(), at(1_400), 3)
        .await
        .unwrap();
    assert_eq!(
        pending,
        [PendingRetirement {
            binding: old_binding,
            bot_user: Some(UserId::new("bot1")),
        }]
    );
}

#[tokio::test]
async fn an_abandoned_creation_without_a_bot_user_owes_nothing() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (_, binding) = create(&store, ada, "helper", 1_000).await;
    assert!(
        store
            .abandon_creation(binding, at(1_000), at(1_001))
            .await
            .unwrap()
    );
    assert!(
        store
            .pending_retirements(SurfaceKind::RocketChat, &team(), at(1_002), 3)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_noted_username_owes_retirement_until_it_is_forgotten() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (_, binding) = create(&store, ada, "helper", 1_000).await;
    assert!(
        store
            .set_binding_bot_username(binding, "helper")
            .await
            .unwrap()
    );
    assert!(
        !store.forget_binding_bot_username(binding).await.unwrap(),
        "still creating"
    );
    assert!(
        store
            .abandon_creation(binding, at(1_000), at(1_001))
            .await
            .unwrap()
    );
    let pending = store
        .pending_retirements(SurfaceKind::RocketChat, &team(), at(1_002), 3)
        .await
        .unwrap();
    assert_eq!(
        pending,
        [PendingRetirement {
            binding,
            bot_user: None,
        }]
    );
    assert!(store.forget_binding_bot_username(binding).await.unwrap());
    assert!(
        store
            .pending_retirements(SurfaceKind::RocketChat, &team(), at(1_002), 3)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .set_binding_bot_user(binding, &UserId::new("bot1"), "helper")
            .await
            .unwrap(),
        Some(false)
    );
    assert!(
        !store.forget_binding_bot_username(binding).await.unwrap(),
        "a recorded bot user is kept"
    );
    assert_eq!(
        store
            .pending_retirements(SurfaceKind::RocketChat, &team(), at(1_002), 3)
            .await
            .unwrap()
            .len(),
        1,
        "a bot user recorded late owes retirement again"
    );
}

#[tokio::test]
async fn a_retirement_is_claimed_once_per_lease_and_retried_until_it_runs_out() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (agent, binding) = active(&store, ada, "helper", "bot1").await;
    assert_eq!(
        store
            .claim_retirement(binding, at(1_050), at(1_060), 2)
            .await
            .unwrap(),
        None,
        "an active binding owes nothing"
    );
    assert!(store.delete_agent(agent.id, at(1_100)).await.unwrap());

    assert_eq!(
        store
            .claim_retirement(binding, at(1_100), at(1_700), 2)
            .await
            .unwrap(),
        Some(1)
    );
    assert_eq!(
        store
            .claim_retirement(binding, at(1_200), at(1_800), 2)
            .await
            .unwrap(),
        None,
        "the lease holds"
    );
    assert!(
        store
            .pending_retirements(SurfaceKind::RocketChat, &team(), at(1_200), 2)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(store.defer_retirement(binding, at(1_300)).await.unwrap());
    assert_eq!(
        store
            .claim_retirement(binding, at(1_300), at(1_900), 2)
            .await
            .unwrap(),
        Some(2)
    );
    assert_eq!(
        store
            .claim_retirement(binding, at(2_000), at(2_600), 2)
            .await
            .unwrap(),
        None,
        "the attempts ran out"
    );
    assert!(store.mark_retired(binding, at(2_000)).await.unwrap());
    assert!(!store.mark_retired(binding, at(2_001)).await.unwrap());
    assert!(!store.defer_retirement(binding, at(2_100)).await.unwrap());
    assert_eq!(
        store.binding(binding).await.unwrap().unwrap().retired_at,
        Some(at(2_000))
    );
}

#[tokio::test]
async fn corrupt_rows_are_reported() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (agent, binding) = create(&store, ada, "helper", 1_000).await;
    sqlx::query("PRAGMA ignore_check_constraints = ON")
        .execute(&store.pool)
        .await
        .unwrap();
    for (sql, column) in [
        ("UPDATE agents SET state = 'odd' WHERE id = ?", "state"),
        (
            "UPDATE agents SET state = 'active', visibility = 'odd' WHERE id = ?",
            "visibility",
        ),
    ] {
        sqlx::query(sql)
            .bind(agent.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        let err = store.agent(agent.id).await.unwrap_err();
        assert!(
            matches!(err, StoreError::Corrupt { table: "agents", column: c } if c == column),
            "{err:?}"
        );
    }
    sqlx::query("UPDATE agent_bindings SET state = 'odd' WHERE id = ?")
        .bind(binding.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    let err = store.binding(binding).await.unwrap_err();
    assert!(
        matches!(
            err,
            StoreError::Corrupt {
                table: "agent_bindings",
                column: "state"
            }
        ),
        "{err:?}"
    );
}

#[test]
fn states_round_trip_through_their_column_values() {
    for state in [AgentState::Active, AgentState::Paused, AgentState::Deleted] {
        assert_eq!(AgentState::parse(state.as_str()).unwrap(), state);
    }
    for visibility in [Visibility::Public, Visibility::Private] {
        assert_eq!(Visibility::parse(visibility.as_str()).unwrap(), visibility);
    }
    for state in [
        BindingState::Creating,
        BindingState::PendingInstall,
        BindingState::Active,
        BindingState::Disabled,
    ] {
        assert_eq!(BindingState::parse(state.as_str()).unwrap(), state);
    }
}

#[tokio::test]
async fn debug_shows_the_persona_length_not_the_persona() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let team = team();
    let new = NewAgent {
        owner: ada,
        name: "helper",
        persona: "the secret plan",
        visibility: Visibility::Public,
        surface: SurfaceKind::RocketChat,
        team: &team,
    };
    let AgentCreation::Created(agent, _) = store.create_agent(&new, MAX, at(1_000)).await.unwrap()
    else {
        panic!("the name is free");
    };
    for debug in [format!("{new:?}"), format!("{agent:?}")] {
        assert!(!debug.contains("secret plan"), "{debug}");
        assert!(debug.contains("persona_len: 15"), "{debug}");
        assert!(debug.contains("helper"), "{debug}");
    }
}

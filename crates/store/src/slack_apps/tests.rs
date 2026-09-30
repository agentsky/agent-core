use core_types::{MemberKey, SurfaceKind};
use secrecy::ExposeSecret;

use super::*;
use crate::agents::{AgentCreation, NewAgent, Visibility};
use crate::test_util::*;

const TEAM: &str = "T0TEAM001";
const MAX: u32 = 3;

fn team() -> TeamId {
    TeamId::new(TEAM)
}

async fn owner(store: &Store, user: &str) -> MemberId {
    let key = MemberKey {
        surface: SurfaceKind::Slack,
        team: team(),
        user: UserId::new(user),
    };
    store.ensure_member(&key, user, at(1_000)).await.unwrap()
}

async fn creating(
    store: &Store,
    owner: MemberId,
    name: &str,
    surface: SurfaceKind,
) -> (AgentId, BindingId) {
    let team = team();
    let new = NewAgent {
        owner,
        name,
        persona: "You help.",
        visibility: Visibility::Public,
        surface,
        team: &team,
    };
    match store.create_agent(&new, 10, at(1_000)).await.unwrap() {
        AgentCreation::Created(agent, binding) => (agent.id, binding),
        other => panic!("the name is free: {other:?}"),
    }
}

fn app(id: &str) -> NewSlackApp {
    NewSlackApp {
        app_id: id.to_owned(),
        client_id: format!("{id}.client"),
        client_secret: SecretString::from(format!("{id}-client-secret")),
        signing_secret: SecretString::from(format!("{id}-signing-secret")),
        scopes: "chat:write,im:history".to_owned(),
    }
}

/// A Slack agent whose app is created and waits for its install since
/// 2,000.
async fn pending(store: &Store, owner: MemberId, name: &str) -> (AgentId, BindingId) {
    let (agent, binding) = creating(store, owner, name, SurfaceKind::Slack).await;
    assert!(
        store
            .set_slack_app(binding, &app(&format!("A{name}")), name, at(2_000))
            .await
            .unwrap()
    );
    (agent, binding)
}

#[tokio::test]
async fn a_binding_is_known_to_the_ingress_from_its_creation_to_its_deletion() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (agent, binding) = creating(&store, ada, "helper", SurfaceKind::Slack).await;

    let keys = store
        .slack_app_keys(binding, &team())
        .await
        .unwrap()
        .unwrap();
    assert!(keys.signing_secret.is_none());
    assert!(keys.bot_user.is_none());
    assert!(
        store
            .slack_app_keys(binding, &TeamId::new("T0OTHER"))
            .await
            .unwrap()
            .is_none()
    );

    assert!(
        store
            .set_slack_app(binding, &app("A1"), "helper", at(2_000))
            .await
            .unwrap()
    );
    let keys = store
        .slack_app_keys(binding, &team())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        keys.signing_secret.unwrap().expose_secret(),
        "A1-signing-secret"
    );
    let row = store.slack_app(binding).await.unwrap().unwrap();
    assert_eq!(
        row,
        SlackAppBinding {
            binding,
            agent,
            team: team(),
            state: BindingState::PendingInstall,
            app_id: Some("A1".to_owned()),
            client_id: Some("A1.client".to_owned()),
            scopes: Some("chat:write,im:history".to_owned()),
            bot_user: None,
        }
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
    assert_eq!(
        store
            .slack_client_secret(binding)
            .await
            .unwrap()
            .unwrap()
            .expose_secret(),
        "A1-client-secret"
    );
    assert!(
        !store
            .set_slack_app(binding, &app("A2"), "helper", at(2_001))
            .await
            .unwrap(),
        "only a creating binding takes an app"
    );

    let token = SecretString::from("xoxb-helper");
    assert!(
        !store
            .install_slack_app(binding, "A2", &UserId::new("U0HELPER"), &token, at(3_000))
            .await
            .unwrap(),
        "only the binding's own app installs it"
    );
    assert!(
        store
            .install_slack_app(binding, "A1", &UserId::new("U0HELPER"), &token, at(3_000))
            .await
            .unwrap()
    );
    assert!(
        !store
            .install_slack_app(binding, "A1", &UserId::new("U0HELPER"), &token, at(3_001))
            .await
            .unwrap(),
        "an install happens once"
    );
    assert_eq!(
        store
            .bot_token(binding)
            .await
            .unwrap()
            .unwrap()
            .expose_secret(),
        "xoxb-helper"
    );
    assert!(store.slack_client_secret(binding).await.unwrap().is_none());
    let keys = store
        .slack_app_keys(binding, &team())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(keys.bot_user, Some(UserId::new("U0HELPER")));
    assert_eq!(
        store
            .agent_for_binding(binding)
            .await
            .unwrap()
            .map(|a| a.id),
        Some(agent)
    );

    assert!(store.delete_agent(agent, at(4_000)).await.unwrap());
    assert!(
        store
            .slack_app_keys(binding, &team())
            .await
            .unwrap()
            .is_none()
    );
    let row = store.slack_app(binding).await.unwrap().unwrap();
    assert_eq!(row.state, BindingState::Disabled);
    assert_eq!(
        row.app_id.as_deref(),
        Some("A1"),
        "the app id stays for deleting the app"
    );
    let secrets: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM agent_bindings WHERE client_secret_enc IS NOT NULL \
         OR signing_secret_enc IS NOT NULL OR bot_token_enc IS NOT NULL",
    )
    .fetch_one(&store.pool)
    .await
    .unwrap();
    assert_eq!(secrets, 0, "a deleted agent's binding keeps no secret");
}

#[tokio::test]
async fn secrets_are_sealed_for_their_own_binding_and_column() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (_, first) = pending(&store, ada, "first").await;
    let (_, second) = pending(&store, ada, "second").await;
    let raw: Vec<(Vec<u8>, Vec<u8>)> =
        sqlx::query_as("SELECT client_secret_enc, signing_secret_enc FROM agent_bindings")
            .fetch_all(&store.pool)
            .await
            .unwrap();
    for (client, signing) in &raw {
        for sealed in [client, signing] {
            assert!(!String::from_utf8_lossy(sealed).contains("secret"));
        }
    }
    sqlx::query(
        "UPDATE agent_bindings SET signing_secret_enc = \
         (SELECT client_secret_enc FROM agent_bindings WHERE id = ?) WHERE id = ?",
    )
    .bind(first.to_string())
    .bind(second.to_string())
    .execute(&store.pool)
    .await
    .unwrap();
    assert!(matches!(
        store.slack_app_keys(second, &team()).await,
        Err(StoreError::Seal { .. })
    ));
}

#[tokio::test]
async fn rocket_chat_bindings_are_not_slack_apps() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (_, binding) = creating(&store, ada, "helper", SurfaceKind::RocketChat).await;
    assert!(store.slack_app(binding).await.unwrap().is_none());
    assert!(
        store
            .slack_app_keys(binding, &team())
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !store
            .set_slack_app(binding, &app("A1"), "helper", at(2_000))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn an_install_state_names_its_binding_and_nothing_else_passes() {
    let store = memory_store().await;
    let binding = BindingId::new_v4();
    let state = store.install_state(binding).unwrap();
    assert!(state.starts_with(&format!("{binding}.")));
    assert_eq!(store.binding_of_install_state(&state), Some(binding));
    assert_ne!(
        store.install_state(binding).unwrap(),
        state,
        "each state is new"
    );

    let other = BindingId::new_v4();
    let (_, sealed) = state.split_once('.').unwrap();
    for forged in [
        format!("{other}.{sealed}"),
        format!("{binding}.{}x", sealed),
        format!("{binding}.{}", &sealed[1..]),
        format!("{binding}."),
        binding.to_string(),
        String::new(),
        format!("{}.{sealed}", binding.to_string().to_uppercase()),
        format!("{state}{}", "A".repeat(300)),
    ] {
        assert_eq!(store.binding_of_install_state(&forged), None, "{forged}");
    }

    let elsewhere = memory_store().await;
    assert_eq!(
        elsewhere.binding_of_install_state(&state),
        None,
        "another master key"
    );
}

#[tokio::test]
async fn an_owner_is_reminded_once_of_an_app_still_waiting_for_its_install() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (agent, binding) = pending(&store, ada, "helper").await;
    let (_, installed) = pending(&store, ada, "done").await;
    assert!(
        store
            .install_slack_app(
                installed,
                "Adone",
                &UserId::new("U0DONE"),
                &SecretString::from("x"),
                at(2_500)
            )
            .await
            .unwrap()
    );
    let (deleted, _) = pending(&store, ada, "gone").await;
    assert!(store.delete_agent(deleted, at(2_500)).await.unwrap());

    let workspace = team();
    let due = |since, now| store.due_install_reminders(&workspace, at(since), at(now), MAX);
    assert!(
        due(1_999, 5_000).await.unwrap().is_empty(),
        "not waiting long enough"
    );
    assert!(
        store
            .due_install_reminders(&TeamId::new("T0OTHER"), at(2_000), at(5_000), MAX)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        due(2_000, 5_000).await.unwrap(),
        [InstallReminder {
            binding,
            agent,
            agent_name: "helper".to_owned(),
            owner: ada,
            client_id: "Ahelper.client".to_owned(),
            scopes: "chat:write,im:history".to_owned(),
        }]
    );

    let claim =
        |now: i64| store.claim_install_reminder(binding, at(2_000), at(now), at(now + 600), MAX);
    assert_eq!(claim(5_000).await.unwrap(), Some(1));
    assert_eq!(claim(5_000).await.unwrap(), None, "the lease holds");
    assert!(due(2_000, 5_599).await.unwrap().is_empty());
    assert_eq!(claim(5_600).await.unwrap(), Some(2), "the lease ended");
    assert!(
        store
            .mark_install_reminded(binding, at(5_700))
            .await
            .unwrap()
    );
    assert!(
        !store
            .mark_install_reminded(binding, at(5_701))
            .await
            .unwrap()
    );
    assert!(due(2_000, 9_000).await.unwrap().is_empty(), "reminded once");
    assert_eq!(claim(9_000).await.unwrap(), None);
}

#[tokio::test]
async fn reminder_attempts_are_capped() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let (_, binding) = pending(&store, ada, "helper").await;
    for attempt in 1..=MAX {
        let now = 5_000 + i64::from(attempt) * 1_000;
        assert_eq!(
            store
                .claim_install_reminder(binding, at(2_000), at(now), at(now + 600), MAX)
                .await
                .unwrap(),
            Some(attempt)
        );
    }
    assert!(
        store
            .due_install_reminders(&team(), at(2_000), at(100_000), MAX)
            .await
            .unwrap()
            .is_empty()
    );
}

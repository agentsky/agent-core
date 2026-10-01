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
        redirect_url: "https://agentd.example.com/slack/oauth/callback".to_owned(),
        manifest_version: 0,
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
    assert_eq!(keys.owner, ada);
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
            redirect_url: Some("https://agentd.example.com/slack/oauth/callback".to_owned()),
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
    let (_, binding) = pending(&store, ada, "helper").await;
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
            agent_name: "helper".to_owned(),
            owner: ada,
            client_id: "Ahelper.client".to_owned(),
            scopes: "chat:write,im:history".to_owned(),
            redirect_url: "https://agentd.example.com/slack/oauth/callback".to_owned(),
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

const CURRENT: u32 = 2;

fn outdated(name: &str, blocked: bool) -> crate::OutdatedSlackApp {
    crate::OutdatedSlackApp {
        agent_name: name.to_owned(),
        blocked,
    }
}

/// A Slack agent of `owner` whose app, made from the manifest of
/// `version`, is installed.
async fn installed(store: &Store, owner: MemberId, name: &str, version: u32) -> BindingId {
    let (_, binding) = creating(store, owner, name, SurfaceKind::Slack).await;
    let made = NewSlackApp {
        manifest_version: version,
        ..app(&format!("A{name}"))
    };
    assert!(
        store
            .set_slack_app(binding, &made, name, at(2_000))
            .await
            .unwrap()
    );
    let token = SecretString::from(format!("xoxb-{name}"));
    let bot = UserId::new(format!("U0{}", name.to_uppercase()));
    assert!(
        store
            .install_slack_app(binding, &format!("A{name}"), &bot, &token, at(3_000))
            .await
            .unwrap()
    );
    binding
}

async fn register_token(store: &Store, owner: MemberId, expires_at: i64) {
    store
        .put_slack_config_token(
            owner,
            &team(),
            &crate::NewSlackConfigToken {
                token: SecretString::from("config-token"),
                refresh_token: SecretString::from("refresh-token"),
                expires_at: at(expires_at),
            },
            at(1_000),
        )
        .await
        .unwrap();
}

async fn due_bindings(store: &Store, now: i64) -> Vec<BindingId> {
    store
        .due_manifest_updates(&team(), CURRENT, at(now), 10)
        .await
        .unwrap()
        .into_iter()
        .map(|due| due.binding)
        .collect()
}

#[tokio::test]
async fn older_installed_apps_are_due_while_their_owner_has_a_usable_token() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let old = installed(&store, ada, "old", 1).await;
    installed(&store, ada, "current", CURRENT).await;
    pending(&store, ada, "waiting").await;
    assert!(due_bindings(&store, 5_000).await.is_empty(), "no token yet");

    register_token(&store, ada, 10_000).await;
    let due = store
        .due_manifest_updates(&team(), CURRENT, at(5_000), 10)
        .await
        .unwrap();
    assert_eq!(
        due,
        [ManifestUpdate {
            binding: old,
            owner: ada,
            app_id: "Aold".to_owned(),
        }]
    );
    assert!(
        due_bindings(&store, 10_000).await.is_empty(),
        "the token expired"
    );
    assert!(
        store
            .due_manifest_updates(&TeamId::new("T0ELSE001"), CURRENT, at(5_000), 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .outdated_slack_apps(ada, &team(), CURRENT)
            .await
            .unwrap(),
        [outdated("old", false)]
    );
}

#[tokio::test]
async fn a_manifest_update_is_claimed_once_until_its_lease_ends() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    register_token(&store, ada, 100_000).await;
    let binding = installed(&store, ada, "helper", 0).await;
    let claim = |now: i64| store.claim_manifest_update(binding, CURRENT, at(now), at(now + 3_600));
    assert!(claim(5_000).await.unwrap());
    assert!(!claim(5_000).await.unwrap(), "claimed already");
    assert!(due_bindings(&store, 8_599).await.is_empty(), "leased");
    assert_eq!(due_bindings(&store, 8_600).await, [binding]);
    assert!(claim(8_600).await.unwrap(), "the lease ended");

    assert!(store.set_manifest_version(binding, CURRENT).await.unwrap());
    assert!(!store.set_manifest_version(binding, CURRENT).await.unwrap());
    assert!(
        !store.set_manifest_version(binding, 1).await.unwrap(),
        "never back"
    );
    assert!(due_bindings(&store, 100_000 - 1).await.is_empty());
    assert!(!claim(100_000 - 1).await.unwrap());
    assert!(
        store
            .outdated_slack_apps(ada, &team(), CURRENT)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn registering_a_token_ends_the_owners_manifest_leases_in_that_workspace() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    let bob = owner(&store, "bob").await;
    register_token(&store, ada, 100_000).await;
    register_token(&store, bob, 100_000).await;
    let adas = installed(&store, ada, "helper", 0).await;
    let bobs = installed(&store, bob, "writer", 0).await;
    for binding in [adas, bobs] {
        assert!(
            store
                .claim_manifest_update(binding, CURRENT, at(5_000), at(8_600))
                .await
                .unwrap()
        );
    }
    register_token(&store, ada, 100_000).await;
    assert_eq!(due_bindings(&store, 5_001).await, [adas]);
}

#[tokio::test]
async fn a_failure_ending_the_leases_still_stores_the_token() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    register_token(&store, ada, 100_000).await;
    let binding = installed(&store, ada, "helper", 0).await;
    assert!(
        store
            .claim_manifest_update(binding, CURRENT, at(5_000), at(8_600))
            .await
            .unwrap()
    );
    sqlx::raw_sql(
        "CREATE TRIGGER fail_lease_clear BEFORE UPDATE OF manifest_lease_until ON agent_bindings \
         WHEN NEW.manifest_lease_until IS NULL BEGIN SELECT RAISE(FAIL, 'injected'); END;",
    )
    .execute(&store.pool)
    .await
    .unwrap();
    let stored = store
        .put_slack_config_token(
            ada,
            &team(),
            &crate::NewSlackConfigToken {
                token: SecretString::from("config-token-2"),
                refresh_token: SecretString::from("refresh-token-2"),
                expires_at: at(100_000),
            },
            at(5_001),
        )
        .await
        .expect("the token is stored although the leases stay");
    let token = store
        .usable_slack_config_token(ada, &team(), at(5_001))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(token.row, stored);
    assert!(due_bindings(&store, 5_002).await.is_empty(), "still leased");
    assert_eq!(due_bindings(&store, 8_600).await, [binding]);
}

#[tokio::test]
async fn a_blocked_update_is_never_claimed_again_for_its_version_and_says_so() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    register_token(&store, ada, 100_000).await;
    let binding = installed(&store, ada, "helper", 0).await;
    let other = installed(&store, ada, "writer", 0).await;
    assert!(
        store
            .claim_manifest_update(binding, CURRENT, at(5_000), at(8_600))
            .await
            .unwrap()
    );
    assert!(store.block_manifest_update(binding, CURRENT).await.unwrap());
    assert_eq!(
        due_bindings(&store, 5_001).await,
        [other],
        "the lease ended too"
    );
    assert!(
        !store
            .claim_manifest_update(binding, CURRENT, at(99_000), at(99_100))
            .await
            .unwrap()
    );
    assert_eq!(
        store
            .outdated_slack_apps(ada, &team(), CURRENT)
            .await
            .unwrap(),
        [outdated("helper", true), outdated("writer", false)]
    );
    assert_eq!(
        store
            .due_manifest_updates(&team(), CURRENT + 1, at(5_001), 10)
            .await
            .unwrap()
            .into_iter()
            .map(|due| due.binding)
            .collect::<Vec<_>>(),
        [binding, other],
        "a later version is tried again"
    );
    assert_eq!(
        store
            .outdated_slack_apps(ada, &team(), CURRENT + 1)
            .await
            .unwrap(),
        [outdated("helper", false), outdated("writer", false)]
    );
    assert!(store.set_manifest_version(other, CURRENT).await.unwrap());
    assert!(
        !store.block_manifest_update(other, CURRENT).await.unwrap(),
        "an updated app isn't blocked"
    );
}

#[tokio::test]
async fn deleted_agents_and_rocket_chat_bindings_are_never_due() {
    let store = memory_store().await;
    let ada = owner(&store, "ada").await;
    register_token(&store, ada, 100_000).await;
    let binding = installed(&store, ada, "helper", 0).await;
    let (_, rocket) = creating(&store, ada, "rocket", SurfaceKind::RocketChat).await;
    let agent = store.binding(binding).await.unwrap().unwrap().agent;
    store.delete_agent(agent, at(4_000)).await.unwrap();
    assert!(due_bindings(&store, 5_000).await.is_empty());
    assert!(
        !store
            .claim_manifest_update(rocket, CURRENT, at(5_000), at(8_600))
            .await
            .unwrap()
    );
    assert!(
        store
            .outdated_slack_apps(ada, &team(), CURRENT)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn existing_bindings_start_at_manifest_version_zero() {
    use std::str::FromStr as _;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    const MANIFEST_MIGRATION: i64 = 20_260_930_300_000;
    let dir = TempDir::new();
    let options = SqliteConnectOptions::from_str(&dir.db_url())
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    let before = crate::MIGRATOR
        .iter()
        .map(|migration| migration.version)
        .filter(|version| *version < MANIFEST_MIGRATION)
        .max()
        .unwrap();
    crate::MIGRATOR.run_to(before, &pool).await.unwrap();
    let ada = MemberId::new_v4();
    let agent = AgentId::new_v4();
    let binding = BindingId::new_v4();
    for (sql, binds) in [
        (
            "INSERT INTO members (id, display_name, created_at) VALUES (?, 'ada', 1000)",
            vec![ada.to_string()],
        ),
        (
            "INSERT INTO agents (id, owner_id, name, persona, visibility, state, created_at) \
             VALUES (?, ?, 'helper', 'p', 'public', 'active', 1000)",
            vec![agent.to_string(), ada.to_string()],
        ),
        (
            "INSERT INTO agent_bindings (id, agent_id, surface, team_id, bot_user_id, state, \
             state_changed_at, app_id, app_scopes, app_redirect_url) VALUES (?, ?, 'slack', ?, \
             'U0HELPER', 'active', 3000, 'Ahelper', 'chat:write', \
             'https://agentd.example.com/slack/oauth/callback')",
            vec![binding.to_string(), agent.to_string(), TEAM.to_owned()],
        ),
    ] {
        let mut query = sqlx::query(sql);
        for bind in binds {
            query = query.bind(bind);
        }
        query.execute(&pool).await.unwrap();
    }
    pool.close().await;

    let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
    let version: i64 =
        sqlx::query_scalar("SELECT manifest_version FROM agent_bindings WHERE id = ?")
            .bind(binding.to_string())
            .fetch_one(&store.pool)
            .await
            .unwrap();
    assert_eq!(version, 0);
    register_token(&store, ada, 100_000).await;
    assert_eq!(
        store
            .due_manifest_updates(&team(), 1, at(5_000), 10)
            .await
            .unwrap()
            .into_iter()
            .map(|due| due.binding)
            .collect::<Vec<_>>(),
        [binding]
    );
    assert_eq!(
        store.outdated_slack_apps(ada, &team(), 1).await.unwrap(),
        [outdated("helper", false)]
    );
}

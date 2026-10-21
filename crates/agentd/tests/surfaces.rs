//! Which surface an agent's bot acts through: `StoreSurfaces`, over the
//! agents' bindings, on Rocket.Chat and Slack.

mod common;

use std::sync::Arc;

use agentd::slack::manager::{ManagerIdentity, SlackManager};
use agentd::{App, Config};
use core_types::{AgentId, ConvRef, MemberKey, SurfaceKind, TeamId, UserId};
use secrecy::SecretString;
use store::{AgentCreation, NewAgent, Store, Visibility};
use surface_slack::SlackClient;
use testkit::rocketchat::FakeRest;
use time::OffsetDateTime;

use common::{CONFIG, master_key};

async fn agent_on(store: &Store, surface: SurfaceKind, team: &str, bot: &str) -> AgentId {
    let now = OffsetDateTime::now_utc();
    let owner = store
        .ensure_member(
            &MemberKey {
                surface,
                team: team.into(),
                user: format!("owner-{bot}").into(),
            },
            "owner",
            now,
        )
        .await
        .unwrap();
    let team = TeamId::new(team);
    let AgentCreation::Created(agent, binding) = store
        .create_agent(
            &NewAgent {
                owner,
                name: bot,
                persona: "p",
                visibility: Visibility::Public,
                surface,
                team: &team,
            },
            10,
            now,
        )
        .await
        .unwrap()
    else {
        panic!("created");
    };
    store
        .set_binding_bot_user(binding, &UserId::new(bot), bot)
        .await
        .unwrap();
    store
        .activate_binding(binding, &SecretString::from("token"), now)
        .await
        .unwrap();
    agent.id
}

fn conv(surface: SurfaceKind, team: &str) -> ConvRef {
    ConvRef {
        surface,
        team: team.into(),
        conversation: "C1".into(),
    }
}

#[tokio::test]
async fn an_agents_bot_acts_through_its_active_binding_on_the_conversations_team() {
    let fake = FakeRest::start().await;
    let text = format!(
        "{CONFIG}\n[rocketchat]\nbase_url = \"{}\"\nteam = \"chat.example\"\nmanager_user_id = \"{}\"\n",
        fake.uri(),
        FakeRest::MANAGER_ID
    );
    let env = vec![
        ("AGENTD_MASTER_KEY".to_owned(), master_key()),
        (
            "AGENTD_RC_MANAGER_TOKEN".to_owned(),
            FakeRest::MANAGER_TOKEN.to_owned(),
        ),
    ];
    let config = Config::parse(&text, env).unwrap();
    let store = agentd::app::open_store(&config).await.unwrap();
    let client = SlackClient::new("http://127.0.0.1:9/api/").unwrap();
    let api = client.bot(SecretString::from("xoxb-manager"));
    let slack = SlackManager::with_identity(
        client,
        api,
        ManagerIdentity {
            team: "T1".into(),
            bot_user: "UMANAGER".into(),
            bot_id: "B1".into(),
            app_id: "A1".into(),
            app_name: None,
        },
    );
    let app = App::new(config, store.clone(), Some(slack)).unwrap();
    let surfaces = app.surfaces();

    let rc = agent_on(&store, SurfaceKind::RocketChat, "chat.example", "rcbot").await;
    let here = conv(SurfaceKind::RocketChat, "chat.example");
    let first = surfaces
        .surface(rc, &here)
        .await
        .unwrap()
        .expect("a surface");
    let again = surfaces.surface(rc, &here).await.unwrap().unwrap();
    assert!(Arc::ptr_eq(&first, &again), "one surface per binding");
    assert!(first.caps().supports_threads);
    assert!(
        surfaces
            .surface(rc, &conv(SurfaceKind::RocketChat, "other.example"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        surfaces
            .surface(rc, &conv(SurfaceKind::Slack, "T1"))
            .await
            .unwrap()
            .is_none(),
        "no binding on Slack"
    );

    let slack_agent = agent_on(&store, SurfaceKind::Slack, "T1", "USLACKBOT").await;
    let slack_surface = surfaces
        .surface(slack_agent, &conv(SurfaceKind::Slack, "T1"))
        .await
        .unwrap()
        .expect("a Slack surface");
    assert!(slack_surface.caps().per_binding_delivery);
    let other_team = agent_on(&store, SurfaceKind::Slack, "T2", "UOTHER").await;
    assert!(
        surfaces
            .surface(other_team, &conv(SurfaceKind::Slack, "T2"))
            .await
            .unwrap()
            .is_none(),
        "agentd serves the manager app's workspace only"
    );

    assert!(
        store
            .delete_agent(rc, OffsetDateTime::now_utc())
            .await
            .unwrap()
    );
    assert!(surfaces.surface(rc, &here).await.unwrap().is_none());
    assert!(
        surfaces
            .surface(AgentId::new_v4(), &here)
            .await
            .unwrap()
            .is_none()
    );
    assert!(format!("{surfaces:?}").contains("StoreSurfaces"));
}

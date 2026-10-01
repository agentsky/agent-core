use core_types::{MemberKey, SurfaceKind, TeamId, UserId};

use super::*;
use crate::ChannelIdChangeRecord::{Full, Known, Recorded};
use crate::agents::{AgentCreation, NewAgent, Visibility};
use crate::test_util::*;

async fn binding(store: &Store, name: &str) -> BindingId {
    let team = TeamId::new("T0TEAM001");
    let key = MemberKey {
        surface: SurfaceKind::Slack,
        team: team.clone(),
        user: UserId::new(format!("U0{}", name.to_uppercase())),
    };
    let owner = store.ensure_member(&key, name, at(1)).await.unwrap();
    let new = NewAgent {
        owner,
        name,
        persona: "p",
        visibility: Visibility::Public,
        surface: SurfaceKind::Slack,
        team: &team,
    };
    match store.create_agent(&new, 10, at(1)).await.unwrap() {
        AgentCreation::Created(_, binding) => binding,
        other => panic!("{other:?}"),
    }
}

fn change(binding: BindingId, old: &str, new: &str, received_at: i64) -> ChannelIdChange {
    ChannelIdChange {
        binding,
        old: old.into(),
        new: new.into(),
        received_at: at(received_at),
    }
}

#[tokio::test]
async fn a_change_is_recorded_once_and_due_at_once() {
    let store = memory_store().await;
    let helper = binding(&store, "helper").await;
    let writer = binding(&store, "writer").await;
    let first = change(helper, "G0PRIVAT1", "C0PRIVAT1", 1_000);
    assert_eq!(
        store.record_channel_id_change(&first, 2).await.unwrap(),
        Recorded
    );
    let again = change(helper, "G0PRIVAT1", "C0PRIVAT1", 1_100);
    assert_eq!(
        store.record_channel_id_change(&again, 2).await.unwrap(),
        Known
    );
    let second = change(helper, "G0PRIVAT2", "C0PRIVAT2", 1_050);
    assert_eq!(
        store.record_channel_id_change(&second, 2).await.unwrap(),
        Recorded
    );
    let third = change(helper, "G0PRIVAT3", "C0PRIVAT3", 1_060);
    assert_eq!(
        store.record_channel_id_change(&third, 2).await.unwrap(),
        Full,
        "the binding has as many recorded as it may"
    );
    let others = change(writer, "G0PRIVAT3", "C0PRIVAT3", 1_060);
    assert_eq!(
        store.record_channel_id_change(&others, 2).await.unwrap(),
        Recorded,
        "another binding has its own"
    );
    assert_eq!(
        store.due_channel_id_changes(at(1_055), 10).await.unwrap(),
        [first.clone(), second.clone()]
    );
    assert_eq!(
        store.due_channel_id_changes(at(2_000), 2).await.unwrap(),
        [first, second]
    );
    assert!(
        store
            .record_channel_id_change(&change(BindingId::new_v4(), "G0X", "C0X", 1), 2)
            .await
            .is_err(),
        "no such binding"
    );
}

#[tokio::test]
async fn a_try_is_claimed_once_until_its_retry_is_due_and_a_settled_change_stays_known() {
    let store = memory_store().await;
    let helper = binding(&store, "helper").await;
    let changed = change(helper, "G0PRIVAT1", "C0PRIVAT1", 1_000);
    store.record_channel_id_change(&changed, 16).await.unwrap();
    let claim = |now: i64| store.claim_channel_id_change(&changed, at(now), at(now + 300));
    assert!(claim(1_000).await.unwrap());
    assert!(!claim(1_299).await.unwrap(), "claimed until 1,300");
    assert!(
        store
            .due_channel_id_changes(at(1_299), 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.due_channel_id_changes(at(1_300), 10).await.unwrap(),
        std::slice::from_ref(&changed)
    );
    assert!(claim(1_300).await.unwrap());
    assert!(
        store
            .settle_channel_id_change(&changed, at(1_310))
            .await
            .unwrap()
    );
    assert!(
        !store
            .settle_channel_id_change(&changed, at(1_320))
            .await
            .unwrap()
    );
    assert!(!claim(9_999).await.unwrap(), "settled");
    assert!(
        store
            .due_channel_id_changes(at(9_999), 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.record_channel_id_change(&changed, 16).await.unwrap(),
        Known,
        "a settled change is known while it is kept"
    );
    assert!(store.delete_channel_id_change(&changed).await.unwrap());
    assert!(!store.delete_channel_id_change(&changed).await.unwrap());
    assert_eq!(
        store.record_channel_id_change(&changed, 16).await.unwrap(),
        Recorded
    );
}

#[tokio::test]
async fn waiting_changes_received_long_ago_expire_and_settled_ones_are_purged() {
    let store = memory_store().await;
    let helper = binding(&store, "helper").await;
    let waiting = change(helper, "G0PRIVAT1", "C0PRIVAT1", 1_000);
    let settled = change(helper, "G0PRIVAT2", "C0PRIVAT2", 1_000);
    let recent = change(helper, "G0PRIVAT3", "C0PRIVAT3", 5_000);
    for changed in [&waiting, &settled, &recent] {
        store.record_channel_id_change(changed, 16).await.unwrap();
    }
    store
        .settle_channel_id_change(&settled, at(1_001))
        .await
        .unwrap();
    assert_eq!(
        store
            .expired_channel_id_changes(at(5_000), 10)
            .await
            .unwrap(),
        std::slice::from_ref(&waiting)
    );
    assert_eq!(
        store
            .purge_settled_channel_id_changes(at(5_000))
            .await
            .unwrap(),
        1
    );
    let kept: Vec<_> = store
        .channel_id_changes_of_agent(store.binding(helper).await.unwrap().unwrap().agent)
        .await
        .unwrap()
        .into_iter()
        .map(|known| known.change)
        .collect();
    assert_eq!(kept, [waiting, recent], "a waiting change is never purged");
}

#[tokio::test]
async fn an_agents_changes_name_their_workspace_and_whether_they_wait() {
    let store = memory_store().await;
    let helper = binding(&store, "helper").await;
    let writer = binding(&store, "writer").await;
    let first = change(helper, "G0PRIVAT1", "C0PRIVAT1", 1_000);
    let second = change(helper, "C0PRIVAT1", "C0PRIVAT2", 1_100);
    for changed in [
        &first,
        &second,
        &change(writer, "G0OTHER01", "C0OTHER01", 900),
    ] {
        store.record_channel_id_change(changed, 16).await.unwrap();
    }
    store
        .settle_channel_id_change(&first, at(1_050))
        .await
        .unwrap();
    let agent = store.binding(helper).await.unwrap().unwrap().agent;
    assert_eq!(
        store.channel_id_changes_of_agent(agent).await.unwrap(),
        [
            KnownChannelIdChange {
                change: first,
                team: TeamId::new("T0TEAM001"),
                waiting: false,
            },
            KnownChannelIdChange {
                change: second,
                team: TeamId::new("T0TEAM001"),
                waiting: true,
            },
        ]
    );
}

#[tokio::test]
async fn only_two_different_channel_ids_are_stored() {
    let store = memory_store().await;
    let helper = binding(&store, "helper").await;
    for (old, new) in [
        ("G0PRIVAT1", "G0PRIVAT1"),
        ("U0HUMAN01", "C0PRIVAT1"),
        ("G0PRIVAT1", "c0privat1"),
        ("G0PRIVAT1", "C"),
        ("G0PRIVAT1", "C0PRIVAT1 "),
    ] {
        assert!(
            store
                .record_channel_id_change(&change(helper, old, new, 1_000), 16)
                .await
                .is_err(),
            "{old} to {new}"
        );
    }
}

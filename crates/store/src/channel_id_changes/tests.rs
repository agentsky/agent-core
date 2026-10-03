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
        "the binding has as many as it may keep, all waiting"
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
    store.record_channel_id_change(&changed, 64).await.unwrap();
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
        store.record_channel_id_change(&changed, 64).await.unwrap(),
        Known,
        "a settled change is known while it is kept"
    );
    assert!(store.delete_channel_id_change(&changed).await.unwrap());
    assert!(!store.delete_channel_id_change(&changed).await.unwrap());
    assert_eq!(
        store.record_channel_id_change(&changed, 64).await.unwrap(),
        Recorded
    );
}

#[tokio::test]
async fn waiting_changes_received_long_ago_expire_and_settled_ones_are_purged_after_them() {
    let store = memory_store().await;
    let helper = binding(&store, "helper").await;
    let waiting = change(helper, "G0PRIVAT1", "C0PRIVAT1", 1_000);
    let settled = change(helper, "C0PRIVAT1", "C0PRIVAT2", 900);
    let recent = change(helper, "G0PRIVAT3", "C0PRIVAT3", 5_000);
    for changed in [&waiting, &settled, &recent] {
        store.record_channel_id_change(changed, 64).await.unwrap();
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
    let agent = store.binding(helper).await.unwrap().unwrap().agent;
    let kept = async || -> Vec<ChannelIdChange> {
        store
            .channel_id_changes_of_agent(agent)
            .await
            .unwrap()
            .into_iter()
            .map(|known| known.change)
            .collect()
    };
    assert_eq!(
        store
            .purge_settled_channel_id_changes(at(5_000))
            .await
            .unwrap(),
        0,
        "the waiting change's chain goes through it"
    );
    assert_eq!(
        kept().await,
        [settled.clone(), waiting.clone(), recent.clone()]
    );
    for changed in [&waiting, &recent] {
        store.delete_channel_id_change(changed).await.unwrap();
    }
    assert_eq!(
        store
            .purge_settled_channel_id_changes(at(5_000))
            .await
            .unwrap(),
        1
    );
    assert!(kept().await.is_empty());
}

#[tokio::test]
async fn the_earliest_settled_changes_make_room_and_waiting_ones_fill_it() {
    let store = memory_store().await;
    let helper = binding(&store, "helper").await;
    let mut recorded = Vec::new();
    for n in 0..20 {
        let changed = change(
            helper,
            &format!("G0BUSY{n:03}"),
            &format!("C0BUSY{n:03}"),
            1_000 + n,
        );
        assert_eq!(
            store.record_channel_id_change(&changed, 4).await.unwrap(),
            Recorded,
            "change {n}"
        );
        store
            .settle_channel_id_change(&changed, at(1_000 + n))
            .await
            .unwrap();
        recorded.push(changed);
    }
    let agent = store.binding(helper).await.unwrap().unwrap().agent;
    let kept = async || -> Vec<ChannelIdChange> {
        store
            .channel_id_changes_of_agent(agent)
            .await
            .unwrap()
            .into_iter()
            .map(|known| known.change)
            .collect()
    };
    assert_eq!(kept().await, recorded[16..], "the latest four are kept");

    let waiting: Vec<ChannelIdChange> = (0..4)
        .map(|n| {
            change(
                helper,
                &format!("G0WAIT{n:03}"),
                &format!("C0WAIT{n:03}"),
                2_000 + n,
            )
        })
        .collect();
    for changed in &waiting {
        assert_eq!(
            store.record_channel_id_change(changed, 4).await.unwrap(),
            Recorded
        );
    }
    assert_eq!(
        store
            .record_channel_id_change(&change(helper, "G0WAIT999", "C0WAIT999", 2_010), 4)
            .await
            .unwrap(),
        Full,
        "four wait already"
    );
    assert_eq!(
        kept().await,
        waiting,
        "waiting changes are never deleted to make room"
    );
}

#[tokio::test]
async fn a_new_change_keeps_the_settled_ones_its_own_chain_runs_through() {
    let store = memory_store().await;
    let helper = binding(&store, "helper").await;
    let onward = change(helper, "G0LINK00N", "C0LINK00M", 1_000);
    let sibling = change(helper, "G0LINK00X", "C0LINK00Y", 1_001);
    let other = change(helper, "G0OTHER01", "C0OTHER01", 1_002);
    let later = change(helper, "G0OTHER02", "C0OTHER02", 1_003);
    for changed in [&onward, &sibling, &other, &later] {
        assert_eq!(
            store.record_channel_id_change(changed, 4).await.unwrap(),
            Recorded
        );
        assert!(
            store
                .settle_channel_id_change(changed, changed.received_at)
                .await
                .unwrap()
        );
    }
    let forged = change(helper, "G0LINK00X", "G0LINK00N", 1_004);
    assert_eq!(
        store.record_channel_id_change(&forged, 4).await.unwrap(),
        Recorded
    );
    let agent = store.binding(helper).await.unwrap().unwrap().agent;
    let kept: Vec<ChannelIdChange> = store
        .channel_id_changes_of_agent(agent)
        .await
        .unwrap()
        .into_iter()
        .map(|known| known.change)
        .collect();
    assert_eq!(
        kept,
        [onward, sibling, later, forged],
        "the links from its old and new ids stay, the earliest other one goes"
    );
}

#[tokio::test]
async fn a_known_change_or_a_full_binding_is_refused_without_the_write_lock() {
    let dir = crate::test_util::TempDir::new();
    let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
    let helper = binding(&store, "helper").await;
    let waiting = change(helper, "G0LOCKED1", "C0LOCKED1", 1_000);
    assert_eq!(
        store.record_channel_id_change(&waiting, 1).await.unwrap(),
        Recorded
    );
    let lock = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let record = |changed: ChannelIdChange| {
        let store = store.clone();
        async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                store.record_channel_id_change(&changed, 1),
            )
            .await
        }
    };
    assert_eq!(record(waiting.clone()).await.unwrap().unwrap(), Known);
    let another = change(helper, "G0LOCKED2", "C0LOCKED2", 1_001);
    assert_eq!(record(another.clone()).await.unwrap().unwrap(), Full);
    drop(lock);
    assert!(
        store
            .settle_channel_id_change(&waiting, at(1_002))
            .await
            .unwrap()
    );
    let lock = store.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    assert!(
        record(another).await.is_err(),
        "one with room waits for the lock"
    );
    drop(lock);
}

#[tokio::test]
async fn settled_changes_a_waiting_chain_runs_through_are_never_forgotten() {
    let store = memory_store().await;
    let helper = binding(&store, "helper").await;
    let record =
        async |changed: &ChannelIdChange| store.record_channel_id_change(changed, 4).await.unwrap();
    let waiting = change(helper, "G0CHAIN0A", "C0CHAIN0B", 1_000);
    let onward = change(helper, "C0CHAIN0B", "C0CHAIN0C", 1_001);
    let further = change(helper, "C0CHAIN0C", "C0CHAIN0D", 1_002);
    let other = change(helper, "G0OTHER01", "C0OTHER01", 1_003);
    assert_eq!(record(&waiting).await, Recorded);
    for changed in [&onward, &further, &other] {
        assert_eq!(record(changed).await, Recorded);
        assert!(
            store
                .settle_channel_id_change(changed, changed.received_at)
                .await
                .unwrap()
        );
    }
    let agent = store.binding(helper).await.unwrap().unwrap().agent;
    let kept = async || -> Vec<ChannelIdChange> {
        store
            .channel_id_changes_of_agent(agent)
            .await
            .unwrap()
            .into_iter()
            .map(|known| known.change)
            .collect()
    };

    let first = change(helper, "G0NEW0001", "C0NEW0001", 1_004);
    assert_eq!(record(&first).await, Recorded);
    assert_eq!(
        kept().await,
        [
            waiting.clone(),
            onward.clone(),
            further.clone(),
            first.clone()
        ],
        "the earliest settled change off the waiting chain made room"
    );

    let second = change(helper, "G0NEW0002", "C0NEW0002", 1_005);
    assert_eq!(record(&second).await, Full, "none can go");
    assert_eq!(kept().await.len(), 4);

    assert!(
        store
            .settle_channel_id_change(&waiting, at(1_006))
            .await
            .unwrap()
    );
    assert_eq!(record(&second).await, Recorded);
    assert_eq!(
        kept().await,
        [onward, further, first, second],
        "a chain no change waits on can go"
    );
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
        store.record_channel_id_change(changed, 64).await.unwrap();
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
                .record_channel_id_change(&change(helper, old, new, 1_000), 64)
                .await
                .is_err(),
            "{old} to {new}"
        );
    }
}

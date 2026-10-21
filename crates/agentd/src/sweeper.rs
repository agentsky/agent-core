//! The background sweeper: deletes expired rows from the store every
//! [`SWEEP_INTERVAL`].

use std::time::Duration;

use store::Store;
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

/// How often the sweeper runs.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Calls [`Store::sweep_expired`] now and then every `every`, until
/// `shutdown` becomes true or its sender is dropped. A sweep in progress
/// finishes before it returns. A failed sweep is logged and retried at the
/// next tick.
pub async fn run(store: Store, every: Duration, mut shutdown: watch::Receiver<bool>) {
    let mut ticks = tokio::time::interval(every);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        let stop = tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => true,
            _ = ticks.tick() => false,
        };
        if stop {
            break;
        }
        sweep_once(&store).await;
    }
}

/// Runs one sweep, logging what it deleted or why it failed.
pub async fn sweep_once(store: &Store) {
    match store.sweep_expired(OffsetDateTime::now_utc()).await {
        Ok(swept) if swept == store::Swept::default() => {}
        Ok(swept) => tracing::debug!(
            pending_logins = swept.pending_logins,
            processed_events = swept.processed_events,
            "swept expired rows"
        ),
        Err(err) => tracing::warn!(error = %err, "sweeping expired rows failed"),
    }
}

#[cfg(test)]
mod tests {
    use core_types::{MemberKey, SurfaceKind, TeamId, UserId};
    use secrecy::SecretString;

    use super::*;
    use crate::telemetry::tests::global_logs;

    async fn store_with_expired_login() -> Store {
        let sealer = store::Sealer::from_base64(&store::Sealer::generate_key().unwrap()).unwrap();
        let store = Store::open_in_memory(sealer).await.unwrap();
        let member = store
            .ensure_member(
                &MemberKey {
                    surface: SurfaceKind::RocketChat,
                    team: TeamId::new("chat.example.org"),
                    user: UserId::new("u1"),
                },
                "Ada",
                OffsetDateTime::from_unix_timestamp(0).unwrap(),
            )
            .await
            .unwrap();
        store
            .put_pending_login(
                "state",
                member,
                &SecretString::from("verifier"),
                OffsetDateTime::from_unix_timestamp(1_000).unwrap(),
            )
            .await
            .unwrap();
        store
    }

    #[tokio::test]
    async fn sweeps_at_start_and_on_every_tick_until_shutdown() {
        let store = store_with_expired_login().await;
        let (stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(run(store.clone(), Duration::from_millis(20), shutdown));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while store.take_pending_login("state").await.unwrap().is_some() {
            assert!(tokio::time::Instant::now() < deadline, "never swept");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        stop.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn stops_when_the_sender_is_dropped() {
        let store = store_with_expired_login().await;
        let (stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(run(store, SWEEP_INTERVAL, shutdown));
        drop(stop);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn logs_what_it_swept_and_failures() {
        let store = store_with_expired_login().await;
        let logs = global_logs().tag();

        sweep_once(&store).await;
        sweep_once(&store).await;
        store.close().await;
        sweep_once(&store).await;

        let out = logs.snapshot();
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|line| line["target"] == "agentd::sweeper")
            .collect();
        assert_eq!(lines.len(), 2, "{out}");
        assert_eq!(lines[0]["fields"]["message"], "swept expired rows");
        assert_eq!(lines[0]["fields"]["pending_logins"], 1);
        assert_eq!(
            lines[1]["fields"]["message"],
            "sweeping expired rows failed"
        );
        assert_eq!(lines[1]["level"], "WARN");
    }
}

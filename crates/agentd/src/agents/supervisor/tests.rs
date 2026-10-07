//! [`Connections`]' bookkeeping.

use super::*;

#[tokio::test]
async fn an_older_connection_ending_keeps_the_newer_one_running() {
    let mut connections = Connections::default();
    let binding = BindingId::new_v4();
    let older = connections.tasks.spawn(async {}).id();
    connections.starts.insert(older, (binding, 1));
    let (stop, _) = watch::channel(false);
    connections.running.insert(
        binding,
        Running {
            generation: 2,
            started: Instant::now(),
            stop,
        },
    );

    connections.ended(Ok(older), Duration::from_secs(60));

    assert_eq!(
        connections.running.get(&binding).map(|r| r.generation),
        Some(2)
    );
    assert!(connections.failing.is_empty(), "the newer one didn't fail");
}

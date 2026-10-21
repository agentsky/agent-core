//! [`KeyedLocks`]: one async mutex per key, created on demand.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// One async mutex per key. An entry lives only while someone holds or waits
/// for its lock, so the map doesn't grow with every key ever locked.
#[derive(Debug)]
pub(crate) struct KeyedLocks<K> {
    map: Arc<Mutex<HashMap<K, Arc<AsyncMutex<()>>>>>,
}

impl<K> Default for KeyedLocks<K> {
    fn default() -> Self {
        Self {
            map: Arc::default(),
        }
    }
}

impl<K: Eq + Hash + Clone> KeyedLocks<K> {
    /// Waits for `key`'s lock and holds it until the guard is dropped.
    ///
    /// Cancel-safe: if the returned future is dropped while waiting, the
    /// key's entry is cleaned up as if the lock had been taken and released.
    pub(crate) async fn lock(&self, key: K) -> KeyedGuard<K> {
        let cleanup = Cleanup {
            key: key.clone(),
            map: Arc::clone(&self.map),
        };
        let mutex = {
            let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(map.entry(key).or_default())
        };
        KeyedGuard {
            _guard: mutex.lock_owned().await,
            _cleanup: cleanup,
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// Holds one key's lock. Dropping it releases the lock, and removes the
/// key's entry if nobody else holds or waits for it.
///
/// Fields drop in declaration order, so the lock (and its reference to the
/// entry) is gone before the cleanup looks at the entry.
#[derive(Debug)]
pub(crate) struct KeyedGuard<K: Eq + Hash> {
    _guard: OwnedMutexGuard<()>,
    _cleanup: Cleanup<K>,
}

#[derive(Debug)]
struct Cleanup<K: Eq + Hash> {
    key: K,
    map: Arc<Mutex<HashMap<K, Arc<AsyncMutex<()>>>>>,
}

impl<K: Eq + Hash> Drop for Cleanup<K> {
    fn drop(&mut self) {
        let mut map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if map
            .get(&self.key)
            .is_some_and(|mutex| Arc::strong_count(mutex) == 1)
        {
            map.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_key_is_held_by_one_task_at_a_time() {
        let locks = Arc::new(KeyedLocks::default());
        let inside = Arc::new(AtomicUsize::new(0));
        let max = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let (locks, inside, max) = (locks.clone(), inside.clone(), max.clone());
                tokio::spawn(async move {
                    let _guard = locks.lock("member").await;
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    max.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(2)).await;
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(max.load(Ordering::SeqCst), 1);
        assert_eq!(locks.len(), 0);
    }

    #[tokio::test]
    async fn different_keys_do_not_block_each_other() {
        let locks = KeyedLocks::default();
        let _a = locks.lock("a").await;
        let b = tokio::time::timeout(Duration::from_secs(1), locks.lock("b")).await;
        assert!(b.is_ok());
        assert_eq!(locks.len(), 2);
    }

    #[tokio::test]
    async fn entries_are_removed_when_released() {
        let locks = KeyedLocks::default();
        let a = locks.lock("a").await;
        assert_eq!(locks.len(), 1);
        drop(a);
        assert_eq!(locks.len(), 0);
        let again = locks.lock("a").await;
        assert_eq!(locks.len(), 1);
        drop(again);
        assert_eq!(locks.len(), 0);
    }

    #[tokio::test]
    async fn a_waiter_keeps_the_entry_alive() {
        let locks = Arc::new(KeyedLocks::default());
        let first = locks.lock("a").await;
        let waiter = {
            let locks = locks.clone();
            tokio::spawn(async move {
                let _guard = locks.lock("a").await;
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(first);
        assert!(locks.len() <= 1);
        waiter.await.unwrap();
        assert_eq!(locks.len(), 0);
    }

    #[tokio::test]
    async fn a_cancelled_waiter_leaves_no_entry() {
        let locks = KeyedLocks::default();
        let first = locks.lock("a").await;
        let waited = tokio::time::timeout(Duration::from_millis(10), locks.lock("a")).await;
        assert!(waited.is_err());
        drop(first);
        assert_eq!(locks.len(), 0);
    }
}

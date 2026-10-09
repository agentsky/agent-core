//! [`KeyedLocks`]: one async mutex per key, created on demand.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// One async mutex per key, granted in the order it was asked for. An
/// entry lives only while someone holds or waits for its lock, so the map
/// doesn't grow with every key ever locked.
pub(super) struct KeyedLocks<K> {
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
    /// Waits for `key`'s lock, after those who asked for it before, and
    /// holds it until the guard is dropped.
    ///
    /// Cancel-safe: if the returned future is dropped while waiting, the
    /// key's entry is cleaned up as if the lock had been taken and released.
    pub(super) async fn lock(&self, key: K) -> KeyedGuard<K> {
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
pub(super) struct KeyedGuard<K: Eq + Hash> {
    _guard: OwnedMutexGuard<()>,
    _cleanup: Cleanup<K>,
}

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
    use std::task::Poll;

    use futures::poll;

    use super::*;

    #[tokio::test]
    async fn waiters_get_a_key_in_the_order_they_asked() {
        let locks = KeyedLocks::default();
        let held = locks.lock("thread").await;
        let mut first = Box::pin(locks.lock("thread"));
        let mut second = Box::pin(locks.lock("thread"));
        assert!(poll!(&mut first).is_pending());
        assert!(poll!(&mut second).is_pending());
        let other = locks.lock("another thread").await;
        assert_eq!(locks.len(), 2);

        drop(held);
        assert!(poll!(&mut second).is_pending(), "the first waiter is next");
        let Poll::Ready(guard) = poll!(&mut first) else {
            panic!("the first waiter has the lock");
        };
        assert!(poll!(&mut second).is_pending());
        drop(guard);
        assert!(poll!(&mut second).is_ready());
        drop(other);
        assert_eq!(locks.len(), 0);
    }

    #[tokio::test]
    async fn a_cancelled_waiter_leaves_no_entry() {
        let locks = KeyedLocks::default();
        let held = locks.lock("thread").await;
        let mut waiter = Box::pin(locks.lock("thread"));
        assert!(poll!(&mut waiter).is_pending());
        drop(waiter);
        drop(held);
        assert_eq!(locks.len(), 0);

        let held = locks.lock("thread").await;
        let mut granted = Box::pin(locks.lock("thread"));
        assert!(poll!(&mut granted).is_pending());
        drop(held);
        drop(granted);
        assert_eq!(locks.len(), 0, "granted the lock but dropped unpolled");
    }
}

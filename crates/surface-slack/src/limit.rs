//! A simple client-side rate limiter for the Web API.
//!
//! Slack limits each Web API method per app and workspace, which is per bot
//! token, in tiers of requests per minute. `chat.postMessage` has its own
//! limit of about one message per second per channel, with bursts allowed.
//! The limiter keeps, for each token and method (and channel, for
//! `chat.postMessage`), the times of the calls in the last minute, and makes
//! a call wait while the tier's quota for that minute is used up. A 429
//! doesn't say which limit it hit, so after one the limiter holds every call
//! to that method with that token, in any channel, until `Retry-After` has
//! passed, so concurrent callers don't keep hitting the limit. Each
//! channel's quota stays its own.
//!
//! Tokens are never stored: a bucket is keyed by a digest of the token.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use sha2::{Digest, Sha256};
use tokio::time::Instant;

/// The window a tier's quota counts over.
const WINDOW: Duration = Duration::from_secs(60);

/// Buckets kept before idle ones are dropped.
const MAX_BUCKETS: usize = 4096;

/// A Web API rate-limit tier, as Slack's SDKs tag each method
/// (`MethodsRateLimits` in `slackapi/java-slack-sdk`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Tier {
    /// 20 requests per minute.
    Tier2,
    /// 50 requests per minute.
    Tier3,
    /// 100 requests per minute.
    Tier4,
    /// `chat.postMessage`: 60 per minute per channel.
    PostMessage,
    /// `auth.test`: several hundred per minute.
    AuthTest,
}

impl Tier {
    /// The calls allowed per minute in one bucket.
    pub(crate) const fn per_minute(self) -> usize {
        match self {
            Self::Tier2 => 20,
            Self::Tier3 => 50,
            Self::Tier4 => 100,
            Self::PostMessage => 60,
            Self::AuthTest => 600,
        }
    }
}

/// A digest of a bot token, so buckets never hold the token itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TokenKey([u8; 16]);

impl TokenKey {
    pub(crate) fn of(token: &SecretString) -> Self {
        let digest = Sha256::digest(token.expose_secret().as_bytes());
        let mut key = [0; 16];
        key.copy_from_slice(&digest[..16]);
        Self(key)
    }
}

/// What one quota applies to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Bucket {
    token: TokenKey,
    method: &'static str,
    channel: Option<String>,
}

impl Bucket {
    pub(crate) fn new(token: TokenKey, method: &'static str, channel: Option<&str>) -> Self {
        Self {
            token,
            method,
            channel: channel.map(str::to_owned),
        }
    }

    /// The bucket of the same token and method in every channel, which
    /// holds the method's 429 block.
    fn method_wide(&self) -> Self {
        Self {
            token: self.token,
            method: self.method,
            channel: None,
        }
    }
}

/// Why a call has to wait, and until when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// The tier's quota for the last minute is used up.
    Quota(Instant),
    /// A 429 asked to wait.
    Blocked(Instant),
}

#[derive(Debug, Default)]
struct Window {
    calls: VecDeque<Instant>,
    blocked_until: Option<Instant>,
}

impl Window {
    fn idle(&self, now: Instant) -> bool {
        self.blocked_until.is_none_or(|until| until <= now)
            && self
                .calls
                .back()
                .is_none_or(|last| now.duration_since(*last) >= WINDOW)
    }
}

/// The limiter, shared by every token a [`SlackClient`](crate::SlackClient)
/// serves.
#[derive(Debug, Default)]
pub(crate) struct Limiter {
    buckets: Mutex<HashMap<Bucket, Window>>,
}

impl Limiter {
    /// Waits until `bucket` may make one more call under `tier`, and counts
    /// it.
    ///
    /// # Errors
    ///
    /// When a 429 blocked the bucket for longer than `max_block`, returns
    /// how much longer it is blocked, without waiting.
    pub(crate) async fn acquire(
        &self,
        bucket: &Bucket,
        tier: Tier,
        max_block: Duration,
    ) -> Result<(), Duration> {
        loop {
            let now = Instant::now();
            match self.try_acquire(bucket, tier, now) {
                None => return Ok(()),
                Some(Wait::Blocked(until)) if until - now > max_block => return Err(until - now),
                Some(Wait::Blocked(until) | Wait::Quota(until)) => {
                    tokio::time::sleep_until(until).await;
                }
            }
        }
    }

    /// Counts a call and returns `None` if `bucket`'s method isn't blocked
    /// and `bucket` has quota left at `now`, or else says until when to
    /// wait.
    fn try_acquire(&self, bucket: &Bucket, tier: Tier, now: Instant) -> Option<Wait> {
        let mut buckets = self.lock();
        if let Some(until) = buckets
            .get(&bucket.method_wide())
            .and_then(|window| window.blocked_until)
            .filter(|until| *until > now)
        {
            return Some(Wait::Blocked(until));
        }
        if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(bucket) {
            buckets.retain(|_, window| !window.idle(now));
        }
        let window = buckets.entry(bucket.clone()).or_default();
        while window
            .calls
            .front()
            .is_some_and(|call| now.duration_since(*call) >= WINDOW)
        {
            window.calls.pop_front();
        }
        if window.calls.len() < tier.per_minute() {
            window.calls.push_back(now);
            return None;
        }
        window
            .calls
            .front()
            .map(|oldest| Wait::Quota(*oldest + WINDOW))
    }

    /// Holds every call to `bucket`'s method with its token, in any channel,
    /// until `until`, after a 429.
    pub(crate) fn block(&self, bucket: &Bucket, until: Instant) {
        let mut buckets = self.lock();
        let window = buckets.entry(bucket.method_wide()).or_default();
        window.blocked_until = Some(window.blocked_until.map_or(until, |at| at.max(until)));
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Bucket, Window>> {
        self.buckets.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(method: &'static str, channel: Option<&str>) -> Bucket {
        Bucket::new(TokenKey::of(&SecretString::from("xoxb-1")), method, channel)
    }

    #[test]
    fn a_tier_allows_its_quota_per_minute_then_waits_for_the_oldest_call() {
        let limiter = Limiter::default();
        let start = Instant::now();
        let users = bucket("users.list", None);
        for i in 0..20 {
            let at = start + Duration::from_millis(i);
            assert_eq!(limiter.try_acquire(&users, Tier::Tier2, at), None);
        }
        let later = start + Duration::from_secs(30);
        assert_eq!(
            limiter.try_acquire(&users, Tier::Tier2, later),
            Some(Wait::Quota(start + WINDOW))
        );
        assert_eq!(
            limiter.try_acquire(&users, Tier::Tier2, start + WINDOW),
            None
        );
    }

    #[test]
    fn quotas_are_separate_per_token_method_and_channel() {
        let limiter = Limiter::default();
        let now = Instant::now();
        let first = bucket("chat.postMessage", Some("C1"));
        for _ in 0..Tier::PostMessage.per_minute() {
            assert_eq!(limiter.try_acquire(&first, Tier::PostMessage, now), None);
        }
        assert!(
            limiter
                .try_acquire(&first, Tier::PostMessage, now)
                .is_some()
        );
        let other_channel = bucket("chat.postMessage", Some("C2"));
        assert_eq!(
            limiter.try_acquire(&other_channel, Tier::PostMessage, now),
            None
        );
        let other_token = Bucket::new(
            TokenKey::of(&SecretString::from("xoxb-2")),
            "chat.postMessage",
            Some("C1"),
        );
        assert_eq!(
            limiter.try_acquire(&other_token, Tier::PostMessage, now),
            None
        );
        let other_method = bucket("chat.update", Some("C1"));
        assert_eq!(limiter.try_acquire(&other_method, Tier::Tier3, now), None);
    }

    #[test]
    fn a_block_holds_the_bucket_until_it_ends() {
        let limiter = Limiter::default();
        let now = Instant::now();
        let replies = bucket("conversations.replies", None);
        let until = now + Duration::from_secs(30);
        limiter.block(&replies, until);
        limiter.block(&replies, now + Duration::from_secs(5));
        assert_eq!(
            limiter.try_acquire(&replies, Tier::Tier3, now),
            Some(Wait::Blocked(until))
        );
        assert_eq!(limiter.try_acquire(&replies, Tier::Tier3, until), None);
    }

    #[test]
    fn a_block_in_one_channel_holds_the_method_in_every_channel() {
        let limiter = Limiter::default();
        let now = Instant::now();
        let until = now + Duration::from_secs(30);
        limiter.block(&bucket("chat.postMessage", Some("C1")), until);
        let other_channel = bucket("chat.postMessage", Some("C2"));
        assert_eq!(
            limiter.try_acquire(&other_channel, Tier::PostMessage, now),
            Some(Wait::Blocked(until))
        );
        let other_method = bucket("chat.update", Some("C2"));
        assert_eq!(limiter.try_acquire(&other_method, Tier::Tier3, now), None);
        let other_token = Bucket::new(
            TokenKey::of(&SecretString::from("xoxb-2")),
            "chat.postMessage",
            Some("C2"),
        );
        assert_eq!(
            limiter.try_acquire(&other_token, Tier::PostMessage, now),
            None
        );
        assert_eq!(
            limiter.try_acquire(&other_channel, Tier::PostMessage, until),
            None
        );
    }

    #[test]
    fn a_block_survives_the_idle_sweep_when_the_map_is_full() {
        let limiter = Limiter::default();
        let start = Instant::now();
        for i in 0..MAX_BUCKETS - 1 {
            let channel = i.to_string();
            let key = bucket("chat.postMessage", Some(&channel));
            assert_eq!(limiter.try_acquire(&key, Tier::PostMessage, start), None);
        }
        let until = start + 2 * WINDOW;
        limiter.block(&bucket("chat.postMessage", Some("0")), until);
        let later = start + WINDOW;
        let fresh = bucket("users.list", None);
        assert_eq!(limiter.try_acquire(&fresh, Tier::Tier2, later), None);
        assert_eq!(limiter.lock().len(), 2);
        assert_eq!(
            limiter.try_acquire(
                &bucket("chat.postMessage", Some("new")),
                Tier::PostMessage,
                later
            ),
            Some(Wait::Blocked(until))
        );
    }

    #[test]
    fn idle_buckets_are_dropped_when_the_map_is_full() {
        let limiter = Limiter::default();
        let start = Instant::now();
        for i in 0..MAX_BUCKETS {
            let channel = i.to_string();
            let key = bucket("chat.postMessage", Some(&channel));
            assert_eq!(limiter.try_acquire(&key, Tier::PostMessage, start), None);
        }
        let busy = bucket("chat.postMessage", Some("0"));
        let later = start + WINDOW;
        let fresh = bucket("chat.postMessage", Some("new"));
        assert_eq!(limiter.try_acquire(&fresh, Tier::PostMessage, later), None);
        assert_eq!(limiter.lock().len(), 1);
        assert!(!limiter.lock().contains_key(&busy));
    }

    #[test]
    fn token_keys_differ_by_token_and_hold_no_token_text() {
        let a = TokenKey::of(&SecretString::from("xoxb-secret-a"));
        let b = TokenKey::of(&SecretString::from("xoxb-secret-b"));
        assert_ne!(a, b);
        assert!(!format!("{a:?}").contains("secret"));
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_sleeps_until_quota_frees_up() {
        let limiter = Limiter::default();
        let key = bucket("users.list", None);
        let start = Instant::now();
        let max = Duration::from_secs(10);
        for _ in 0..Tier::Tier2.per_minute() {
            limiter.acquire(&key, Tier::Tier2, max).await.unwrap();
        }
        assert_eq!(Instant::now(), start);
        limiter.acquire(&key, Tier::Tier2, max).await.unwrap();
        assert_eq!(Instant::now(), start + WINDOW);
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_waits_out_a_short_block_and_refuses_a_long_one() {
        let limiter = Limiter::default();
        let key = bucket("users.info", None);
        let start = Instant::now();
        let max = Duration::from_secs(10);
        limiter.block(&key, start + Duration::from_secs(5));
        limiter.acquire(&key, Tier::Tier4, max).await.unwrap();
        assert_eq!(Instant::now(), start + Duration::from_secs(5));
        limiter.block(&key, Instant::now() + Duration::from_secs(30));
        assert_eq!(
            limiter.acquire(&key, Tier::Tier4, max).await,
            Err(Duration::from_secs(30))
        );
    }
}

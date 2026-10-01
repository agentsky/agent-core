//! The relink notice: a DM telling a member their Claude link broke.
//!
//! `auth` marks a link broken when the token endpoint says its refresh token
//! is dead, and sends the member on [`auth::Auth::take_relink_notices`] when
//! its mark set `claude_links.broken_at`. That channel only wakes the
//! [`RelinkNotifier`] up. What is owed lives in the store: every broken link
//! whose member hasn't been told. The notifier claims each one with a
//! conditional update before sending, so across instances and restarts one
//! instance at a time sends it, and marks it sent afterwards. The claim is a
//! lease of [`RELINK_LEASE`]: if the instance dies before marking the notice
//! sent, it is pending again when the lease ends, so a notice is sent at
//! least once, and twice only after such a crash. A notice nobody could send
//! is tried again after a backoff that starts at [`RELINK_BACKOFF_INITIAL`]
//! and doubles up to [`RELINK_BACKOFF_MAX`], at most
//! [`RELINK_MAX_ATTEMPTS`] times. A pass runs at startup, on every wake-up
//! and every [`RELINK_SWEEP_INTERVAL`].

use std::time::Duration;

use core_types::{MemberId, MemberKey};
use store::{PendingRelinkNotice, Store, StoreError};
use time::OffsetDateTime;
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;

use super::{Replies, login_command};

/// How often pending notices are looked for without a wake-up.
pub const RELINK_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// How long a claim keeps other instances from sending the notice while
/// its sender posts it.
pub const RELINK_LEASE: Duration = Duration::from_secs(10 * 60);

/// How long after the first failed attempt the notice is tried again.
pub const RELINK_BACKOFF_INITIAL: Duration = Duration::from_secs(60);

/// The longest wait between attempts.
pub const RELINK_BACKOFF_MAX: Duration = Duration::from_secs(6 * 60 * 60);

/// How many attempts a notice gets before the notifier gives up on it:
/// about three days of retries.
pub const RELINK_MAX_ATTEMPTS: u32 = 20;

/// How long to wait after failed attempt number `attempt` (from 1).
fn backoff(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(31);
    RELINK_BACKOFF_INITIAL
        .saturating_mul(1 << doublings)
        .min(RELINK_BACKOFF_MAX)
}

/// The notice, for a member on `identity`'s surface.
pub fn relink_notice(identity: &MemberKey) -> String {
    format!(
        "Your Claude account link stopped working: Anthropic refused to renew it. Send {} to \
         link your account again.",
        login_command(identity.surface)
    )
}

/// Sends relink notices through the manager bots.
#[derive(Debug, Clone)]
pub struct RelinkNotifier {
    store: Store,
    replies: Replies,
}

impl RelinkNotifier {
    /// A notifier over `store`, sending through `replies`.
    pub fn new(store: Store, replies: Replies) -> Self {
        Self { store, replies }
    }

    /// Sends every pending notice a manager bot can deliver, and returns how
    /// many members were told.
    ///
    /// A member is sent the notice on each identity a manager bot can reach.
    /// If none can reach them, their notice stays pending without an
    /// attempt; if every send fails, it is tried again after a backoff.
    ///
    /// # Errors
    ///
    /// The first [`StoreError`] the pass met. A notice the store fails on
    /// is logged and left for a later pass, and the others still go out.
    pub async fn send_pending(&self) -> Result<usize, StoreError> {
        self.send_pending_at(OffsetDateTime::now_utc).await
    }

    /// [`send_pending`](Self::send_pending), reading the time from `now`.
    pub(super) async fn send_pending_at(
        &self,
        now: impl Fn() -> OffsetDateTime,
    ) -> Result<usize, StoreError> {
        let pending = self
            .store
            .pending_relink_notices(now(), RELINK_MAX_ATTEMPTS)
            .await?;
        let mut told = 0;
        let mut first_err = None;
        for notice in pending {
            let member = notice.member;
            match self.send_one(&notice, &now).await {
                Ok(true) => told += 1,
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(%member, error = %err, "a relink notice failed in the store; left it for a later pass");
                    first_err.get_or_insert(err);
                }
            }
        }
        match first_err {
            Some(err) => Err(err),
            None => Ok(told),
        }
    }

    /// Claims and sends `notice`, or defers it if no send worked; true if
    /// the member was told.
    async fn send_one(
        &self,
        notice: &PendingRelinkNotice,
        now: &impl Fn() -> OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let member = notice.member;
        let reachable: Vec<MemberKey> = self
            .store
            .member_identities(member)
            .await?
            .into_iter()
            .filter(|identity| self.replies.can_dm(identity))
            .collect();
        if reachable.is_empty() {
            tracing::debug!(%member, "no manager bot reaches this member; the relink notice waits");
            return Ok(false);
        }
        let claimed_at = now();
        let Some(attempt) = self
            .store
            .claim_relink_notice(
                member,
                notice.generation,
                claimed_at,
                claimed_at + RELINK_LEASE,
                RELINK_MAX_ATTEMPTS,
            )
            .await?
        else {
            return Ok(false);
        };
        if self.send(member, &reachable).await {
            self.store
                .mark_relink_notice_sent(member, notice.generation, now())
                .await?;
            return Ok(true);
        }
        self.store
            .defer_relink_notice(member, notice.generation, now() + backoff(attempt))
            .await?;
        if attempt >= RELINK_MAX_ATTEMPTS {
            tracing::warn!(%member, attempts = attempt, "giving up on the relink notice");
        }
        Ok(false)
    }

    /// Sends the notice to each of `identities`; true if any send worked.
    async fn send(&self, member: MemberId, identities: &[MemberKey]) -> bool {
        let mut sent = false;
        for identity in identities {
            match self.replies.dm(identity, &relink_notice(identity)).await {
                Ok(()) => sent = true,
                Err(err) => {
                    tracing::warn!(%member, %identity, error = %err, "couldn't send the relink notice");
                }
            }
        }
        if sent {
            tracing::info!(%member, "sent the relink notice");
        }
        sent
    }

    /// Runs a pass now, then on each member `wake` yields and every `every`,
    /// until `stopping` becomes true or its sender is dropped. A pass in
    /// progress finishes first.
    pub async fn run(
        self,
        mut wake: Option<mpsc::UnboundedReceiver<MemberId>>,
        every: Duration,
        mut stopping: watch::Receiver<bool>,
    ) {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = stopping.wait_for(|stop| *stop) => break,
                woke = next_wake(&mut wake) => {
                    if woke.is_none() {
                        wake = None;
                    }
                }
                _ = ticks.tick() => {}
            }
            if let Some(rx) = &mut wake {
                while rx.try_recv().is_ok() {}
            }
            match self.send_pending().await {
                Ok(0) => {}
                Ok(told) => tracing::debug!(told, "sent relink notices"),
                Err(err) => tracing::warn!(error = %err, "sending relink notices failed"),
            }
        }
    }
}

/// The next member on `wake`, or never without a channel.
async fn next_wake(wake: &mut Option<mpsc::UnboundedReceiver<MemberId>>) -> Option<MemberId> {
    match wake {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

//! The relink notice: a DM telling a member their Claude link broke.
//!
//! `auth` marks a link broken when the token endpoint says its refresh token
//! is dead, and sends the member on [`auth::Auth::take_relink_notices`] when
//! its mark set `claude_links.broken_at`. That channel only wakes the
//! [`RelinkNotifier`] up. What is owed lives in the store: every broken link
//! whose notice hasn't been claimed. The notifier claims each one with a
//! conditional update before sending, so across instances and restarts a
//! break is announced once, and a notice nobody could send is released and
//! tried again at the next pass. A pass runs at startup, on every wake-up and
//! every [`RELINK_SWEEP_INTERVAL`].

use std::time::Duration;

use core_types::{MemberId, MemberKey};
use store::{Store, StoreError};
use time::OffsetDateTime;
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;

use super::{Replies, login_command};

/// How often pending notices are looked for without a wake-up.
pub const RELINK_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

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
    /// If none can reach them, their notice stays pending; if every send
    /// fails, it is released and tried again at the next pass.
    ///
    /// # Errors
    ///
    /// A [`StoreError`] if the store fails; notices already sent stay sent.
    pub async fn send_pending(&self) -> Result<usize, StoreError> {
        let mut told = 0;
        for notice in self.store.pending_relink_notices().await? {
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
                continue;
            }
            let claimed = self
                .store
                .claim_relink_notice(member, notice.generation, OffsetDateTime::now_utc())
                .await?;
            if !claimed {
                continue;
            }
            if self.send(member, &reachable).await {
                told += 1;
            } else {
                self.store
                    .release_relink_notice(member, notice.generation)
                    .await?;
            }
        }
        Ok(told)
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

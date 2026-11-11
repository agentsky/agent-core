//! [`CloudNotifier`]: the pass that ends a hand-off left `sending`, tells
//! its member, and forgets old hand-offs.
//!
//! A hand-off is written `sending` before its request, and its outcome is
//! recorded when the request returns. If agentd stops in between, or the
//! record fails, the row stays `sending`. Every
//! [`CLOUD_SWEEP_INTERVAL`] a pass marks the `sending` rows older than
//! twice `[cloud] timeout_secs` `unknown`, and owes each member a notice in
//! the manager bot's DM, sent as the relink notice is: the notice is
//! claimed with a lease before it is sent, a failed send backs off, and the
//! store gives a notice up a day after its row became `unknown`. The pass
//! then deletes the hand-offs asked more than `[cloud] retention_days`
//! ago. It runs whether or not `[cloud]` is configured, with its defaults
//! when it isn't, so a notice owed from before the section was removed
//! still goes out.

use std::time::Duration;

use core_types::{MemberKey, SurfaceKind};
use store::{CloudHandoff, Store, StoreError};
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use super::super::Replies;
use super::super::sessions::when;
use crate::config::CloudConfig;

/// How often the pass runs.
pub const CLOUD_SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The notice for `handoff`, sent to `identity`: its routine and when it
/// was asked, that it may have started, and to check claude.ai/code before
/// running it again.
pub fn stale_notice(handoff: &CloudHandoff, identity: &MemberKey) -> String {
    let list = match identity.surface {
        SurfaceKind::Slack => "`/agent cloud list`",
        SurfaceKind::RocketChat => "`cloud list`",
    };
    format!(
        "Your cloud hand-off to routine `{}`, asked at {}, got no answer I could record: I \
         stopped while it was being sent, or couldn't record the answer. A cloud session may \
         have started, so check claude.ai/code before running it again. {list} shows what I \
         know.",
        handoff.routine_label,
        when(handoff.created_at),
    )
}

/// Ends hand-offs left `sending`, sends the notices they owe through the
/// manager bots, and purges old hand-offs.
#[derive(Debug, Clone)]
pub struct CloudNotifier {
    store: Store,
    replies: Replies,
    stale_after: Duration,
    retention: Duration,
}

/// What one pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CloudPass {
    /// Hand-offs it marked `unknown`.
    pub marked: usize,
    /// Notices it sent.
    pub told: usize,
    /// Hand-offs it deleted.
    pub purged: u64,
}

impl CloudNotifier {
    /// A notifier over `store`, sending through `replies`, with the timeout
    /// and retention of `config`, `[cloud]`, or their defaults without it.
    pub fn new(store: Store, replies: Replies, config: Option<&CloudConfig>) -> Self {
        let config = config.cloned().unwrap_or_default();
        Self {
            store,
            replies,
            stale_after: config.timeout().saturating_mul(2),
            retention: config.retention(),
        }
    }

    /// Runs one pass now.
    ///
    /// # Errors
    ///
    /// The first [`StoreError`] the pass met. Every step runs whatever
    /// another met, and so does every notice: a hand-off the store fails
    /// on is logged and left for a later pass, and the purge still runs.
    pub async fn pass(&self) -> Result<CloudPass, StoreError> {
        self.pass_at(OffsetDateTime::now_utc).await
    }

    /// [`pass`](Self::pass), reading the time from `now`.
    pub(crate) async fn pass_at(
        &self,
        now: impl Fn() -> OffsetDateTime,
    ) -> Result<CloudPass, StoreError> {
        let mut first_err = None;
        let at = now();
        let stale = match self
            .store
            .stale_cloud_handoffs(earlier(at, self.stale_after), at)
            .await
        {
            Ok(stale) => stale,
            Err(err) => {
                tracing::warn!(error = %err, "couldn't mark unanswered cloud hand-offs");
                first_err.get_or_insert(err);
                Vec::new()
            }
        };
        for handoff in &stale {
            tracing::warn!(
                handoff = %handoff.id,
                member = %handoff.member,
                routine = handoff.routine_id.as_str(),
                "a cloud hand-off got no recorded answer; marked it unknown"
            );
        }
        let due = match self.store.due_cloud_handoff_notices(now()).await {
            Ok(due) => due,
            Err(err) => {
                tracing::warn!(error = %err, "couldn't read the cloud hand-off notices owed");
                first_err.get_or_insert(err);
                Vec::new()
            }
        };
        let mut told = 0;
        for handoff in &due {
            match self.notify(handoff, &now).await {
                Ok(true) => told += 1,
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(handoff = %handoff.id, member = %handoff.member, error = %err, "a cloud hand-off notice failed in the store; left it for a later pass");
                    first_err.get_or_insert(err);
                }
            }
        }
        let at = now();
        let purged = match self
            .store
            .purge_cloud_handoffs(earlier(at, self.retention), at)
            .await
        {
            Ok(purged) => purged,
            Err(err) => {
                tracing::warn!(error = %err, "couldn't delete old cloud hand-offs");
                first_err.get_or_insert(err);
                0
            }
        };
        if purged > 0 {
            tracing::info!(purged, "deleted old cloud hand-offs");
        }
        match first_err {
            Some(err) => Err(err),
            None => Ok(CloudPass {
                marked: stale.len(),
                told,
                purged,
            }),
        }
    }

    /// Claims `handoff`'s notice and sends it to each of the member's
    /// identities a manager bot reaches; true if any send worked. A member
    /// no manager bot reaches is left owed without a claim; a notice no
    /// send delivered is deferred.
    async fn notify(
        &self,
        handoff: &CloudHandoff,
        now: &impl Fn() -> OffsetDateTime,
    ) -> Result<bool, StoreError> {
        let member = handoff.member;
        let reachable: Vec<MemberKey> = self
            .store
            .member_identities(member)
            .await?
            .into_iter()
            .filter(|identity| self.replies.can_dm(identity))
            .collect();
        if reachable.is_empty() {
            tracing::debug!(%member, handoff = %handoff.id, "no manager bot reaches this member; the cloud hand-off notice waits");
            return Ok(false);
        }
        let Some(claim) = self
            .store
            .claim_cloud_handoff_notice(handoff.id, now())
            .await?
        else {
            return Ok(false);
        };
        let mut sent = false;
        for identity in &reachable {
            match self
                .replies
                .dm(identity, &stale_notice(handoff, identity))
                .await
            {
                Ok(()) => sent = true,
                Err(err) => {
                    tracing::warn!(%member, %identity, handoff = %handoff.id, error = %err, "couldn't send a cloud hand-off notice");
                }
            }
        }
        if sent {
            self.store
                .mark_cloud_handoff_notified(handoff.id, claim, now())
                .await?;
            tracing::info!(%member, handoff = %handoff.id, "sent a cloud hand-off notice");
        } else {
            self.store
                .defer_cloud_handoff_notice(handoff.id, claim, now())
                .await?;
        }
        Ok(sent)
    }

    /// Runs a pass now and then every `every`, until `stopping` becomes
    /// true or its sender is dropped. A pass in progress finishes first.
    pub async fn run(self, every: Duration, mut stopping: watch::Receiver<bool>) {
        let mut ticks = tokio::time::interval(every);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = stopping.wait_for(|stop| *stop) => break,
                _ = ticks.tick() => {}
            }
            match self.pass().await {
                Ok(pass) if pass != CloudPass::default() => {
                    tracing::debug!(?pass, "cloud hand-off pass");
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::debug!(error = %err, "the cloud hand-off pass met a store failure")
                }
            }
        }
    }
}

fn earlier(at: OffsetDateTime, by: Duration) -> OffsetDateTime {
    at.saturating_sub(time::Duration::try_from(by).unwrap_or(time::Duration::MAX))
}

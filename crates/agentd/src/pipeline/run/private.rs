//! The work a decided consent owes, which the pipeline does: an approved
//! private task, run in a fresh session on the owner's private volume, or
//! the declined or expired outcome, posted in the thread that asked.

use store::{AgentState, Consent, ConsentState};

use super::*;
use crate::consents::{self, Consents, card};

/// How long a claim on a consent's work lasts. A running task renews it
/// every third of that.
pub const WORK_LEASE: Duration = Duration::from_secs(10 * 60);
/// How long after a task failed before reaching the model it is tried
/// again.
pub const WORK_RETRY: Duration = Duration::from_secs(60);
/// How many times a consent's work is tried before the thread is told it
/// couldn't be done.
pub const WORK_MAX_ATTEMPTS: u32 = 3;

impl Pipeline {
    /// Does the work each decided consent in `consents` owes, oldest
    /// decision first, unless the pipeline is closed:
    ///
    /// - An approved task, of an agent that is active, runs in a task of
    ///   the pipeline's own, holding a place among the owner's agents'
    ///   messages ([`PipelineSettings::max_pending_per_owner`]) and the
    ///   pipeline's; while none is free it waits for the next pass. The
    ///   task gets a new session
    ///   ([`SessionManager::create_private`](runner::SessionManager::create_private))
    ///   on the owner's private volume, never the owner's DM session, with
    ///   the consent's files copied into its `work/`. Its turn message is
    ///   the task text and the files' names alone, with no thread history;
    ///   it runs on the owner's credential, on the owner's side only when
    ///   the owner asked for it, as a
    ///   [`TurnKind::PrivateTask`](core_types::TurnKind::PrivateTask), so
    ///   agentctl allows only `attach`. Its container is stopped right
    ///   after, and the turn is billed to the owner. Its reply, headed
    ///   with the consent's id, and its attached files are posted in the
    ///   thread that asked as the agent's bot, each chunk recorded in
    ///   `message_refs` with the consent's requester and hop and the
    ///   private session, where that thread's session finds it on its next
    ///   turn.
    /// - A declined or expired consent's outcome is posted in the thread
    ///   the same way, under a session id that is the consent's own, since
    ///   no session ran.
    ///
    /// The work is leased ([`WORK_LEASE`]), so an instance that dies
    /// leaves it to the next pass anywhere. A task that fails before its
    /// turn reached the model is tried again after [`WORK_RETRY`]; after
    /// [`WORK_MAX_ATTEMPTS`] tries the thread is told it couldn't be run.
    /// The consent's files are deleted once its work is done.
    ///
    /// # Errors
    ///
    /// If the store fails.
    pub async fn settle_consents(&self, consents: &Consents) -> Result<(), StoreError> {
        let store = &self.inner.store;
        for consent in store.consent_work_owed(Consents::now()).await? {
            if self.is_closed() {
                return Ok(());
            }
            let agent = store.agent(consent.agent).await?;
            match agent {
                Some(agent)
                    if consent.state == ConsentState::Approved
                        && agent.state == AgentState::Active =>
                {
                    self.start_private_task(consents, consent, agent).await?;
                }
                agent => self.post_outcome(consents, &consent, agent).await?,
            }
        }
        Ok(())
    }

    /// Posts the outcome of `consent`, whose agent is `agent` if it isn't
    /// deleted: declined, expired, or, for an approved task whose agent
    /// was paused, that it couldn't be run.
    async fn post_outcome(
        &self,
        consents: &Consents,
        consent: &Consent,
        agent: Option<Agent>,
    ) -> Result<(), StoreError> {
        let store = &self.inner.store;
        let now = Consents::now();
        let Some(attempt) = store
            .claim_consent_work(consent.id, now, now + WORK_LEASE)
            .await?
        else {
            return Ok(());
        };
        let text = agent
            .filter(|agent| agent.state != AgentState::Deleted)
            .map(|agent| match consent.state {
                ConsentState::Declined => card::declined_text(consent.id, &agent.name),
                ConsentState::Expired => card::expired_text(
                    consent.id,
                    &agent.name,
                    consent.expires_at - consent.created_at,
                ),
                ConsentState::Approved | ConsentState::Pending => card::failed_text(consent.id),
            });
        let posted = match text {
            Some(text) => self.tell_thread(consent, &text).await,
            None => true,
        };
        if posted || attempt >= WORK_MAX_ATTEMPTS {
            self.finish_consent(consents, consent.id, attempt).await?;
        }
        Ok(())
    }

    /// Takes the places `consent`'s task holds and claims its work, then
    /// runs it in a task of the pipeline's own. Nothing when a place isn't
    /// free, the work is claimed elsewhere, or the pipeline closed.
    async fn start_private_task(
        &self,
        consents: &Consents,
        consent: Consent,
        agent: Agent,
    ) -> Result<(), StoreError> {
        let Some(places) = self.places(agent.owner) else {
            tracing::debug!(consent = %consent.id, "no place for a private task yet; it waits");
            return Ok(());
        };
        let store = &self.inner.store;
        let now = Consents::now();
        let Some(attempt) = store
            .claim_consent_work(consent.id, now, now + WORK_LEASE)
            .await?
        else {
            return Ok(());
        };
        if attempt > WORK_MAX_ATTEMPTS {
            tracing::warn!(consent = %consent.id, "a private task was claimed too often; giving up");
            self.tell_thread(&consent, &card::failed_text(consent.id))
                .await;
            return self.finish_consent(consents, consent.id, attempt).await;
        }
        let mut tasks = lock(&self.inner.tasks);
        if self.is_closed() {
            return Ok(());
        }
        reap(&mut tasks);
        tasks.spawn(
            self.clone()
                .private_task(consents.clone(), consent, agent, attempt, places),
        );
        Ok(())
    }

    /// Runs claim `attempt` of `consent`'s task for `agent`, renewing the
    /// claim while it runs, and finishes the consent's work when it is
    /// done.
    async fn private_task(
        self,
        consents: Consents,
        consent: Consent,
        agent: Agent,
        attempt: u32,
        _places: (OwnedSemaphorePermit, OwnedSemaphorePermit),
    ) {
        let id = consent.id;
        let ran = tokio::select! {
            ran = self.run_private_task(&consents, &consent, &agent, attempt) => ran,
            () = self.renew_work(id, attempt) => return,
        };
        let store = &self.inner.store;
        let done = match ran {
            Ok(true) => true,
            Ok(false) => return,
            Err(err) if attempt >= WORK_MAX_ATTEMPTS => {
                tracing::warn!(consent = %id, attempt, error = %err, "a private task failed before it reached the model; giving up");
                self.tell_thread(&consent, &card::failed_text(id)).await;
                true
            }
            Err(err) => {
                tracing::warn!(consent = %id, attempt, error = %err, "a private task failed before it reached the model; it runs again later");
                let retry = Consents::now() + WORK_RETRY;
                if let Err(err) = store.renew_consent_work(id, attempt, retry).await {
                    tracing::warn!(consent = %id, error = %err, "couldn't set when a private task runs again");
                }
                false
            }
        };
        if done && let Err(err) = self.finish_consent(&consents, id, attempt).await {
            tracing::warn!(consent = %id, error = %err, "couldn't record that a private task is done");
        }
    }

    /// Renews claim `attempt` on consent `id`'s work every third of
    /// [`WORK_LEASE`]. It ends only once another claim took the work over.
    async fn renew_work(&self, id: ConsentId, attempt: u32) {
        loop {
            tokio::time::sleep(WORK_LEASE / 3).await;
            match self
                .inner
                .store
                .renew_consent_work(id, attempt, Consents::now() + WORK_LEASE)
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(consent = %id, attempt, "another claim took a private task over");
                    return;
                }
                Err(err) => {
                    tracing::warn!(consent = %id, error = %err, "couldn't renew the claim on a private task");
                }
            }
        }
    }

    /// Runs `consent`'s task, as [`settle_consents`](Self::settle_consents)
    /// describes. True once it is done with, delivered or not deliverable,
    /// and false when another claim took it over.
    async fn run_private_task(
        &self,
        consents: &Consents,
        consent: &Consent,
        agent: &Agent,
        attempt: u32,
    ) -> Result<bool, PipelineError> {
        let conv = &consent.thread.conv;
        let Some(surface) = self.inner.surfaces.surface(agent.id, conv).await else {
            tracing::warn!(consent = %consent.id, "the agent has no surface in the thread that asked; the private task can't report");
            return Ok(true);
        };
        if !surface.can_post(conv).await? {
            tracing::info!(consent = %consent.id, "the agent's bot left the thread that asked; not running the private task");
            return Ok(true);
        }
        let credential = CredentialRef::Member(agent.owner);
        let Some(prepared) = self.prepare(agent.id, conv, credential).await? else {
            return Ok(true);
        };
        let store = &self.inner.store;
        let sessions = self.inner.turns.sessions();
        let session = sessions
            .create_private(agent.id, consent.id, &consent.thread)
            .await?;
        if !store
            .set_consent_session(consent.id, attempt, session.id)
            .await?
        {
            return Ok(false);
        }
        let work = sessions.work_dir(&session).await?;
        consents
            .hand_over(consent, work)
            .await
            .map_err(PipelineError::HandOver)?;
        let side = if consent.requester.member == Some(agent.owner) {
            Side::Owner
        } else {
            Side::Public
        };
        let request = TurnRequest {
            turn: TurnId::new_v4(),
            message: task_message(&consent.task, &consents::attachments(consent)),
            credential,
            model: prepared.model,
            requester: consent.requester.clone(),
            hop: consent.hop,
            side,
            kind: TurnKind::PrivateTask(consent.id),
            trigger: None,
        };
        let turn = request.turn;
        tracing::info!(consent = %consent.id, session = %session.id, ?side, "running a private task");
        let report = sessions.run_turn(session.id, request).await;
        sessions.stop(session.id).await;
        let report = report?;
        self.bill(
            agent.owner,
            agent.id,
            &consent.thread,
            true,
            &report.outcome,
        )
        .await;
        Delivery {
            store,
            surface: surface.as_ref(),
            session: session.id,
            agent: agent.id,
            requester: &consent.requester,
            hop: consent.hop,
            credential,
            target: ReplyTarget::from(consent.thread.clone()),
            answering: Answering::PrivateTask(consent.id),
        }
        .report(turn, report)
        .await;
        Ok(true)
    }

    /// Posts Markdown `text` about `consent` in the thread that asked, as
    /// its agent's bot, recorded in `message_refs` like a reply under a
    /// session id that is the consent's own. True once posted, or when
    /// the agent can't post there any more.
    async fn tell_thread(&self, consent: &Consent, text: &str) -> bool {
        let conv = &consent.thread.conv;
        let Some(surface) = self.inner.surfaces.surface(consent.agent, conv).await else {
            return true;
        };
        match surface.can_post(conv).await {
            Ok(true) => {}
            Ok(false) => return true,
            Err(err) => {
                tracing::warn!(consent = %consent.id, error = %err, "couldn't check that the agent can post a private task's outcome");
                return false;
            }
        }
        let delivery = Delivery {
            store: &self.inner.store,
            surface: surface.as_ref(),
            session: SessionId::from_uuid(*consent.id.as_uuid()),
            agent: consent.agent,
            requester: &consent.requester,
            hop: consent.hop,
            credential: CredentialRef::Community,
            target: ReplyTarget::from(consent.thread.clone()),
            answering: Answering::PrivateTask(consent.id),
        };
        let posted = delivery.post(None, text).await;
        if posted {
            tracing::info!(consent = %consent.id, "posted a private task's outcome");
        }
        posted
    }

    /// Records that claim `attempt` did consent `id`'s work, and deletes
    /// its files.
    async fn finish_consent(
        &self,
        consents: &Consents,
        id: ConsentId,
        attempt: u32,
    ) -> Result<(), StoreError> {
        if self
            .inner
            .store
            .finish_consent(id, attempt, Consents::now())
            .await?
        {
            consents.forget_files(id).await;
        }
        Ok(())
    }
}

/// A private task's turn message: the task text, and the names of the
/// files handed to it, which are in its working directory.
fn task_message(task: &str, files: &[String]) -> String {
    if files.is_empty() {
        return task.to_owned();
    }
    format!(
        "{task}\n\nFiles handed to this task, in the working directory: {}",
        files.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_message_is_the_task_and_its_files_names() {
        assert_eq!(task_message("do it", &[]), "do it");
        assert_eq!(
            task_message("do it", &["a.txt".to_owned(), "b.csv".to_owned()]),
            "do it\n\nFiles handed to this task, in the working directory: a.txt, b.csv"
        );
    }
}

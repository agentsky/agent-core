//! The work a decided consent owes, which the pipeline does: an approved
//! private task, run in a fresh session on the owner's private volume, or
//! the outcome of one that didn't run, posted in the thread that asked.

use store::{AgentState, Approval, Consent, ConsentState};
use tokio::task::JoinHandle;

use super::*;
use crate::consents::{self, Consents, card};

/// How long a claim on a consent's work lasts. A running task renews it
/// every third of that.
pub const WORK_LEASE: Duration = Duration::from_secs(10 * 60);
/// How long after a try of a consent's work failed it is tried again.
pub const WORK_RETRY: Duration = Duration::from_secs(60);
/// How many tries of a consent's work may fail before the thread is told
/// it couldn't be done. A try cut short by a shutdown isn't one of them.
pub const WORK_MAX_ATTEMPTS: u32 = 3;
/// How long a private task cut short has its session's containers killed
/// for before its turn is left to end by itself.
const KILL_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a private task cut short has its session's containers killed
/// until its turn ends.
const KILL_EVERY: Duration = Duration::from_millis(200);
/// How long past [`KILL_TIMEOUT`] a shutdown waits for a killed turn's
/// session to be stopped and its claim released.
const KILL_GRACE: Duration = Duration::from_secs(5);

/// A claim this instance holds on a private task's work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Claim {
    /// The claim's attempt.
    attempt: u32,
    /// Whether its turn was started, after which a shutdown doesn't
    /// release it.
    turned: bool,
}

/// What became of a claim on a private task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ran {
    /// The work is done with: delivered, or not deliverable.
    Done,
    /// Another claim took it over.
    TakenOver,
}

impl Pipeline {
    /// Does the work each decided consent in `consents` owes, oldest
    /// decision first, unless the pipeline is closed:
    ///
    /// - An approved task, of an agent that is active, runs in a task of
    ///   the pipeline's own, holding a place among the owner's agents'
    ///   messages ([`PipelineSettings::max_pending_per_owner`]) and the
    ///   pipeline's; while none is free it waits for the next pass. The
    ///   thread's caps ([`Limits::thread_budget`]) are checked first, as
    ///   for any turn outside a DM: a capped thread is told so, and the
    ///   task doesn't run. The task gets a new session
    ///   ([`SessionManager::create_private`](runner::SessionManager::create_private))
    ///   on the owner's private volume, never the owner's DM session, with
    ///   the consent's files copied into its `work/`. Its turn message is
    ///   the task text and the files' names alone, with no thread history;
    ///   it runs on the owner's credential, as a
    ///   [`TurnKind::PrivateTask`](core_types::TurnKind::PrivateTask), so
    ///   agentctl allows only `attach`, and on the owner's side only when
    ///   the owner asked for it in their own DM with the agent, or approved
    ///   on its card a task their own identity asked for. The turn runs in
    ///   a task of its own that bills it to the owner when it ends; its
    ///   container is stopped then, or killed at once when the task is cut
    ///   short or taken over, which ends the turn as a crash, billed as
    ///   one. Its reply, headed with the consent's id, and its
    ///   attached files are posted in the thread that asked as the agent's
    ///   bot, each chunk recorded in `message_refs` with the consent's
    ///   requester and hop, the private session and the consent, where
    ///   that thread's session finds it on its next turn. A mention in it
    ///   starts no agent's turn.
    /// - An approved task of a paused agent waits for the agent to be
    ///   resumed until the consent's expiry, and then the thread is told it
    ///   didn't run.
    /// - A declined or expired consent's outcome is posted in the thread
    ///   the same way, under a session id that is the consent's own, since
    ///   no session ran. Nothing is posted for a deleted agent.
    ///
    /// The work is leased ([`WORK_LEASE`]), so an instance that dies
    /// leaves it to the next pass anywhere, and a shutdown releases what it
    /// cut short once no turn of it can still be running. A task is run
    /// again only if no turn of it reached the model: a claim that finds an
    /// earlier claim's session had its turn sent to the CLI tells the
    /// thread the task was interrupted instead. A claim that finds anything
    /// already posted for the consent just finishes. A try that fails is tried again after [`WORK_RETRY`]; after
    /// [`WORK_MAX_ATTEMPTS`] failures the thread is told it couldn't be
    /// done. The consent's files, and its private sessions' directories,
    /// are deleted once its work is done, however it ends.
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
            let agent = store
                .agent(consent.agent)
                .await?
                .filter(|agent| agent.state != AgentState::Deleted);
            match agent {
                Some(agent)
                    if consent.state == ConsentState::Approved
                        && agent.state == AgentState::Active =>
                {
                    self.start_private_task(consents, consent, agent).await?;
                }
                Some(agent)
                    if consent.state == ConsentState::Approved
                        && Consents::now() < consent.expires_at =>
                {
                    tracing::debug!(consent = %consent.id, agent = %agent.id, "a private task waits for its paused agent");
                }
                agent => self.post_outcome(consents, &consent, agent).await?,
            }
        }
        Ok(())
    }

    /// Posts the outcome of `consent`, whose agent is `agent` unless it was
    /// deleted: declined, expired, or, for an approved task, that its
    /// agent stayed paused.
    async fn post_outcome(
        &self,
        consents: &Consents,
        consent: &Consent,
        agent: Option<Agent>,
    ) -> Result<(), StoreError> {
        let store = &self.inner.store;
        let now = Consents::now();
        let Some(claimed) = store
            .claim_consent_work(consent.id, now, now + WORK_LEASE)
            .await?
        else {
            return Ok(());
        };
        let attempt = claimed.work_attempts;
        let id = consent.id;
        if store.consent_posted(id).await? {
            tracing::info!(consent = %id, "a consent's outcome was already posted; finishing its work");
            return self.finish_consent(consents, id, attempt).await;
        }
        let text = agent.map(|agent| match consent.state {
            ConsentState::Declined => card::declined_text(id, &agent.name),
            ConsentState::Expired if consent.card.is_none() => {
                card::unreachable_text(id, &agent.name)
            }
            ConsentState::Expired => {
                card::expired_text(id, &agent.name, consent.expires_at - consent.created_at)
            }
            ConsentState::Approved => card::paused_text(id, &agent.name),
            ConsentState::Pending => card::failed_text(id),
        });
        let told = match text {
            Some(text) => self.tell_thread(consent, &text).await,
            None => Ok(()),
        };
        match told {
            Ok(()) => self.finish_consent(consents, id, attempt).await,
            Err(err) => self.failed(consents, consent, attempt, &err, false).await,
        }
    }

    /// Records that claim `attempt` of `consent`'s work failed with `err`,
    /// to be tried again after [`WORK_RETRY`], or, after
    /// [`WORK_MAX_ATTEMPTS`] failures, gives up: telling the thread the
    /// task couldn't be run when `tell` says to, and finishing the work.
    async fn failed(
        &self,
        consents: &Consents,
        consent: &Consent,
        attempt: u32,
        err: &PipelineError,
        tell: bool,
    ) -> Result<(), StoreError> {
        let id = consent.id;
        let retry = Consents::now() + WORK_RETRY;
        let Some(failures) = self
            .inner
            .store
            .fail_consent_work(id, attempt, retry)
            .await?
        else {
            return Ok(());
        };
        if failures < WORK_MAX_ATTEMPTS {
            tracing::warn!(consent = %id, failures, error = %err, "a private task's work failed; it is tried again later");
            return Ok(());
        }
        tracing::warn!(consent = %id, failures, error = %err, "a private task's work failed too often; giving up");
        if tell && let Err(err) = self.tell_thread(consent, &card::failed_text(id)).await {
            tracing::warn!(consent = %id, error = %err, "couldn't tell the thread a private task couldn't be run");
        }
        self.finish_consent(consents, id, attempt).await
    }

    /// Takes the places `consent`'s task holds and claims its work, then
    /// runs it in a task of the pipeline's own. Nothing when the pipeline
    /// closed, a place isn't free, or the work is claimed elsewhere.
    async fn start_private_task(
        &self,
        consents: &Consents,
        consent: Consent,
        agent: Agent,
    ) -> Result<(), StoreError> {
        if self.is_closed() {
            return Ok(());
        }
        let Some(places) = self.places(agent.owner) else {
            tracing::debug!(consent = %consent.id, "no place for a private task yet; it waits");
            return Ok(());
        };
        let store = &self.inner.store;
        let now = Consents::now();
        let Some(claimed) = store
            .claim_consent_work(consent.id, now, now + WORK_LEASE)
            .await?
        else {
            return Ok(());
        };
        let attempt = claimed.work_attempts;
        let mut tasks = lock(&self.inner.tasks);
        if self.is_closed() {
            drop(tasks);
            if let Err(err) = store
                .release_consent_work(consent.id, attempt, Consents::now())
                .await
            {
                tracing::warn!(consent = %consent.id, error = %err, "couldn't release a private task the pipeline closed on");
            }
            return Ok(());
        }
        reap(&mut tasks);
        lock(&self.inner.private).insert(
            consent.id,
            Claim {
                attempt,
                turned: false,
            },
        );
        tasks.spawn(
            self.clone()
                .private_task(consents.clone(), claimed, agent, places),
        );
        Ok(())
    }

    /// Runs `consent`'s task for `agent`, as claimed, renewing the claim
    /// while it runs, and finishes the consent's work when it is done.
    async fn private_task(
        self,
        consents: Consents,
        consent: Consent,
        agent: Agent,
        _places: (OwnedSemaphorePermit, OwnedSemaphorePermit),
    ) {
        let id = consent.id;
        let attempt = consent.work_attempts;
        let ran = tokio::select! {
            ran = self.run_private_task(&consents, &consent, &agent, attempt) => ran,
            () = self.renew_work(id, attempt) => Ok(Ran::TakenOver),
        };
        let settled = match ran {
            Ok(Ran::Done) => self.finish_consent(&consents, id, attempt).await,
            Ok(Ran::TakenOver) => Ok(()),
            Err(err) => self.failed(&consents, &consent, attempt, &err, true).await,
        };
        if let Err(err) = settled {
            tracing::warn!(consent = %id, error = %err, "couldn't record how a private task went");
        }
        let mut private = lock(&self.inner.private);
        if private
            .get(&id)
            .is_some_and(|claim| claim.attempt == attempt)
        {
            private.remove(&id);
        }
    }

    /// Marks claim `attempt` on consent `id`'s work as having started its
    /// turn.
    fn mark_turned(&self, id: ConsentId, attempt: u32) {
        if let Some(claim) = lock(&self.inner.private).get_mut(&id)
            && claim.attempt == attempt
        {
            claim.turned = true;
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

    /// Releases the claims of the private tasks a shutdown cut short
    /// before their turn started, so another instance takes them up at
    /// once, none of them counted as a failure. A claim whose turn started
    /// is released by its kill once the turn ended
    /// ([`kill_turn`](Self::kill_turn)), and otherwise left to lapse with
    /// its lease ([`WORK_LEASE`]), so the task can't run again beside a
    /// turn that may still be starting.
    pub(super) async fn release_cut_tasks(&self) {
        let cut: Vec<(ConsentId, Claim)> = lock(&self.inner.private).drain().collect();
        for (id, Claim { attempt, turned }) in cut {
            if turned {
                tracing::info!(consent = %id, "a shutdown cut a private task's turn short; its claim lapses");
                continue;
            }
            match self
                .inner
                .store
                .release_consent_work(id, attempt, Consents::now())
                .await
            {
                Ok(true) => {
                    tracing::info!(consent = %id, "released a private task a shutdown cut short");
                }
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(consent = %id, error = %err, "couldn't release a private task a shutdown cut short");
                }
            }
        }
    }

    /// Runs `consent`'s task, as [`settle_consents`](Self::settle_consents)
    /// describes.
    async fn run_private_task(
        &self,
        consents: &Consents,
        consent: &Consent,
        agent: &Agent,
        attempt: u32,
    ) -> Result<Ran, PipelineError> {
        let store = &self.inner.store;
        if store.consent_posted(consent.id).await? {
            tracing::info!(consent = %consent.id, "a private task's result or outcome was already posted; finishing its work");
            return Ok(Ran::Done);
        }
        if let Some(earlier) = consent.private_session
            && let Some(session) = store.session(earlier).await?
            && (session.resumes() || session.last_turn_at.is_some())
        {
            tracing::warn!(consent = %consent.id, session = %earlier, "a private task's earlier run reached the model before it was cut short; not running it again");
            self.tell_thread(consent, &card::interrupted_text(consent.id))
                .await?;
            return Ok(Ran::Done);
        }
        let conv = &consent.thread.conv;
        let Some(surface) = self.inner.surfaces.surface(agent.id, conv).await? else {
            tracing::warn!(consent = %consent.id, "the agent has no surface in the thread that asked; the private task can't report");
            return Ok(Ran::Done);
        };
        if !surface.can_post(conv).await? {
            tracing::info!(consent = %consent.id, "the agent's bot left the thread that asked; not running the private task");
            return Ok(Ran::Done);
        }
        if let Some(reason) = self.thread_capped(consent).await? {
            tracing::info!(consent = %consent.id, %reason, "the thread that asked is capped; not running the private task");
            let refusal = refusal_text(&agent.name, reason);
            self.tell_thread(consent, &card::limited_text(consent.id, &refusal))
                .await?;
            return Ok(Ran::Done);
        }
        let credential = CredentialRef::Member(agent.owner);
        let Some(prepared) = self.prepare(agent.id, conv, credential).await? else {
            return Ok(Ran::Done);
        };
        let sessions = self.inner.turns.sessions();
        let session = sessions
            .create_private(agent.id, consent.id, &consent.thread)
            .await?;
        if !store
            .set_consent_session(consent.id, attempt, session.id)
            .await?
        {
            return Ok(Ran::TakenOver);
        }
        let dirs = sessions.work_dir(&session).await?;
        consents
            .hand_over(consent, dirs.work, dirs.owner)
            .await
            .map_err(PipelineError::HandOver)?;
        let side = side(consent, agent.owner);
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
        self.mark_turned(consent.id, attempt);
        let report = TurnTask::start(self, agent, consent, attempt, session.id, request)
            .join()
            .await;
        sessions.stop(session.id).await;
        let report = report?;
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
            hand_offs: None,
        }
        .report(turn, report)
        .await;
        Ok(Ran::Done)
    }

    /// Why the thread `consent` was asked for in takes no more turns now,
    /// by its caps, if it doesn't. A task asked for in a DM, which the
    /// caps don't cover, is never capped.
    async fn thread_capped(&self, consent: &Consent) -> Result<Option<RefuseReason>, StoreError> {
        let store = &self.inner.store;
        let in_dm = store
            .session(consent.origin_session)
            .await?
            .is_some_and(|session| matches!(session.scope, ScopeKey::Dm(_) | ScopeKey::Private));
        if in_dm {
            return Ok(None);
        }
        let settings = &self.inner.settings;
        let spend = store
            .thread_spend(&consent.thread, (settings.now)())
            .await?;
        Ok(settings.limits.thread_budget(spend).exceeded())
    }

    /// Posts Markdown `text` about `consent` in the thread that asked, as
    /// its agent's bot, recorded in `message_refs` like a private task's
    /// reply under a session id that is the consent's own. Done when it is
    /// posted, even if its row couldn't be recorded, since posting it again
    /// would post it twice, or when the agent can't post there any more.
    ///
    /// # Errors
    ///
    /// When it wasn't posted but may be later.
    async fn tell_thread(&self, consent: &Consent, text: &str) -> Result<(), PipelineError> {
        let conv = &consent.thread.conv;
        let Some(surface) = self.inner.surfaces.surface(consent.agent, conv).await? else {
            tracing::warn!(consent = %consent.id, "the agent has no surface in the thread that asked; its outcome isn't posted");
            return Ok(());
        };
        if !surface.can_post(conv).await? {
            return Ok(());
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
            hand_offs: None,
        };
        match delivery.post(None, text, &mut Handing::default()).await {
            Ok(()) | Err(Lost::Row) => {}
            Err(Lost::Chunk) => return Err(PipelineError::NotPosted),
        }
        tracing::info!(consent = %consent.id, "posted a private task's outcome");
        Ok(())
    }

    /// Once claim `attempt` on consent `id`'s work is known to be the
    /// current one, which renews it so no other claim can take the work
    /// over meanwhile, deletes the directories of the consent's private
    /// sessions, each once its container is stopped, which waits for a
    /// turn running in it here, then records that the claim did the work
    /// and deletes its files: every path a consent's work takes ends here.
    /// The directories go first, so nothing is left if the record is the
    /// last thing that happens; a later claim finds them gone. A stale
    /// claim touches nothing.
    async fn finish_consent(
        &self,
        consents: &Consents,
        id: ConsentId,
        attempt: u32,
    ) -> Result<(), StoreError> {
        let store = &self.inner.store;
        if !store
            .renew_consent_work(id, attempt, Consents::now() + WORK_LEASE)
            .await?
        {
            tracing::warn!(consent = %id, attempt, "a claim that lost a private task's work didn't finish it");
            return Ok(());
        }
        let sessions = self.inner.turns.sessions();
        for session in store.private_sessions_of(id).await? {
            sessions.stop(session.id).await;
            consents::discard(consents.session_dir(&session.volume(), session.id)).await;
        }
        if store.finish_consent(id, attempt, Consents::now()).await? {
            consents.forget_files(id).await;
        }
        Ok(())
    }

    /// Kills the containers of `session`, whose private task for claim
    /// `attempt` on consent `id` was cut short while `turn` ran in it,
    /// until the turn ends, for at most [`KILL_TIMEOUT`], and then stops the
    /// session and releases the claim, so the next claim can take the task
    /// up at once; it finds the session marked if the turn reached the
    /// model. A turn that outlives the kills keeps its claim until the lease
    /// lapses, and its container is left to the runner's idle reaping.
    async fn kill_turn(
        self,
        id: ConsentId,
        attempt: u32,
        session: SessionId,
        turn: JoinHandle<Result<TurnReport<Option<Outbox>>, RunnerError>>,
    ) {
        let sessions = self.inner.turns.sessions();
        let killed = tokio::time::timeout(KILL_TIMEOUT, async {
            while !turn.is_finished() {
                sessions.kill(session).await;
                tokio::time::sleep(KILL_EVERY).await;
            }
        })
        .await
        .is_ok();
        if !killed {
            tracing::warn!(%session, "a private task's turn outlived its kills");
            return;
        }
        tracing::info!(%session, "killed the turn of a private task cut short");
        sessions.stop(session).await;
        match self
            .inner
            .store
            .release_consent_work(id, attempt, Consents::now())
            .await
        {
            Ok(true) => {
                tracing::info!(consent = %id, "released a private task whose turn was killed")
            }
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(consent = %id, error = %err, "couldn't release a private task whose turn was killed");
            }
        }
    }

    /// Waits for the kills of the private tasks' turns that were cut short
    /// or taken over, for at most about 35 seconds, so those turns are
    /// billed and their claims released while the store is open. Cancelled,
    /// it leaves the kills running.
    pub async fn wait_for_kills(&self) {
        let waited = tokio::time::timeout(KILL_TIMEOUT + KILL_GRACE, async {
            while let Some(joined) =
                std::future::poll_fn(|cx| lock(&self.inner.kills).poll_join_next(cx)).await
            {
                if let Err(err) = joined {
                    tracing::error!(error = %err, "killing a private task's turn failed");
                }
            }
        })
        .await;
        if waited.is_err() {
            let mut kills = lock(&self.inner.kills);
            tracing::warn!(
                left = kills.len(),
                "private tasks' turns cut short are still being killed"
            );
            kills.detach_all();
        }
    }
}

/// The side an approved `consent` of an agent `owner` owns runs on: the
/// owner's when the owner asked for it in their own DM, or approved on its
/// card a task their own identity asked for, and the public side
/// otherwise. It never follows from who the requester is alone.
fn side(consent: &Consent, owner: MemberId) -> Side {
    let owners = consent.requester.member == Some(owner);
    match consent.approval {
        Some(Approval::Asked) if owners && consent.hop == Hop::ZERO => Side::Owner,
        Some(Approval::Card) if owners => Side::Owner,
        _ => Side::Public,
    }
}

/// A private task's turn, run in a task of its own that bills it to the
/// owner when it ends, however the private task ends. Dropped before it
/// ends, as when the private task is cut short or taken over, it has the
/// pipeline kill the session's containers until the turn ends
/// ([`Pipeline::kill_turn`]), so the turn ends as a crash, billed as one;
/// a shutdown waits for that ([`Pipeline::wait_for_kills`]).
struct TurnTask {
    pipeline: Pipeline,
    consent: ConsentId,
    attempt: u32,
    session: SessionId,
    turn: Option<JoinHandle<Result<TurnReport<Option<Outbox>>, RunnerError>>>,
}

impl TurnTask {
    /// Starts `request` on `consent`'s session `session`, for claim
    /// `attempt`, billed to the owner of `agent`.
    fn start(
        pipeline: &Pipeline,
        agent: &Agent,
        consent: &Consent,
        attempt: u32,
        session: SessionId,
        request: TurnRequest,
    ) -> Self {
        let (owner, id, thread) = (agent.owner, agent.id, consent.thread.clone());
        let running = pipeline.clone();
        let turn = tokio::spawn(async move {
            let report = running
                .inner
                .turns
                .sessions()
                .run_turn(session, request)
                .await?;
            running
                .bill(owner, id, &thread, true, &report.outcome)
                .await;
            Ok(report)
        });
        Self {
            pipeline: pipeline.clone(),
            consent: consent.id,
            attempt,
            session,
            turn: Some(turn),
        }
    }

    /// Waits for the turn to end.
    async fn join(mut self) -> Result<TurnReport<Option<Outbox>>, PipelineError> {
        let Some(turn) = self.turn.as_mut() else {
            return Err(PipelineError::Runner(RunnerError::TurnTask));
        };
        let joined = turn.await;
        self.turn = None;
        joined
            .map_err(|_| PipelineError::Runner(RunnerError::TurnTask))?
            .map_err(PipelineError::from)
    }
}

impl Drop for TurnTask {
    fn drop(&mut self) {
        let Some(turn) = self.turn.take() else {
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            return;
        }
        let killing =
            self.pipeline
                .clone()
                .kill_turn(self.consent, self.attempt, self.session, turn);
        let mut kills = lock(&self.pipeline.inner.kills);
        reap(&mut kills);
        kills.spawn(killing);
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

    #[test]
    fn the_owners_side_follows_how_a_task_was_approved_not_who_asked() {
        let owner = MemberId::new_v4();
        let thread = ThreadKey {
            conv: ConvRef {
                surface: core_types::SurfaceKind::RocketChat,
                team: "chat.example".into(),
                conversation: "GENERAL".into(),
            },
            root: None,
        };
        let requester = Requester {
            member: Some(owner),
            key: MemberKey {
                surface: core_types::SurfaceKind::RocketChat,
                team: "chat.example".into(),
                user: "alice".into(),
            },
            outside: None,
        };
        let new = store::NewConsent {
            id: ConsentId::new_v4(),
            agent: AgentId::new_v4(),
            requester: &requester,
            hop: Hop::ZERO,
            task: "t",
            attachments_json: "[]",
            thread: &thread,
            origin_session: SessionId::new_v4(),
            expires_at: OffsetDateTime::UNIX_EPOCH,
            approved_by_owner: Some(&requester.key),
        };
        let mut consent = new.draft(OffsetDateTime::UNIX_EPOCH);
        assert_eq!(
            side(&consent, owner),
            Side::Owner,
            "asked in the owner's DM"
        );
        consent.hop = Hop(1);
        assert_eq!(side(&consent, owner), Side::Public, "never asked at a hop");
        consent.approval = Some(Approval::Card);
        assert_eq!(side(&consent, owner), Side::Owner, "the owner's card");
        consent.requester.member = None;
        assert_eq!(side(&consent, owner), Side::Public, "someone else's card");
        consent.approval = None;
        consent.requester.member = Some(owner);
        assert_eq!(side(&consent, owner), Side::Public, "not approved");
    }
}

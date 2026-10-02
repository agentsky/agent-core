//! [`StoreView`]: the router's view of the world, loaded from the store
//! for one event and one candidate agent.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use core_types::{
    AgentId, BindingId, InboundEvent, MemberId, MemberKey, MsgRef, Requester, ThreadKey,
};
use router::{
    AgentPolicy, AgentState, Attribution, LinkState, ManagedBot, RouterView, ThreadBudget,
};
use store::{MessageRef, Store, StoreError};
use time::OffsetDateTime;
use tokio::time::Instant;

use crate::policy::{Limits, agent_policy, pending_denials};

/// The first pause between two reads of an attribution; each next one is
/// twice as long.
const ATTRIBUTION_FIRST_PAUSE: Duration = Duration::from_millis(25);

/// The row attributing `msg` to the agent agentd posted it as, read again
/// with growing pauses for up to `wait`.
async fn attribution(
    store: &Store,
    msg: &MsgRef,
    wait: Duration,
) -> Result<Option<MessageRef>, StoreError> {
    let deadline = Instant::now() + wait;
    let mut pause = ATTRIBUTION_FIRST_PAUSE;
    loop {
        let posted = store.posted_message_ref(msg).await?;
        let now = Instant::now();
        if posted.is_some() || now >= deadline {
            return Ok(posted);
        }
        tokio::time::sleep(pause.min(deadline - now)).await;
        pause *= 2;
    }
}

/// Everything [`router::route`] may ask about one event and one agent,
/// loaded first, since the store is asynchronous and the view isn't.
///
/// It loads each lookup the [`RouterView`] rustdoc lists: the agent's owner
/// and state, which bots the sender and every mention are (the manager
/// bots included), whose binding received the event, the attribution of
/// the event's message and whether its reply-to message is the agent's,
/// the members of the sender and of an attributed requester, whether the
/// owner and those members have a link and whether it broke, and whether a
/// community admin has set the community API key; then whether any of those
/// members is banned, the agent's policy with the turns it took today, and,
/// outside a one-to-one DM, what agents spent in the event's thread. A
/// sender or attributed requester whose identity is a community admin's is
/// never banned, as [`Commands`](crate::commands::Commands) never holds an
/// admin back, so a ban row left on an admin's member can't silence them.
///
/// Those last three fail closed on their own: a lookup that fails is logged
/// and leaves its answer `None`, which the router refuses, rather than
/// failing the whole view.
///
/// The attribution is waited for only when the router reads it: another
/// agent's bot sent the message, mentioning this agent. A post of an
/// agent's bot that has none, such as a file a turn uploaded, holds no lane
/// up otherwise. Only a post whose row hands off (`hands_off`: a turn's
/// post in the turn's own thread) is given its attribution, so the router
/// takes a mention as a hop only there. A private task's result or
/// outcome never hands off, so private context doesn't flow to another
/// agent's turn and no hop chains on the owner's credential from it; nor
/// does a post a turn made in another thread or conversation, so a hop
/// never starts a thread of its own with a fresh budget.
#[derive(Debug, Default)]
pub(crate) struct StoreView {
    agent: Option<(AgentId, MemberId, AgentState)>,
    bots: HashMap<MemberKey, ManagedBot>,
    binding: Option<(BindingId, AgentId)>,
    attribution: Option<(MsgRef, Attribution)>,
    replied: Option<(MsgRef, AgentId)>,
    members: HashMap<MemberKey, MemberId>,
    links: HashMap<MemberId, LinkState>,
    community_key: bool,
    admins: Vec<MemberKey>,
    banned: Option<HashSet<MemberId>>,
    policy: Option<AgentPolicy>,
    thread: Option<ThreadBudget>,
}

/// What [`StoreView::load`] needs besides the store and the event.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ViewContext<'a> {
    /// The manager bots' identities.
    pub(crate) managers: &'a [MemberKey],
    /// The community admins' identities, whom a ban never holds back.
    pub(crate) admins: &'a [MemberKey],
    /// The thread the event is in, as its turn would run.
    pub(crate) thread: &'a ThreadKey,
    /// The community's caps.
    pub(crate) limits: &'a Limits,
    /// Now, for today's and this hour's counts.
    pub(crate) now: OffsetDateTime,
    /// How long the attribution of another agent's post is waited for
    /// ([`PipelineSettings::attribution_wait`](super::PipelineSettings::attribution_wait)).
    pub(crate) attribution_wait: Duration,
}

impl StoreView {
    /// Loads the view of `event` for `agent`.
    pub(crate) async fn load(
        store: &Store,
        event: &InboundEvent,
        agent: AgentId,
        context: ViewContext<'_>,
    ) -> Result<Self, StoreError> {
        let managers = context.managers;
        let mut view = Self::default();
        if let Some(row) = store.agent(agent).await? {
            let state = match row.state {
                store::AgentState::Active => AgentState::Active,
                store::AgentState::Paused => AgentState::Paused,
                store::AgentState::Deleted => AgentState::Deleted,
            };
            view.agent = Some((row.id, row.owner, state));
            view.link(store, row.owner).await?;
        }
        let mentions: Vec<_> = event
            .mentions
            .iter()
            .map(|user| MemberKey {
                surface: event.conv.surface,
                team: event.conv.team.clone(),
                user: user.clone(),
            })
            .collect();
        for key in std::iter::once(&event.sender).chain(&mentions) {
            if managers.contains(key) {
                view.bots.insert(key.clone(), ManagedBot::Manager);
            } else if let Some(owner) = store.agent_of_bot_user(key).await? {
                view.bots.insert(key.clone(), ManagedBot::Agent(owner));
            }
        }
        if let Some(bound) = store.agent_for_binding(event.binding).await? {
            view.binding = Some((event.binding, bound.id));
        }
        let from_other_agent = matches!(
            view.bots.get(&event.sender),
            Some(ManagedBot::Agent(poster)) if *poster != agent
        );
        let mentions_agent = mentions
            .iter()
            .any(|key| view.bots.get(key) == Some(&ManagedBot::Agent(agent)));
        let wait = if from_other_agent && mentions_agent {
            context.attribution_wait
        } else {
            Duration::ZERO
        };
        if let Some(posted) = attribution(store, &event.message, wait).await?
            && posted.hands_off
            && let Some(poster) = posted.agent
        {
            view.member(store, &posted.requester.key).await?;
            if let Some(member) = posted.requester.member {
                view.link(store, member).await?;
            }
            view.attribution = Some((
                event.message.clone(),
                Attribution {
                    agent: poster,
                    requester: posted.requester,
                    hop: posted.hop,
                },
            ));
        }
        if let Some(reply_to) = &event.reply_to
            && let Some(posted) = store.posted_message_ref(reply_to).await?
            && let Some(poster) = posted.agent
        {
            view.replied = Some((reply_to.clone(), poster));
        }
        view.member(store, &event.sender).await?;
        view.admins = std::iter::once(&event.sender)
            .chain(
                view.attribution
                    .as_ref()
                    .map(|(_, attributed)| &attributed.requester.key),
            )
            .filter(|key| context.admins.contains(key))
            .cloned()
            .collect();
        view.community_key = store.community_api_key_set().await?;
        view.banned = view.bans(store, agent).await;
        view.policy = policy(store, agent, context).await;
        if !event.is_dm() {
            view.thread = match store.thread_spend(context.thread, context.now).await {
                Ok(spend) => Some(context.limits.thread_budget(spend)),
                Err(err) => {
                    tracing::warn!(%agent, error = %err, "couldn't read what agents spent in a thread");
                    None
                }
            };
        }
        Ok(view)
    }

    /// Which of the members the view knows are banned, or `None` if the
    /// store couldn't say.
    async fn bans(&self, store: &Store, agent: AgentId) -> Option<HashSet<MemberId>> {
        let members: HashSet<MemberId> = self
            .members
            .values()
            .copied()
            .chain(
                self.attribution
                    .as_ref()
                    .and_then(|(_, attribution)| attribution.requester.member),
            )
            .collect();
        let mut banned = HashSet::new();
        for member in members {
            match store.is_banned(member).await {
                Ok(true) => {
                    banned.insert(member);
                }
                Ok(false) => {}
                Err(err) => {
                    tracing::warn!(%agent, error = %err, "couldn't read whether a requester is banned");
                    return None;
                }
            }
        }
        Some(banned)
    }

    /// Records the member `key` belongs to, and whether it is linked.
    async fn member(&mut self, store: &Store, key: &MemberKey) -> Result<(), StoreError> {
        if let Some(member) = store.member_for_identity(key).await? {
            self.members.insert(key.clone(), member);
            self.link(store, member).await?;
        }
        Ok(())
    }

    /// Records whether `member` has a link, and whether it broke.
    async fn link(&mut self, store: &Store, member: MemberId) -> Result<(), StoreError> {
        let state = match store.claude_link_status(member).await? {
            None => LinkState::Unlinked,
            Some(status) if status.broken_at.is_some() => LinkState::Broken,
            Some(_) => LinkState::Linked,
        };
        self.links.insert(member, state);
        Ok(())
    }
}

impl RouterView for StoreView {
    fn managed_bot(&self, bot: &MemberKey) -> Option<ManagedBot> {
        self.bots.get(bot).copied()
    }

    fn binding_agent(&self, binding: BindingId) -> Option<AgentId> {
        self.binding
            .filter(|(known, _)| *known == binding)
            .map(|(_, agent)| agent)
    }

    fn message_ref(&self, msg: &MsgRef) -> Option<Attribution> {
        self.attribution
            .as_ref()
            .filter(|(known, _)| known == msg)
            .map(|(_, attribution)| attribution.clone())
    }

    fn member_for(&self, key: &MemberKey) -> Option<MemberId> {
        self.members.get(key).copied()
    }

    fn link_state(&self, member: MemberId) -> LinkState {
        self.links
            .get(&member)
            .copied()
            .unwrap_or(LinkState::Unlinked)
    }

    fn community_key_configured(&self) -> bool {
        self.community_key
    }

    fn agent_owner(&self, agent: AgentId) -> Option<MemberId> {
        self.agent
            .filter(|(known, _, _)| *known == agent)
            .map(|(_, owner, _)| owner)
    }

    fn agent_state(&self, agent: AgentId) -> Option<AgentState> {
        self.agent
            .filter(|(known, _, _)| *known == agent)
            .map(|(_, _, state)| state)
    }

    fn is_reply_to_agent(&self, msg: &MsgRef, agent: AgentId) -> bool {
        self.replied
            .as_ref()
            .is_some_and(|(known, poster)| known == msg && *poster == agent)
    }

    fn policy(&self, agent: AgentId) -> Option<AgentPolicy> {
        self.policy
            .clone()
            .filter(|_| self.agent.is_some_and(|(known, _, _)| known == agent))
    }

    fn is_banned(&self, requester: &Requester) -> Option<bool> {
        if self.admins.contains(&requester.key) {
            return Some(false);
        }
        let banned = self.banned.as_ref()?;
        Some(
            requester
                .member
                .into_iter()
                .chain(self.member_for(&requester.key))
                .any(|member| banned.contains(&member)),
        )
    }

    fn thread_budget(&self) -> Option<ThreadBudget> {
        self.thread
    }
}

/// `agent`'s policy under `context`'s limits, with the turns it took
/// today and its denies on the old ids of channel id changes still waiting
/// applying to their new ids ([`pending_denials`]), or `None` if the store
/// couldn't say or its rules don't read.
///
/// The changes are read before the rules. Settling a change, or giving it
/// up, writes the rules first and only then marks the change settled or
/// deletes it, so rules read after a change was seen waiting are either
/// the old ones, which its pending denials cover, or the new ones, which
/// cover themselves; read the other way round, a settle between the two
/// reads would leave neither.
async fn policy(store: &Store, agent: AgentId, context: ViewContext<'_>) -> Option<AgentPolicy> {
    let loaded = async {
        let changes = store.channel_id_changes_of_agent(agent).await?;
        let settings = store.agent_settings(agent).await?;
        let turns = store.capped_turns_on(agent, context.now).await?;
        Ok::<_, StoreError>((settings, turns, pending_denials(&changes)))
    };
    match loaded.await {
        Ok((settings, turns, pending)) => {
            match agent_policy(&settings, context.limits, turns, &pending) {
                Ok(policy) => Some(policy),
                Err(err) => {
                    tracing::warn!(%agent, kind = ?err.classify(), column = err.column(), "an agent's allow and deny rules don't read");
                    None
                }
            }
        }
        Err(err) => {
            tracing::warn!(%agent, error = %err, "couldn't read an agent's policy");
            None
        }
    }
}

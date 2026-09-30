//! [`StoreView`]: the router's view of the world, loaded from the store
//! for one event and one candidate agent.

use std::collections::HashMap;
use std::time::Duration;

use core_types::{AgentId, BindingId, InboundEvent, MemberId, MemberKey, MsgRef, Requester};
use router::{AgentPolicy, AgentState, Attribution, LinkState, ManagedBot, RouterView};
use store::{MessageRef, Store, StoreError};
use tokio::time::Instant;

/// How long the attribution of a message an agent's bot sent is waited
/// for: agentd records it just after posting, and the platform may deliver
/// the message sooner.
pub(crate) const ATTRIBUTION_WAIT: Duration = Duration::from_secs(2);

/// The first pause between two reads of an attribution; each next one is
/// twice as long.
const ATTRIBUTION_FIRST_PAUSE: Duration = Duration::from_millis(25);

/// The row attributing `msg` to the agent agentd posted it as. When
/// `wait`, the row is read again, with growing pauses, for up to
/// [`ATTRIBUTION_WAIT`].
async fn attribution(
    store: &Store,
    msg: &MsgRef,
    wait: bool,
) -> Result<Option<MessageRef>, StoreError> {
    let deadline = Instant::now() + ATTRIBUTION_WAIT;
    let mut pause = ATTRIBUTION_FIRST_PAUSE;
    loop {
        let posted = store.posted_message_ref(msg).await?;
        let now = Instant::now();
        if posted.is_some() || !wait || now >= deadline {
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
/// community admin has set the community API key. Until T27, `policy` answers
/// [`AgentPolicy::default`] and `is_banned` `Some(false)`.
///
/// The attribution is waited for only when the router reads it: another
/// agent's bot sent the message, mentioning this agent. A post of an
/// agent's bot that has none, such as a file a turn uploaded, holds no lane
/// up otherwise.
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
}

impl StoreView {
    /// Loads the view of `event` for `agent`. `managers` are the manager
    /// bots' identities.
    pub(crate) async fn load(
        store: &Store,
        event: &InboundEvent,
        agent: AgentId,
        managers: &[MemberKey],
    ) -> Result<Self, StoreError> {
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
        let wait = from_other_agent && mentions_agent;
        if let Some(posted) = attribution(store, &event.message, wait).await?
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
        view.community_key = store.community_api_key_set().await?;
        Ok(view)
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

    fn policy(&self, _agent: AgentId) -> Option<AgentPolicy> {
        Some(AgentPolicy::default())
    }

    fn is_banned(&self, _requester: &Requester) -> Option<bool> {
        Some(false)
    }
}

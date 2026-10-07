//! Mention gating and credential policy for agent-core turns.
//!
//! [`route`] decides, for one [`InboundEvent`] and one candidate agent,
//! whether the agent answers, and if so for whom, on whose credential, in
//! which scope and on which side. It reads the world only through
//! [`RouterView`] and does no I/O. [`ModelPolicy`] picks the model from the
//! requester's plan.
//!
//! # Precedence
//!
//! [`route`] checks these in order and returns at the first that applies:
//!
//! 1. **The agent.** Unknown: [`IgnoreReason::UnknownAgent`]. Deleted:
//!    [`IgnoreReason::AgentDeleted`].
//! 2. **The sender.** A bot whose `sender_bot_user` is not `sender.user`
//!    (a Slack bot known only by its bot id) is
//!    [`IgnoreReason::UnmanagedBot`] without a lookup. Otherwise the sender
//!    is looked up with [`RouterView::managed_bot`], whatever
//!    `sender_is_bot` says, so a managed bot's post is never taken for a
//!    person's. This agent: [`IgnoreReason::OwnMessage`], even if it
//!    mentions itself. The manager bot: [`IgnoreReason::ManagerBot`]. A bot
//!    that isn't managed: [`IgnoreReason::UnmanagedBot`].
//! 3. **Whose DM.** A one-to-one DM that didn't come in through one of this
//!    agent's bindings: [`IgnoreReason::NotThisAgentsDm`], even if it
//!    mentions the agent.
//! 4. **Gating.** Another managed agent must mention this agent
//!    ([`IgnoreReason::NotMentionedByAgent`]). A person must mention it,
//!    reply to one of its messages in the same conversation, or be in its
//!    DM ([`IgnoreReason::NotAddressed`]). A reply counts only if it
//!    mentions no other managed agent, so a reply in this agent's thread
//!    that names another agent runs that agent alone, and the person pays
//!    for one turn, not two. Mentions are user ids in the conversation's
//!    own surface and team.
//! 5. **Attribution.** A managed agent's message must have a
//!    [`RouterView::message_ref`] recorded for that same agent:
//!    [`IgnoreReason::UnattributedManagedBot`]. The turn inherits its
//!    requester and the next hop. The requester's member is the recorded
//!    one, or, if none was recorded, the member its key belongs to now.
//! 6. **Paused**: [`RefuseReason::Paused`].
//! 7. **Banned requester**: [`RefuseReason::Banned`]. For a hop that is the
//!    inherited requester, so a ban can't be sidestepped through an agent.
//!    If the view can't say which member the requester is, or whether they
//!    are banned: [`RefuseReason::PolicyUnavailable`].
//! 8. **Allow and deny rules**, for anyone but the owner:
//!    [`RefuseReason::Denied`]. If the view has no policy for the agent:
//!    [`RefuseReason::PolicyUnavailable`], for the owner too, since the
//!    policy also holds the hop cap.
//! 9. **Hop cap**: [`RefuseReason::HopCap`], also when the hop counter
//!    would overflow.
//! 10. **Daily cap**, for anyone but the owner: [`RefuseReason::DailyCap`]
//!     once the agent has taken [`AgentPolicy::turns_per_day`] turns today.
//! 11. **Thread caps**, outside one-to-one DMs, for everyone, the owner
//!     included: [`RefuseReason::ThreadTurns`] once agents took the hour's
//!     turns in the thread, then [`RefuseReason::ThreadTokens`] once their
//!     turns used the day's token budget. If the view can't say:
//!     [`RefuseReason::PolicyUnavailable`]. A one-to-one DM has one agent
//!     in it, so no agents can answer each other there.
//! 12. **Credential.** The owner runs on their own credential, or gets
//!     [`Decision::LinkPrompt`] if they have none; the community key is
//!     never used for the owner. Anyone else runs on their own credential
//!     if linked, else on the community key if one is configured, else gets
//!     [`Decision::LinkPrompt`]. A requester whose link is broken, the
//!     owner included, gets [`Decision::RelinkPrompt`], never the community
//!     key.
//!
//! Every ignore comes before every refusal, so an unaddressed or
//! unattributed message never produces a visible reply, however the agent
//! is configured. Every refusal comes before the credential, so a link
//! prompt or a community-key turn is never offered to a requester who would
//! be refused anyway. Refusals follow the plan's order: the agent, then the
//! person, then the rules, then the chain, then the limits. None of them
//! spends anything, so their order only decides which notice is shown.
//!
//! [`RouterView`] documents which lookups [`route`] makes for an event, in
//! order, so a store-backed view knows what to load first.
//!
//! # Scope and side
//!
//! Only the owner's own message in a one-to-one DM with the agent runs on
//! [`ScopeKind::Private`] and [`Side::Owner`]. Every other turn, the owner's
//! channel turns and every agent-to-agent hop included, runs on the public
//! side in the conversation's own scope: [`ScopeKind::Channel`],
//! [`ScopeKind::GroupDm`], or [`ScopeKind::Dm`] for a non-owner's DM.

#![warn(missing_docs)]

mod decision;
mod model;
mod view;

use core_types::{
    AgentId, CredentialRef, Hop, InboundEvent, MemberKey, Requester, ScopeKey, ScopeKind, Side,
};

pub use decision::{Decision, IgnoreReason, RefuseReason};
pub use model::ModelPolicy;
pub use view::{
    AgentPolicy, AgentState, Attribution, DEFAULT_MAX_HOPS, LinkState, ManagedBot, PolicyTarget,
    RouterView, ThreadBudget,
};

/// Decides whether `agent` answers `event`, and how. See the
/// [crate docs](crate#precedence) for the order of the checks.
pub fn route(event: &InboundEvent, agent: AgentId, view: &dyn RouterView) -> Decision {
    let (Some(owner), Some(state)) = (view.agent_owner(agent), view.agent_state(agent)) else {
        return Decision::Ignore(IgnoreReason::UnknownAgent);
    };
    if state == AgentState::Deleted {
        return Decision::Ignore(IgnoreReason::AgentDeleted);
    }

    let sender = match classify_sender(event, agent, view) {
        Ok(sender) => sender,
        Err(reason) => return Decision::Ignore(reason),
    };

    if event.is_dm() && view.binding_agent(event.binding) != Some(agent) {
        return Decision::Ignore(IgnoreReason::NotThisAgentsDm);
    }

    let (key, member, hop) = match sender {
        Sender::Person => {
            let mentions = mentions(event, agent, view);
            let addressed = mentions == Mentions::ThisAgent
                || event.is_dm()
                || (mentions == Mentions::NoAgent
                    && event.reply_to.as_ref().is_some_and(|msg| {
                        msg.conv == event.conv && view.is_reply_to_agent(msg, agent)
                    }));
            if !addressed {
                return Decision::Ignore(IgnoreReason::NotAddressed);
            }
            (
                event.sender.clone(),
                view.member_for(&event.sender),
                Some(Hop::ZERO),
            )
        }
        Sender::Agent(posted_by) => {
            if mentions(event, agent, view) != Mentions::ThisAgent {
                return Decision::Ignore(IgnoreReason::NotMentionedByAgent);
            }
            let Some(attribution) = view
                .message_ref(&event.message)
                .filter(|attribution| attribution.agent == posted_by)
            else {
                return Decision::Ignore(IgnoreReason::UnattributedManagedBot);
            };
            let Requester { member, key } = attribution.requester;
            let member = member.map(Some).or_else(|| view.member_for(&key));
            (key, member, attribution.hop.next())
        }
    };

    if state == AgentState::Paused {
        return Decision::Refuse {
            reason: RefuseReason::Paused,
            requester,
        };
    }
    let Some(member) = member else {
        return Decision::Refuse(RefuseReason::PolicyUnavailable);
    };
    let requester = Requester { member, key };
    match view.is_banned(&requester) {
        Some(false) => {}
        Some(true) => {
            return Decision::Refuse {
                reason: RefuseReason::Banned,
                requester,
            };
        }
        None => {
            return Decision::Refuse {
                reason: RefuseReason::PolicyUnavailable,
                requester,
            };
        }
    }
    let Some(policy) = view.policy(agent) else {
        return Decision::Refuse {
            reason: RefuseReason::PolicyUnavailable,
            requester,
        };
    };
    let is_owner = requester.member == Some(owner);
    if !is_owner && !policy.permits(&requester, &event.conv) {
        return Decision::Refuse {
            reason: RefuseReason::Denied,
            requester,
        };
    }
    let Some(hop) = hop.filter(|hop| *hop <= policy.max_hops) else {
        return Decision::Refuse {
            reason: RefuseReason::HopCap {
                max: policy.max_hops,
            },
            requester,
        };
    };
    if !is_owner && let Some(max) = policy.turns_per_day.filter(|_| policy.daily_cap_reached()) {
        return Decision::Refuse {
            reason: RefuseReason::DailyCap { max },
            requester,
        };
    }
    if !event.is_dm() {
        let Some(budget) = view.thread_budget() else {
            return Decision::Refuse {
                reason: RefuseReason::PolicyUnavailable,
                requester,
            };
        };
        if let Some(reason) = budget.exceeded() {
            return Decision::Refuse { reason, requester };
        }
    }

    let credential = match credential(view, &requester, is_owner) {
        Ok(credential) => credential,
        Err(Prompt::Link) => return Decision::LinkPrompt { requester },
        Err(Prompt::Relink) => return Decision::RelinkPrompt { requester },
    };
    let (scope, side) = if is_owner && sender == Sender::Person && event.is_dm() {
        (ScopeKind::Private, Side::Owner)
    } else {
        let scope = ScopeKey::for_conversation(event.conv_kind, event.conv.clone());
        (scope.kind(), Side::Public)
    };
    Decision::Run {
        requester,
        hop,
        credential,
        scope,
        side,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sender {
    Person,
    Agent(AgentId),
}

fn classify_sender(
    event: &InboundEvent,
    agent: AgentId,
    view: &dyn RouterView,
) -> Result<Sender, IgnoreReason> {
    let is_bot = event.sender_is_bot || event.sender_bot_user.is_some();
    if is_bot && event.sender_bot_user.as_ref() != Some(&event.sender.user) {
        return Err(IgnoreReason::UnmanagedBot);
    }
    match view.managed_bot(&event.sender) {
        Some(ManagedBot::Agent(sender)) if sender == agent => Err(IgnoreReason::OwnMessage),
        Some(ManagedBot::Agent(sender)) => Ok(Sender::Agent(sender)),
        Some(ManagedBot::Manager) => Err(IgnoreReason::ManagerBot),
        None if is_bot => Err(IgnoreReason::UnmanagedBot),
        None => Ok(Sender::Person),
    }
}

/// Which managed agents an event mentions, from one agent's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mentions {
    /// It mentions the agent, and maybe others.
    ThisAgent,
    /// It mentions other managed agents, but not this one.
    OtherAgentsOnly,
    /// It mentions no managed agent.
    NoAgent,
}

/// Which managed agents the event mentions. Mentions are user ids in the
/// conversation's own surface and team. The manager bot is no agent.
fn mentions(event: &InboundEvent, agent: AgentId, view: &dyn RouterView) -> Mentions {
    let mut found = Mentions::NoAgent;
    for user in &event.mentions {
        let key = MemberKey {
            surface: event.conv.surface,
            team: event.conv.team.clone(),
            user: user.clone(),
        };
        match view.managed_bot(&key) {
            Some(ManagedBot::Agent(mentioned)) if mentioned == agent => {
                return Mentions::ThisAgent;
            }
            Some(ManagedBot::Agent(_)) => found = Mentions::OtherAgentsOnly,
            Some(ManagedBot::Manager) | None => {}
        }
    }
    found
}

/// Which prompt a requester without a credential to run on gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Prompt {
    Link,
    Relink,
}

/// The credential a turn runs on, or the prompt its requester gets. The
/// community key is for requesters with no link, never for the owner or
/// for a member whose link broke.
fn credential(
    view: &dyn RouterView,
    requester: &Requester,
    is_owner: bool,
) -> Result<CredentialRef, Prompt> {
    let link = requester
        .member
        .map(|member| (member, view.link_state(member)));
    match link {
        Some((member, LinkState::Linked)) => Ok(CredentialRef::Member(member)),
        Some((_, LinkState::Broken)) => Err(Prompt::Relink),
        _ if !is_owner && view.community_key_configured() => Ok(CredentialRef::Community),
        _ => Err(Prompt::Link),
    }
}

#[cfg(test)]
mod tests;

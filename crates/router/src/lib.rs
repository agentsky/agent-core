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
//!    is looked up with [`RouterView::is_managed_bot`], whatever
//!    `sender_is_bot` says, so a managed agent's post is never taken for a
//!    person's. This agent: [`IgnoreReason::OwnMessage`], even if it
//!    mentions itself. A bot that isn't managed:
//!    [`IgnoreReason::UnmanagedBot`].
//! 3. **Whose DM.** A one-to-one DM that didn't come in through one of this
//!    agent's bindings: [`IgnoreReason::NotThisAgentsDm`], even if it
//!    mentions the agent.
//! 4. **Gating.** Another managed agent must mention this agent
//!    ([`IgnoreReason::NotMentionedByAgent`]). A person must mention it,
//!    reply to one of its messages in the same conversation, or be in its
//!    DM ([`IgnoreReason::NotAddressed`]). Mentions are user ids in the
//!    conversation's own surface and team.
//! 5. **Attribution.** A managed agent's message must have a
//!    [`RouterView::message_ref`] recorded for that same agent:
//!    [`IgnoreReason::UnattributedManagedBot`]. The turn inherits its
//!    requester and the next hop. The requester's member is the recorded
//!    one, or, if none was recorded, the member its key belongs to now.
//! 6. **Paused**: [`RefuseReason::Paused`].
//! 7. **Banned requester**: [`RefuseReason::Banned`]. For a hop that is the
//!    inherited requester, so a ban can't be sidestepped through an agent.
//! 8. **Allow and deny rules**, for anyone but the owner:
//!    [`RefuseReason::Denied`].
//! 9. **Hop cap**: [`RefuseReason::HopCap`], also when the hop counter
//!    would overflow.
//! 10. **Credential.** The owner runs on their own credential, or gets
//!     [`Decision::LinkPrompt`] if they have none; the community key is
//!     never used for the owner. Anyone else runs on their own credential
//!     if linked, else on the community key if one is configured, else gets
//!     [`Decision::LinkPrompt`].
//!
//! Every ignore comes before every refusal, so an unaddressed or
//! unattributed message never produces a visible reply, however the agent
//! is configured. Every refusal comes before the credential, so a link
//! prompt or a community-key turn is never offered to a requester who would
//! be refused anyway. Refusals follow the plan's order: the agent, then the
//! person, then the rules, then the chain. None of them spends anything, so
//! their order only decides which notice is shown.
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
    AgentId, CredentialRef, Hop, InboundEvent, MemberId, MemberKey, Requester, ScopeKey, ScopeKind,
    Side,
};

pub use decision::{Decision, IgnoreReason, RefuseReason};
pub use model::ModelPolicy;
pub use view::{AgentPolicy, AgentState, Attribution, DEFAULT_MAX_HOPS, PolicyTarget, RouterView};

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

    let (requester, hop) = match sender {
        Sender::Person => {
            let addressed = mentions(event, agent, view)
                || event.is_dm()
                || event.reply_to.as_ref().is_some_and(|msg| {
                    msg.conv == event.conv && view.is_reply_to_agent(msg, agent)
                });
            if !addressed {
                return Decision::Ignore(IgnoreReason::NotAddressed);
            }
            let requester = Requester {
                member: view.member_for(&event.sender),
                key: event.sender.clone(),
            };
            (requester, Some(Hop::ZERO))
        }
        Sender::Agent(posted_by) => {
            if !mentions(event, agent, view) {
                return Decision::Ignore(IgnoreReason::NotMentionedByAgent);
            }
            let Some(attribution) = view
                .message_ref(&event.message)
                .filter(|attribution| attribution.agent == posted_by)
            else {
                return Decision::Ignore(IgnoreReason::UnattributedManagedBot);
            };
            let Requester { member, key } = attribution.requester;
            let member = member.or_else(|| view.member_for(&key));
            (Requester { member, key }, attribution.hop.next())
        }
    };

    if state == AgentState::Paused {
        return Decision::Refuse(RefuseReason::Paused);
    }
    if view.is_banned(&requester) {
        return Decision::Refuse(RefuseReason::Banned);
    }
    let is_owner = requester.member == Some(owner);
    let policy = view.policy(agent);
    if !is_owner && !policy.permits(&requester.key, &event.conv) {
        return Decision::Refuse(RefuseReason::Denied);
    }
    let Some(hop) = hop.filter(|hop| *hop <= policy.max_hops) else {
        return Decision::Refuse(RefuseReason::HopCap {
            max: policy.max_hops,
        });
    };

    let Some(credential) = credential(view, &requester, is_owner.then_some(owner)) else {
        return Decision::LinkPrompt { requester };
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
    match view.is_managed_bot(&event.sender) {
        Some(sender) if sender == agent => Err(IgnoreReason::OwnMessage),
        Some(sender) => Ok(Sender::Agent(sender)),
        None if is_bot => Err(IgnoreReason::UnmanagedBot),
        None => Ok(Sender::Person),
    }
}

/// Whether the event mentions one of `agent`'s bot users. Mentions are user
/// ids in the conversation's own surface and team.
fn mentions(event: &InboundEvent, agent: AgentId, view: &dyn RouterView) -> bool {
    event.mentions.iter().any(|user| {
        let key = MemberKey {
            surface: event.conv.surface,
            team: event.conv.team.clone(),
            user: user.clone(),
        };
        view.is_managed_bot(&key) == Some(agent)
    })
}

/// The credential a turn runs on, or `None` for a link prompt. `owner` is
/// set when the requester is the agent's owner.
fn credential(
    view: &dyn RouterView,
    requester: &Requester,
    owner: Option<MemberId>,
) -> Option<CredentialRef> {
    if let Some(owner) = owner {
        return view
            .is_linked(owner)
            .then_some(CredentialRef::Member(owner));
    }
    match requester.member {
        Some(member) if view.is_linked(member) => Some(CredentialRef::Member(member)),
        _ if view.community_key_configured() => Some(CredentialRef::Community),
        _ => None,
    }
}

#[cfg(test)]
mod tests;

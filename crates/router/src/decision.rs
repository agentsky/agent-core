//! [`Decision`]: what [`route`](crate::route) decides for one event and one
//! agent, and the reasons it gives.

use std::fmt;

use core_types::{CredentialRef, Hop, Requester, ScopeKind, Side};

/// What the router decides for one event and one candidate agent.
///
/// The enum is deliberately not `#[non_exhaustive]`: the pipeline matches it
/// without a wildcard arm, so a new variant fails to compile there instead of
/// being dropped silently.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Do nothing and say nothing.
    Ignore(IgnoreReason),
    /// Tell the requester, privately, to link a Claude account. Nothing runs.
    ///
    /// `requester` is who to tell: the sender for a person's message, or the
    /// inherited requester for an agent-to-agent hop.
    LinkPrompt {
        /// Who should link an account.
        requester: Requester,
    },
    /// Tell the requester, privately, that their linked Claude account
    /// stopped working and to link it again. Nothing runs: a member whose
    /// link broke is never moved to the community key.
    ///
    /// `requester` is who to tell, as for [`Decision::LinkPrompt`].
    RelinkPrompt {
        /// Whose link broke.
        requester: Requester,
    },
    /// Run a turn.
    Run {
        /// Who caused the turn, and so who pays for it.
        requester: Requester,
        /// How many agent-to-agent hops led to it: 0 for a person's message.
        hop: Hop,
        /// Whose credential the turn runs on.
        credential: CredentialRef,
        /// Which scope, and so which volume, the turn uses.
        /// [`ScopeKind::Private`] only for the owner's own DM with the agent.
        scope: ScopeKind,
        /// Which side of the agent the turn runs on. [`Side::Owner`] exactly
        /// when `scope` is [`ScopeKind::Private`].
        side: Side,
    },
    /// Don't run, and say why: to `requester` privately when the refusal
    /// is of them ([`RefuseReason::Banned`] and [`RefuseReason::Denied`])
    /// and they sent the message, to no one for such a refusal on a hop,
    /// and otherwise to the thread, in one line.
    Refuse {
        /// Why.
        reason: RefuseReason,
        /// Whose request is refused, as for [`Decision::LinkPrompt`]. Its
        /// member is `None` when the view couldn't say which member the
        /// requester is.
        requester: Requester,
    },
}

impl Decision {
    /// Whose request the decision answers: who pays for a turn, who is
    /// prompted or refused. `None` for [`Decision::Ignore`].
    pub fn requester(&self) -> Option<&Requester> {
        match self {
            Self::Ignore(_) => None,
            Self::LinkPrompt { requester }
            | Self::RelinkPrompt { requester }
            | Self::Run { requester, .. }
            | Self::Refuse { requester, .. } => Some(requester),
        }
    }
}

/// Why the router ignored an event. Ignored events get no reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IgnoreReason {
    /// The view knows no such agent.
    UnknownAgent,
    /// The agent is deleted.
    AgentDeleted,
    /// The agent posted the message itself.
    OwnMessage,
    /// A bot agentd doesn't manage sent it, or a bot known only by its bot
    /// id, which never matches a managed agent.
    UnmanagedBot,
    /// The manager bot sent it. The manager bot never starts a turn, and
    /// nobody is billed for what it posts.
    ManagerBot,
    /// A one-to-one DM that came in through another bot's binding, so it is
    /// not this agent's DM.
    NotThisAgentsDm,
    /// Another managed agent posted it without mentioning this agent.
    /// Replying in a thread, or in a DM, is not enough for an agent.
    NotMentionedByAgent,
    /// A managed agent mentioned this agent, but agentd has no record of
    /// posting that message for that agent, so nobody can be billed.
    UnattributedManagedBot,
    /// A person's message that neither mentions the agent, replies to one of
    /// its messages, nor is a DM with it. A reply that mentions another
    /// managed agent but not this one is addressed to that agent only.
    NotAddressed,
    /// The requester is from outside the workspace agentd serves
    /// ([`Requester::outside`](core_types::Requester::outside)), whom
    /// nothing admits yet.
    Outside,
}

impl IgnoreReason {
    /// A short, stable description for logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UnknownAgent => "unknown agent",
            Self::AgentDeleted => "agent deleted",
            Self::OwnMessage => "own message",
            Self::UnmanagedBot => "unmanaged bot",
            Self::ManagerBot => "manager bot",
            Self::NotThisAgentsDm => "another bot's dm",
            Self::NotMentionedByAgent => "managed bot did not mention the agent",
            Self::UnattributedManagedBot => "unattributed managed bot",
            Self::NotAddressed => "not addressed",
            Self::Outside => "requester from outside the workspace",
        }
    }
}

impl fmt::Display for IgnoreReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why the router refused an addressed event. The pipeline renders each as a
/// one-line notice.
///
/// [`Banned`](Self::Banned) and [`Denied`](Self::Denied) are about the
/// requester, so the pipeline tells them privately; see
/// [`is_personal`](Self::is_personal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefuseReason {
    /// The owner paused the agent.
    Paused,
    /// A community admin banned the requester.
    Banned,
    /// The agent's allow and deny rules don't let the requester use it in
    /// this conversation.
    Denied,
    /// The turn would be past the agent's hop cap.
    HopCap {
        /// The largest hop the agent accepts.
        max: Hop,
    },
    /// The agent has taken the turns its owner allows it a day.
    DailyCap {
        /// The turns it takes a day for anyone but its owner.
        max: u32,
    },
    /// Agents have taken the most turns a thread allows in an hour.
    ThreadTurns {
        /// The turns agents may take in a thread in an hour.
        max: u32,
    },
    /// Agents' turns have used the thread's token budget for the day.
    ThreadTokens {
        /// The tokens agents' turns may use in a thread in a day.
        max: u64,
    },
    /// The view couldn't say which member the requester is, whether they
    /// are banned, or what the agent's rules are, so the router refuses
    /// rather than assume the requester is allowed or a stranger.
    PolicyUnavailable,
}

impl RefuseReason {
    /// Whether the refusal is about who the requester is, not about the
    /// agent, the chain or the thread, so only the requester is told.
    pub const fn is_personal(self) -> bool {
        matches!(self, Self::Banned | Self::Denied)
    }

    /// A short, stable description for logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Paused => "agent paused",
            Self::Banned => "requester banned",
            Self::Denied => "denied by agent policy",
            Self::HopCap { .. } => "hop cap reached",
            Self::DailyCap { .. } => "daily turn cap reached",
            Self::ThreadTurns { .. } => "thread turn cap reached",
            Self::ThreadTokens { .. } => "thread token budget used up",
            Self::PolicyUnavailable => "agent policy unavailable",
        }
    }
}

impl fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

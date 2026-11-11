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
    /// Don't run, and tell the thread why in one line.
    Refuse(RefuseReason),
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
    /// The view couldn't say which member the requester is, whether they
    /// are banned, or what the agent's rules are, so the router refuses
    /// rather than assume the requester is allowed or a stranger.
    PolicyUnavailable,
}

impl RefuseReason {
    /// A short, stable description for logs.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Paused => "agent paused",
            Self::Banned => "requester banned",
            Self::Denied => "denied by agent policy",
            Self::HopCap { .. } => "hop cap reached",
            Self::PolicyUnavailable => "agent policy unavailable",
        }
    }
}

impl fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

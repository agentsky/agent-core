//! [`RouterView`]: everything the router reads about the world, and the
//! types its answers use.

use core_types::{AgentId, BindingId, ConvRef, Hop, MemberId, MemberKey, MsgRef, Requester};

/// The largest hop [`AgentPolicy::default`] accepts. T27 replaces it with the
/// configured global cap, which a per-agent `hops` limit can only lower.
pub const DEFAULT_MAX_HOPS: Hop = Hop(3);

/// A read-only view of what the router needs to know. agentd implements it
/// over the store (T23), tests over plain maps.
///
/// Every method is a synchronous lookup, so the router stays pure. The
/// pipeline loads whatever the view needs before calling
/// [`route`](crate::route), and retries a missing [`message_ref`] briefly
/// itself (T34).
///
/// [`message_ref`]: RouterView::message_ref
pub trait RouterView {
    /// The managed agent whose bot user is `bot`, or `None`.
    ///
    /// The key is the full `(surface, team, user)`, so a matching user id
    /// from another team or server is never taken for a managed agent. It
    /// must answer for every bot user agentd created, whatever the agent's
    /// or binding's state, so that a paused or deleted agent's bot is never
    /// mistaken for a person.
    fn is_managed_bot(&self, bot: &MemberKey) -> Option<AgentId>;

    /// The agent whose binding `binding` is, or `None` for the manager bot's
    /// and unknown bindings.
    ///
    /// The router uses it to tell whose one-to-one DM an event is in: a DM
    /// has exactly one bot in it, and only that bot's binding receives it.
    fn binding_agent(&self, binding: BindingId) -> Option<AgentId>;

    /// How agentd attributed a message it posted as an agent, from
    /// `message_refs`, or `None` if it has no such record. Rows for inbound
    /// messages, which name no agent, are `None` too.
    fn message_ref(&self, msg: &MsgRef) -> Option<Attribution>;

    /// The member a surface identity belongs to, linked or not.
    fn member_for(&self, key: &MemberKey) -> Option<MemberId>;

    /// Whether `member` has a Claude account linked that turns can run on.
    fn is_linked(&self, member: MemberId) -> bool;

    /// Whether a community admin has set the community API key.
    fn community_key_configured(&self) -> bool;

    /// The member who owns `agent`, or `None` if there is no such agent.
    fn agent_owner(&self, agent: AgentId) -> Option<MemberId>;

    /// The lifecycle state of `agent`, or `None` if there is no such agent.
    fn agent_state(&self, agent: AgentId) -> Option<AgentState>;

    /// Whether `msg` is a message agentd posted as `agent`. For a thread
    /// reply the router asks about the thread root.
    fn is_reply_to_agent(&self, msg: &MsgRef, agent: AgentId) -> bool;

    /// The agent's allow and deny rules and hop cap.
    /// [`AgentPolicy::default`] allows everyone.
    fn policy(&self, agent: AgentId) -> AgentPolicy;

    /// Whether a community admin banned the requester: the member it names,
    /// or the member its key belongs to.
    fn is_banned(&self, requester: &Requester) -> bool;
}

/// An agent's lifecycle state, as `agents.state` holds it (T14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentState {
    /// Answering.
    Active,
    /// Paused by its owner: addressed messages are refused.
    Paused,
    /// Deleted: every event is ignored.
    Deleted,
}

/// Who a message agentd posted is billed to: its `message_refs` row.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Attribution {
    /// The agent agentd posted the message as.
    pub agent: AgentId,
    /// The requester of the turn that posted it.
    pub requester: Requester,
    /// That turn's hop.
    pub hop: Hop,
}

/// Who an allow or deny rule covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PolicyTarget {
    /// One member's identity on one surface.
    Member(MemberKey),
    /// Every requester in one conversation.
    Room(ConvRef),
    /// Every requester everywhere.
    Everyone,
}

impl PolicyTarget {
    fn covers(&self, requester: &MemberKey, conv: &ConvRef) -> bool {
        match self {
            Self::Member(member) => member == requester,
            Self::Room(room) => room == conv,
            Self::Everyone => true,
        }
    }
}

/// An agent's rules: who may use it, where, and how long an agent-to-agent
/// chain may get.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentPolicy {
    /// If not empty, only requesters these cover may use the agent.
    pub allow: Vec<PolicyTarget>,
    /// Requesters these cover may not use the agent, even if `allow` covers
    /// them too.
    pub deny: Vec<PolicyTarget>,
    /// The largest hop a turn may have. 0 turns agent-to-agent hand-off off.
    pub max_hops: Hop,
}

impl Default for AgentPolicy {
    /// Allows everyone, with [`DEFAULT_MAX_HOPS`].
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            deny: Vec::new(),
            max_hops: DEFAULT_MAX_HOPS,
        }
    }
}

impl AgentPolicy {
    /// Whether `requester` may use the agent in `conv`.
    ///
    /// Deny wins: a requester any deny rule covers is refused. Otherwise an
    /// empty allow list allows everyone, and a non-empty one allows only the
    /// requesters one of its rules covers, by member or by room.
    pub fn permits(&self, requester: &MemberKey, conv: &ConvRef) -> bool {
        let covered = |rules: &[PolicyTarget]| rules.iter().any(|r| r.covers(requester, conv));
        !covered(&self.deny) && (self.allow.is_empty() || covered(&self.allow))
    }
}

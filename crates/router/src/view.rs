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
/// store is asynchronous, so the pipeline loads whatever the view needs
/// before calling [`route`](crate::route), and retries a missing
/// [`message_ref`] briefly itself (T34).
///
/// # Lookups, in order
///
/// For one event and one candidate `agent`, [`route`](crate::route) makes
/// at most these lookups, in this order, and stops at the first answer that
/// decides. A view that preloads all of them for every candidate answers
/// every call.
///
/// 1. [`agent_owner`] and [`agent_state`] for `agent`.
/// 2. [`managed_bot`] for `event.sender`, unless the sender is flagged as a
///    bot with a `sender_bot_user` other than `sender.user`.
/// 3. For a one-to-one DM, [`binding_agent`] for `event.binding`.
/// 4. [`managed_bot`] for each of `event.mentions`, keyed by the
///    conversation's surface and team.
/// 5. For a person's message: [`is_reply_to_agent`] for `event.reply_to`
///    when it is in the same conversation, then [`member_for`] for
///    `event.sender`. For a managed agent's message: [`message_ref`] for
///    `event.message`, then [`member_for`] for the recorded requester's key
///    if no member was recorded.
/// 6. [`is_banned`] for the requester.
/// 7. [`policy`] for `agent`.
/// 8. [`is_linked`] for the owner, or for the requester's member, then
///    [`community_key_configured`].
///
/// # Missing answers
///
/// The lookups that grant or withhold permission fail closed. [`is_banned`]
/// and [`policy`] return `None` when the view doesn't have the answer, for
/// example because the pipeline didn't preload it, and the router then
/// refuses with [`RefuseReason::PolicyUnavailable`] instead of assuming the
/// requester is allowed. A missing answer anywhere else can only withhold a
/// turn: an unknown agent is ignored, an unknown mention or reply doesn't
/// address the agent, and an unknown link or community key gives a link
/// prompt.
///
/// [`agent_owner`]: RouterView::agent_owner
/// [`agent_state`]: RouterView::agent_state
/// [`managed_bot`]: RouterView::managed_bot
/// [`binding_agent`]: RouterView::binding_agent
/// [`is_reply_to_agent`]: RouterView::is_reply_to_agent
/// [`member_for`]: RouterView::member_for
/// [`message_ref`]: RouterView::message_ref
/// [`is_banned`]: RouterView::is_banned
/// [`policy`]: RouterView::policy
/// [`is_linked`]: RouterView::is_linked
/// [`community_key_configured`]: RouterView::community_key_configured
/// [`RefuseReason::PolicyUnavailable`]: crate::RefuseReason::PolicyUnavailable
pub trait RouterView {
    /// Which bot agentd manages as `bot`, or `None` for anyone else.
    ///
    /// The key is the full `(surface, team, user)`, so a matching user id
    /// from another team or server is never taken for a managed bot. It
    /// must answer for every bot user agentd created, the manager bot's
    /// included, whatever the agent's or binding's state, so that a paused
    /// or deleted agent's bot, or the manager bot, is never mistaken for a
    /// person.
    fn managed_bot(&self, bot: &MemberKey) -> Option<ManagedBot>;

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

    /// The agent's allow and deny rules and effective hop cap, or `None` if
    /// the view doesn't have them. An agent with no rules set has
    /// [`AgentPolicy::default`], which allows everyone; `None` refuses.
    fn policy(&self, agent: AgentId) -> Option<AgentPolicy>;

    /// Whether a community admin banned the requester: the member it names,
    /// or the member its key belongs to. `None` if the view doesn't know,
    /// which refuses.
    fn is_banned(&self, requester: &Requester) -> Option<bool>;
}

/// A bot user agentd manages: an agent's, or the manager bot's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ManagedBot {
    /// The bot user of this agent's binding.
    Agent(AgentId),
    /// The manager bot. Its posts are never routed, and a mention of it
    /// addresses no agent.
    Manager,
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
    /// One member, named by one of their surface identities.
    ///
    /// It covers a requester with that identity, and, when `member` is
    /// set, any requester with the same member, whichever surface they
    /// come from. So a rule written against someone's Slack identity also
    /// covers their linked Rocket.Chat identity.
    Member {
        /// The identity the rule named.
        key: MemberKey,
        /// The member that identity belonged to when the rule was set, if
        /// any.
        member: Option<MemberId>,
    },
    /// Every requester in one conversation.
    Room(ConvRef),
    /// Every requester everywhere.
    Everyone,
}

impl PolicyTarget {
    fn covers(&self, requester: &Requester, conv: &ConvRef) -> bool {
        match self {
            Self::Member { key, member } => {
                *key == requester.key || (member.is_some() && *member == requester.member)
            }
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
    /// requesters one of its rules covers, by member or by room. A member
    /// rule matches the requester's identity, or their member on any
    /// surface (see [`PolicyTarget::Member`]).
    pub fn permits(&self, requester: &Requester, conv: &ConvRef) -> bool {
        let covered = |rules: &[PolicyTarget]| rules.iter().any(|r| r.covers(requester, conv));
        !covered(&self.deny) && (self.allow.is_empty() || covered(&self.allow))
    }
}

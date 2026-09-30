//! [`RouterView`]: everything the router reads about the world, and the
//! types its answers use.

use core_types::{AgentId, BindingId, ConvRef, Hop, MemberId, MemberKey, MsgRef, Requester};

/// The largest hop [`AgentPolicy::default`] accepts, and the default of
/// agentd's global cap (`[limits] max_hops`), which a per-agent `hops` limit
/// can only lower.
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
/// 8. [`thread_budget`], unless the event is in a one-to-one DM.
/// 9. [`link_state`] for the requester's member (the owner's, when the
///    requester is the owner), then, unless it is linked or broken,
///    [`community_key_configured`], which is never read for the owner.
///
/// # Missing answers
///
/// The lookups that grant or withhold permission fail closed. [`is_banned`],
/// [`policy`] and [`thread_budget`] return `None` when the view doesn't have
/// the answer, for
/// example because the pipeline didn't preload it, and the router then
/// refuses with [`RefuseReason::PolicyUnavailable`] instead of assuming the
/// requester is allowed. A missing answer elsewhere withholds a turn: an
/// unknown agent is ignored, an unknown mention or reply doesn't address
/// the agent, and an unknown community key gives a link prompt.
///
/// [`link_state`] is the exception: it has no "unknown", and a view without
/// the answer reads as [`LinkState::Unlinked`], which runs a non-owner's
/// turn on the community key when one is configured, even if their link
/// broke. So a view must answer it for every member the router can ask
/// about: the owner, the sender's member, and an attributed requester's
/// member. agentd's `StoreView::load` preloads all of them.
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
/// [`thread_budget`]: RouterView::thread_budget
/// [`link_state`]: RouterView::link_state
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

    /// Whether `member` has a Claude account linked, and whether turns can
    /// run on it.
    fn link_state(&self, member: MemberId) -> LinkState;

    /// Whether a community admin has set the community API key.
    fn community_key_configured(&self) -> bool;

    /// The member who owns `agent`, or `None` if there is no such agent.
    fn agent_owner(&self, agent: AgentId) -> Option<MemberId>;

    /// The lifecycle state of `agent`, or `None` if there is no such agent.
    fn agent_state(&self, agent: AgentId) -> Option<AgentState>;

    /// Whether `msg` is a message agentd posted as `agent`. For a thread
    /// reply the router asks about the thread root.
    fn is_reply_to_agent(&self, msg: &MsgRef, agent: AgentId) -> bool;

    /// The agent's allow and deny rules, effective hop cap and daily turn
    /// cap with the turns it took today, or `None` if the view doesn't have
    /// them. An agent with no rules or limits set has
    /// [`AgentPolicy::default`] with the global hop cap, which allows
    /// everyone; `None` refuses.
    fn policy(&self, agent: AgentId) -> Option<AgentPolicy>;

    /// Whether a community admin banned the requester: the member it names,
    /// or the member its key belongs to. `None` if the view doesn't know,
    /// which refuses.
    fn is_banned(&self, requester: &Requester) -> Option<bool>;

    /// What agents have spent in the thread the event is in, every agent's
    /// turns counted, and the community's caps on it, or `None` if the view
    /// doesn't know, which refuses. The router asks only outside one-to-one
    /// DMs, where no other agent can answer.
    fn thread_budget(&self) -> Option<ThreadBudget>;
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

/// Whether a member has a Claude account linked, as `claude_links` holds
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinkState {
    /// No account is linked.
    Unlinked,
    /// An account is linked and turns can run on it.
    Linked,
    /// An account is linked, but Anthropic refused to renew it
    /// (`claude_links.broken_at` is set). Turns can't run on it, and the
    /// member is asked to link again rather than moved to the community
    /// key.
    Broken,
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

/// An agent's rules: who may use it, where, how long an agent-to-agent
/// chain may get, and how many turns it takes a day.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentPolicy {
    /// If not empty, only requesters these cover may use the agent.
    pub allow: Vec<PolicyTarget>,
    /// Requesters these cover may not use the agent, even if `allow` covers
    /// them too.
    pub deny: Vec<PolicyTarget>,
    /// The largest hop a turn may have: the global cap, lowered by the
    /// agent's own. 0 turns agent-to-agent hand-off off.
    pub max_hops: Hop,
    /// The most turns the agent takes a day (UTC) for anyone but its owner,
    /// or `None` for no cap. 0 leaves it to its owner alone.
    pub turns_per_day: Option<u32>,
    /// The turns the agent has taken today (UTC), for anyone, its owner
    /// included.
    pub turns_today: u32,
}

impl Default for AgentPolicy {
    /// Allows everyone, with [`DEFAULT_MAX_HOPS`] and no daily cap.
    fn default() -> Self {
        Self {
            allow: Vec::new(),
            deny: Vec::new(),
            max_hops: DEFAULT_MAX_HOPS,
            turns_per_day: None,
            turns_today: 0,
        }
    }
}

impl AgentPolicy {
    /// Whether the agent has taken its daily turns: false without a cap.
    pub fn daily_cap_reached(&self) -> bool {
        self.turns_per_day
            .is_some_and(|cap| self.turns_today >= cap)
    }

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

/// What agents have spent in one thread, and the caps on it, which stop
/// agents that keep answering each other.
///
/// A conversation without threads, such as a group DM on a surface without
/// them, is one thread.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ThreadBudget {
    /// Turns agents took in the thread this hour (UTC), every agent and
    /// requester counted.
    pub turns_this_hour: u32,
    /// The most turns agents may take in a thread in an hour, or `None`
    /// for no cap.
    pub max_turns_per_hour: Option<u32>,
    /// Tokens agents' turns used in the thread today (UTC): input and
    /// output, cache reads left out.
    pub tokens_today: u64,
    /// The most tokens agents' turns may use in a thread in a day, or
    /// `None` for no budget.
    pub max_tokens_per_day: Option<u64>,
}

impl ThreadBudget {
    /// The refusal the thread's caps give a new turn, if any: the turns
    /// cap first, then the token budget.
    pub fn exceeded(&self) -> Option<crate::RefuseReason> {
        if let Some(max) = self.max_turns_per_hour
            && self.turns_this_hour >= max
        {
            return Some(crate::RefuseReason::ThreadTurns { max });
        }
        if let Some(max) = self.max_tokens_per_day
            && self.tokens_today >= max
        {
            return Some(crate::RefuseReason::ThreadTokens { max });
        }
        None
    }
}

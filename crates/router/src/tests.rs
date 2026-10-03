use std::collections::{HashMap, HashSet};

use core_types::{BindingId, ConvKind, ConvRef, MemberId, MessageId, MsgRef, SurfaceKind};
use time::macros::datetime;

use super::*;

const TEAM: &str = "T1";

fn key(user: &str) -> MemberKey {
    MemberKey {
        surface: SurfaceKind::Slack,
        team: TEAM.into(),
        user: user.into(),
    }
}

fn conv(id: &str) -> ConvRef {
    ConvRef {
        surface: SurfaceKind::Slack,
        team: TEAM.into(),
        conversation: id.into(),
    }
}

#[derive(Default)]
struct FakeView {
    bots: HashMap<MemberKey, ManagedBot>,
    bindings: HashMap<BindingId, AgentId>,
    refs: HashMap<MsgRef, Attribution>,
    members: HashMap<MemberKey, MemberId>,
    linked: HashSet<MemberId>,
    community_key: bool,
    owners: HashMap<AgentId, MemberId>,
    states: HashMap<AgentId, AgentState>,
    agent_posts: HashSet<(MsgRef, AgentId)>,
    policies: HashMap<AgentId, AgentPolicy>,
    banned_members: HashSet<MemberId>,
    banned_keys: HashSet<MemberKey>,
    bans_unavailable: bool,
}

impl RouterView for FakeView {
    fn managed_bot(&self, bot: &MemberKey) -> Option<ManagedBot> {
        self.bots.get(bot).copied()
    }

    fn binding_agent(&self, binding: BindingId) -> Option<AgentId> {
        self.bindings.get(&binding).copied()
    }

    fn message_ref(&self, msg: &MsgRef) -> Option<Attribution> {
        self.refs.get(msg).cloned()
    }

    fn member_for(&self, key: &MemberKey) -> Option<MemberId> {
        self.members.get(key).copied()
    }

    fn is_linked(&self, member: MemberId) -> bool {
        self.linked.contains(&member)
    }

    fn community_key_configured(&self) -> bool {
        self.community_key
    }

    fn agent_owner(&self, agent: AgentId) -> Option<MemberId> {
        self.owners.get(&agent).copied()
    }

    fn agent_state(&self, agent: AgentId) -> Option<AgentState> {
        self.states.get(&agent).copied()
    }

    fn is_reply_to_agent(&self, msg: &MsgRef, agent: AgentId) -> bool {
        self.agent_posts.contains(&(msg.clone(), agent))
    }

    fn policy(&self, agent: AgentId) -> Option<AgentPolicy> {
        self.policies.get(&agent).cloned()
    }

    fn is_banned(&self, requester: &Requester) -> Option<bool> {
        let banned = requester
            .member
            .is_some_and(|member| self.banned_members.contains(&member))
            || self.banned_keys.contains(&requester.key);
        (!self.bans_unavailable).then_some(banned)
    }
}

/// Agent A, owned by `owner`, and agent B, owned by someone else, both in
/// channel C1. `linked` has a linked account, `known` is a member with none,
/// and `stranger` is not a member at all.
struct World {
    view: FakeView,
    a: AgentId,
    a_bot: MemberKey,
    a_binding: BindingId,
    b: AgentId,
    b_bot: MemberKey,
    b_binding: BindingId,
    manager_binding: BindingId,
    owner: MemberId,
    owner_key: MemberKey,
    linked: MemberId,
    linked_key: MemberKey,
    known: MemberId,
    known_key: MemberKey,
    stranger_key: MemberKey,
}

impl World {
    fn new() -> Self {
        let mut view = FakeView::default();
        let (a, b) = (AgentId::new_v4(), AgentId::new_v4());
        let (a_bot, b_bot) = (key("UBOTA"), key("UBOTB"));
        let (a_binding, b_binding) = (BindingId::new_v4(), BindingId::new_v4());
        let (owner, b_owner, linked, known) = (
            MemberId::new_v4(),
            MemberId::new_v4(),
            MemberId::new_v4(),
            MemberId::new_v4(),
        );
        let (owner_key, linked_key, known_key) = (key("UOWNER"), key("ULINKED"), key("UKNOWN"));

        view.bots.insert(a_bot.clone(), ManagedBot::Agent(a));
        view.bots.insert(b_bot.clone(), ManagedBot::Agent(b));
        view.bots.insert(key("UMANAGER"), ManagedBot::Manager);
        view.policies.insert(a, AgentPolicy::default());
        view.policies.insert(b, AgentPolicy::default());
        view.bindings.insert(a_binding, a);
        view.bindings.insert(b_binding, b);
        view.owners.insert(a, owner);
        view.owners.insert(b, b_owner);
        view.states.insert(a, AgentState::Active);
        view.states.insert(b, AgentState::Active);
        view.members.insert(owner_key.clone(), owner);
        view.members.insert(linked_key.clone(), linked);
        view.members.insert(known_key.clone(), known);
        view.linked.extend([owner, b_owner, linked]);

        Self {
            view,
            a,
            a_bot,
            a_binding,
            b,
            b_bot,
            b_binding,
            manager_binding: BindingId::new_v4(),
            owner,
            owner_key,
            linked,
            linked_key,
            known,
            known_key,
            stranger_key: key("USTRANGER"),
        }
    }

    /// A top-level message in channel C1 from a person, received by A's
    /// binding, mentioning nobody.
    fn message(&self, sender: &MemberKey) -> InboundEvent {
        let conv = conv("C1");
        InboundEvent {
            event_id: "Ev1".into(),
            binding: self.a_binding,
            sender: sender.clone(),
            sender_is_bot: false,
            sender_bot_user: None,
            conv: conv.clone(),
            conv_kind: ConvKind::Channel,
            thread_root: None,
            message: MsgRef {
                conv,
                id: "100.1".into(),
            },
            text: "hello".into(),
            mentions: vec![],
            reply_to: None,
            files: vec![],
            received_at: datetime!(2026-09-30 12:00 UTC),
        }
    }

    /// A message from `sender` mentioning agent A.
    fn mention(&self, sender: &MemberKey) -> InboundEvent {
        let mut event = self.message(sender);
        event.mentions.push(self.a_bot.user.clone());
        event.text = "<@UBOTA> do the thing".into();
        event
    }

    /// A one-to-one DM from `sender` to agent A, mentioning nobody.
    fn dm(&self, sender: &MemberKey) -> InboundEvent {
        let mut event = self.message(sender);
        let conv = conv("D1");
        event.conv = conv.clone();
        event.conv_kind = ConvKind::Dm;
        event.message = MsgRef {
            conv,
            id: "200.1".into(),
        };
        event
    }

    /// A message agent B posted mentioning A, attributed to a turn of
    /// `requester` at `hop`.
    fn b_mentions_a(&mut self, requester: Requester, hop: Hop) -> InboundEvent {
        let mut event = self.mention(&self.b_bot.clone());
        event.sender_is_bot = true;
        event.sender_bot_user = Some(self.b_bot.user.clone());
        event.binding = self.b_binding;
        self.view.refs.insert(
            event.message.clone(),
            Attribution {
                agent: self.b,
                requester,
                hop,
            },
        );
        event
    }

    fn requester(&self, key: &MemberKey) -> Requester {
        Requester {
            member: self.view.member_for(key),
            key: key.clone(),
        }
    }

    fn route(&self, event: &InboundEvent) -> Decision {
        route(event, self.a, &self.view)
    }

    /// A rule naming `key`, and the member it belongs to, if any.
    fn member_target(&self, key: &MemberKey) -> PolicyTarget {
        PolicyTarget::Member {
            key: key.clone(),
            member: self.view.member_for(key),
        }
    }

    fn set_policy(&mut self, policy: AgentPolicy) {
        self.view.policies.insert(self.a, policy);
    }
}

fn run(requester: Requester, hop: Hop, credential: CredentialRef, scope: ScopeKind) -> Decision {
    let side = if scope == ScopeKind::Private {
        Side::Owner
    } else {
        Side::Public
    };
    Decision::Run {
        requester,
        hop,
        credential,
        scope,
        side,
    }
}

fn ignored(reason: IgnoreReason) -> Decision {
    Decision::Ignore(reason)
}

fn refused(reason: RefuseReason) -> Decision {
    Decision::Refuse(reason)
}

#[test]
fn unmanaged_bot_is_ignored() {
    let w = World::new();
    let bot = key("UOTHERBOT");
    let mut event = w.mention(&bot);
    event.sender_is_bot = true;
    event.sender_bot_user = Some(bot.user.clone());
    assert_eq!(w.route(&event), ignored(IgnoreReason::UnmanagedBot));
}

#[test]
fn bot_known_only_by_bot_id_is_ignored_as_unmanaged() {
    let mut w = World::new();
    let bot_id = key("B0LEGACY");
    let mut event = w.mention(&bot_id);
    event.sender_is_bot = true;
    event.sender_bot_user = None;
    assert_eq!(w.route(&event), ignored(IgnoreReason::UnmanagedBot));

    w.view.bots.insert(bot_id, ManagedBot::Agent(w.b));
    assert_eq!(
        w.route(&event),
        ignored(IgnoreReason::UnmanagedBot),
        "a bot-id-only sender is never looked up, even if a binding matched it"
    );
}

#[test]
fn bot_whose_bot_user_disagrees_with_sender_is_ignored_as_unmanaged() {
    let mut w = World::new();
    let requester = w.requester(&w.linked_key.clone());
    let mut event = w.b_mentions_a(requester, Hop::ZERO);
    event.sender_bot_user = Some("USOMEONEELSE".into());
    assert_eq!(w.route(&event), ignored(IgnoreReason::UnmanagedBot));

    event.sender_is_bot = false;
    assert_eq!(
        w.route(&event),
        ignored(IgnoreReason::UnmanagedBot),
        "a bot user id alone marks the sender as a bot"
    );
}

#[test]
fn managed_bot_that_does_not_mention_agent_is_ignored() {
    let mut w = World::new();
    let requester = w.requester(&w.linked_key.clone());
    let mut event = w.b_mentions_a(requester, Hop::ZERO);
    event.mentions = vec![w.b_bot.user.clone(), "ULINKED".into()];
    assert_eq!(w.route(&event), ignored(IgnoreReason::NotMentionedByAgent));
}

#[test]
fn managed_bot_replying_in_agents_thread_without_mention_is_ignored() {
    let mut w = World::new();
    let requester = w.requester(&w.linked_key.clone());
    let mut event = w.b_mentions_a(requester, Hop::ZERO);
    event.mentions.clear();
    let root = MsgRef {
        conv: conv("C1"),
        id: "99.9".into(),
    };
    w.view.agent_posts.insert((root.clone(), w.a));
    event.thread_root = Some(root.id.clone());
    event.reply_to = Some(root);
    assert_eq!(w.route(&event), ignored(IgnoreReason::NotMentionedByAgent));
}

#[test]
fn managed_bot_mentioning_agent_inherits_requester_and_next_hop() {
    let mut w = World::new();
    let requester = w.requester(&w.linked_key.clone());
    let event = w.b_mentions_a(requester.clone(), Hop(1));
    assert_eq!(
        w.route(&event),
        run(
            requester,
            Hop(2),
            CredentialRef::Member(w.linked),
            ScopeKind::Channel
        )
    );
}

#[test]
fn hop_bills_the_inherited_requester_never_the_thread_starter_or_posting_agents_owner() {
    let mut w = World::new();
    let requester = w.requester(&w.stranger_key.clone());
    let mut event = w.b_mentions_a(requester.clone(), Hop::ZERO);
    let root = MsgRef {
        conv: conv("C1"),
        id: "1.0".into(),
    };
    event.thread_root = Some(root.id.clone());
    event.reply_to = Some(root);
    assert_eq!(
        w.route(&event),
        Decision::LinkPrompt { requester },
        "an unlinked requester gets a link prompt, not B's owner's credential"
    );

    w.view.community_key = true;
    assert!(matches!(
        w.route(&event),
        Decision::Run {
            credential: CredentialRef::Community,
            ..
        }
    ));
}

#[test]
fn hop_requester_member_is_resolved_from_key_when_not_recorded() {
    let mut w = World::new();
    let recorded = Requester {
        member: None,
        key: w.linked_key.clone(),
    };
    let event = w.b_mentions_a(recorded, Hop::ZERO);
    assert_eq!(
        w.route(&event),
        run(
            w.requester(&w.linked_key),
            Hop(1),
            CredentialRef::Member(w.linked),
            ScopeKind::Channel
        )
    );
}

#[test]
fn managed_bot_mention_without_message_ref_is_ignored_as_unattributed() {
    let mut w = World::new();
    let requester = w.requester(&w.linked_key.clone());
    let event = w.b_mentions_a(requester, Hop::ZERO);
    w.view.refs.clear();
    let decision = w.route(&event);
    assert_eq!(decision, ignored(IgnoreReason::UnattributedManagedBot));
    let Decision::Ignore(reason) = decision else {
        unreachable!()
    };
    assert_eq!(reason.to_string(), "unattributed managed bot");
}

#[test]
fn message_ref_recorded_for_another_agent_is_unattributed() {
    let mut w = World::new();
    let requester = w.requester(&w.linked_key.clone());
    let event = w.b_mentions_a(requester, Hop::ZERO);
    let attribution = w.view.refs.get_mut(&event.message).unwrap();
    attribution.agent = AgentId::new_v4();
    assert_eq!(
        w.route(&event),
        ignored(IgnoreReason::UnattributedManagedBot)
    );
}

#[test]
fn agents_own_message_is_ignored_even_when_it_mentions_itself() {
    let mut w = World::new();
    let mut event = w.mention(&w.a_bot);
    event.sender_is_bot = true;
    event.sender_bot_user = Some(w.a_bot.user.clone());
    w.view.refs.insert(
        event.message.clone(),
        Attribution {
            agent: w.a,
            requester: w.requester(&w.owner_key),
            hop: Hop::ZERO,
        },
    );
    assert_eq!(w.route(&event), ignored(IgnoreReason::OwnMessage));

    event.conv_kind = ConvKind::Dm;
    assert_eq!(w.route(&event), ignored(IgnoreReason::OwnMessage));
}

#[test]
fn managed_bot_flagged_as_person_is_still_treated_as_the_agent() {
    let mut w = World::new();
    let mut own = w.mention(&w.a_bot);
    own.sender_is_bot = false;
    assert_eq!(w.route(&own), ignored(IgnoreReason::OwnMessage));

    let requester = w.requester(&w.linked_key.clone());
    let mut other = w.b_mentions_a(requester, Hop::ZERO);
    other.sender_is_bot = false;
    other.sender_bot_user = None;
    w.view.refs.clear();
    assert_eq!(
        w.route(&other),
        ignored(IgnoreReason::UnattributedManagedBot),
        "an agent's post never runs as a person's, whatever the surface flagged"
    );
}

#[test]
fn manager_bot_post_is_ignored_even_when_not_flagged_as_a_bot() {
    let mut w = World::new();
    w.view.community_key = true;
    let manager = key("UMANAGER");
    let mut event = w.mention(&manager);
    assert_eq!(w.route(&event), ignored(IgnoreReason::ManagerBot));

    event.sender_is_bot = true;
    event.sender_bot_user = Some(manager.user.clone());
    assert_eq!(w.route(&event), ignored(IgnoreReason::ManagerBot));

    event.conv_kind = ConvKind::Dm;
    event.binding = w.a_binding;
    assert_eq!(w.route(&event), ignored(IgnoreReason::ManagerBot));
}

#[test]
fn mention_of_the_manager_bot_addresses_no_agent() {
    let w = World::new();
    let mut event = w.message(&w.linked_key);
    event.mentions.push("UMANAGER".into());
    assert_eq!(w.route(&event), ignored(IgnoreReason::NotAddressed));
}

#[test]
fn person_not_addressed_is_ignored() {
    let w = World::new();
    let mut event = w.message(&w.owner_key);
    assert_eq!(w.route(&event), ignored(IgnoreReason::NotAddressed));

    event.mentions = vec![w.b_bot.user.clone(), "ULINKED".into()];
    assert_eq!(w.route(&event), ignored(IgnoreReason::NotAddressed));

    event.conv_kind = ConvKind::GroupDm;
    assert_eq!(w.route(&event), ignored(IgnoreReason::NotAddressed));
}

#[test]
fn mention_of_same_user_id_in_another_team_does_not_count() {
    let w = World::new();
    let mut event = w.mention(&w.linked_key);
    event.conv.team = "T2".into();
    event.sender.team = "T2".into();
    assert_eq!(w.route(&event), ignored(IgnoreReason::NotAddressed));

    let mut event = w.mention(&w.linked_key);
    event.conv.surface = SurfaceKind::RocketChat;
    event.sender.surface = SurfaceKind::RocketChat;
    assert_eq!(w.route(&event), ignored(IgnoreReason::NotAddressed));
}

#[test]
fn owner_dm_runs_on_owner_credential_in_private_scope_on_owner_side() {
    let w = World::new();
    assert_eq!(
        w.route(&w.dm(&w.owner_key)),
        Decision::Run {
            requester: w.requester(&w.owner_key),
            hop: Hop::ZERO,
            credential: CredentialRef::Member(w.owner),
            scope: ScopeKind::Private,
            side: Side::Owner,
        }
    );
}

#[test]
fn owner_in_channel_runs_on_owner_credential_on_public_side() {
    let w = World::new();
    assert_eq!(
        w.route(&w.mention(&w.owner_key)),
        Decision::Run {
            requester: w.requester(&w.owner_key),
            hop: Hop::ZERO,
            credential: CredentialRef::Member(w.owner),
            scope: ScopeKind::Channel,
            side: Side::Public,
        }
    );

    let mut event = w.mention(&w.owner_key);
    event.conv_kind = ConvKind::GroupDm;
    assert_eq!(
        w.route(&event),
        run(
            w.requester(&w.owner_key),
            Hop::ZERO,
            CredentialRef::Member(w.owner),
            ScopeKind::GroupDm
        )
    );
}

#[test]
fn linked_non_owner_runs_on_requester_credential_in_channel_scope() {
    let w = World::new();
    assert_eq!(
        w.route(&w.mention(&w.linked_key)),
        Decision::Run {
            requester: w.requester(&w.linked_key),
            hop: Hop::ZERO,
            credential: CredentialRef::Member(w.linked),
            scope: ScopeKind::Channel,
            side: Side::Public,
        }
    );
}

#[test]
fn non_owner_dm_runs_in_dm_scope_on_public_side() {
    let w = World::new();
    assert_eq!(
        w.route(&w.dm(&w.linked_key)),
        Decision::Run {
            requester: w.requester(&w.linked_key),
            hop: Hop::ZERO,
            credential: CredentialRef::Member(w.linked),
            scope: ScopeKind::Dm,
            side: Side::Public,
        }
    );
}

#[test]
fn unlinked_with_community_key_runs_on_community_credential() {
    let mut w = World::new();
    w.view.community_key = true;
    for sender in [w.known_key.clone(), w.stranger_key.clone()] {
        assert_eq!(
            w.route(&w.mention(&sender)),
            run(
                w.requester(&sender),
                Hop::ZERO,
                CredentialRef::Community,
                ScopeKind::Channel
            )
        );
    }
    assert_eq!(
        w.route(&w.dm(&w.known_key)),
        run(
            w.requester(&w.known_key),
            Hop::ZERO,
            CredentialRef::Community,
            ScopeKind::Dm
        )
    );
}

#[test]
fn unlinked_without_community_key_gets_link_prompt() {
    let w = World::new();
    for sender in [w.known_key.clone(), w.stranger_key.clone()] {
        assert_eq!(
            w.route(&w.mention(&sender)),
            Decision::LinkPrompt {
                requester: w.requester(&sender)
            }
        );
    }
    assert_eq!(
        w.route(&w.dm(&w.known_key)),
        Decision::LinkPrompt {
            requester: Requester {
                member: Some(w.known),
                key: w.known_key.clone(),
            }
        }
    );
}

#[test]
fn unlinked_owner_gets_link_prompt_never_the_community_key() {
    let mut w = World::new();
    w.view.linked.remove(&w.owner);
    w.view.community_key = true;
    for event in [w.dm(&w.owner_key), w.mention(&w.owner_key)] {
        assert_eq!(
            w.route(&event),
            Decision::LinkPrompt {
                requester: w.requester(&w.owner_key)
            }
        );
    }
}

#[test]
fn thread_reply_not_addressed_to_agent_is_ignored() {
    let w = World::new();
    let mut event = w.message(&w.linked_key);
    let root = MsgRef {
        conv: conv("C1"),
        id: "50.0".into(),
    };
    event.thread_root = Some(root.id.clone());
    event.reply_to = Some(root);
    assert_eq!(w.route(&event), ignored(IgnoreReason::NotAddressed));
}

#[test]
fn thread_reply_to_agents_message_is_addressed() {
    let mut w = World::new();
    let mut event = w.message(&w.linked_key);
    let root = MsgRef {
        conv: conv("C1"),
        id: "50.0".into(),
    };
    event.thread_root = Some(root.id.clone());
    event.reply_to = Some(root.clone());
    w.view.agent_posts.insert((root.clone(), w.b));
    assert_eq!(
        w.route(&event),
        ignored(IgnoreReason::NotAddressed),
        "a thread B started doesn't address A"
    );

    w.view.agent_posts.insert((root, w.a));
    assert_eq!(
        w.route(&event),
        run(
            w.requester(&w.linked_key),
            Hop::ZERO,
            CredentialRef::Member(w.linked),
            ScopeKind::Channel
        )
    );
}

#[test]
fn reply_to_agents_message_in_another_conversation_does_not_count() {
    let mut w = World::new();
    let mut event = w.message(&w.linked_key);
    let elsewhere = MsgRef {
        conv: conv("C2"),
        id: "50.0".into(),
    };
    w.view.agent_posts.insert((elsewhere.clone(), w.a));
    event.reply_to = Some(elsewhere);
    assert_eq!(w.route(&event), ignored(IgnoreReason::NotAddressed));
}

/// A reply in a thread A started, from the linked member.
fn reply_in_as_thread(w: &mut World) -> InboundEvent {
    let mut event = w.message(&w.linked_key);
    let root = MsgRef {
        conv: conv("C1"),
        id: "50.0".into(),
    };
    w.view.agent_posts.insert((root.clone(), w.a));
    event.thread_root = Some(root.id.clone());
    event.reply_to = Some(root);
    event
}

#[test]
fn reply_in_agents_thread_mentioning_only_another_agent_runs_that_agent_alone() {
    let mut w = World::new();
    let mut event = reply_in_as_thread(&mut w);
    event.mentions.push(w.b_bot.user.clone());
    assert_eq!(
        w.route(&event),
        ignored(IgnoreReason::NotAddressed),
        "the reply is addressed to B, so A doesn't bill the person a second turn"
    );
    assert_eq!(
        route(&event, w.b, &w.view),
        run(
            w.requester(&w.linked_key),
            Hop::ZERO,
            CredentialRef::Member(w.linked),
            ScopeKind::Channel
        )
    );
}

#[test]
fn reply_in_agents_thread_mentioning_nobody_the_agent_or_no_agent_still_counts() {
    let mut w = World::new();
    let base = reply_in_as_thread(&mut w);
    let expected = run(
        w.requester(&w.linked_key),
        Hop::ZERO,
        CredentialRef::Member(w.linked),
        ScopeKind::Channel,
    );
    let mentioning = |users: &[&str]| {
        let mut event = base.clone();
        event.mentions = users.iter().map(|user| (*user).into()).collect();
        event
    };
    for users in [
        &[][..],
        &["UBOTA"],
        &["UBOTA", "UBOTB"],
        &["UKNOWN"],
        &["UMANAGER"],
    ] {
        assert_eq!(w.route(&mentioning(users)), expected, "mentions {users:?}");
    }
    assert!(
        matches!(
            route(&mentioning(&["UBOTA", "UBOTB"]), w.b, &w.view),
            Decision::Run { .. }
        ),
        "naming both agents runs both"
    );
}

#[test]
fn dm_through_another_bots_binding_is_ignored_even_with_a_mention() {
    let w = World::new();
    for binding in [w.b_binding, w.manager_binding] {
        for mut event in [w.dm(&w.owner_key), w.dm(&w.linked_key)] {
            event.binding = binding;
            event.mentions.push(w.a_bot.user.clone());
            assert_eq!(w.route(&event), ignored(IgnoreReason::NotThisAgentsDm));
        }
    }
}

#[test]
fn group_dm_is_not_a_dm_and_needs_a_mention() {
    let w = World::new();
    let mut event = w.message(&w.owner_key);
    event.conv_kind = ConvKind::GroupDm;
    event.binding = w.manager_binding;
    assert_eq!(w.route(&event), ignored(IgnoreReason::NotAddressed));
    event.mentions.push(w.a_bot.user.clone());
    assert!(matches!(
        w.route(&event),
        Decision::Run {
            scope: ScopeKind::GroupDm,
            side: Side::Public,
            ..
        }
    ));
}

#[test]
fn hop_requested_by_owner_runs_on_owner_credential_on_public_side() {
    let mut w = World::new();
    let requester = w.requester(&w.owner_key.clone());
    let event = w.b_mentions_a(requester.clone(), Hop::ZERO);
    assert_eq!(
        w.route(&event),
        run(
            requester,
            Hop(1),
            CredentialRef::Member(w.owner),
            ScopeKind::Channel
        )
    );
}

#[test]
fn hop_in_a_dm_never_runs_on_the_private_side() {
    let mut w = World::new();
    let requester = w.requester(&w.owner_key.clone());
    let mut event = w.b_mentions_a(requester.clone(), Hop::ZERO);
    let attribution = w.view.refs.remove(&event.message).unwrap();
    event.conv = conv("D9");
    event.conv_kind = ConvKind::Dm;
    event.message.conv = event.conv.clone();
    event.binding = w.a_binding;
    w.view.refs.insert(event.message.clone(), attribution);
    assert_eq!(
        w.route(&event),
        run(
            requester,
            Hop(1),
            CredentialRef::Member(w.owner),
            ScopeKind::Dm
        )
    );
}

#[test]
fn over_hop_cap_is_refused() {
    let mut w = World::new();
    let requester = w.requester(&w.linked_key.clone());
    let at_cap = w.b_mentions_a(requester.clone(), Hop(DEFAULT_MAX_HOPS.0 - 1));
    assert!(matches!(w.route(&at_cap), Decision::Run { hop, .. } if hop == DEFAULT_MAX_HOPS));

    let over = w.b_mentions_a(requester.clone(), DEFAULT_MAX_HOPS);
    assert_eq!(
        w.route(&over),
        refused(RefuseReason::HopCap {
            max: DEFAULT_MAX_HOPS
        })
    );
}

#[test]
fn per_agent_hop_cap_applies_and_zero_turns_hand_off_off() {
    let mut w = World::new();
    let requester = w.requester(&w.linked_key.clone());
    let event = w.b_mentions_a(requester, Hop::ZERO);
    w.set_policy(AgentPolicy {
        max_hops: Hop::ZERO,
        ..AgentPolicy::default()
    });
    assert_eq!(
        w.route(&event),
        refused(RefuseReason::HopCap { max: Hop::ZERO })
    );
    assert!(
        matches!(w.route(&w.mention(&w.linked_key)), Decision::Run { .. }),
        "a person's turn is hop 0 and passes any cap"
    );
}

#[test]
fn hop_counter_overflow_is_refused_as_hop_cap() {
    let mut w = World::new();
    let requester = w.requester(&w.linked_key.clone());
    let event = w.b_mentions_a(requester, Hop(u8::MAX));
    w.set_policy(AgentPolicy {
        max_hops: Hop(u8::MAX),
        ..AgentPolicy::default()
    });
    assert_eq!(
        w.route(&event),
        refused(RefuseReason::HopCap { max: Hop(u8::MAX) })
    );
}

#[test]
fn paused_agent_is_refused() {
    let mut w = World::new();
    w.view.states.insert(w.a, AgentState::Paused);
    for event in [
        w.mention(&w.linked_key),
        w.dm(&w.owner_key),
        w.mention(&w.stranger_key),
    ] {
        assert_eq!(w.route(&event), refused(RefuseReason::Paused));
    }
    let requester = w.requester(&w.linked_key.clone());
    let hop = w.b_mentions_a(requester, Hop::ZERO);
    assert_eq!(w.route(&hop), refused(RefuseReason::Paused));
}

#[test]
fn paused_agent_ignores_what_is_not_addressed_to_it() {
    let mut w = World::new();
    w.view.states.insert(w.a, AgentState::Paused);
    assert_eq!(
        w.route(&w.message(&w.linked_key)),
        ignored(IgnoreReason::NotAddressed)
    );
}

#[test]
fn deleted_or_unknown_agent_is_ignored() {
    let mut w = World::new();
    w.view.states.insert(w.a, AgentState::Deleted);
    assert_eq!(
        w.route(&w.dm(&w.owner_key)),
        ignored(IgnoreReason::AgentDeleted)
    );

    let unknown = AgentId::new_v4();
    assert_eq!(
        route(&w.mention(&w.owner_key), unknown, &w.view),
        ignored(IgnoreReason::UnknownAgent)
    );
    w.view.states.insert(unknown, AgentState::Active);
    assert_eq!(
        route(&w.mention(&w.owner_key), unknown, &w.view),
        ignored(IgnoreReason::UnknownAgent),
        "an agent without an owner is unknown"
    );
}

#[test]
fn banned_requester_is_refused() {
    let mut w = World::new();
    w.view.community_key = true;
    w.view.banned_members.insert(w.linked);
    w.view.banned_keys.insert(w.stranger_key.clone());
    assert_eq!(
        w.route(&w.mention(&w.linked_key)),
        refused(RefuseReason::Banned)
    );
    assert_eq!(
        w.route(&w.mention(&w.stranger_key)),
        refused(RefuseReason::Banned)
    );

    w.view.banned_members.insert(w.owner);
    assert_eq!(
        w.route(&w.dm(&w.owner_key)),
        refused(RefuseReason::Banned),
        "a ban covers the owner too"
    );
}

#[test]
fn banned_requester_cannot_spend_through_another_agent() {
    let mut w = World::new();
    w.view.banned_members.insert(w.linked);
    let requester = w.requester(&w.linked_key.clone());
    let event = w.b_mentions_a(requester, Hop::ZERO);
    assert_eq!(w.route(&event), refused(RefuseReason::Banned));
}

#[test]
fn deny_rule_refuses_and_deny_wins_over_allow() {
    let mut w = World::new();
    w.set_policy(AgentPolicy {
        allow: vec![PolicyTarget::Everyone],
        deny: vec![w.member_target(&w.linked_key)],
        ..AgentPolicy::default()
    });
    assert_eq!(
        w.route(&w.mention(&w.linked_key)),
        refused(RefuseReason::Denied)
    );

    w.set_policy(AgentPolicy {
        allow: vec![w.member_target(&w.linked_key)],
        deny: vec![PolicyTarget::Room(conv("C1"))],
        ..AgentPolicy::default()
    });
    assert_eq!(
        w.route(&w.mention(&w.linked_key)),
        refused(RefuseReason::Denied)
    );
    assert!(matches!(
        w.route(&w.dm(&w.linked_key)),
        Decision::Run { .. }
    ));
}

#[test]
fn allow_list_admits_only_the_members_and_rooms_it_names() {
    let mut w = World::new();
    w.set_policy(AgentPolicy {
        allow: vec![
            w.member_target(&w.linked_key),
            PolicyTarget::Room(conv("C2")),
        ],
        ..AgentPolicy::default()
    });
    assert!(matches!(
        w.route(&w.mention(&w.linked_key)),
        Decision::Run { .. }
    ));
    assert_eq!(
        w.route(&w.mention(&w.known_key)),
        refused(RefuseReason::Denied)
    );

    let mut in_c2 = w.mention(&w.known_key);
    in_c2.conv = conv("C2");
    in_c2.message.conv = conv("C2");
    assert_eq!(
        w.route(&in_c2),
        Decision::LinkPrompt {
            requester: w.requester(&w.known_key)
        }
    );
}

#[test]
fn deny_everyone_never_locks_out_the_owner() {
    let mut w = World::new();
    w.set_policy(AgentPolicy {
        deny: vec![PolicyTarget::Everyone],
        ..AgentPolicy::default()
    });
    assert!(matches!(
        w.route(&w.dm(&w.owner_key)),
        Decision::Run {
            scope: ScopeKind::Private,
            ..
        }
    ));
    assert!(matches!(
        w.route(&w.mention(&w.owner_key)),
        Decision::Run { .. }
    ));
    assert_eq!(
        w.route(&w.mention(&w.linked_key)),
        refused(RefuseReason::Denied)
    );
}

#[test]
fn deny_rule_applies_to_the_inherited_requester_of_a_hop() {
    let mut w = World::new();
    w.set_policy(AgentPolicy {
        deny: vec![w.member_target(&w.linked_key)],
        ..AgentPolicy::default()
    });
    let requester = w.requester(&w.linked_key.clone());
    let event = w.b_mentions_a(requester, Hop::ZERO);
    assert_eq!(w.route(&event), refused(RefuseReason::Denied));
}

/// The linked member's identity on a Rocket.Chat server, and a mention of
/// agent A's bot there.
fn rocketchat_mention(w: &mut World) -> InboundEvent {
    let rc = |user: &str| MemberKey {
        surface: SurfaceKind::RocketChat,
        team: "chat.example.com".into(),
        user: user.into(),
    };
    let (rc_linked, rc_a_bot) = (rc("rc-linked"), rc("rc-bot-a"));
    w.view.members.insert(rc_linked.clone(), w.linked);
    w.view.bots.insert(rc_a_bot.clone(), ManagedBot::Agent(w.a));
    let conv = ConvRef {
        surface: SurfaceKind::RocketChat,
        team: "chat.example.com".into(),
        conversation: "GENERAL".into(),
    };
    let mut event = w.mention(&rc_linked);
    event.conv = conv.clone();
    event.message = MsgRef {
        conv,
        id: "rc-msg-1".into(),
    };
    event.mentions = vec![rc_a_bot.user];
    event
}

#[test]
fn member_rule_covers_the_same_member_on_another_surface() {
    let mut w = World::new();
    let event = rocketchat_mention(&mut w);
    assert!(matches!(w.route(&event), Decision::Run { .. }));

    w.set_policy(AgentPolicy {
        deny: vec![w.member_target(&w.linked_key)],
        ..AgentPolicy::default()
    });
    assert_eq!(
        w.route(&event),
        refused(RefuseReason::Denied),
        "a deny on the Slack identity covers the linked Rocket.Chat identity"
    );

    w.set_policy(AgentPolicy {
        allow: vec![w.member_target(&w.linked_key)],
        ..AgentPolicy::default()
    });
    assert!(matches!(w.route(&event), Decision::Run { .. }));
    assert_eq!(
        w.route(&w.mention(&w.known_key)),
        refused(RefuseReason::Denied)
    );
}

#[test]
fn member_rule_without_a_member_covers_only_its_identity() {
    let mut w = World::new();
    w.view.community_key = true;
    let event = rocketchat_mention(&mut w);
    w.set_policy(AgentPolicy {
        deny: vec![PolicyTarget::Member {
            key: w.linked_key.clone(),
            member: None,
        }],
        ..AgentPolicy::default()
    });
    assert_eq!(
        w.route(&w.mention(&w.linked_key)),
        refused(RefuseReason::Denied),
        "the identity it names is covered, whoever it belongs to now"
    );
    assert!(matches!(w.route(&event), Decision::Run { .. }));

    w.set_policy(AgentPolicy {
        deny: vec![PolicyTarget::Member {
            key: w.stranger_key.clone(),
            member: None,
        }],
        ..AgentPolicy::default()
    });
    assert_eq!(
        w.route(&w.mention(&w.stranger_key)),
        refused(RefuseReason::Denied),
        "a requester with no member is matched by identity"
    );
    assert!(matches!(
        w.route(&w.mention(&w.linked_key)),
        Decision::Run { .. }
    ));
}

#[test]
fn missing_ban_answer_refuses_instead_of_allowing() {
    let mut w = World::new();
    w.view.community_key = true;
    w.view.bans_unavailable = true;
    for event in [
        w.mention(&w.linked_key),
        w.mention(&w.stranger_key),
        w.dm(&w.owner_key),
    ] {
        assert_eq!(w.route(&event), refused(RefuseReason::PolicyUnavailable));
    }
    let requester = w.requester(&w.linked_key.clone());
    let hop = w.b_mentions_a(requester, Hop::ZERO);
    assert_eq!(w.route(&hop), refused(RefuseReason::PolicyUnavailable));
}

#[test]
fn missing_policy_refuses_instead_of_allowing_even_for_the_owner() {
    let mut w = World::new();
    w.view.community_key = true;
    w.view.policies.remove(&w.a);
    for event in [
        w.mention(&w.linked_key),
        w.mention(&w.stranger_key),
        w.dm(&w.owner_key),
        w.mention(&w.owner_key),
    ] {
        assert_eq!(w.route(&event), refused(RefuseReason::PolicyUnavailable));
    }
}

#[test]
fn precedence_missing_policy_after_ignores_paused_and_bans() {
    let mut w = World::new();
    w.view.policies.remove(&w.a);
    w.view.bans_unavailable = true;
    assert_eq!(
        w.route(&w.message(&w.linked_key)),
        ignored(IgnoreReason::NotAddressed),
        "an unaddressed message draws no notice, even with nothing loaded"
    );
    assert_eq!(
        w.route(&w.mention(&w.linked_key)),
        refused(RefuseReason::PolicyUnavailable)
    );

    w.view.bans_unavailable = false;
    w.view.banned_members.insert(w.linked);
    assert_eq!(
        w.route(&w.mention(&w.linked_key)),
        refused(RefuseReason::Banned),
        "a known ban is reported before a missing policy"
    );

    w.view.states.insert(w.a, AgentState::Paused);
    assert_eq!(
        w.route(&w.mention(&w.linked_key)),
        refused(RefuseReason::Paused)
    );
}

#[test]
fn precedence_ignores_before_refusals() {
    let mut w = World::new();
    w.view.states.insert(w.a, AgentState::Paused);
    w.view.banned_members.insert(w.linked);
    w.set_policy(AgentPolicy {
        deny: vec![PolicyTarget::Everyone],
        max_hops: Hop::ZERO,
        ..AgentPolicy::default()
    });
    assert_eq!(
        w.route(&w.message(&w.linked_key)),
        ignored(IgnoreReason::NotAddressed)
    );
    let requester = w.requester(&w.linked_key.clone());
    let event = w.b_mentions_a(requester, Hop(9));
    w.view.refs.clear();
    assert_eq!(
        w.route(&event),
        ignored(IgnoreReason::UnattributedManagedBot)
    );
}

#[test]
fn precedence_paused_then_banned_then_denied_then_hop_cap() {
    let mut w = World::new();
    w.view.states.insert(w.a, AgentState::Paused);
    w.view.banned_members.insert(w.linked);
    w.set_policy(AgentPolicy {
        deny: vec![PolicyTarget::Everyone],
        max_hops: Hop::ZERO,
        ..AgentPolicy::default()
    });
    let requester = w.requester(&w.linked_key.clone());
    let event = w.b_mentions_a(requester, Hop::ZERO);

    assert_eq!(w.route(&event), refused(RefuseReason::Paused));
    w.view.states.insert(w.a, AgentState::Active);
    assert_eq!(w.route(&event), refused(RefuseReason::Banned));
    w.view.banned_members.clear();
    assert_eq!(w.route(&event), refused(RefuseReason::Denied));
    w.set_policy(AgentPolicy {
        max_hops: Hop::ZERO,
        ..AgentPolicy::default()
    });
    assert_eq!(
        w.route(&event),
        refused(RefuseReason::HopCap { max: Hop::ZERO })
    );
}

#[test]
fn precedence_refusals_before_link_prompt_and_community_key() {
    let mut w = World::new();
    w.view.banned_keys.insert(w.stranger_key.clone());
    assert_eq!(
        w.route(&w.mention(&w.stranger_key)),
        refused(RefuseReason::Banned)
    );
    w.view.community_key = true;
    assert_eq!(
        w.route(&w.mention(&w.stranger_key)),
        refused(RefuseReason::Banned)
    );
}

#[test]
fn decision_matches_exhaustively() {
    fn describe(decision: &Decision) -> String {
        match decision {
            Decision::Ignore(reason) => format!("ignore: {reason}"),
            Decision::LinkPrompt { requester } => format!("link prompt for {}", requester.key),
            Decision::Run {
                requester,
                hop,
                credential,
                scope,
                side,
            } => format!(
                "run for {} at hop {hop}: {credential:?} {scope:?} {side:?}",
                requester.key
            ),
            Decision::Refuse(reason) => format!("refuse: {reason}"),
        }
    }

    let mut w = World::new();
    assert_eq!(
        describe(&w.route(&w.message(&w.linked_key))),
        "ignore: not addressed"
    );
    assert_eq!(
        describe(&w.route(&w.mention(&w.stranger_key))),
        "link prompt for slack:T1:USTRANGER"
    );
    assert!(
        describe(&w.route(&w.dm(&w.owner_key))).starts_with("run for slack:T1:UOWNER at hop 0")
    );
    w.view.states.insert(w.a, AgentState::Paused);
    assert_eq!(
        describe(&w.route(&w.mention(&w.linked_key))),
        "refuse: agent paused"
    );
}

#[test]
fn reasons_have_distinct_log_text() {
    let ignores = [
        IgnoreReason::UnknownAgent,
        IgnoreReason::AgentDeleted,
        IgnoreReason::OwnMessage,
        IgnoreReason::UnmanagedBot,
        IgnoreReason::ManagerBot,
        IgnoreReason::NotThisAgentsDm,
        IgnoreReason::NotMentionedByAgent,
        IgnoreReason::UnattributedManagedBot,
        IgnoreReason::NotAddressed,
    ];
    let refusals = [
        RefuseReason::Paused,
        RefuseReason::Banned,
        RefuseReason::Denied,
        RefuseReason::HopCap { max: Hop(2) },
        RefuseReason::PolicyUnavailable,
    ];
    let texts: HashSet<String> = ignores
        .iter()
        .map(ToString::to_string)
        .chain(refusals.iter().map(ToString::to_string))
        .collect();
    assert_eq!(texts.len(), ignores.len() + refusals.len());
    assert!(texts.iter().all(|text| !text.is_empty()));
}

/// One point of the invariant grid.
#[derive(Debug, Clone, Copy)]
struct Case {
    owner_linked: bool,
    community_key: bool,
    kind: ConvKind,
    binding_is_a: bool,
    mentioned: bool,
    replied: bool,
    mentions_b: bool,
    sender: Who,
    recorded_member: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Who {
    Person(usize),
    AgentB,
    AgentA,
}

/// Every combination of sender, conversation, linking, community key,
/// binding, mentions and reply, checked against the billing and privacy
/// rules rather than against expected decisions.
#[test]
fn no_combination_breaks_the_billing_and_privacy_invariants() {
    let senders = [
        Who::Person(0),
        Who::Person(1),
        Who::Person(2),
        Who::Person(3),
        Who::AgentB,
        Who::AgentA,
    ];
    let bools = [false, true];
    let mut runs = 0;
    for owner_linked in bools {
        for community_key in bools {
            for kind in [ConvKind::Dm, ConvKind::GroupDm, ConvKind::Channel] {
                for binding_is_a in bools {
                    for mentioned in bools {
                        for replied in bools {
                            for mentions_b in bools {
                                for sender in senders {
                                    for recorded_member in bools {
                                        runs += check_invariants(Case {
                                            owner_linked,
                                            community_key,
                                            kind,
                                            binding_is_a,
                                            mentioned,
                                            replied,
                                            mentions_b,
                                            sender,
                                            recorded_member,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    assert!(runs > 50, "the grid should exercise many runs, got {runs}");
}

fn check_invariants(case: Case) -> usize {
    let mut w = World::new();
    if !case.owner_linked {
        w.view.linked.remove(&w.owner);
    }
    w.view.community_key = case.community_key;
    let people = [
        w.owner_key.clone(),
        w.linked_key.clone(),
        w.known_key.clone(),
        w.stranger_key.clone(),
    ];

    let mut event = match case.sender {
        Who::Person(index) => w.message(&people[index]),
        Who::AgentB => {
            let requester_key =
                people[usize::from(case.mentioned) + usize::from(case.replied) * 2].clone();
            let requester = Requester {
                member: w
                    .view
                    .member_for(&requester_key)
                    .filter(|_| case.recorded_member),
                key: requester_key,
            };
            let mut event = w.b_mentions_a(requester, Hop::ZERO);
            event.mentions.clear();
            event
        }
        Who::AgentA => {
            let mut event = w.message(&w.a_bot);
            event.sender_is_bot = true;
            event.sender_bot_user = Some(w.a_bot.user.clone());
            event
        }
    };
    event.conv_kind = case.kind;
    event.binding = if case.binding_is_a {
        w.a_binding
    } else {
        w.b_binding
    };
    if case.mentioned || case.sender == Who::AgentB {
        event.mentions.push(w.a_bot.user.clone());
    }
    if case.mentions_b {
        event.mentions.push(w.b_bot.user.clone());
    }
    if case.replied {
        let root = MsgRef {
            conv: event.conv.clone(),
            id: MessageId::new("1.0"),
        };
        w.view.agent_posts.insert((root.clone(), w.a));
        event.reply_to = Some(root);
    }

    let decision = w.route(&event);
    let from_person = matches!(case.sender, Who::Person(_));
    if from_person && case.mentions_b && !case.mentioned && case.kind != ConvKind::Dm {
        assert!(
            matches!(decision, Decision::Ignore(_)),
            "a message naming only B never engages A outside A's DM: {case:?}"
        );
    }
    let Decision::Run {
        requester,
        hop,
        credential,
        scope,
        side,
    } = decision
    else {
        if let Decision::LinkPrompt { requester } = &decision {
            assert!(
                !requester
                    .member
                    .is_some_and(|member| w.view.is_linked(member)),
                "a linked requester never gets a link prompt: {case:?}"
            );
        }
        return 0;
    };

    assert_eq!(
        scope == ScopeKind::Private,
        side == Side::Owner,
        "Private scope and Owner side go together: {case:?}"
    );
    if scope == ScopeKind::Private {
        assert!(from_person && case.kind == ConvKind::Dm && case.binding_is_a);
        assert_eq!(requester.key, w.owner_key);
        assert_eq!(credential, CredentialRef::Member(w.owner));
        assert_eq!(hop, Hop::ZERO);
    }
    if requester.member != Some(w.owner) {
        assert_ne!(scope, ScopeKind::Private, "a non-owner never gets Private");
        assert_ne!(
            credential,
            CredentialRef::Member(w.owner),
            "a non-owner never runs on the owner's credential: {case:?}"
        );
    }
    match credential {
        CredentialRef::Member(member) => {
            assert_eq!(
                requester.member,
                Some(member),
                "runs on the requester's own account: {case:?}"
            );
            assert!(w.view.is_linked(member));
        }
        CredentialRef::Community => {
            assert!(case.community_key);
            assert_eq!(side, Side::Public);
            assert_ne!(requester.member, Some(w.owner));
            assert!(!requester.member.is_some_and(|m| w.view.is_linked(m)));
        }
    }
    if from_person {
        assert_eq!(
            requester.key, event.sender,
            "a person pays for their own turn"
        );
        assert_eq!(hop, Hop::ZERO);
    } else {
        assert_eq!(hop, Hop(1));
        assert_ne!(requester.key, w.b_bot, "an agent is never the requester");
    }
    if case.kind == ConvKind::Dm {
        assert!(case.binding_is_a, "only A's own DMs run A: {case:?}");
    }
    1
}

#[test]
fn model_policy_maps_plans_and_falls_back_to_the_default() {
    let policy = ModelPolicy::new("claude-sonnet").with_plan("max", "claude-opus");
    assert_eq!(policy.model_for(Some("max")), "claude-opus");
    assert_eq!(policy.model_for(Some("Max")), "claude-sonnet");
    assert_eq!(policy.model_for(Some("pro")), "claude-sonnet");
    assert_eq!(policy.model_for(None), "claude-sonnet");
    assert_eq!(policy.default_model(), "claude-sonnet");
}

#[test]
fn model_policy_configuration_needs_a_default_and_rejects_unknown_keys() {
    let only_default: ModelPolicy = toml::from_str(r#"default = "claude-sonnet""#).unwrap();
    assert_eq!(only_default, ModelPolicy::new("claude-sonnet"));
    assert!(toml::from_str::<ModelPolicy>(r#"plans = { max = "claude-opus" }"#).is_err());
    assert!(toml::from_str::<ModelPolicy>("default = \"a\"\nmodel = \"b\"").is_err());
}

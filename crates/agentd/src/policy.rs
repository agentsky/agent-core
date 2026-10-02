//! Agents' limits and allow and deny rules, and the community's caps on
//! threads and hops: how agentd keeps the rules in `agent_policies`, how an
//! owner's `allow` and `deny` change them, and the router's view of them.
//!
//! # Allow and deny
//!
//! The router applies the rules ([`AgentPolicy::permits`]): deny wins, an
//! empty allow list lets everyone in, a non-empty one only whom it covers,
//! and the owner is never refused. The commands change them so that each
//! undoes the other:
//!
//! - `deny <target>` puts the target on the deny list and leaves the allow
//!   list as it is, so a deny never lets anyone in: denying the one target
//!   an agent allows leaves it to its owner.
//! - `allow <target>` puts the target on the allow list, so the first
//!   `allow` of a member or a channel limits the agent to it, and takes it
//!   off the deny list. A denied target is put on the allow list only if
//!   the list already limits the agent, so lifting a deny never limits an
//!   agent open to everyone, and one `allow` always lets the target in.
//! - `allow everyone` empties the allow list and takes `everyone` off the
//!   deny list, so everyone not denied by name may use the agent again.

use std::collections::{HashMap, HashSet};

use core_types::{
    BindingId, ConvRef, ConversationId, Hop, MemberId, MemberKey, SurfaceKind, TeamId,
};
use router::{AgentPolicy, PolicyTarget, ThreadBudget};
use serde::{Deserialize, Serialize};
use store::{AgentSettings, KnownChannelIdChange, ThreadSpend};

use crate::config::LimitsConfig;

/// The most rules the owner may put in an agent's allow or deny list, but
/// for `deny everyone`, which is always taken. The denies agentd copies from
/// channels' old ids count, and may take a deny list past it.
pub const MAX_RULES: usize = 100;

/// The most rules an agent's deny list holds with the denies copied from
/// channels' old ids ([`Rules::copy_denies`]): a copy past it denies
/// everyone instead. A real channel id change copies at most one deny, so
/// only a flood of forged changes, which only the agent's owner can send,
/// takes a list there.
pub const MAX_DENIES: usize = 2 * MAX_RULES;

/// The community's caps, from `[limits]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The global hop cap, which an agent's own can only lower.
    pub max_hops: Hop,
    /// The most turns agents take in a thread in an hour, or `None`.
    pub thread_turns_per_hour: Option<u32>,
    /// The most tokens agents' turns use in a thread in a day, or `None`.
    pub thread_tokens_per_day: Option<u64>,
}

impl Limits {
    /// The caps `[limits]` sets, where 0 turns a thread cap off.
    pub fn from_config(config: &LimitsConfig) -> Self {
        Self {
            max_hops: Hop(config.max_hops),
            thread_turns_per_hour: (config.thread_turns_per_hour > 0)
                .then_some(config.thread_turns_per_hour),
            thread_tokens_per_day: (config.thread_tokens_per_day > 0)
                .then_some(config.thread_tokens_per_day),
        }
    }

    /// The hop cap of an agent whose own limit is `own`: the global one,
    /// lowered by the agent's.
    pub fn hops_for(&self, own: Option<u8>) -> Hop {
        own.map_or(self.max_hops, |own| Hop(own.min(self.max_hops.0)))
    }

    /// The router's view of a thread where agents spent `spend`.
    pub fn thread_budget(&self, spend: ThreadSpend) -> ThreadBudget {
        ThreadBudget {
            turns_this_hour: spend.turns_this_hour,
            max_turns_per_hour: self.thread_turns_per_hour,
            tokens_today: spend.tokens_today,
            max_tokens_per_day: self.thread_tokens_per_day,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::from_config(&LimitsConfig::default())
    }
}

/// One allow or deny rule, as `agent_policies` keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Rule {
    /// A member, named by one of their identities.
    Member {
        /// The identity the owner named.
        key: MemberKey,
        /// The member it belonged to then, so the rule covers their other
        /// identities too.
        member: Option<MemberId>,
        /// How the owner wrote it, such as `@alice`.
        label: String,
    },
    /// Everyone in one conversation.
    Room {
        /// The conversation.
        conv: ConvRef,
        /// How the owner wrote it, such as `#general`.
        label: String,
    },
    /// Everyone.
    Everyone,
}

impl Rule {
    fn target(&self) -> PolicyTarget {
        match self {
            Self::Member { key, member, .. } => PolicyTarget::Member {
                key: key.clone(),
                member: *member,
            },
            Self::Room { conv, .. } => PolicyTarget::Room(conv.clone()),
            Self::Everyone => PolicyTarget::Everyone,
        }
    }

    /// Whether `self` and `other` name the same member, room or everyone.
    fn same_target(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Member { key: a, .. }, Self::Member { key: b, .. }) => a == b,
            (Self::Room { conv: a, .. }, Self::Room { conv: b, .. }) => a == b,
            (Self::Everyone, Self::Everyone) => true,
            _ => false,
        }
    }

    /// How replies name it: as the owner wrote it, in a code span, so it
    /// can't mention anyone.
    fn describe(&self) -> String {
        match self {
            Self::Member { label, .. } | Self::Room { label, .. } => {
                format!("`{}`", label.replace('`', ""))
            }
            Self::Everyone => "everyone".to_owned(),
        }
    }
}

/// What [`Rules::allow`] or [`Rules::deny`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The rules changed.
    Changed,
    /// They already said so.
    Unchanged,
    /// The list is full: [`MAX_RULES`].
    Full,
}

/// An agent's allow and deny rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    /// If not empty, only whom these cover may use the agent.
    pub allow: Vec<Rule>,
    /// Whom these cover may not use the agent.
    pub deny: Vec<Rule>,
}

impl Rules {
    /// The rules in `settings`.
    ///
    /// # Errors
    ///
    /// The JSON error if a list doesn't parse.
    pub fn read(settings: &AgentSettings) -> Result<Self, serde_json::Error> {
        Ok(Self {
            allow: serde_json::from_str(&settings.allow_json)?,
            deny: serde_json::from_str(&settings.deny_json)?,
        })
    }

    /// Writes the lists into `settings` as JSON, for
    /// [`update_agent_settings`](store::Store::update_agent_settings). A
    /// rule always serializes; if one ever didn't, its list would be empty
    /// text, which doesn't read, so the agent would refuse everyone rather
    /// than drop a deny.
    pub fn write(&self, settings: &mut AgentSettings) {
        settings.allow_json = serde_json::to_string(&self.allow).unwrap_or_default();
        settings.deny_json = serde_json::to_string(&self.deny).unwrap_or_default();
    }

    /// Applies `allow <rule>`; see the [module docs](self).
    pub fn allow(&mut self, rule: Rule) -> Change {
        if rule == Rule::Everyone {
            let before = (self.allow.len(), self.deny.len());
            self.allow.clear();
            self.deny.retain(|denied| *denied != Rule::Everyone);
            return changed(before != (self.allow.len(), self.deny.len()));
        }
        if !self.deny.iter().any(|denied| denied.same_target(&rule)) {
            return add(&mut self.allow, rule);
        }
        if !self.allow.is_empty() && add(&mut self.allow, rule.clone()) == Change::Full {
            return Change::Full;
        }
        self.deny.retain(|denied| !denied.same_target(&rule));
        Change::Changed
    }

    /// Applies `deny <rule>`; see the [module docs](self).
    pub fn deny(&mut self, rule: Rule) -> Change {
        add(&mut self.deny, rule)
    }

    /// Moves the rules on the conversation `from` to `to`, as when Slack
    /// gave a channel a new id, and says whether any moved. Where a list
    /// already has a rule on `to`, the moved one is dropped as a duplicate,
    /// so a deny on either id is a deny on `to`, which wins over an allow
    /// of it as any deny does.
    pub fn move_room(&mut self, from: &ConvRef, to: &ConvRef) -> bool {
        if from == to {
            return false;
        }
        let allow = move_room(&mut self.allow, from, to);
        let deny = move_room(&mut self.deny, from, to);
        allow || deny
    }

    /// For each `(from, to)` in `pairs`, puts a deny on `to` beside each
    /// deny on `from`, keeping its label, unless the deny list names `to`
    /// already, so a deny on a channel's old id applies under its new ones
    /// too. Says whether the list changed. Copies never let anyone in, so
    /// they may take the list past [`MAX_RULES`]; one that would take it
    /// past [`MAX_DENIES`] denies everyone instead of all of them, which
    /// refuses at least as much and keeps the list short. A list that
    /// denies everyone takes no copies.
    pub fn copy_denies<'a>(
        &mut self,
        pairs: impl IntoIterator<Item = (&'a ConvRef, &'a ConvRef)>,
    ) -> bool {
        if self.denies_everyone() {
            return false;
        }
        let before = self.deny.len();
        for (from, to) in pairs {
            let copies: Vec<Rule> = self
                .deny
                .iter()
                .filter_map(|rule| match rule {
                    Rule::Room { conv, label } if conv == from => Some(Rule::Room {
                        conv: to.clone(),
                        label: label.clone(),
                    }),
                    _ => None,
                })
                .collect();
            for copy in copies {
                if !self.deny.iter().any(|known| known.same_target(&copy)) {
                    self.deny.push(copy);
                }
            }
            if self.deny.len() > MAX_DENIES {
                self.deny.truncate(before);
                self.deny.push(Rule::Everyone);
                return true;
            }
        }
        self.deny.len() > before
    }

    /// Whether `everyone` is denied, which leaves the agent to its owner
    /// whatever the allow list says.
    pub fn denies_everyone(&self) -> bool {
        self.deny.contains(&Rule::Everyone)
    }

    /// Who may use the agent `name`, in a sentence or two.
    pub fn describe(&self, name: &str) -> String {
        let list = |rules: &[Rule]| {
            rules
                .iter()
                .map(Rule::describe)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let allowed: Vec<Rule> = self
            .allow
            .iter()
            .filter(|allowed| !self.deny.iter().any(|denied| denied.same_target(allowed)))
            .cloned()
            .collect();
        if self.denies_everyone() || (allowed.is_empty() && !self.allow.is_empty()) {
            return format!("Only you may use `{name}`.");
        }
        let mut text = if allowed.is_empty() {
            format!("Everyone may use `{name}`")
        } else {
            format!("Only you and {} may use `{name}`", list(&allowed))
        };
        if self.deny.is_empty() {
            text.push('.');
        } else {
            text.push_str(&format!(", except {}.", list(&self.deny)));
        }
        text
    }
}

fn changed(changed: bool) -> Change {
    if changed {
        Change::Changed
    } else {
        Change::Unchanged
    }
}

/// Moves the rules on `from` in `rules` to `to`, keeping the first rule on
/// each target, and says whether any moved.
fn move_room(rules: &mut Vec<Rule>, from: &ConvRef, to: &ConvRef) -> bool {
    let mut moved = false;
    for rule in rules.iter_mut() {
        if let Rule::Room { conv, .. } = rule
            && conv == from
        {
            *conv = to.clone();
            moved = true;
        }
    }
    if moved {
        let mut kept: Vec<Rule> = Vec::with_capacity(rules.len());
        for rule in rules.drain(..) {
            if !kept.iter().any(|known| known.same_target(&rule)) {
                kept.push(rule);
            }
        }
        *rules = kept;
    }
    moved
}

/// Adds `rule` to `rules` unless it names a target already there or the
/// list holds [`MAX_RULES`]; `everyone`, which only ever narrows who may use
/// an agent, is taken all the same.
fn add(rules: &mut Vec<Rule>, rule: Rule) -> Change {
    if rules.iter().any(|known| known.same_target(&rule)) {
        Change::Unchanged
    } else if rule != Rule::Everyone && rules.len() >= MAX_RULES {
        Change::Full
    } else {
        rules.push(rule);
        Change::Changed
    }
}

/// The ids a channel that was `start` had since, as `binding`'s recorded
/// channel id changes in `changes` say, waiting or settled: those it was
/// changed to, then those they were changed to, and so on, each once, the
/// last the latest a chain reaches.
pub fn later_ids(
    changes: &[KnownChannelIdChange],
    binding: BindingId,
    start: &ConversationId,
) -> Vec<ConversationId> {
    let mut onward: HashMap<&ConversationId, Vec<&ConversationId>> = HashMap::new();
    for known in changes
        .iter()
        .filter(|known| known.change.binding == binding)
    {
        onward
            .entry(&known.change.old)
            .or_default()
            .push(&known.change.new);
    }
    let mut seen = HashSet::from([start]);
    let mut order = vec![start];
    let mut next = 0;
    while let Some(id) = order.get(next).copied() {
        next += 1;
        for &new in onward.get(id).into_iter().flatten() {
            if seen.insert(new) {
                order.push(new);
            }
        }
    }
    order.into_iter().skip(1).cloned().collect()
}

/// The conversations whose denies apply to others too while `changes`, an
/// agent's recorded channel id changes, wait: for each waiting change,
/// its old id and each id the channel had since ([`later_ids`]).
pub fn pending_denials(changes: &[KnownChannelIdChange]) -> Vec<(ConvRef, ConvRef)> {
    let room = |team: &TeamId, conversation: &ConversationId| ConvRef {
        surface: SurfaceKind::Slack,
        team: team.clone(),
        conversation: conversation.clone(),
    };
    changes
        .iter()
        .filter(|known| known.waiting)
        .flat_map(|known| {
            let change = &known.change;
            later_ids(changes, change.binding, &change.old)
                .into_iter()
                .map(|to| (room(&known.team, &change.old), room(&known.team, &to)))
        })
        .collect()
}

/// The router's policy for an agent with `settings`, under `limits`, that
/// took `turns_today` turns today, each deny on the first conversation of
/// a pair in `pending` applying to the second too ([`pending_denials`]).
///
/// # Errors
///
/// The JSON error if a rule list doesn't parse.
pub fn agent_policy(
    settings: &AgentSettings,
    limits: &Limits,
    turns_today: u32,
    pending: &[(ConvRef, ConvRef)],
) -> Result<AgentPolicy, serde_json::Error> {
    let mut rules = Rules::read(settings)?;
    rules.copy_denies(pending.iter().map(|(from, to)| (from, to)));
    Ok(AgentPolicy {
        allow: rules.allow.iter().map(Rule::target).collect(),
        deny: rules.deny.iter().map(Rule::target).collect(),
        max_hops: limits.hops_for(settings.max_hops),
        turns_per_day: settings.turns_per_day,
        turns_today,
    })
}

#[cfg(test)]
mod tests {
    use core_types::{Requester, SurfaceKind};

    use super::*;

    fn key(user: &str) -> MemberKey {
        MemberKey {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            user: user.into(),
        }
    }

    fn conv(id: &str) -> ConvRef {
        ConvRef {
            surface: SurfaceKind::Slack,
            team: "T1".into(),
            conversation: id.into(),
        }
    }

    fn member(user: &str) -> Rule {
        Rule::Member {
            key: key(user),
            member: None,
            label: format!("@{user}"),
        }
    }

    fn room(id: &str) -> Rule {
        Rule::Room {
            conv: conv(id),
            label: format!("#{id}"),
        }
    }

    fn permits(rules: &Rules, user: &str, room: &str) -> bool {
        let mut settings = AgentSettings::default();
        rules.write(&mut settings);
        let policy = agent_policy(&settings, &Limits::default(), 0, &[]).unwrap();
        policy.permits(
            &Requester {
                member: None,
                key: key(user),
                outside: None,
            },
            &conv(room),
        )
    }

    #[test]
    fn deny_wins_and_allow_undoes_it() {
        let mut rules = Rules::default();
        assert!(permits(&rules, "bob", "C1"), "the default lets everyone in");
        assert_eq!(rules.deny(member("bob")), Change::Changed);
        assert_eq!(rules.deny(member("bob")), Change::Unchanged);
        assert!(!permits(&rules, "bob", "C1"));
        assert!(permits(&rules, "carol", "C1"));
        assert_eq!(rules.allow(member("bob")), Change::Changed);
        assert!(
            permits(&rules, "carol", "C1"),
            "without an allow list, allowing a denied member only lifts the deny"
        );
        assert!(permits(&rules, "bob", "C1"));
        assert_eq!(rules, Rules::default());
    }

    #[test]
    fn the_first_allow_limits_the_agent_to_it() {
        let mut rules = Rules::default();
        assert_eq!(rules.allow(member("alice")), Change::Changed);
        assert_eq!(rules.allow(member("alice")), Change::Unchanged);
        assert_eq!(rules.allow(room("C2")), Change::Changed);
        assert!(permits(&rules, "alice", "C1"));
        assert!(permits(&rules, "bob", "C2"));
        assert!(!permits(&rules, "bob", "C1"));
        assert_eq!(rules.deny(member("alice")), Change::Changed);
        assert!(
            !permits(&rules, "alice", "C2"),
            "deny wins over a room rule"
        );
        assert_eq!(rules.allow, [member("alice"), room("C2")]);
        assert_eq!(
            rules.describe("helper"),
            "Only you and `#C2` may use `helper`, except `@alice`."
        );
        assert_eq!(rules.allow(Rule::Everyone), Change::Changed);
        assert!(permits(&rules, "bob", "C1"));
        assert!(
            !permits(&rules, "alice", "C1"),
            "allow everyone keeps denies by name"
        );
    }

    #[test]
    fn deny_everyone_leaves_the_owner_and_allow_everyone_opens_again() {
        let mut rules = Rules::default();
        rules.deny(member("bob"));
        assert_eq!(rules.deny(Rule::Everyone), Change::Changed);
        assert!(!permits(&rules, "alice", "C1"));
        assert_eq!(rules.describe("helper"), "Only you may use `helper`.");
        assert_eq!(rules.allow(Rule::Everyone), Change::Changed);
        assert_eq!(rules.allow(Rule::Everyone), Change::Unchanged);
        assert!(permits(&rules, "alice", "C1"));
        assert!(!permits(&rules, "bob", "C1"));
    }

    #[test]
    fn a_deny_never_opens_the_agent_to_anyone() {
        let mut rules = Rules::default();
        rules.allow(member("bob"));
        assert_eq!(rules.deny(member("bob")), Change::Changed);
        assert!(!permits(&rules, "bob", "C1"));
        assert!(
            !permits(&rules, "carol", "C1"),
            "denying the only allowed member leaves the agent to its owner"
        );
        assert_eq!(rules.describe("helper"), "Only you may use `helper`.");
        assert_eq!(rules.allow(member("bob")), Change::Changed);
        assert!(permits(&rules, "bob", "C1"), "allow undoes the deny");
        assert!(!permits(&rules, "carol", "C1"));
    }

    #[test]
    fn allowing_a_denied_target_lets_it_in_with_one_command() {
        let mut rules = Rules::default();
        rules.allow(member("alice"));
        rules.deny(member("bob"));
        assert_eq!(rules.allow(member("bob")), Change::Changed);
        assert!(permits(&rules, "bob", "C1"));
        assert!(!permits(&rules, "carol", "C1"));
        assert_eq!(rules.allow, [member("alice"), member("bob")]);
        assert!(rules.deny.is_empty());

        let mut rules = Rules::default();
        rules.deny(member("bob"));
        assert_eq!(rules.allow(member("bob")), Change::Changed);
        assert_eq!(
            rules,
            Rules::default(),
            "an agent open to everyone stays open"
        );

        let mut rules = Rules::default();
        for n in 0..MAX_RULES {
            rules.allow(member(&format!("u{n}")));
        }
        rules.deny(member("bob"));
        let full = rules.clone();
        assert_eq!(rules.allow(member("bob")), Change::Full);
        assert_eq!(rules, full, "a full allow list keeps the deny too");
    }

    #[test]
    fn a_full_list_takes_no_more() {
        let mut rules = Rules::default();
        for n in 0..MAX_RULES {
            assert_eq!(rules.deny(member(&format!("u{n}"))), Change::Changed);
        }
        assert_eq!(rules.deny(member("one-more")), Change::Full);
        rules.allow.push(member("x"));
        assert_eq!(rules.deny(member("x")), Change::Full);
        assert_eq!(rules.allow, [member("x")], "a refused deny changes nothing");
        assert_eq!(
            rules.deny(member("u0")),
            Change::Unchanged,
            "a target denied already is no new rule"
        );
    }

    #[test]
    fn rules_are_described_without_mentions() {
        let mut rules = Rules::default();
        assert_eq!(rules.describe("helper"), "Everyone may use `helper`.");
        rules.deny(member("bob"));
        assert_eq!(
            rules.describe("helper"),
            "Everyone may use `helper`, except `@bob`."
        );
        rules.allow(member("al`ice"));
        rules.allow(room("C2"));
        assert_eq!(
            rules.describe("helper"),
            "Only you and `@alice`, `#C2` may use `helper`, except `@bob`."
        );
    }

    #[test]
    fn rules_round_trip_as_json_and_bad_json_is_an_error() {
        let mut rules = Rules::default();
        rules.allow(Rule::Member {
            key: key("U1"),
            member: Some(MemberId::new_v4()),
            label: "@U1".into(),
        });
        rules.allow(room("C1"));
        rules.deny(Rule::Everyone);
        let mut settings = AgentSettings::default();
        rules.write(&mut settings);
        assert_eq!(Rules::read(&settings).unwrap(), rules);
        let bad = AgentSettings {
            deny_json: r#"[{"kind":"nobody"}]"#.into(),
            ..AgentSettings::default()
        };
        assert!(Rules::read(&bad).is_err());
        assert!(agent_policy(&bad, &Limits::default(), 0, &[]).is_err());
    }

    #[test]
    fn an_agents_hops_only_lower_the_global_cap() {
        let limits = Limits {
            max_hops: Hop(3),
            ..Limits::default()
        };
        assert_eq!(limits.hops_for(None), Hop(3));
        assert_eq!(limits.hops_for(Some(1)), Hop(1));
        assert_eq!(limits.hops_for(Some(9)), Hop(3));
        let settings = AgentSettings {
            turns_per_day: Some(7),
            max_hops: Some(0),
            ..AgentSettings::default()
        };
        let policy = agent_policy(&settings, &limits, 5, &[]).unwrap();
        assert_eq!(
            (policy.max_hops, policy.turns_per_day, policy.turns_today),
            (Hop(0), Some(7), 5)
        );
    }

    #[test]
    fn zero_turns_a_thread_cap_off() {
        let config = LimitsConfig {
            thread_turns_per_hour: 0,
            thread_tokens_per_day: 0,
            ..LimitsConfig::default()
        };
        let limits = Limits::from_config(&config);
        assert_eq!(
            (limits.thread_turns_per_hour, limits.thread_tokens_per_day),
            (None, None)
        );
        let budget = Limits::default().thread_budget(ThreadSpend {
            turns_this_hour: 4,
            tokens_today: 9,
        });
        assert_eq!(
            budget,
            ThreadBudget {
                turns_this_hour: 4,
                max_turns_per_hour: Some(crate::config::DEFAULT_THREAD_TURNS_PER_HOUR),
                tokens_today: 9,
                max_tokens_per_day: Some(crate::config::DEFAULT_THREAD_TOKENS_PER_DAY),
            }
        );
    }

    #[test]
    fn colliding_rules_merge_with_deny_winning() {
        let (old, new) = (conv("G0PRIVAT1"), conv("C0PRIVAT1"));
        let mut rules = Rules {
            allow: vec![room("C0PRIVAT1"), member("bob"), room("C0OTHER01")],
            deny: vec![room("G0PRIVAT1"), member("carol")],
        };
        assert!(
            permits(&rules, "dave", "C0PRIVAT1"),
            "allowed on the new id"
        );
        assert!(rules.move_room(&old, &new));
        assert_eq!(
            rules.deny,
            [
                Rule::Room {
                    conv: new.clone(),
                    label: "#G0PRIVAT1".to_owned()
                },
                member("carol")
            ]
        );
        assert_eq!(
            rules.allow,
            [room("C0PRIVAT1"), member("bob"), room("C0OTHER01")]
        );
        assert!(
            !permits(&rules, "dave", "C0PRIVAT1"),
            "the old id's deny is a deny on the new one, over its allow"
        );
        assert!(!rules.move_room(&old, &new), "nothing left on the old id");

        let mut duplicated = Rules {
            allow: vec![room("G0PRIVAT1"), room("C0PRIVAT1")],
            deny: vec![room("C0PRIVAT1"), room("G0PRIVAT1"), Rule::Everyone],
        };
        assert!(duplicated.move_room(&old, &new));
        assert_eq!(duplicated.allow.len(), 1, "{:?}", duplicated.allow);
        assert_eq!(duplicated.deny, [room("C0PRIVAT1"), Rule::Everyone]);

        let mut allowed_old = Rules {
            allow: vec![room("G0PRIVAT1")],
            deny: vec![room("C0PRIVAT1")],
        };
        assert!(allowed_old.move_room(&old, &new));
        assert!(
            !permits(&allowed_old, "dave", "C0PRIVAT1"),
            "a deny on the new id stays a deny"
        );
        assert!(!allowed_old.move_room(&new, &new));

        let mut elsewhere = Rules {
            allow: vec![Rule::Room {
                conv: ConvRef {
                    team: "T0OTHER01".into(),
                    ..old.clone()
                },
                label: "#G0PRIVAT1".to_owned(),
            }],
            deny: vec![member("G0PRIVAT1")],
        };
        let unchanged = elsewhere.clone();
        assert!(
            !elsewhere.move_room(&old, &new),
            "another workspace's channel and a member are no room on the old id"
        );
        assert_eq!(elsewhere, unchanged);
    }

    fn known(binding: BindingId, old: &str, new: &str, waiting: bool) -> KnownChannelIdChange {
        KnownChannelIdChange {
            change: store::ChannelIdChange {
                binding,
                old: old.into(),
                new: new.into(),
                received_at: time::OffsetDateTime::UNIX_EPOCH,
            },
            team: "T1".into(),
            waiting,
        }
    }

    #[test]
    fn a_channel_is_followed_through_its_bindings_changes_only() {
        let (ours, theirs) = (BindingId::new_v4(), BindingId::new_v4());
        let changes = [
            known(ours, "C2", "C3", false),
            known(ours, "G1", "C2", true),
            known(theirs, "C3", "C9", true),
            known(ours, "C3", "G1", true),
        ];
        let ids = |start: &str| -> Vec<String> {
            later_ids(&changes, ours, &start.into())
                .into_iter()
                .map(|id| id.as_str().to_owned())
                .collect()
        };
        assert_eq!(ids("G1"), ["C2", "C3"], "in any order, and a cycle ends");
        assert_eq!(ids("C3"), ["G1", "C2"]);
        assert!(ids("C9").is_empty());
    }

    #[test]
    fn a_waiting_change_applies_the_old_ids_denies_and_never_its_allows() {
        let binding = BindingId::new_v4();
        let rules = Rules {
            allow: vec![room("G1"), member("bob")],
            deny: vec![room("G1"), room("C7")],
        };
        let mut settings = AgentSettings::default();
        rules.write(&mut settings);
        let changes = [
            known(binding, "G1", "C2", true),
            known(binding, "C2", "C3", false),
            known(binding, "C7", "C8", false),
        ];
        let pending = pending_denials(&changes);
        assert_eq!(
            pending,
            [(conv("G1"), conv("C2")), (conv("G1"), conv("C3"))],
            "only a waiting change counts, along the chain it starts"
        );
        let policy = agent_policy(&settings, &Limits::default(), 0, &pending).unwrap();
        let alice = Requester {
            member: None,
            key: key("alice"),
            outside: None,
        };
        let bob = Requester {
            member: None,
            key: key("bob"),
            outside: None,
        };
        for channel in ["G1", "C2", "C3"] {
            assert!(!policy.permits(&bob, &conv(channel)), "{channel}");
        }
        assert!(
            policy.permits(&bob, &conv("C8")),
            "a settled change applies nothing"
        );
        assert!(
            !policy.permits(&alice, &conv("C2")),
            "the allow on the old id doesn't move early"
        );
    }

    #[test]
    fn copied_denies_may_go_past_the_most_rules() {
        let mut rules = Rules {
            allow: Vec::new(),
            deny: (0..MAX_RULES).map(|n| room(&format!("C{n}"))).collect(),
        };
        assert!(rules.copy_denies([(&conv("C0"), &conv("G9"))]));
        assert_eq!(rules.deny.len(), MAX_RULES + 1);
        assert!(!rules.copy_denies([(&conv("C0"), &conv("G9"))]), "once");
        assert!(
            !rules.copy_denies([(&conv("C99999"), &conv("G8"))]),
            "no deny there"
        );
    }

    #[test]
    fn a_real_copy_past_the_most_rules_survives_allow_everyone() {
        let mut rules = Rules {
            allow: vec![room("C0")],
            deny: (0..MAX_RULES).map(|n| room(&format!("C{n}"))).collect(),
        };
        assert!(rules.copy_denies([(&conv("C1"), &conv("G0SHARED1"))]));
        assert_eq!(rules.allow(Rule::Everyone), Change::Changed);
        assert_eq!(rules.deny.len(), MAX_RULES + 1, "nothing dropped");
        assert!(
            !permits(&rules, "bob", "G0SHARED1"),
            "the channel under its new id stays denied"
        );
    }

    #[test]
    fn after_a_flood_of_copies_the_owner_can_deny_everyone_and_open_the_agent_again() {
        let mut rules = Rules {
            allow: Vec::new(),
            deny: vec![room("C0")],
        };
        let from = conv("C0");
        let targets: Vec<ConvRef> = (0..MAX_DENIES).map(|n| conv(&format!("G{n}"))).collect();
        for to in &targets {
            rules.copy_denies([(&from, to)]);
        }
        assert!(rules.denies_everyone());
        assert_eq!(rules.deny.len(), MAX_DENIES + 1);
        assert_eq!(rules.allow(Rule::Everyone), Change::Changed);
        assert!(!rules.denies_everyone());
        assert_eq!(rules.deny.len(), MAX_DENIES, "nothing else dropped");
        assert_eq!(
            rules.deny(member("mallory")),
            Change::Full,
            "the copies fill the list"
        );
        assert_eq!(rules.deny(Rule::Everyone), Change::Changed);
        assert!(rules.denies_everyone());
    }

    #[test]
    fn copies_past_the_most_denies_deny_everyone_instead() {
        let mut rules = Rules {
            allow: vec![room("C1")],
            deny: (0..MAX_RULES).map(|n| room(&format!("C{n}"))).collect(),
        };
        let from = conv("C0");
        let targets: Vec<ConvRef> = (0..MAX_DENIES).map(|n| conv(&format!("G{n}"))).collect();
        assert!(
            rules.copy_denies(
                targets[..MAX_DENIES - MAX_RULES]
                    .iter()
                    .map(|to| (&from, to))
            )
        );
        assert_eq!(rules.deny.len(), MAX_DENIES);
        assert!(!rules.denies_everyone());
        let full = rules.deny.clone();
        assert!(rules.copy_denies(targets.iter().map(|to| (&from, to))));
        assert_eq!(
            rules.deny,
            [full, vec![Rule::Everyone]].concat(),
            "no copy past the bound is kept"
        );
        assert!(
            !rules.copy_denies([(&from, &conv("C9999999"))]),
            "everyone is denied already"
        );
        assert_eq!(rules.deny.len(), MAX_DENIES + 1);
    }
}

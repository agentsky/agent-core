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

use core_types::{ConvRef, Hop, MemberId, MemberKey};
use router::{AgentPolicy, PolicyTarget, ThreadBudget};
use serde::{Deserialize, Serialize};
use store::{AgentSettings, ThreadSpend};

use crate::config::LimitsConfig;

/// The most rules an agent's allow or deny list holds.
pub const MAX_RULES: usize = 100;

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

/// Adds `rule` to `rules` unless it names a target already there or the
/// list is full.
fn add(rules: &mut Vec<Rule>, rule: Rule) -> Change {
    if rules.iter().any(|known| known.same_target(&rule)) {
        Change::Unchanged
    } else if rules.len() >= MAX_RULES {
        Change::Full
    } else {
        rules.push(rule);
        Change::Changed
    }
}

/// The router's policy for an agent with `settings`, under `limits`, that
/// took `turns_today` turns today.
///
/// # Errors
///
/// The JSON error if a rule list doesn't parse.
pub fn agent_policy(
    settings: &AgentSettings,
    limits: &Limits,
    turns_today: u32,
) -> Result<AgentPolicy, serde_json::Error> {
    let rules = Rules::read(settings)?;
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
        let policy = agent_policy(&settings, &Limits::default(), 0).unwrap();
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
        assert!(agent_policy(&bad, &Limits::default(), 0).is_err());
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
        let policy = agent_policy(&settings, &limits, 5).unwrap();
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
}

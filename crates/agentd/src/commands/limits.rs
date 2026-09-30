//! `limits`, `allow` and `deny`: an owner's limits and rules for their
//! agent, which the router applies to every turn (see [`crate::policy`]).

use commands::{RoomRef, Setting, Target, UserRef};
use core_types::{ConvRef, ConversationId, MemberKey};

use super::agents::no_such_agent;
use super::{Commands, Failure};
use crate::policy::{Change, MAX_RULES, Rule, Rules};

/// A limit after `setting`: unchanged when it is `None`.
fn apply<T>(setting: Option<Setting<T>>, old: Option<T>) -> Option<T> {
    match setting {
        None => old,
        Some(Setting::To(value)) => Some(value),
        Some(Setting::Off) => None,
    }
}

impl Commands {
    /// Runs `limits <name> [turns=N/day] [hops=N]` for `key`: a setting
    /// given changes, one left out stays.
    pub(super) async fn limits(
        &self,
        key: &MemberKey,
        name: &str,
        turns: Option<Setting<u32>>,
        hops: Option<Setting<u8>>,
    ) -> Result<String, Failure> {
        let Some(agent) = self.own_agent(key, name).await? else {
            return Ok(no_such_agent(name));
        };
        let (turns_per_day, max_hops) = self
            .inner
            .store
            .update_agent_settings(agent.id, |settings| {
                settings.turns_per_day = apply(turns, settings.turns_per_day);
                settings.max_hops = apply(hops, settings.max_hops);
                (settings.turns_per_day, settings.max_hops)
            })
            .await?;
        tracing::info!(agent = %agent.id, ?turns_per_day, ?max_hops, "set an agent's limits");
        Ok(self.describe_limits(name, turns_per_day, max_hops))
    }

    /// `name`'s limits in a sentence or two.
    fn describe_limits(&self, name: &str, turns_per_day: Option<u32>, hops: Option<u8>) -> String {
        let turns = match turns_per_day {
            None => format!("`{name}` has no daily limit"),
            Some(0) => format!("`{name}` takes requests from you only"),
            Some(max) => format!(
                "`{name}` takes at most {max} requests a day from anyone but you (the day \
                 starts at midnight UTC)"
            ),
        };
        let community = self.limits.max_hops.0;
        let hops = match hops {
            None => format!("the community's limit of {community} agent-to-agent hops applies"),
            Some(0) => "no other agent can hand work to it".to_owned(),
            Some(own) if own < community => {
                format!("agents can hand work to it along chains of at most {own} hops")
            }
            Some(own) => format!(
                "the community's limit of {community} agent-to-agent hops applies, since \
                 hops={own} isn't lower"
            ),
        };
        format!("{turns}, and {hops}.")
    }

    /// Runs `allow <name> <target>`, or `deny` when `allow` is false, for
    /// `key`.
    pub(super) async fn allow_or_deny(
        &self,
        key: &MemberKey,
        name: &str,
        target: &Target,
        allow: bool,
    ) -> Result<String, Failure> {
        let Some(agent) = self.own_agent(key, name).await? else {
            return Ok(no_such_agent(name));
        };
        let rule = match self.rule_for(key, target).await? {
            Ok(rule) => rule,
            Err(reply) => return Ok(reply),
        };
        let allows_some = allow && rule != Rule::Everyone;
        let changed = self
            .inner
            .store
            .update_agent_settings(agent.id, |settings| {
                let mut rules = match Rules::read(settings) {
                    Ok(rules) => rules,
                    Err(_) if allow && rule == Rule::Everyone => Rules::default(),
                    Err(err) => return Err(err),
                };
                let change = if allow {
                    rules.allow(rule)
                } else {
                    rules.deny(rule)
                };
                if change != Change::Full {
                    rules.write(settings);
                }
                Ok((change, rules))
            })
            .await?;
        let rules = match changed {
            Ok((Change::Full, _)) => {
                return Ok(format!(
                    "`{name}` has {MAX_RULES} rules of that kind already, the most it can have."
                ));
            }
            Ok((change, rules)) => {
                if change == Change::Changed {
                    tracing::info!(agent = %agent.id, allow, "changed an agent's rules");
                }
                rules
            }
            Err(err) => {
                tracing::warn!(agent = %agent.id, kind = ?err.classify(), "an agent's rules don't read");
                return Ok(format!(
                    "I can't read `{name}`'s rules, so it refuses everyone, you too. Send `allow \
                     {name} everyone` to clear them."
                ));
            }
        };
        let mut reply = rules.describe(name);
        if allows_some && rules.denies_everyone() {
            reply.push_str(&format!(
                " `everyone` is denied, which wins over any allow: to let only your allowed \
                 targets use it, send `allow {name} everyone` first, then allow them again."
            ));
        }
        Ok(reply)
    }

    /// The rule `target` names, from `key`'s surface and team, or the reply
    /// when it names nobody agentd can find.
    async fn rule_for(
        &self,
        key: &MemberKey,
        target: &Target,
    ) -> Result<Result<Rule, String>, Failure> {
        Ok(match target {
            Target::Everyone => Ok(Rule::Everyone),
            Target::Member(user) => {
                let written = match user {
                    UserRef::Name(name) | UserRef::Id(name) => name,
                };
                let label = format!("@{written}");
                match self.resolve_user(key, user).await? {
                    None => Err(format!("I don't know `{}`.", label.replace('`', ""))),
                    Some(id) => {
                        let identity = MemberKey {
                            user: id,
                            ..key.clone()
                        };
                        let member = self.member(&identity).await?;
                        Ok(Rule::Member {
                            key: identity,
                            member,
                            label,
                        })
                    }
                }
            }
            Target::Room(room) => {
                let written = match room {
                    RoomRef::Name(name) | RoomRef::Id(name) => name,
                };
                let label = format!("#{written}");
                match self.resolve_room(key, room).await? {
                    None => Err(format!(
                        "I don't know `{}`. Name a channel agentd can see.",
                        label.replace('`', "")
                    )),
                    Some(conversation) => Ok(Rule::Room {
                        conv: ConvRef {
                            surface: key.surface,
                            team: key.team.clone(),
                            conversation,
                        },
                        label,
                    }),
                }
            }
        })
    }

    /// The conversation `room` names on `key`'s surface, or `None` if
    /// nothing by that name is found. A Slack channel arrives as its id;
    /// a Rocket.Chat one by name, which the manager looks up.
    async fn resolve_room(
        &self,
        key: &MemberKey,
        room: &RoomRef,
    ) -> Result<Option<ConversationId>, Failure> {
        match room {
            RoomRef::Id(id) => Ok(Some(ConversationId::new(id.as_str()))),
            RoomRef::Name(name) => match self.agents_for(key) {
                Some(agents) => Ok(agents.room_named(name).await?),
                None => Ok(None),
            },
        }
    }
}

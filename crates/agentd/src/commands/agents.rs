//! The agent commands: `create`, `persona`, `list`, `pause`, `resume` and
//! `delete`.
//!
//! An agent is addressed by its name among its owner's agents, so a member
//! can only ever change their own: someone else's agent of that name is
//! "no agent of yours". On Rocket.Chat an agent is a bot user; on Slack it
//! is an app of its own (see [`slack_agents`](super::slack_agents)).

use commands::UserRef;
use core_types::{AgentId, InFile, MemberId, MemberKey, SurfaceError, SurfaceKind, UserId};
use store::{Agent, AgentCreation, AgentState, BindingState, NewAgent, StoreError, Visibility};
use time::OffsetDateTime;

use super::reply::shown_name;
use super::{Commands, Failure, Origin};
use crate::agents::{CreateError, RocketChatAgents};

/// The largest persona, in bytes: 64 KB.
pub const PERSONA_MAX_BYTES: usize = 64 * 1024;

/// The default persona, with `{name}` and `{owner}` to fill in.
const DEFAULT_PERSONA: &str = include_str!("../../assets/persona.md");

/// The default persona of the agent `name`, owned by `owner`.
pub(super) fn default_persona(name: &str, owner: &str) -> String {
    DEFAULT_PERSONA
        .replace("{name}", name)
        .replace("{owner}", owner)
}

/// Why a persona can't be used, or `None` if it can.
pub(super) fn persona_problem(persona: &str) -> Option<String> {
    if persona.trim().is_empty() {
        Some("A persona can't be empty.".to_owned())
    } else if persona.len() > PERSONA_MAX_BYTES {
        Some(format!(
            "That persona is {} bytes; the limit is {} KB.",
            persona.len(),
            PERSONA_MAX_BYTES / 1024
        ))
    } else {
        None
    }
}

/// What [`Commands::download`] got.
pub(super) enum Download {
    /// The file's bytes.
    Bytes(Vec<u8>),
    /// The file is over the limit.
    TooLarge,
    /// The command came from somewhere agentd doesn't read files from.
    NotHere,
}

pub(super) fn no_such_agent(name: &str) -> String {
    format!("You have no agent named `{name}`. Only an agent's owner can change it.")
}

impl Commands {
    /// The agents on `member`'s surface and team, if agentd manages agents
    /// there.
    pub(super) fn agents_for(&self, member: &MemberKey) -> Option<&RocketChatAgents> {
        self.inner.rocketchat.as_ref().filter(|agents| {
            member.surface == SurfaceKind::RocketChat && *agents.team() == member.team
        })
    }

    /// `member`'s agent named `name`, if they have one.
    pub(super) async fn own_agent(
        &self,
        member: &MemberKey,
        name: &str,
    ) -> Result<Option<Agent>, Failure> {
        let Some(owner) = self.member(member).await? else {
            return Ok(None);
        };
        Ok(self.inner.store.agent_by_name(owner, name).await?)
    }

    /// `key`'s member, if their Claude account is linked, or else the reply
    /// that asks them to link one.
    pub(super) async fn linked_owner(
        &self,
        key: &MemberKey,
        origin: &Origin,
    ) -> Result<Result<MemberId, String>, Failure> {
        let linked = match self.member(key).await? {
            Some(member) => self
                .inner
                .auth
                .status(member)
                .await?
                .linked
                .then_some(member),
            None => None,
        };
        Ok(linked.ok_or_else(|| {
            format!(
                "Link your Claude account first: send {}. Your agents run on it.",
                origin.command("login")
            )
        }))
    }

    pub(super) async fn create(
        &self,
        key: &MemberKey,
        name: &str,
        persona: Option<String>,
        origin: &Origin,
    ) -> Result<String, Failure> {
        if key.surface == SurfaceKind::Slack {
            return self.create_on_slack(key, name, persona, origin).await;
        }
        let Some(agents) = self.agents_for(key) else {
            return Ok("Creating agents here isn't available yet.".to_owned());
        };
        let member = match self.linked_owner(key, origin).await? {
            Ok(member) => member,
            Err(reply) => return Ok(reply),
        };
        if let Some(problem) = persona.as_deref().and_then(persona_problem) {
            return Ok(problem);
        }
        let owner = agents.username(&key.user).await?;
        self.inner
            .store
            .set_member_display_name(member, &owner)
            .await?;
        let persona = persona.unwrap_or_else(|| default_persona(name, &owner));
        let new = NewAgent {
            owner: member,
            name,
            persona: &persona,
            visibility: Visibility::Public,
            surface: SurfaceKind::RocketChat,
            team: agents.team(),
        };
        let max = agents.max_per_owner();
        let (agent, binding) = match self
            .inner
            .store
            .create_agent(&new, max, OffsetDateTime::now_utc())
            .await?
        {
            AgentCreation::Created(agent, binding) => (agent, binding),
            AgentCreation::NameTaken => {
                return Ok(format!("You already have an agent named `{name}`."));
            }
            AgentCreation::LimitReached => {
                return Ok(format!(
                    "You already have as many agents as one member may have ({max}), so I \
                     didn't create `{name}`. Delete one first with {}.",
                    origin.command("delete <name>")
                ));
            }
        };
        let bot = match agents.create_bot(binding, name, &owner).await {
            Ok(bot) => bot,
            Err(CreateError::NamesTaken(tried)) => {
                let tried: Vec<String> = tried.iter().map(|u| format!("`{u}`")).collect();
                return Ok(format!(
                    "The usernames {} are taken on this server, so I didn't create `{name}`. \
                     Pick another name.",
                    tried.join(" and ")
                ));
            }
            Err(err) => {
                tracing::warn!(agent = %agent.id, %binding, error = %err, "couldn't create an agent's bot user");
                return Ok(format!(
                    "I couldn't create a bot user for `{name}`, so I didn't create it. Please \
                     try again in a minute."
                ));
            }
        };
        agents.poke();
        let username = &bot.username;
        let mut reply = format!("Created `{name}`. Its bot user is @{username}");
        if username != name {
            reply.push_str(&format!(", since the username `{name}` isn't available"));
        }
        reply.push('.');
        let invited = match origin {
            Origin::RocketChatChannel { room } => match agents.invite(room, &bot.user).await {
                Ok(()) => true,
                Err(err) => {
                    tracing::info!(agent = %agent.id, %room, error = %err, "couldn't invite a new bot into the room");
                    false
                }
            },
            Origin::RocketChatDm { .. } | Origin::SlackSlash { .. } | Origin::SlackDm { .. } => {
                false
            }
        };
        if invited {
            reply.push_str(" I added it to the room you asked in.");
        }
        reply.push_str(&format!(
            " To use it in a room, invite @{username} there (from the room's members, or with \
             `/invite @{username}`) and mention it. You can also send it a direct message. \
             Change its persona with {}.",
            origin.command(&format!("persona {name} <text>"))
        ));
        Ok(reply)
    }

    pub(super) async fn persona(
        &self,
        key: &MemberKey,
        name: &str,
        text: Option<String>,
        origin: &Origin,
        files: &[InFile],
    ) -> Result<String, Failure> {
        let Some(agent) = self.own_agent(key, name).await? else {
            return Ok(no_such_agent(name));
        };
        let persona = match text {
            Some(text) => text,
            None => match self.attached_persona(key, origin, files).await? {
                Ok(persona) => persona,
                Err(problem) => return Ok(problem),
            },
        };
        if let Some(problem) = persona_problem(&persona) {
            return Ok(problem);
        }
        if !self
            .inner
            .store
            .set_agent_persona(agent.id, &persona)
            .await?
        {
            return Ok(no_such_agent(name));
        }
        tracing::info!(agent = %agent.id, bytes = persona.len(), "replaced an agent's persona");
        Ok(format!(
            "Replaced `{name}`'s persona. Its conversations use it from their next start."
        ))
    }

    /// The persona in the one file attached to a direct message with the
    /// manager bot, or what is wrong with it.
    async fn attached_persona(
        &self,
        key: &MemberKey,
        origin: &Origin,
        files: &[InFile],
    ) -> Result<Result<String, String>, Failure> {
        let how = "Put the persona after the name, or attach it as a `persona.md` file to that \
                   command in a direct message with me.";
        let [file] = files else {
            return Ok(Err(how.to_owned()));
        };
        if !file.name.to_ascii_lowercase().ends_with(".md") {
            return Ok(Err(format!("The persona file must be a `.md` file. {how}")));
        }
        let max = u64::try_from(PERSONA_MAX_BYTES).unwrap_or(u64::MAX);
        let bytes = match self.download(key, origin, file, max).await? {
            Download::Bytes(bytes) => bytes,
            Download::TooLarge => {
                return Ok(Err(format!(
                    "That file is over the {} KB limit.",
                    PERSONA_MAX_BYTES / 1024
                )));
            }
            Download::NotHere => return Ok(Err(how.to_owned())),
        };
        Ok(String::from_utf8(bytes).map_err(|_| "The persona file must be UTF-8 text.".to_owned()))
    }

    /// Downloads `file`, attached to a command from `key` in a direct
    /// message with a manager bot, reading at most `max` bytes: with the
    /// Rocket.Chat manager's credentials in its DM, and with the Slack
    /// manager app's bot token in its DM. Files anywhere else aren't read.
    pub(super) async fn download(
        &self,
        key: &MemberKey,
        origin: &Origin,
        file: &InFile,
        max: u64,
    ) -> Result<Download, Failure> {
        if file.size.is_some_and(|size| size > max) {
            return Ok(Download::TooLarge);
        }
        let downloaded = match origin {
            Origin::RocketChatDm { .. } => match self.agents_for(key) {
                Some(agents) => agents
                    .download(&file.id, &file.name, max)
                    .await
                    .map(|bytes| bytes.to_vec()),
                None => return Ok(Download::NotHere),
            },
            Origin::SlackDm { .. } => match &self.inner.slack {
                Some(slack) if key.surface == SurfaceKind::Slack => {
                    slack.surface().api().download_file(file, max).await
                }
                _ => return Ok(Download::NotHere),
            },
            Origin::SlackSlash { .. } | Origin::RocketChatChannel { .. } => {
                return Ok(Download::NotHere);
            }
        };
        match downloaded {
            Ok(bytes) => Ok(Download::Bytes(bytes)),
            Err(SurfaceError::TooLarge(_)) => Ok(Download::TooLarge),
            Err(err) => Err(err.into()),
        }
    }

    pub(super) async fn list(
        &self,
        key: &MemberKey,
        user: Option<&UserRef>,
        origin: &Origin,
    ) -> Result<String, Failure> {
        let asker = self.member(key).await?;
        let (owner, whose) = match user {
            None => (None, None),
            Some(user) => {
                let Some(id) = self.resolve_user(key, user).await? else {
                    return Ok("I don't know that member.".to_owned());
                };
                let identity = MemberKey {
                    user: id,
                    ..key.clone()
                };
                match self.member(&identity).await? {
                    Some(member) => (Some(member), Some(member)),
                    None => return Ok("That member has no agents.".to_owned()),
                }
            }
        };
        let entries: Vec<_> = self
            .inner
            .store
            .directory(key.surface, &key.team, owner)
            .await?
            .into_iter()
            .filter(|e| e.agent.visibility == Visibility::Public || Some(e.agent.owner) == asker)
            .collect();
        if entries.is_empty() {
            return Ok(match whose {
                Some(_) => "That member has no agents.".to_owned(),
                None => format!(
                    "There are no agents yet. Create one with {}.",
                    origin.command("create <name>")
                ),
            });
        }
        let slack = key.surface == SurfaceKind::Slack;
        if slack && let Some(agents) = &self.slack_agents {
            agents.name_managed().await;
        }
        let mut reply = String::from("Agents:");
        for entry in entries {
            let bot = if slack {
                entry.bot_user.map(|u| u.to_string())
            } else {
                entry.bot_username
            };
            let bot = bot.map_or_else(|| "no bot here".to_owned(), |u| format!("@{u}"));
            let paused = if entry.agent.state == AgentState::Paused {
                ", paused"
            } else {
                ""
            };
            reply.push_str(&format!(
                "\n- `{}` ({bot}), owned by {}{paused}",
                entry.agent.name,
                code_span(&entry.owner_name)
            ));
        }
        Ok(reply)
    }

    /// The user id `user` names on `key`'s surface, or `None` if nobody by
    /// that name exists.
    pub(super) async fn resolve_user(
        &self,
        key: &MemberKey,
        user: &UserRef,
    ) -> Result<Option<UserId>, Failure> {
        match user {
            UserRef::Id(id) => Ok(Some(UserId::new(id.as_str()))),
            UserRef::Name(name) => match self.agents_for(key) {
                Some(agents) => Ok(agents.user_named(name).await?),
                None => Ok(None),
            },
        }
    }

    pub(super) async fn set_paused(
        &self,
        key: &MemberKey,
        name: &str,
        paused: bool,
        origin: &Origin,
    ) -> Result<String, Failure> {
        let Some(agent) = self.own_agent(key, name).await? else {
            return Ok(no_such_agent(name));
        };
        let changed = self.inner.store.set_agent_paused(agent.id, paused).await?;
        self.wake_consents();
        tracing::info!(agent = %agent.id, paused, changed, "pausing or resuming an agent");
        Ok(match (paused, changed) {
            (true, true) => format!(
                "Paused `{name}`. It ignores messages until you send {}.",
                origin.command(&format!("resume {name}"))
            ),
            (true, false) => format!("`{name}` is paused already."),
            (false, true) => format!("Resumed `{name}`."),
            (false, false) => format!("`{name}` isn't paused."),
        })
    }

    pub(super) async fn delete(&self, key: &MemberKey, name: &str) -> Result<String, Failure> {
        let Some(agent) = self.own_agent(key, name).await? else {
            return Ok(no_such_agent(name));
        };
        let store = &self.inner.store;
        let bindings = store.bindings_of(agent.id).await?;
        if !store
            .delete_agent(agent.id, OffsetDateTime::now_utc())
            .await?
        {
            return Ok(no_such_agent(name));
        }
        tracing::info!(agent = %agent.id, "deleted an agent");
        self.wake_consents();
        if let Some(reply) = self.delete_on_slack(&agent, name, &bindings).await {
            return Ok(reply);
        }
        let Some(agents) = self.agents_for(key) else {
            return Ok(format!("Deleted `{name}`."));
        };
        let retired = match retire_bots(agents, agent.id).await {
            Ok(retired) => retired,
            Err(err) => {
                tracing::warn!(agent = %agent.id, error = %err, "couldn't retire a deleted agent's bot users");
                false
            }
        };
        agents.poke();
        Ok(if retired {
            format!("Deleted `{name}` and deactivated its bot user.")
        } else {
            format!(
                "Deleted `{name}`. I couldn't deactivate its bot user yet, and will keep trying."
            )
        })
    }
}

/// A member's display name `text` as a Markdown code span, so it can't
/// form a link, a mention or any other formatting, shown as
/// [`shown_name`] shows a name, its backticks left out; `someone` when
/// nothing is left.
fn code_span(text: &str) -> String {
    match shown_name(text) {
        Some(name) => format!("`{}`", name.replace('`', "").trim()),
        None => "someone".to_owned(),
    }
}

/// Retires the bot users of the deleted `agent` on `agents`' server, and
/// returns whether every bot user it had anywhere is retired now.
async fn retire_bots(agents: &RocketChatAgents, agent: AgentId) -> Result<bool, StoreError> {
    let mut retired = true;
    for binding in agents.store().bindings_of(agent).await? {
        if binding.state == BindingState::Disabled
            && binding.bot_user.is_some()
            && binding.retired_at.is_none()
        {
            let ours = binding.surface == SurfaceKind::RocketChat && binding.team == *agents.team();
            retired &= ours && agents.retire(binding.id).await?;
        }
    }
    Ok(retired)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_are_shown_as_code_that_forms_nothing() {
        let shown = code_span("[Admin](https://evil.example)");
        assert_eq!(shown, "`[Admin](https://evil.example)`");
        let rendered = render::slack::to_mrkdwn(&format!("owned by {shown}"), &NoNames);
        assert!(!rendered.contains('<'), "{rendered}");
        assert_eq!(code_span("a`b\n@here"), "`ab\u{FFFD}@here`");
        assert_eq!(code_span(" ` "), "someone");
        assert_eq!(
            code_span("ad\u{202E}nimda\u{202C} \u{2066}x\u{2069}"),
            "`ad\u{FFFD}nimda\u{FFFD} \u{FFFD}x\u{FFFD}`"
        );
        assert_eq!(
            code_span("a\u{200B}d\u{200D}a\u{FEFF}\u{200F}\u{2060}"),
            "`a\u{FFFD}da\u{FFFD}\u{FFFD}\u{FFFD}`",
            "a name can't pass for `ada` by hiding characters"
        );
        assert_eq!(code_span("\u{200B}\u{202E}"), "`\u{FFFD}\u{FFFD}`");
    }

    struct NoNames;

    impl render::MentionDirectory for NoNames {
        fn resolve(&self, _name: &str) -> Option<String> {
            None
        }
    }

    #[test]
    fn a_persona_is_non_empty_text_of_at_most_64_kb() {
        assert_eq!(persona_problem("You help."), None);
        assert_eq!(
            persona_problem(" \n\t"),
            Some("A persona can't be empty.".to_owned())
        );
        assert_eq!(persona_problem(&"x".repeat(PERSONA_MAX_BYTES)), None);
        assert_eq!(
            persona_problem(&"x".repeat(PERSONA_MAX_BYTES + 1)),
            Some("That persona is 65537 bytes; the limit is 64 KB.".to_owned())
        );
    }

    #[test]
    fn the_default_persona_names_the_agent_and_its_owner() {
        let persona = default_persona("helper", "alice");
        assert!(persona.starts_with("You are helper, an agent that alice created."));
        assert!(!persona.contains('{'));
    }
}

//! `create` and `delete` on Slack, where every agent is an app of its own
//! (see [`SlackAgents`](crate::slack::agents::SlackAgents)).

use core_types::{MemberKey, SurfaceKind};
use store::{Agent, AgentBinding};
use surface_slack::manifest::MANIFEST_VERSION;

use super::agents::{default_persona, persona_problem};
use super::{Commands, Failure, Origin};
use crate::slack::agents::{APPS_PAGE, AppDeletion, Creation};

/// An app id as shown in a reply: Slack's ids are letters and digits, and
/// anything else is left out.
fn shown(app_id: &str) -> String {
    app_id.chars().filter(char::is_ascii_alphanumeric).collect()
}

impl Commands {
    /// The `me` lines naming `key`'s agents whose Slack apps in `key`'s
    /// workspace are on an older manifest, which don't follow a private
    /// channel shared later: those agentd still updates, and those it
    /// can't; `None` when there are none.
    pub(super) async fn outdated_apps_status(
        &self,
        key: &MemberKey,
        origin: &Origin,
    ) -> Result<Option<String>, Failure> {
        let Some(member) = self.member(key).await? else {
            return Ok(None);
        };
        let outdated = self
            .inner
            .store
            .outdated_slack_apps(member, &key.team, MANIFEST_VERSION)
            .await?;
        let list = |blocked: bool| {
            outdated
                .iter()
                .filter(|app| app.blocked == blocked)
                .map(|app| format!("`{}`", app.agent_name.replace('`', "")))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut lines = Vec::new();
        let (waiting, blocked) = (list(false), list(true));
        if !waiting.is_empty() {
            lines.push(format!(
                "Agents whose Slack apps I haven't updated yet: {waiting}. Until I do, they won't \
                 follow a private channel that is shared with another organization: their rules \
                 on it stop applying. I update them with your configuration token ({}) and try \
                 again every hour.",
                origin.command("slack-token <token> <refresh token>")
            ));
        }
        if !blocked.is_empty() {
            lines.push(format!(
                "Agents whose Slack apps I can't update: {blocked}. Slack says the app is gone, \
                 or it subscribes to no events. They won't follow a private channel that is \
                 shared with another organization; to fix that, delete the agent and create it \
                 again."
            ));
        }
        Ok((!lines.is_empty()).then(|| lines.join("\n")))
    }

    /// `create` on Slack: the agent and its app, and an install link in a
    /// DM.
    pub(super) async fn create_on_slack(
        &self,
        key: &MemberKey,
        name: &str,
        persona: Option<String>,
        origin: &Origin,
    ) -> Result<String, Failure> {
        let Some(agents) = self
            .slack_agents
            .as_ref()
            .filter(|agents| *agents.team() == key.team)
        else {
            return Ok("Creating agents here isn't available yet.".to_owned());
        };
        if render::slack::BROADCASTS.contains(&name) {
            return Ok(format!(
                "Slack reads `@{name}` as a message to everyone, so an agent can't be called \
                 `{name}` here. Pick another name."
            ));
        }
        let member = match self.linked_owner(key, origin).await? {
            Ok(member) => member,
            Err(reply) => return Ok(reply),
        };
        if let Some(problem) = persona.as_deref().and_then(persona_problem) {
            return Ok(problem);
        }
        let owner = agents.display_name(&key.user).await;
        self.inner
            .store
            .set_member_display_name(member, &owner)
            .await?;
        let persona = persona.unwrap_or_else(|| default_persona(name, &owner));
        let register = origin.command("slack-token <token> <refresh token>");
        let installed = "Once it is installed, I'll tell you how to invite it to a channel.";
        Ok(match agents.create(member, key, name, &persona).await? {
            Creation::Created { dm_sent: true, .. } => format!(
                "Created `{name}` as a Slack app. I sent you a direct message with the link that \
                 installs it. {installed}"
            ),
            Creation::Created {
                install_url,
                dm_sent: false,
            } => format!(
                "Created `{name}` as a Slack app. [Install {name}]({install_url}) in this \
                 workspace. {installed}"
            ),
            Creation::NameTaken => format!("You already have an agent named `{name}`."),
            Creation::LimitReached => format!(
                "You already have as many agents as one member may have ({}), so I didn't \
                 create `{name}`. Delete one first with {}.",
                agents.max_per_owner(),
                origin.command("delete <name>")
            ),
            Creation::NoPublicUrl => "Creating agents on Slack needs agentd's public URL, which \
                                      isn't set. Ask whoever runs agentd to set \
                                      `[slack] public_url`."
                .to_owned(),
            Creation::NoConfigToken => format!(
                "Each agent on Slack is an app I create as you, with your app configuration \
                 token, and I don't have one that works. Generate one at {APPS_PAGE} (\"Your App \
                 Configuration Tokens\"), send it with {register}, then create `{name}` again."
            ),
            Creation::TokenRenewing => format!(
                "Your app configuration token expired, and I'm renewing it, so I didn't create \
                 `{name}` yet. Try again in a minute."
            ),
            Creation::TokenRefused => format!(
                "Slack refused your configuration token, so I didn't create `{name}`. Generate a \
                 new one at {APPS_PAGE}, send it with {register}, and try again."
            ),
            Creation::Refused(code) if code == "invalid_manifest" => format!(
                "Slack refused to create an app for `{name}` (`invalid_manifest`), so I didn't \
                 create it. Slack checks that it can reach agentd while it creates the app: ask \
                 whoever runs agentd to check that `[slack] public_url` is reachable from Slack."
            ),
            Creation::Refused(code) => format!(
                "Slack refused to create an app for `{name}` (`{}`), so I didn't create it.",
                code.replace('`', "")
            ),
            Creation::Failed => format!(
                "I couldn't create a Slack app for `{name}`, so I didn't create it. Please try \
                 again in a minute."
            ),
        })
    }

    /// The reply to deleting `agent`, named `name`, whose `bindings` were
    /// read before it was deleted, when it was on Slack: its apps are
    /// deleted with its owner's configuration token, or the owner is told
    /// to delete them. `None` for an agent with no Slack binding.
    pub(super) async fn delete_on_slack(
        &self,
        agent: &Agent,
        name: &str,
        bindings: &[AgentBinding],
    ) -> Option<String> {
        if !bindings
            .iter()
            .any(|binding| binding.surface == SurfaceKind::Slack)
        {
            return None;
        }
        let Some(agents) = &self.slack_agents else {
            return Some(format!(
                "Deleted `{name}`. Delete its Slack app yourself at {APPS_PAGE}."
            ));
        };
        let deletions = agents.delete_apps(agent.owner, bindings).await;
        let left = deletions.iter().find_map(|deletion| match deletion {
            AppDeletion::Deleted => None,
            AppDeletion::NoToken(app) => Some((Some(app), true)),
            AppDeletion::Failed(app) => Some((app.as_ref(), false)),
        });
        let Some((app, no_token)) = left else {
            return Some(if deletions.is_empty() {
                format!("Deleted `{name}`.")
            } else {
                format!("Deleted `{name}` and its Slack app.")
            });
        };
        let why = if no_token {
            "deleting its Slack app needs your configuration token, and I don't have one that \
             works"
        } else {
            "deleting its Slack app failed"
        };
        let page = match app {
            Some(app) => format!("{APPS_PAGE}/{}", shown(app)),
            None => APPS_PAGE.to_owned(),
        };
        Some(format!(
            "Deleted `{name}`: it no longer answers, but {why}. Delete the app yourself at \
             {page}."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_are_shown_as_letters_and_digits() {
        assert_eq!(shown("A0AGENT01"), "A0AGENT01");
        assert_eq!(shown("A0`x /y"), "A0xy");
    }
}

//! `create` and `delete` on Slack, where every agent is an app of its own
//! (see [`SlackAgents`](crate::slack::agents::SlackAgents)).

use core_types::{MemberKey, SurfaceKind};
use store::Agent;

use super::agents::{default_persona, persona_problem};
use super::{Commands, Failure, Origin};
use crate::slack::agents::{APPS_PAGE, AppDeletion, Creation};

/// An app id as shown in a reply: Slack's ids are letters and digits, and
/// anything else is left out.
fn shown(app_id: &str) -> String {
    app_id.chars().filter(char::is_ascii_alphanumeric).collect()
}

impl Commands {
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
            .inner
            .slack_agents
            .as_ref()
            .filter(|agents| *agents.team() == key.team)
        else {
            return Ok("Creating agents here isn't available yet.".to_owned());
        };
        if !agents.can_create() {
            return Ok(
                "Creating agents on Slack needs agentd's public URL, which isn't set. Ask whoever \
                 runs agentd to set `[slack] public_url`."
                    .to_owned(),
            );
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
        let invite = format!(
            "Once it is installed, invite it to a channel with `/invite @{name}` and mention it \
             there: it only hears the channels it's in. You can also send it a direct message."
        );
        Ok(match agents.create(member, key, name, &persona).await? {
            Creation::Created { dm_sent: true, .. } => format!(
                "Created `{name}` as a Slack app. I sent you a direct message with the link that \
                 installs it. {invite}"
            ),
            Creation::Created {
                install_url,
                dm_sent: false,
            } => format!(
                "Created `{name}` as a Slack app. Install it with this link:\n{install_url}\n\n\
                 {invite}"
            ),
            Creation::NameTaken => format!("You already have an agent named `{name}`."),
            Creation::LimitReached => format!(
                "You already have as many agents as one member may have ({}), so I didn't \
                 create `{name}`. Delete one first with {}.",
                agents.max_per_owner(),
                origin.command("delete <name>")
            ),
            Creation::NoConfigToken => format!(
                "Each agent on Slack is an app I create as you, with your app configuration \
                 token, and I don't have one that works. Generate one at {APPS_PAGE} (\"Your App \
                 Configuration Tokens\"), send it with {register}, then create `{name}` again."
            ),
            Creation::TokenRefused => format!(
                "Slack refused your configuration token, so I didn't create `{name}`. Generate a \
                 new one at {APPS_PAGE}, send it with {register}, and try again."
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

    /// The reply to deleting `agent`, named `name`, when it was on Slack:
    /// its apps are deleted with its owner's configuration token, or the
    /// owner is told to delete them. `None` for an agent with no Slack
    /// binding.
    pub(super) async fn delete_on_slack(
        &self,
        agent: &Agent,
        name: &str,
    ) -> Result<Option<String>, Failure> {
        let bindings = self.inner.store.bindings_of(agent.id).await?;
        if !bindings
            .iter()
            .any(|binding| binding.surface == SurfaceKind::Slack)
        {
            return Ok(None);
        }
        let Some(agents) = &self.inner.slack_agents else {
            return Ok(Some(format!(
                "Deleted `{name}`. Delete its Slack app yourself at {APPS_PAGE}."
            )));
        };
        let deletions = agents.delete_apps(agent.owner, &bindings).await?;
        let left: Vec<(&String, bool)> = deletions
            .iter()
            .filter_map(|deletion| match deletion {
                AppDeletion::Deleted => None,
                AppDeletion::NoToken(app) => Some((app, true)),
                AppDeletion::Failed(app) => Some((app, false)),
            })
            .collect();
        let Some(&(app, no_token)) = left.first() else {
            return Ok(Some(if deletions.is_empty() {
                format!("Deleted `{name}`.")
            } else {
                format!("Deleted `{name}` and its Slack app.")
            }));
        };
        let why = if no_token {
            "deleting its Slack app needs your configuration token, and I don't have one that \
             works"
        } else {
            "deleting its Slack app failed"
        };
        Ok(Some(format!(
            "Deleted `{name}`: it no longer answers, but {why}. Delete the app yourself at \
             {APPS_PAGE}/{}.",
            shown(app)
        )))
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

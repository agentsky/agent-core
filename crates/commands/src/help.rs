//! Usage text for every command, the one source for `help`, for
//! [`Command::help`](crate::Command::help) and for the usage line in parse
//! errors.

/// One command: its words, usage, a one-line description, and how many
/// positional arguments come before a free-text tail, if it has one.
#[derive(Debug)]
pub(crate) struct Spec {
    pub(crate) name: &'static str,
    usage: &'static str,
    about: &'static str,
    pub(crate) tail_after: Option<usize>,
}

impl Spec {
    const fn new(name: &'static str, usage: &'static str, about: &'static str) -> Self {
        Self {
            name,
            usage,
            about,
            tail_after: None,
        }
    }

    const fn with_tail_after(mut self, args: usize) -> Self {
        self.tail_after = Some(args);
        self
    }

    /// The command's words, for example `["admin", "api-key", "set"]`.
    pub(crate) fn words(&self) -> impl Iterator<Item = &'static str> {
        self.name.split(' ')
    }

    /// The first word, which `help <command>` matches.
    pub(crate) fn group(&self) -> &'static str {
        self.name.split(' ').next().unwrap_or(self.name)
    }

    /// One Markdown line: the usage as code, then the description.
    pub(crate) fn line(&self) -> String {
        format!("`{}`: {}", self.usage, self.about)
    }

    pub(crate) fn usage_line(&self) -> String {
        format!("Usage: `{}`", self.usage)
    }
}

pub(crate) const SPECS: &[Spec] = &[
    Spec::new(
        "login",
        "login [code]",
        "link your Claude account; run it again with the code the login page shows",
    ),
    Spec::new("logout", "logout", "unlink your Claude account"),
    Spec::new("me", "me", "show your link status, usage and agents"),
    Spec::new(
        "slack-token",
        "slack-token <token> <refresh-token>",
        "register your Slack app configuration token and its refresh token",
    ),
    Spec::new(
        "create",
        "create <name> [persona]",
        "create an agent; the rest of the line is its persona",
    )
    .with_tail_after(1),
    Spec::new(
        "persona",
        "persona <name> [text]",
        "replace an agent's persona with the rest of the line, \
         or with a persona.md attached to a direct message with me",
    )
    .with_tail_after(1),
    Spec::new(
        "skill add",
        "skill add <name> [source]",
        "add a skill to an agent from a Git URL (optionally ending in #ref), \
         or from a SKILL.md or .zip attached to a direct message with me",
    ),
    Spec::new(
        "skill rm",
        "skill rm <name> <skill>",
        "remove a skill from an agent",
    ),
    Spec::new(
        "allow",
        "allow <name> <target>",
        "let @member, #channel or everyone use an agent",
    ),
    Spec::new(
        "deny",
        "deny <name> <target>",
        "stop @member, #channel or everyone from using an agent; deny wins over allow",
    ),
    Spec::new(
        "limits",
        "limits <name> [turns=N/day] [hops=N]",
        "set an agent's daily turn limit and agent-to-agent hop limit, in either order",
    ),
    Spec::new("pause", "pause <name>", "stop an agent from answering"),
    Spec::new("resume", "resume <name>", "let a paused agent answer again"),
    Spec::new(
        "delete",
        "delete <name>",
        "delete an agent and deactivate its bot",
    ),
    Spec::new(
        "sessions",
        "sessions <name>",
        "show an agent's active and recent sessions",
    ),
    Spec::new(
        "reset",
        "reset <name> [here]",
        "reset every session of an agent, or with `here` only this conversation's",
    ),
    Spec::new(
        "list",
        "list [@member]",
        "list agents, or one member's agents",
    ),
    Spec::new(
        "admin api-key set",
        "admin api-key set <key>",
        "set the community API key (admins)",
    ),
    Spec::new(
        "admin api-key clear",
        "admin api-key clear",
        "remove the community API key (admins)",
    ),
    Spec::new(
        "admin ban",
        "admin ban <@member> [reason]",
        "stop a member from using agents and commands other than `me` (admins)",
    )
    .with_tail_after(1),
    Spec::new(
        "admin unban",
        "admin unban <@member>",
        "lift a ban (admins)",
    ),
    Spec::new(
        "admin slack",
        "admin slack",
        "Slack configuration (admins; not available yet)",
    ),
    Spec::new(
        "approve",
        "approve <consent-id>",
        "approve a private task someone asked one of your agents for",
    ),
    Spec::new("decline", "decline <consent-id>", "decline a private task"),
    Spec::new("help", "help [command]", "show this help, or one command's"),
];

/// The spec named `name`. Every name the crate asks for is in [`SPECS`].
pub(crate) fn spec(name: &str) -> &'static Spec {
    SPECS
        .iter()
        .find(|spec| spec.name == name)
        .unwrap_or(&SPECS[SPECS.len() - 1])
}

/// Every spec whose first word is `group`.
pub(crate) fn group(group: &str) -> impl Iterator<Item = &'static Spec> {
    SPECS.iter().filter(move |spec| spec.group() == group)
}

/// The help text for every command, as Markdown.
///
/// ```
/// let text = commands::help();
/// assert!(text.contains("`login [code]`"));
/// ```
pub fn help() -> String {
    let mut text = String::from("Commands:");
    for spec in SPECS {
        text.push_str("\n- ");
        text.push_str(&spec.line());
    }
    text
}

/// The help text for the commands whose first word is `group`, or `None`
/// when there are none.
pub(crate) fn group_help(group_name: &str) -> Option<String> {
    let lines: Vec<String> = group(group_name).map(Spec::line).collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The usage of every command whose words start with `prefix`.
pub(crate) fn prefix_usage(prefix: &[&str]) -> String {
    let usages: Vec<String> = SPECS
        .iter()
        .filter(|spec| spec.words().take(prefix.len()).eq(prefix.iter().copied()))
        .map(|spec| format!("`{}`", spec.usage))
        .collect();
    format!("Usage: {}", usages.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_names_are_unique_and_usages_start_with_them() {
        for (i, spec) in SPECS.iter().enumerate() {
            assert!(spec.usage.starts_with(spec.name), "{}", spec.name);
            assert!(
                SPECS[..i].iter().all(|other| other.name != spec.name),
                "{} twice",
                spec.name
            );
        }
    }

    #[test]
    fn help_lists_every_command() {
        let text = help();
        for spec in SPECS {
            assert!(text.contains(&spec.line()), "{}", spec.name);
        }
    }

    #[test]
    fn group_help_and_usage() {
        let admin = group_help("admin").unwrap();
        assert_eq!(admin.lines().count(), 5);
        assert!(group_help("nope").is_none());
        assert_eq!(
            prefix_usage(&["skill"]),
            "Usage: `skill add <name> [source]`, `skill rm <name> <skill>`"
        );
        assert_eq!(
            prefix_usage(&["admin", "api-key"]),
            "Usage: `admin api-key set <key>`, `admin api-key clear`"
        );
    }

    #[test]
    fn unknown_spec_falls_back_to_help() {
        assert_eq!(spec("nope").name, "help");
    }
}

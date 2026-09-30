//! The `/agent` command grammar for agent-core.
//!
//! [`parse`] turns command text into a [`Command`]. The text is whatever
//! follows `/agent` in a Slack slash command, the whole text of a direct
//! message to the manager bot, or what follows `!agent` in a Rocket.Chat
//! channel. [`strip_prefix`] picks it out of the last two, so every surface
//! feeds the same parser and the command set is identical everywhere.
//!
//! ```
//! use commands::{Command, parse};
//!
//! let command = parse("persona helper You are terse.\nAnswer in English.").unwrap();
//! let Command::Persona { name, text } = command else { panic!() };
//! assert_eq!(name.as_str(), "helper");
//! assert_eq!(text.as_deref(), Some("You are terse.\nAnswer in English."));
//! ```
//!
//! # Grammar
//!
//! - The text is split into words at whitespace (any Unicode white space,
//!   including line breaks). Quotes are ordinary characters: there is no
//!   quoting and no escaping, so text pasted from a chat client means what
//!   it says.
//! - Command words (`persona`, `skill add`, `admin api-key set`) match
//!   regardless of case, since phones capitalize the first word of a
//!   message. Arguments keep their case.
//! - A free-text argument (`create`'s persona, `persona`'s text,
//!   `admin ban`'s reason) takes the rest of the text verbatim, line breaks
//!   and quotes included, with only the surrounding white space trimmed.
//! - A member is `@name`, a channel `#name`. Slack delivers mentions it
//!   recognizes as `<@U123|name>` and `<#C123|name>` tokens; both forms are
//!   accepted, see [`UserRef`], [`RoomRef`] and [`Target`]. A skill source
//!   is an `https://` Git URL with an optional `#ref`; a Slack link token
//!   `<url|label>` becomes its URL.
//! - `help`, `help <command>` and empty text ask for help, and an unknown
//!   command gets the full help text. Both come back as a [`ParseError`],
//!   whose message the caller replies with.
//!
//! The parser works on plain text. The Slack surface decodes the `&amp;`,
//! `&lt;` and `&gt;` entities in message text before calling it; mention
//! and link tokens are recognized either way.
//!
//! # Secrets
//!
//! Login codes, Slack configuration tokens and the community API key are
//! held as [`SecretString`], so a [`Command`]'s `Debug` output redacts them.
//! [`Command::is_secret_bearing`] tells callers when to refuse a command sent
//! in a channel, and [`ParseError::is_secret_bearing`] says the same of text
//! that failed to parse. Parse errors never repeat the text they were given.
//!
//! The crate does no I/O.

use core_types::{ConsentId, ConvKind};
use secrecy::SecretString;

mod help;
mod names;
mod parse;

pub use help::help;
pub use names::{AgentName, RoomRef, SkillName, Target, UserRef};
pub use parse::parse;

/// A parsed `/agent` command. Handlers live in agentd.
#[derive(Debug, Clone)]
pub enum Command {
    /// `login [code]`: start linking a Claude account, or finish with the
    /// code the login page shows. Secret-bearing with a code.
    Login {
        /// The authorization code from the login page.
        code: Option<SecretString>,
    },
    /// `logout`: unlink the member's Claude account.
    Logout,
    /// `me`: link status, usage, own agents and the manager app's name.
    Me,
    /// `slack-token <token> <refresh-token>`: register a Slack app
    /// configuration token. Secret-bearing.
    SlackToken {
        /// The configuration access token.
        token: SecretString,
        /// Its refresh token.
        refresh: SecretString,
    },
    /// `create <name> [persona]`: create an agent.
    Create {
        /// The new agent's name.
        name: AgentName,
        /// Its persona, the rest of the text verbatim. `None` means the
        /// default persona.
        persona: Option<String>,
    },
    /// `persona <name> [text]`: replace an agent's persona.
    Persona {
        /// The agent.
        name: AgentName,
        /// The new persona, the rest of the text verbatim. `None` means the
        /// persona comes from a `persona.md` attached to the message.
        text: Option<String>,
    },
    /// `skill add …` and `skill rm …`.
    Skill(SkillCommand),
    /// `allow <name> <target>`: let a member, a channel or everyone use an
    /// agent.
    Allow {
        /// The agent.
        name: AgentName,
        /// Who is allowed.
        target: Target,
    },
    /// `deny <name> <target>`: stop a member, a channel or everyone from
    /// using an agent.
    Deny {
        /// The agent.
        name: AgentName,
        /// Who is denied.
        target: Target,
    },
    /// `limits <name> [turns=N/day] [hops=N]`, the settings in either order.
    /// At least one is present; a missing one keeps its current value.
    Limits {
        /// The agent.
        name: AgentName,
        /// Turns per day, from `turns=N/day` (or `turns=N`).
        turns_per_day: Option<u32>,
        /// The agent-to-agent hop limit, from `hops=N`.
        hops: Option<u8>,
    },
    /// `pause <name>`.
    Pause {
        /// The agent.
        name: AgentName,
    },
    /// `resume <name>`.
    Resume {
        /// The agent.
        name: AgentName,
    },
    /// `delete <name>`: delete the agent and deactivate its bot identity.
    Delete {
        /// The agent.
        name: AgentName,
    },
    /// `sessions <name>`: the agent's active and recent sessions.
    Sessions {
        /// The agent.
        name: AgentName,
    },
    /// `reset <name> [here]`: reset every session of the agent, or with
    /// `here` only the current conversation's.
    Reset {
        /// The agent.
        name: AgentName,
        /// Whether `here` was given.
        here: bool,
    },
    /// `list [@member]`: the agent directory, or one member's agents.
    List {
        /// The member whose agents to list.
        user: Option<UserRef>,
    },
    /// `admin …`, for community admins.
    Admin(AdminCommand),
    /// `approve <consent-id>`: the text form of a consent card's Approve
    /// button.
    Approve {
        /// The consent being decided.
        consent: ConsentId,
    },
    /// `decline <consent-id>`: the text form of a consent card's Decline
    /// button.
    Decline {
        /// The consent being decided.
        consent: ConsentId,
    },
}

/// `skill add` and `skill rm`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillCommand {
    /// `skill add <name> [source]`. The skill's own name comes from its
    /// `SKILL.md`.
    Add {
        /// The agent.
        name: AgentName,
        /// An `https://` Git URL, optionally ending in `#ref`. The parser
        /// refuses any other form, so `git clone` can't read the source or
        /// its ref as an option, and a URL carrying credentials is refused
        /// too. `None` means the skill is a `SKILL.md` or `.zip` attached to
        /// the message.
        source: Option<String>,
    },
    /// `skill rm <name> <skill>`.
    Rm {
        /// The agent.
        name: AgentName,
        /// The skill to remove.
        skill: SkillName,
    },
}

/// `admin …` commands.
#[derive(Debug, Clone)]
pub enum AdminCommand {
    /// `admin api-key set <key>` and `admin api-key clear`.
    ApiKey(ApiKeyCommand),
    /// `admin ban <@member> [reason]`.
    Ban {
        /// The member to ban.
        user: UserRef,
        /// Why, the rest of the text verbatim.
        reason: Option<String>,
    },
    /// `admin unban <@member>`.
    Unban {
        /// The member to unban.
        user: UserRef,
    },
    /// `admin slack`: a placeholder for Slack configuration commands, which
    /// no task defines yet. It takes no arguments.
    Slack,
}

/// `admin api-key set` and `admin api-key clear`.
#[derive(Debug, Clone)]
pub enum ApiKeyCommand {
    /// `admin api-key set <key>`. Secret-bearing.
    Set {
        /// The community API key.
        key: SecretString,
    },
    /// `admin api-key clear`.
    Clear,
}

impl Command {
    /// Whether the command carries a secret: `login <code>`, `slack-token`
    /// and `admin api-key set`.
    ///
    /// Callers refuse such a command outside a private channel and never
    /// log its arguments.
    pub fn is_secret_bearing(&self) -> bool {
        matches!(
            self,
            Command::Login { code: Some(_) }
                | Command::SlackToken { .. }
                | Command::Admin(AdminCommand::ApiKey(ApiKeyCommand::Set { .. }))
        )
    }

    /// The command's words, such as `"skill add"`, for logs: they carry no
    /// arguments, and so no secrets or message text.
    pub fn name(&self) -> &'static str {
        match self {
            Command::Login { .. } => "login",
            Command::Logout => "logout",
            Command::Me => "me",
            Command::SlackToken { .. } => "slack-token",
            Command::Create { .. } => "create",
            Command::Persona { .. } => "persona",
            Command::Skill(SkillCommand::Add { .. }) => "skill add",
            Command::Skill(SkillCommand::Rm { .. }) => "skill rm",
            Command::Allow { .. } => "allow",
            Command::Deny { .. } => "deny",
            Command::Limits { .. } => "limits",
            Command::Pause { .. } => "pause",
            Command::Resume { .. } => "resume",
            Command::Delete { .. } => "delete",
            Command::Sessions { .. } => "sessions",
            Command::Reset { .. } => "reset",
            Command::List { .. } => "list",
            Command::Admin(AdminCommand::ApiKey(ApiKeyCommand::Set { .. })) => "admin api-key set",
            Command::Admin(AdminCommand::ApiKey(ApiKeyCommand::Clear)) => "admin api-key clear",
            Command::Admin(AdminCommand::Ban { .. }) => "admin ban",
            Command::Admin(AdminCommand::Unban { .. }) => "admin unban",
            Command::Admin(AdminCommand::Slack) => "admin slack",
            Command::Approve { .. } => "approve",
            Command::Decline { .. } => "decline",
        }
    }

    /// Short usage text for this command, as one Markdown line.
    ///
    /// ```
    /// let command = commands::parse("pause helper").unwrap();
    /// assert_eq!(command.help(), "`pause <name>`: stop an agent from answering");
    /// ```
    pub fn help(&self) -> String {
        help::spec(self.name()).line()
    }
}

/// Picks the command text out of a Rocket.Chat message, or returns `None`
/// when the message isn't a command.
///
/// In a direct message with the manager bot (`ConvKind::Dm`) the whole text
/// is the command; a leading `!agent` is dropped too, for members who type it
/// out of habit. Elsewhere only text starting with `!agent` is a command,
/// and the rest is returned. `!agent` matches regardless of case and must be
/// followed by white space or the end of the text.
///
/// ```
/// use commands::strip_prefix;
/// use core_types::ConvKind;
///
/// assert_eq!(strip_prefix("!agent me", ConvKind::Channel), Some("me"));
/// assert_eq!(strip_prefix("hello", ConvKind::Channel), None);
/// assert_eq!(strip_prefix("me", ConvKind::Dm), Some("me"));
/// ```
pub fn strip_prefix(text: &str, kind: ConvKind) -> Option<&str> {
    const PREFIX: &str = "!agent";
    let text = text.trim_start();
    let rest = text
        .get(..PREFIX.len())
        .filter(|head| head.eq_ignore_ascii_case(PREFIX))
        .map(|_| &text[PREFIX.len()..])
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace));
    match (rest, kind) {
        (Some(rest), _) => Some(rest.trim_start()),
        (None, ConvKind::Dm) => Some(text),
        (None, ConvKind::GroupDm | ConvKind::Channel) => None,
    }
}

/// Why text didn't parse as a command. Its `Display` is the reply for the
/// member, as Markdown.
///
/// The message never repeats the text it was given, which may hold a
/// secret, so it is safe to show and to log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ParseError {
    kind: ParseErrorKind,
    message: String,
    secret_bearing: bool,
}

/// What kind of [`ParseError`] it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ParseErrorKind {
    /// The text was empty or asked for `help`. The message is the help text.
    Help,
    /// The first word isn't a command. The message is the help text.
    UnknownCommand,
    /// A known command with a missing, extra or malformed argument. The
    /// message says what was wrong and gives the command's usage.
    Invalid,
}

impl ParseError {
    pub(crate) fn new(kind: ParseErrorKind, message: String) -> Self {
        Self {
            kind,
            message,
            secret_bearing: false,
        }
    }

    /// What kind of error it is.
    pub fn kind(&self) -> ParseErrorKind {
        self.kind
    }

    /// Whether the text that didn't parse looks like it holds a secret.
    ///
    /// That is the case when a word naming a secret (`login`, `api-key` or
    /// `slack-token`, also misspelt as `apikey`, `api_key`, `slack_token`
    /// or `slacktoken`) is followed by a value, anywhere in the text: a
    /// secret-bearing command with extra or missing words, or with a
    /// misspelt or missing command word such as `api-key set <key>` without
    /// `admin`. It is also the case when any word holds a known token prefix
    /// (`sk-ant-`, `xoxb-`, `xoxp-`, `xoxe.`, `xoxe-` or `xapp-`), and for
    /// unknown commands and help requests too.
    ///
    /// The secret may still be in the text, so callers apply the same
    /// channel rules as for [`Command::is_secret_bearing`]. The heuristic
    /// errs towards caution: `how do I login here` counts.
    pub fn is_secret_bearing(&self) -> bool {
        self.secret_bearing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_prefix_in_channels_needs_the_prefix() {
        for kind in [ConvKind::Channel, ConvKind::GroupDm] {
            assert_eq!(strip_prefix("!agent login", kind), Some("login"));
            assert_eq!(strip_prefix("  !AGENT\n  me ", kind), Some("me "));
            assert_eq!(strip_prefix("!agent", kind), Some(""));
            assert_eq!(strip_prefix("!agents me", kind), None);
            assert_eq!(strip_prefix("!agen", kind), None);
            assert_eq!(strip_prefix("hi !agent me", kind), None);
            assert_eq!(strip_prefix("login", kind), None);
            assert_eq!(strip_prefix("", kind), None);
            assert_eq!(strip_prefix("!agenté", kind), None);
        }
    }

    #[test]
    fn strip_prefix_in_dms_takes_the_whole_text() {
        assert_eq!(strip_prefix("login", ConvKind::Dm), Some("login"));
        assert_eq!(strip_prefix("!agent login", ConvKind::Dm), Some("login"));
        assert_eq!(strip_prefix("!agentx", ConvKind::Dm), Some("!agentx"));
        assert_eq!(strip_prefix("", ConvKind::Dm), Some(""));
        assert_eq!(strip_prefix("é", ConvKind::Dm), Some("é"));
    }

    #[test]
    fn parse_error_accessors() {
        let err = ParseError::new(ParseErrorKind::Invalid, "Nope.".into());
        assert_eq!(err.kind(), ParseErrorKind::Invalid);
        assert!(!err.is_secret_bearing());
        assert_eq!(err.to_string(), "Nope.");
    }
}

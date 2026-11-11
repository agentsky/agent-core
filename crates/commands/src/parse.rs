//! Tokenizing chat text and parsing it with clap.
//!
//! Chat text isn't a shell command line, so clap never sees it raw. The text
//! is split at white space, the command words are matched against the
//! [specs](crate::help::SPECS) regardless of case, and a free-text tail is
//! cut from the original text verbatim and handed to clap as one argument.
//! clap then checks arity and runs the value parsers, and its errors are
//! reworded without the offending text.

use std::error::Error as _;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use core_types::ConsentId;
use secrecy::SecretString;

use crate::help::{self, SPECS, Spec};
use crate::names::{
    Reason, parse_agent_name, parse_skill_name, parse_skill_source, parse_target, parse_user,
};
use crate::{
    AdminCommand, AgentName, ApiKeyCommand, Command, ParseError, ParseErrorKind, Setting,
    SkillCommand, SkillName, Target, UserRef,
};

/// Parses command text: the text after `/agent`, the whole text of a direct
/// message to the manager bot, or the text after `!agent` (see
/// [`strip_prefix`](crate::strip_prefix)).
///
/// The [crate docs](crate#grammar) describe the grammar.
///
/// # Errors
///
/// A [`ParseError`] whose message is the reply for the member: the help
/// text for empty text, `help` and unknown commands, and otherwise what was
/// wrong and the command's usage.
///
/// ```
/// use commands::{Command, ParseErrorKind, parse};
///
/// assert!(matches!(parse("reset helper here"), Ok(Command::Reset { here: true, .. })));
/// let err = parse("reset").unwrap_err();
/// assert_eq!(err.kind(), ParseErrorKind::Invalid);
/// assert_eq!(err.to_string(), "Missing `<name>`.\nUsage: `reset <name> [here]`");
/// ```
pub fn parse(text: &str) -> Result<Command, ParseError> {
    let tokens = tokenize(text);
    parse_tokens(text, &tokens).map_err(|err| ParseError {
        secret_bearing: looks_secret_bearing(&tokens),
        ..err
    })
}

fn parse_tokens(text: &str, tokens: &[Token<'_>]) -> Result<Command, ParseError> {
    let Some(first) = tokens.first() else {
        return Err(ParseError::new(ParseErrorKind::Help, help::help()));
    };
    if first.text.eq_ignore_ascii_case("help") {
        return Err(help_topic(tokens.get(1)));
    }
    let words = command_words(tokens);
    if words.is_empty() {
        return Err(unknown_command());
    }
    let spec = SPECS
        .iter()
        .find(|spec| spec.words().eq(words.iter().copied()));
    let args = clap_args(text, tokens, &words, spec);
    let invalid = |problem: String| {
        let usage = spec.map_or_else(|| help::prefix_usage(&words), Spec::usage_line);
        ParseError::new(ParseErrorKind::Invalid, format!("{problem}\n{usage}"))
    };
    if args[words.len()..].contains(&"--") {
        return Err(invalid("A lone `--` isn't an argument.".to_owned()));
    }
    let cli = clap_command()
        .try_get_matches_from(args)
        .and_then(|matches| Cli::from_arg_matches(&matches))
        .map_err(|err| invalid(clap_problem(&err)))?;
    cli.command
        .into_command()
        .map_err(|reason| invalid(reason.0.to_owned()))
}

/// A word of the input and where it starts, so a free-text tail can be cut
/// from the original text.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Token<'a> {
    start: usize,
    text: &'a str,
}

fn tokenize(text: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                tokens.push(Token {
                    start: s,
                    text: &text[s..i],
                });
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        tokens.push(Token {
            start: s,
            text: &text[s..],
        });
    }
    tokens
}

/// The longest run of leading tokens that spells the start of some command,
/// as the specs' lowercase words.
fn command_words(tokens: &[Token<'_>]) -> Vec<&'static str> {
    let mut words: Vec<&'static str> = Vec::new();
    for token in tokens {
        let next = SPECS
            .iter()
            .filter(|spec| spec.words().take(words.len()).eq(words.iter().copied()))
            .find_map(|spec| {
                spec.words()
                    .nth(words.len())
                    .filter(|word| word.eq_ignore_ascii_case(token.text))
            });
        match next {
            Some(word) => words.push(word),
            None => break,
        }
    }
    words
}

/// Whether text that failed to parse may hold a secret, so callers treat
/// it like a secret-bearing command.
///
/// It does when a word naming a secret (`login`, `api-key` or
/// `slack-token`, ignoring case, `-` and `_`, and surrounding punctuation)
/// is followed by anything but a bare `set` or `clear`, wherever the word
/// stands, so misspelt commands such as `api-key set <key>` without
/// `admin`, `slack_token <token>` or `admin apikey set <key>` count. It also
/// does when any word holds a known token prefix (`sk-ant-`, `xoxb-`,
/// `xoxp-`, `xoxe.`, `xoxe-`, `xapp-`), or has the shape of a pasted login
/// code, `<code>#<state>` or a `code=` query parameter, whatever the command.
fn looks_secret_bearing(tokens: &[Token<'_>]) -> bool {
    const TOKEN_PREFIXES: [&str; 6] = ["sk-ant-", "xoxb-", "xoxp-", "xoxe.", "xoxe-", "xapp-"];
    let names_a_secret = |token: &Token<'_>| {
        let word: String = token
            .text
            .trim_matches(|c: char| !c.is_ascii_alphanumeric())
            .chars()
            .filter(|c| !matches!(c, '-' | '_'))
            .map(|c| c.to_ascii_lowercase())
            .collect();
        matches!(word.as_str(), "login" | "apikey" | "slacktoken")
    };
    let is_verb = |token: &Token<'_>| {
        ["set", "clear"]
            .iter()
            .any(|verb| token.text.eq_ignore_ascii_case(verb))
    };
    let keyword_with_value = tokens.iter().enumerate().any(|(i, token)| {
        names_a_secret(token)
            && match &tokens[i + 1..] {
                [] => false,
                [only] => !is_verb(only),
                _ => true,
            }
    });
    keyword_with_value
        || tokens.iter().any(|token| {
            let word = token.text.to_ascii_lowercase();
            TOKEN_PREFIXES.iter().any(|prefix| word.contains(prefix))
        })
        || tokens.iter().any(|token| is_login_code(token.text))
}

/// Whether a word, less surrounding punctuation, is shaped like a pasted
/// Claude login code in either form the login accepts: a word with a
/// `code=` query parameter, such as the callback URL, or the
/// `<code>#<state>` string the callback page shows, two non-empty runs of
/// printable ASCII other than `#`, `&`, `?`, `=` and `|` joined by one `#`.
/// A channel (`#general`) or a Git URL with a ref (`https://x.io/r#main`,
/// with the scheme in any case) isn't, since the login reads a URL only by
/// its query. A word such as
/// `PR#42` is a false positive: the heuristic only decides whether a failed
/// command is handled like a secret-bearing one, so it errs that way.
fn is_login_code(word: &str) -> bool {
    let is_part = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_graphic() && !matches!(b, b'#' | b'&' | b'?' | b'=' | b'|'))
    };
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '#');
    if word.contains("?code=") || word.contains("&code=") {
        return true;
    }
    let has_scheme = |scheme: &str| {
        word.get(..scheme.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
    };
    !(has_scheme("https://") || has_scheme("http://"))
        && word
            .split_once('#')
            .is_some_and(|(code, state)| is_part(code) && is_part(state))
}

/// The arguments for clap: the command words in lowercase, then the other
/// tokens, with a free-text tail joined back into one argument exactly as
/// it was written.
fn clap_args<'a>(
    text: &'a str,
    tokens: &[Token<'a>],
    words: &[&'static str],
    spec: Option<&Spec>,
) -> Vec<&'a str> {
    let rest = &tokens[words.len()..];
    let mut args: Vec<&str> = words.to_vec();
    match spec.and_then(|spec| spec.tail_after) {
        Some(n) if rest.len() > n => {
            args.extend(rest[..n].iter().map(|token| token.text));
            args.push(text[rest[n].start..].trim_end());
        }
        _ => args.extend(rest.iter().map(|token| token.text)),
    }
    args
}

fn unknown_command() -> ParseError {
    ParseError::new(
        ParseErrorKind::UnknownCommand,
        format!("Unknown command.\n\n{}", help::help()),
    )
}

fn help_topic(topic: Option<&Token<'_>>) -> ParseError {
    let Some(topic) = topic else {
        return ParseError::new(ParseErrorKind::Help, help::help());
    };
    help::group_help(&topic.text.to_ascii_lowercase()).map_or_else(unknown_command, |text| {
        ParseError::new(ParseErrorKind::Help, text)
    })
}

/// Rewords a clap error without the text it quotes: clap errors repeat the
/// offending argument, which may be a secret.
fn clap_problem(err: &clap::Error) -> String {
    match err.kind() {
        ErrorKind::MissingRequiredArgument => match err.get(ContextKind::InvalidArg) {
            Some(ContextValue::Strings(args)) if !args.is_empty() => {
                let args: Vec<String> = args.iter().map(|arg| format!("`{arg}`")).collect();
                format!("Missing {}.", args.join(", "))
            }
            _ => "Missing an argument.".to_owned(),
        },
        ErrorKind::ValueValidation => err
            .source()
            .map_or_else(|| "Invalid argument.".to_owned(), ToString::to_string),
        ErrorKind::MissingSubcommand | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
            "Missing a subcommand.".to_owned()
        }
        ErrorKind::InvalidSubcommand => "Unknown subcommand.".to_owned(),
        ErrorKind::UnknownArgument | ErrorKind::TooManyValues => "Too many arguments.".to_owned(),
        _ => "Invalid arguments.".to_owned(),
    }
}

/// The clap command, with clap's own help and version handling switched
/// off at every level and every argument allowed to start with `-`, since
/// chat text has no options.
fn clap_command() -> clap::Command {
    fn plain(command: clap::Command) -> clap::Command {
        command
            .disable_help_flag(true)
            .disable_help_subcommand(true)
            .disable_version_flag(true)
            .mut_args(|arg| arg.allow_hyphen_values(true))
            .mut_subcommands(plain)
    }
    plain(Cli::command())
}

#[derive(Parser)]
#[command(name = "agent", no_binary_name = true)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Login {
        #[arg(value_name = "code")]
        code: Option<String>,
    },
    Logout,
    Me,
    SlackToken {
        #[arg(value_name = "token")]
        token: String,
        #[arg(value_name = "refresh-token")]
        refresh: String,
    },
    Create {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
        #[arg(value_name = "persona")]
        persona: Option<String>,
    },
    Persona {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
        #[arg(value_name = "text")]
        text: Option<String>,
    },
    Skill {
        #[command(subcommand)]
        command: SkillCmd,
    },
    Allow {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
        #[arg(value_name = "target", value_parser = parse_target)]
        target: Target,
    },
    Deny {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
        #[arg(value_name = "target", value_parser = parse_target)]
        target: Target,
    },
    Limits {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
        #[arg(
            value_name = "setting",
            value_parser = parse_limit,
            num_args = 0..=2,
        )]
        settings: Vec<Limit>,
    },
    Pause {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
    },
    Resume {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
    },
    Delete {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
    },
    Sessions {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
    },
    Reset {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
        #[arg(value_name = "here", value_parser = parse_here)]
        here: Option<Here>,
    },
    List {
        #[arg(value_name = "@member", value_parser = parse_user)]
        user: Option<UserRef>,
    },
    Admin {
        #[command(subcommand)]
        command: AdminCmd,
    },
    Approve {
        #[arg(value_name = "consent-id", value_parser = parse_consent)]
        consent: ConsentId,
    },
    Decline {
        #[arg(value_name = "consent-id", value_parser = parse_consent)]
        consent: ConsentId,
    },
}

#[derive(Subcommand)]
enum SkillCmd {
    Add {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
        #[arg(value_name = "source", value_parser = parse_skill_source)]
        source: Option<String>,
    },
    Confirm {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
        #[arg(value_name = "skill", value_parser = parse_skill_name)]
        skill: SkillName,
    },
    Rm {
        #[arg(value_name = "name", value_parser = parse_agent_name)]
        name: AgentName,
        #[arg(value_name = "skill", value_parser = parse_skill_name)]
        skill: SkillName,
    },
}

#[derive(Subcommand)]
enum AdminCmd {
    ApiKey {
        #[command(subcommand)]
        command: ApiKeyCmd,
    },
    Ban {
        #[arg(value_name = "@member", value_parser = parse_user)]
        user: UserRef,
        #[arg(value_name = "reason")]
        reason: Option<String>,
    },
    Unban {
        #[arg(value_name = "@member", value_parser = parse_user)]
        user: UserRef,
    },
    Slack,
}

#[derive(Subcommand)]
enum ApiKeyCmd {
    Set {
        #[arg(value_name = "key")]
        key: String,
    },
    Clear,
}

impl Cmd {
    fn into_command(self) -> Result<Command, Reason> {
        Ok(match self {
            Cmd::Login { code } => Command::Login {
                code: code.map(SecretString::from),
            },
            Cmd::Logout => Command::Logout,
            Cmd::Me => Command::Me,
            Cmd::SlackToken { token, refresh } => Command::SlackToken {
                token: token.into(),
                refresh: refresh.into(),
            },
            Cmd::Create { name, persona } => Command::Create { name, persona },
            Cmd::Persona { name, text } => Command::Persona { name, text },
            Cmd::Skill { command } => Command::Skill(match command {
                SkillCmd::Add { name, source } => SkillCommand::Add { name, source },
                SkillCmd::Confirm { name, skill } => SkillCommand::Confirm { name, skill },
                SkillCmd::Rm { name, skill } => SkillCommand::Rm { name, skill },
            }),
            Cmd::Allow { name, target } => Command::Allow { name, target },
            Cmd::Deny { name, target } => Command::Deny { name, target },
            Cmd::Limits { name, settings } => limits(name, &settings)?,
            Cmd::Pause { name } => Command::Pause { name },
            Cmd::Resume { name } => Command::Resume { name },
            Cmd::Delete { name } => Command::Delete { name },
            Cmd::Sessions { name } => Command::Sessions { name },
            Cmd::Reset { name, here } => Command::Reset {
                name,
                here: here.is_some(),
            },
            Cmd::List { user } => Command::List { user },
            Cmd::Admin { command } => Command::Admin(match command {
                AdminCmd::ApiKey {
                    command: ApiKeyCmd::Set { key },
                } => AdminCommand::ApiKey(ApiKeyCommand::Set { key: key.into() }),
                AdminCmd::ApiKey {
                    command: ApiKeyCmd::Clear,
                } => AdminCommand::ApiKey(ApiKeyCommand::Clear),
                AdminCmd::Ban { user, reason } => AdminCommand::Ban { user, reason },
                AdminCmd::Unban { user } => AdminCommand::Unban { user },
                AdminCmd::Slack => AdminCommand::Slack,
            }),
            Cmd::Approve { consent } => Command::Approve { consent },
            Cmd::Decline { consent } => Command::Decline { consent },
        })
    }
}

/// One `limits` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Limit {
    TurnsPerDay(Setting<u32>),
    Hops(Setting<u8>),
}

const LIMIT_RULE: Reason =
    Reason("A setting is turns=N/day or hops=N, with N a whole number or off.");

/// Parses `turns=N/day`, `turns=N`, `hops=N`, `turns=off` or `hops=off`,
/// ignoring case.
fn parse_limit(s: &str) -> Result<Limit, Reason> {
    let s = s.to_ascii_lowercase();
    let (key, value) = s.split_once('=').ok_or(LIMIT_RULE)?;
    match key {
        "turns" => setting(
            value.strip_suffix("/day").unwrap_or(value),
            Reason("turns is at most 4294967295 a day."),
        )
        .map(Limit::TurnsPerDay),
        "hops" => setting(value, Reason("hops is at most 255.")).map(Limit::Hops),
        _ => Err(LIMIT_RULE),
    }
}

/// `off`, or a number as [`number`] reads it, or `too_large` for a number
/// past `T`'s largest.
fn setting<T: std::str::FromStr>(s: &str, too_large: Reason) -> Result<Setting<T>, Reason> {
    if s == "off" {
        Ok(Setting::Off)
    } else {
        number(s, too_large).map(Setting::To)
    }
}

/// A number written as plain ASCII digits, which `u32::from_str` alone
/// would stretch to allow a leading `+`, or `too_large` for one past `T`'s
/// largest.
fn number<T: std::str::FromStr>(s: &str, too_large: Reason) -> Result<T, Reason> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(LIMIT_RULE);
    }
    s.parse().map_err(|_| too_large)
}

fn limits(name: AgentName, settings: &[Limit]) -> Result<Command, Reason> {
    let mut turns_per_day = None;
    let mut hops = None;
    for setting in settings {
        match *setting {
            Limit::TurnsPerDay(value) if turns_per_day.is_none() => turns_per_day = Some(value),
            Limit::Hops(value) if hops.is_none() => hops = Some(value),
            _ => return Err(Reason("Give each setting once.")),
        }
    }
    if settings.is_empty() {
        return Err(Reason("Give turns=N/day, hops=N or both."));
    }
    Ok(Command::Limits {
        name,
        turns_per_day,
        hops,
    })
}

#[derive(Debug, Clone, Copy)]
struct Here;

fn parse_here(s: &str) -> Result<Here, Reason> {
    if s.eq_ignore_ascii_case("here") {
        Ok(Here)
    } else {
        Err(Reason("The only word allowed after the name is here."))
    }
}

/// Parses a consent id. Ids are lowercase; an uppercased copy is accepted,
/// since some clients change case when text is copied or retyped.
fn parse_consent(s: &str) -> Result<ConsentId, Reason> {
    s.to_ascii_lowercase()
        .parse()
        .map_err(|_| Reason("A consent id is the id shown on the request, such as 67e55044-10b1-426f-9247-bb680e5fe0c8."))
}

#[cfg(test)]
mod tests;

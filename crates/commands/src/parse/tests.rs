use secrecy::ExposeSecret;

use super::*;

fn ok(text: &str) -> Command {
    parse(text).unwrap_or_else(|err| panic!("{text:?} failed: {err}"))
}

fn fail(text: &str) -> ParseError {
    match parse(text) {
        Ok(command) => panic!("{text:?} parsed as {}", command.name()),
        Err(err) => err,
    }
}

/// Checks that `text` is refused as `Invalid` with `message`, and that the
/// message doesn't repeat any of the text's words beyond the command words.
fn invalid(text: &str, message: &str) {
    let err = fail(text);
    assert_eq!(err.kind(), ParseErrorKind::Invalid, "{text:?}");
    assert_eq!(err.to_string(), message, "{text:?}");
}

fn name(s: &str) -> AgentName {
    s.parse().unwrap()
}

const CONSENT: &str = "67e55044-10b1-426f-9247-bb680e5fe0c8";

#[test]
fn tokenize_splits_at_any_white_space_and_records_offsets() {
    let tokens = tokenize(" a\tbc\n\u{a0}d  ");
    let pairs: Vec<(usize, &str)> = tokens.iter().map(|t| (t.start, t.text)).collect();
    assert_eq!(pairs, [(1, "a"), (3, "bc"), (8, "d")]);
    assert!(tokenize(" \n ").is_empty());
    assert_eq!(tokenize("é x")[1].start, 3);
}

#[test]
fn empty_text_and_help_give_the_help_text() {
    for text in ["", "   ", "\n", "help", "HELP"] {
        let err = fail(text);
        assert_eq!(err.kind(), ParseErrorKind::Help, "{text:?}");
        assert_eq!(err.to_string(), help::help());
        assert!(!err.is_secret_bearing());
    }
}

#[test]
fn help_for_one_command() {
    let err = fail("help persona");
    assert_eq!(err.kind(), ParseErrorKind::Help);
    assert_eq!(
        err.to_string(),
        "`persona <name> [text]`: replace an agent's persona with the rest of the line, \
         or with a persona.md attached to a direct message with me"
    );
    let admin = fail("help Admin ban").to_string();
    assert!(admin.contains("`admin api-key set <key>`"));
    assert!(admin.contains("`admin slack`"));
    assert_eq!(fail("help skill").to_string().lines().count(), 3);
}

#[test]
fn help_for_an_unknown_command_gives_the_help_text() {
    let err = fail("help sk-ant-secret");
    assert_eq!(err.kind(), ParseErrorKind::UnknownCommand);
    assert_eq!(
        err.to_string(),
        format!("Unknown command.\n\n{}", help::help())
    );
}

#[test]
fn unknown_commands_give_the_help_text_without_echoing() {
    for text in [
        "logn abc123",
        "frobnicate",
        "/agent me",
        "!agent me",
        "-h",
        "--help",
    ] {
        let err = fail(text);
        assert_eq!(err.kind(), ParseErrorKind::UnknownCommand, "{text:?}");
        assert_eq!(
            err.to_string(),
            format!("Unknown command.\n\n{}", help::help())
        );
        assert!(!err.to_string().contains("abc123"));
    }
}

#[test]
fn command_words_ignore_case() {
    assert!(matches!(ok("Me"), Command::Me));
    assert!(matches!(ok("LOGOUT"), Command::Logout));
    assert!(matches!(
        ok("Skill RM helper x"),
        Command::Skill(SkillCommand::Rm { .. })
    ));
    assert!(matches!(
        ok("Admin Api-Key Clear"),
        Command::Admin(AdminCommand::ApiKey(ApiKeyCommand::Clear))
    ));
}

#[test]
fn arguments_keep_their_case() {
    let Command::Login { code: Some(code) } = ok("Login AbC#DeF") else {
        panic!()
    };
    assert_eq!(code.expose_secret(), "AbC#DeF");
}

#[test]
fn login() {
    assert!(matches!(ok("login"), Command::Login { code: None }));
    assert!(!ok("login").is_secret_bearing());
    let command = ok("  login   abc123#state-xyz  ");
    let Command::Login { code: Some(code) } = &command else {
        panic!()
    };
    assert_eq!(code.expose_secret(), "abc123#state-xyz");
    assert!(command.is_secret_bearing());

    let err = fail("login abc123 extra");
    assert_eq!(
        err.to_string(),
        "Too many arguments.\nUsage: `login [code]`"
    );
    assert!(err.is_secret_bearing());
}

#[test]
fn logout() {
    assert!(matches!(ok("logout"), Command::Logout));
    invalid("logout now", "Too many arguments.\nUsage: `logout`");
    assert!(!fail("logout now").is_secret_bearing());
}

#[test]
fn me() {
    assert!(matches!(ok("me"), Command::Me));
    invalid("me please", "Too many arguments.\nUsage: `me`");
}

#[test]
fn slack_token() {
    let command = ok("slack-token xoxe.xoxp-1-AAA xoxe-1-BBB");
    let Command::SlackToken { token, refresh } = &command else {
        panic!()
    };
    assert_eq!(token.expose_secret(), "xoxe.xoxp-1-AAA");
    assert_eq!(refresh.expose_secret(), "xoxe-1-BBB");
    assert!(command.is_secret_bearing());

    for text in ["slack-token xoxe.xoxp-1-AAA", "slack-token a b c"] {
        let err = fail(text);
        assert_eq!(err.kind(), ParseErrorKind::Invalid);
        assert!(err.is_secret_bearing(), "{text:?}");
        assert!(!err.to_string().contains("xoxe"), "{text:?}");
    }
    invalid(
        "slack-token xoxe.xoxp-1-AAA",
        "Missing `<refresh-token>`.\nUsage: `slack-token <token> <refresh-token>`",
    );
    invalid(
        "slack-token",
        "Missing `<token>`, `<refresh-token>`.\nUsage: `slack-token <token> <refresh-token>`",
    );
    assert!(!fail("slack-token").is_secret_bearing());
}

#[test]
fn create() {
    let Command::Create { name: n, persona } = ok("create helper") else {
        panic!()
    };
    assert_eq!(n, name("helper"));
    assert_eq!(persona, None);

    let Command::Create { persona, .. } = ok("create helper You are \"Helper\".\n\n- Be terse.  ")
    else {
        panic!()
    };
    assert_eq!(
        persona.as_deref(),
        Some("You are \"Helper\".\n\n- Be terse.")
    );

    invalid(
        "create",
        "Missing `<name>`.\nUsage: `create <name> [persona]`",
    );
    invalid(
        "create Helper",
        "An agent name is 2 to 32 characters, each a-z, 0-9 or -.\n\
         Usage: `create <name> [persona]`",
    );
}

#[test]
fn persona_takes_the_rest_of_the_text_verbatim() {
    let Command::Persona { name: n, text } =
        ok("persona helper   You're \"terse\" --help -x\n\tand   spaced.\n")
    else {
        panic!()
    };
    assert_eq!(n, name("helper"));
    assert_eq!(
        text.as_deref(),
        Some("You're \"terse\" --help -x\n\tand   spaced.")
    );
    let Command::Persona { text, .. } = ok("persona helper\n-- dash first") else {
        panic!()
    };
    assert_eq!(text.as_deref(), Some("-- dash first"));
}

#[test]
fn persona_without_text_is_an_upload() {
    assert!(matches!(
        ok("persona helper"),
        Command::Persona { text: None, .. }
    ));
    assert!(matches!(
        ok("persona helper  \n "),
        Command::Persona { text: None, .. }
    ));
}

#[test]
fn persona_needs_a_valid_name() {
    invalid(
        "persona",
        "Missing `<name>`.\nUsage: `persona <name> [text]`",
    );
    invalid(
        "persona x be nice",
        "An agent name is 2 to 32 characters, each a-z, 0-9 or -.\n\
         Usage: `persona <name> [text]`",
    );
}

#[test]
fn skill_add() {
    let Command::Skill(SkillCommand::Add { name: n, source }) =
        ok("skill add helper https://github.com/o/r.git#v1")
    else {
        panic!()
    };
    assert_eq!(n, name("helper"));
    assert_eq!(source.as_deref(), Some("https://github.com/o/r.git#v1"));

    let Command::Skill(SkillCommand::Add { source, .. }) =
        ok("skill add helper <https://github.com/o/r.git#v1|github.com/o/r.git#v1>")
    else {
        panic!()
    };
    assert_eq!(source.as_deref(), Some("https://github.com/o/r.git#v1"));

    assert!(matches!(
        ok("skill add helper"),
        Command::Skill(SkillCommand::Add { source: None, .. })
    ));
    invalid(
        "skill add helper a b",
        "Too many arguments.\nUsage: `skill add <name> [source]`",
    );
}

#[test]
fn skill_add_refuses_sources_other_than_https_git_urls() {
    let message = "A skill source is an https:// Git URL, optionally ending in #ref. \
                   Leave it out to add a SKILL.md or .zip attached to a direct message with me.\n\
                   Usage: `skill add <name> [source]`";
    for text in [
        "skill add helper --upload-pack=SECRET",
        "skill add helper -uSECRET",
        "skill add helper https://github.com/o/r#--upload-pack=SECRET",
        "skill add helper git@github.com:o/SECRET.git",
        "skill add helper file:///SECRET",
        "skill add helper https://user:SECRET@github.com/o/r",
        "skill add helper <ext::sh%20SECRET|x>",
    ] {
        invalid(text, message);
        assert!(!format!("{:?}", fail(text)).contains("SECRET"), "{text}");
    }
}

#[test]
fn skill_rm() {
    let Command::Skill(SkillCommand::Rm { name: n, skill }) = ok("skill rm helper pdf-tools")
    else {
        panic!()
    };
    assert_eq!(n, name("helper"));
    assert_eq!(skill.as_str(), "pdf-tools");
    invalid(
        "skill rm helper ../../etc",
        "A skill name is 1 to 64 characters, each a-z, 0-9 or -.\n\
         Usage: `skill rm <name> <skill>`",
    );
    invalid(
        "skill rm helper",
        "Missing `<skill>`.\nUsage: `skill rm <name> <skill>`",
    );
}

#[test]
fn skill_confirm() {
    let Command::Skill(SkillCommand::Confirm { name: n, skill }) =
        ok("skill confirm helper pdf-tools")
    else {
        panic!()
    };
    assert_eq!(n, name("helper"));
    assert_eq!(skill.as_str(), "pdf-tools");
    invalid(
        "skill confirm helper",
        "Missing `<skill>`.\nUsage: `skill confirm <name> <skill>`",
    );
    invalid(
        "skill confirm helper Tool",
        "A skill name is 1 to 64 characters, each a-z, 0-9 or -.\n\
         Usage: `skill confirm <name> <skill>`",
    );
}

#[test]
fn skill_needs_a_subcommand() {
    let usage = "Usage: `skill add <name> [source]`, `skill confirm <name> <skill>`, \
                 `skill rm <name> <skill>`";
    invalid("skill", &format!("Missing a subcommand.\n{usage}"));
    invalid(
        "skill remove helper x",
        &format!("Unknown subcommand.\n{usage}"),
    );
}

#[test]
fn allow_and_deny() {
    let cases = [
        ("@bob", Target::Member(UserRef::Name("bob".into()))),
        (
            "<@U024BE7LH|bob>",
            Target::Member(UserRef::Id("U024BE7LH".into())),
        ),
        (
            "#general",
            Target::Room(crate::RoomRef::Name("general".into())),
        ),
        (
            "<#C024BE7LR|general>",
            Target::Room(crate::RoomRef::Id("C024BE7LR".into())),
        ),
        ("everyone", Target::Everyone),
    ];
    for (text, want) in cases {
        let Command::Allow { name: n, target } = ok(&format!("allow helper {text}")) else {
            panic!()
        };
        assert_eq!(n, name("helper"));
        assert_eq!(target, want, "{text}");
        let Command::Deny { target, .. } = ok(&format!("deny helper {text}")) else {
            panic!()
        };
        assert_eq!(target, want, "{text}");
    }
    invalid(
        "allow helper bob",
        "A target is @member, #channel or everyone.\nUsage: `allow <name> <target>`",
    );
    invalid(
        "deny helper",
        "Missing `<target>`.\nUsage: `deny <name> <target>`",
    );
    invalid(
        "deny helper @bob #general",
        "Too many arguments.\nUsage: `deny <name> <target>`",
    );
}

#[test]
fn limits_in_any_order() {
    for text in [
        "limits helper turns=50/day hops=2",
        "limits helper hops=2 turns=50/day",
        "limits helper HOPS=2 Turns=50/Day",
        "limits helper turns=50 hops=2",
    ] {
        let Command::Limits {
            name: n,
            turns_per_day,
            hops,
        } = ok(text)
        else {
            panic!()
        };
        assert_eq!(n, name("helper"));
        assert_eq!((turns_per_day, hops), (Some(50), Some(2)), "{text}");
    }
    assert!(matches!(
        ok("limits helper hops=0"),
        Command::Limits {
            turns_per_day: None,
            hops: Some(0),
            ..
        }
    ));
    assert!(matches!(
        ok("limits helper turns=4294967295/day"),
        Command::Limits {
            turns_per_day: Some(u32::MAX),
            hops: None,
            ..
        }
    ));
}

#[test]
fn limits_rejects_malformed_settings() {
    let usage = "Usage: `limits <name> [turns=N/day] [hops=N]`";
    let rule = "A setting is turns=N/day or hops=N, with N a whole number.";
    for text in [
        "limits helper turns=",
        "limits helper turns=-1/day",
        "limits helper turns=+5",
        "limits helper turns=5/week",
        "limits helper hops",
        "limits helper tokens=5",
        "limits helper hops=2.5",
    ] {
        invalid(text, &format!("{rule}\n{usage}"));
    }
    invalid(
        "limits helper hops=256",
        &format!("That limit is too large.\n{usage}"),
    );
    invalid(
        "limits helper hops=1 hops=2",
        &format!("Give each setting once.\n{usage}"),
    );
    invalid(
        "limits helper turns=1 hops=2 hops=3",
        &format!("Too many arguments.\n{usage}"),
    );
    invalid(
        "limits helper",
        &format!("Give turns=N/day, hops=N or both.\n{usage}"),
    );
    invalid("limits", &format!("Missing `<name>`.\n{usage}"));
}

#[test]
fn pause_resume_delete_sessions() {
    fn agent(command: &Command) -> Option<(&'static str, &AgentName)> {
        match command {
            Command::Pause { name } => Some(("pause", name)),
            Command::Resume { name } => Some(("resume", name)),
            Command::Delete { name } => Some(("delete", name)),
            Command::Sessions { name } => Some(("sessions", name)),
            _ => None,
        }
    }
    for word in ["pause", "resume", "delete", "sessions"] {
        let command = ok(&format!("{word} helper"));
        assert_eq!(agent(&command), Some((word, &name("helper"))));
        let usage = format!("Usage: `{word} <name>`");
        invalid(word, &format!("Missing `<name>`.\n{usage}"));
        invalid(
            &format!("{word} helper extra"),
            &format!("Too many arguments.\n{usage}"),
        );
        invalid(
            &format!("{word} h"),
            &format!("An agent name is 2 to 32 characters, each a-z, 0-9 or -.\n{usage}"),
        );
    }
}

#[test]
fn agent_names_may_start_with_a_hyphen() {
    assert!(matches!(ok("pause -x"), Command::Pause { name } if name.as_str() == "-x"));
    assert!(matches!(ok("pause ---"), Command::Pause { name } if name.as_str() == "---"));
}

#[test]
fn a_lone_double_hyphen_is_refused_rather_than_skipped() {
    for (text, usage) in [
        ("pause --", "Usage: `pause <name>`"),
        ("login --", "Usage: `login [code]`"),
        ("allow -- helper everyone", "Usage: `allow <name> <target>`"),
        ("persona helper --", "Usage: `persona <name> [text]`"),
        ("skill add helper --", "Usage: `skill add <name> [source]`"),
    ] {
        invalid(text, &format!("A lone `--` isn't an argument.\n{usage}"));
    }
    let Command::Persona { text, .. } = ok("persona helper -- x") else {
        panic!()
    };
    assert_eq!(text.as_deref(), Some("-- x"));
}

#[test]
fn reset() {
    assert!(matches!(
        ok("reset helper"),
        Command::Reset { here: false, .. }
    ));
    assert!(matches!(
        ok("reset helper here"),
        Command::Reset { here: true, .. }
    ));
    assert!(matches!(
        ok("reset helper HERE"),
        Command::Reset { here: true, .. }
    ));
    invalid(
        "reset helper everywhere",
        "The only word allowed after the name is here.\nUsage: `reset <name> [here]`",
    );
    invalid(
        "reset helper here now",
        "Too many arguments.\nUsage: `reset <name> [here]`",
    );
}

#[test]
fn list() {
    assert!(matches!(ok("list"), Command::List { user: None }));
    assert!(matches!(
        ok("list @alice"),
        Command::List { user: Some(UserRef::Name(n)) } if n == "alice"
    ));
    assert!(matches!(
        ok("list <@U024BE7LH>"),
        Command::List { user: Some(UserRef::Id(id)) } if id == "U024BE7LH"
    ));
    invalid(
        "list alice",
        "A member is written @name.\nUsage: `list [@member]`",
    );
    invalid(
        "list #general",
        "A member is written @name.\nUsage: `list [@member]`",
    );
}

#[test]
fn admin_api_key_set() {
    let command = ok("admin api-key set sk-ant-api03-SECRET");
    let Command::Admin(AdminCommand::ApiKey(ApiKeyCommand::Set { key })) = &command else {
        panic!()
    };
    assert_eq!(key.expose_secret(), "sk-ant-api03-SECRET");
    assert!(command.is_secret_bearing());

    let err = fail("admin api-key set sk-ant-api03-SECRET extra");
    assert_eq!(
        err.to_string(),
        "Too many arguments.\nUsage: `admin api-key set <key>`"
    );
    assert!(err.is_secret_bearing());
    invalid(
        "admin api-key set",
        "Missing `<key>`.\nUsage: `admin api-key set <key>`",
    );
    assert!(!fail("admin api-key set").is_secret_bearing());
}

#[test]
fn admin_api_key_typed_without_set_is_still_secret_bearing() {
    let err = fail("admin api-key sk-ant-api03-SECRET");
    assert_eq!(
        err.to_string(),
        "Unknown subcommand.\nUsage: `admin api-key set <key>`, `admin api-key clear`"
    );
    assert!(err.is_secret_bearing());
    assert!(!fail("admin api-key").is_secret_bearing());
}

#[test]
fn admin_api_key_clear() {
    let command = ok("admin api-key clear");
    assert!(matches!(
        command,
        Command::Admin(AdminCommand::ApiKey(ApiKeyCommand::Clear))
    ));
    assert!(!command.is_secret_bearing());
    let err = fail("admin api-key clear now");
    assert_eq!(
        err.to_string(),
        "Too many arguments.\nUsage: `admin api-key clear`"
    );
    assert!(err.is_secret_bearing());
}

#[test]
fn admin_ban() {
    let Command::Admin(AdminCommand::Ban { user, reason }) = ok("admin ban @mallory") else {
        panic!()
    };
    assert_eq!(user, UserRef::Name("mallory".into()));
    assert_eq!(reason, None);

    let Command::Admin(AdminCommand::Ban { user, reason }) =
        ok("admin ban <@U1|mallory> spam in #general, twice")
    else {
        panic!()
    };
    assert_eq!(user, UserRef::Id("U1".into()));
    assert_eq!(reason.as_deref(), Some("spam in #general, twice"));

    invalid(
        "admin ban mallory spam",
        "A member is written @name.\nUsage: `admin ban <@member> [reason]`",
    );
    invalid(
        "admin ban",
        "Missing `<@member>`.\nUsage: `admin ban <@member> [reason]`",
    );
}

#[test]
fn admin_unban() {
    let Command::Admin(AdminCommand::Unban { user }) = ok("admin unban @mallory") else {
        panic!()
    };
    assert_eq!(user, UserRef::Name("mallory".into()));
    invalid(
        "admin unban @mallory @eve",
        "Too many arguments.\nUsage: `admin unban <@member>`",
    );
}

#[test]
fn admin_slack() {
    let command = ok("admin slack");
    assert!(matches!(command, Command::Admin(AdminCommand::Slack)));
    assert!(!command.is_secret_bearing());
    invalid(
        "admin slack xoxb-token",
        "Too many arguments.\nUsage: `admin slack`",
    );
}

#[test]
fn admin_needs_a_subcommand() {
    let err = fail("admin");
    assert_eq!(err.kind(), ParseErrorKind::Invalid);
    assert!(
        err.to_string()
            .starts_with("Missing a subcommand.\nUsage: `admin api-key set <key>`")
    );
    assert!(
        fail("admin promote @bob")
            .to_string()
            .starts_with("Unknown subcommand.\n")
    );
}

#[test]
fn approve_and_decline() {
    let consent: ConsentId = CONSENT.parse().unwrap();
    assert!(
        matches!(ok(&format!("approve {CONSENT}")), Command::Approve { consent: c } if c == consent)
    );
    assert!(matches!(
        ok(&format!("decline {}", CONSENT.to_uppercase())),
        Command::Decline { consent: c } if c == consent
    ));
    let rule = "A consent id is the id shown on the request, such as \
                67e55044-10b1-426f-9247-bb680e5fe0c8.";
    invalid(
        "approve 1234",
        &format!("{rule}\nUsage: `approve <consent-id>`"),
    );
    invalid(
        "decline",
        "Missing `<consent-id>`.\nUsage: `decline <consent-id>`",
    );
}

#[test]
fn secret_bearing_variants_redact_themselves_in_debug() {
    for (text, secrets) in [
        ("login code-SECRET-1", &["code-SECRET-1"][..]),
        (
            "slack-token tok-SECRET-2 ref-SECRET-3",
            &["tok-SECRET-2", "ref-SECRET-3"],
        ),
        ("admin api-key set key-SECRET-4", &["key-SECRET-4"]),
    ] {
        let command = ok(text);
        assert!(command.is_secret_bearing(), "{text}");
        let debug = format!("{command:?} {command:#?}");
        for secret in secrets {
            assert!(!debug.contains(secret), "{debug}");
            assert!(!debug.contains("SECRET"), "{debug}");
        }
        assert!(debug.contains(command.name()), "{debug}");
    }
}

#[test]
fn debug_prints_the_command_name_and_no_free_text() {
    for text in ["persona helper PERSONA-TEXT", "admin ban @alice BAN-REASON"] {
        let command = ok(text);
        let debug = format!("{command:?} {command:#?}");
        assert!(debug.contains(command.name()), "{debug}");
        assert!(!debug.contains("TEXT"), "{debug}");
        assert!(!debug.contains("REASON"), "{debug}");
    }
}

#[test]
fn admin_command_debug_prints_the_variant_and_no_free_text() {
    for (text, variant) in [
        ("admin ban @alice BAN-REASON", "Ban"),
        ("admin unban @alice", "Unban"),
        ("admin api-key set key-SECRET", "ApiKey"),
        ("admin slack", "Slack"),
    ] {
        let Command::Admin(admin) = ok(text) else {
            panic!("{text} is not an admin command");
        };
        let debug = format!("{admin:?} {admin:#?}");
        assert!(debug.contains(variant), "{debug}");
        assert!(!debug.contains("REASON"), "{debug}");
        assert!(!debug.contains("SECRET"), "{debug}");
        assert!(!debug.contains("alice"), "{debug}");
    }
}

#[test]
fn only_the_three_secret_bearing_commands_say_so() {
    let secret = ["login x", "slack-token a b", "admin api-key set k"];
    let plain = [
        "login",
        "logout",
        "me",
        "create helper",
        "persona helper sk-ant-in-a-persona",
        "skill add helper",
        "skill confirm helper x",
        "skill rm helper x",
        "allow helper everyone",
        "deny helper everyone",
        "limits helper hops=1",
        "pause helper",
        "resume helper",
        "delete helper",
        "sessions helper",
        "reset helper",
        "list",
        "admin api-key clear",
        "admin ban @x",
        "admin unban @x",
        "admin slack",
    ];
    for text in secret {
        assert!(ok(text).is_secret_bearing(), "{text}");
    }
    for text in plain {
        assert!(!ok(text).is_secret_bearing(), "{text}");
    }
    assert!(!ok(&format!("approve {CONSENT}")).is_secret_bearing());
    assert!(!ok(&format!("decline {CONSENT}")).is_secret_bearing());
}

#[test]
fn every_command_has_its_own_help_line() {
    let texts = [
        "login",
        "logout",
        "me",
        "slack-token a b",
        "create helper",
        "persona helper",
        "skill add helper",
        "skill confirm helper x",
        "skill rm helper x",
        "allow helper everyone",
        "deny helper everyone",
        "limits helper hops=1",
        "pause helper",
        "resume helper",
        "delete helper",
        "sessions helper",
        "reset helper",
        "list",
        "admin api-key set k",
        "admin api-key clear",
        "admin ban @x",
        "admin unban @x",
        "admin slack",
        &format!("approve {CONSENT}"),
        &format!("decline {CONSENT}"),
    ];
    for text in texts {
        let command = ok(text);
        assert!(text.starts_with(command.name()), "{text}");
        let line = command.help();
        assert!(
            line.starts_with(&format!("`{}", command.name())),
            "{text}: {line}"
        );
        assert!(help::help().contains(&line));
    }
    assert_eq!(texts.len() + 1, SPECS.len());
}

#[test]
fn errors_never_repeat_the_text() {
    for text in [
        "login SECRET1 SECRET2",
        "slack-token SECRET1",
        "admin api-key set SECRET1 SECRET2",
        "admin api-key SECRET1",
        "pause SECRET1",
        "limits helper SECRET1=1",
        "approve SECRET1",
        "list SECRET1",
        "skill SECRET1",
        "reset helper SECRET1",
        "api-key set SECRET1",
        "slack_token SECRET1 SECRET2",
        "admin apikey set SECRET1",
    ] {
        let err = fail(text);
        assert!(!err.to_string().contains("SECRET"), "{text}: {err}");
        assert!(!format!("{err:?}").contains("SECRET"), "{text}: {err:?}");
    }
}

#[test]
fn quotes_are_ordinary_characters() {
    invalid(
        "pause \"helper\"",
        "An agent name is 2 to 32 characters, each a-z, 0-9 or -.\nUsage: `pause <name>`",
    );
    let Command::Admin(AdminCommand::Ban { reason, .. }) = ok("admin ban @x 'quoted reason'")
    else {
        panic!()
    };
    assert_eq!(reason.as_deref(), Some("'quoted reason'"));
}

#[test]
fn misspelt_secret_bearing_commands_are_still_secret_bearing() {
    for text in [
        "api-key set sk-ant-api03-SECRET",
        "apikey set k",
        "api_key k",
        "Admin APIKEY set k",
        "admin apikey set k",
        "admin api_key set k",
        "admin key set sk-ant-api03-SECRET",
        "slack_token a b",
        "slacktoken a",
        "Slack-Token: a b",
        "SlackToken a",
        "logn please LOGIN abc#def",
        "log-in abc#def",
        "please login abc#def",
        "admin login x",
        "pause helper api-key k",
    ] {
        let err = fail(text);
        assert_ne!(err.kind(), ParseErrorKind::Help, "{text:?}");
        assert!(err.is_secret_bearing(), "{text:?}");
    }
}

#[test]
fn a_pasted_login_code_is_secret_bearing_under_any_verb() {
    for text in [
        "logn abc123#state",
        "lgoin abc123#state-xyz",
        "frobnicate x_Y-1#Z_2",
        "pause helper abc#def.",
        "persona Bad-Name (abc123#state)",
        "lgoin https://console.anthropic.com/oauth/code/callback?code=abc123&state=xyz",
        "frobnicate <https://x.io/cb?state=s&code=abc123|link>",
        "logn abc.def~1#state",
        "logn abc+/#state",
        "logn ABC123%2F#state",
    ] {
        let err = fail(text);
        assert!(err.is_secret_bearing(), "{text:?}");
        assert!(!err.to_string().contains("abc123#state"), "{text:?}");
    }
}

#[test]
fn errors_holding_a_known_token_prefix_are_secret_bearing() {
    for text in [
        "sk-ant-api03-SECRET",
        "set sk-ant-oat01-SECRET",
        "admin api-key sk-ant-api03-SECRET",
        "key=SK-ANT-api03-SECRET",
        "xoxb-1-SECRET",
        "tokens xoxe.xoxp-1-SECRET xoxe-1-SECRET",
        "app xapp-1-SECRET",
        "user xoxp-1-SECRET",
        "persona Bad-Name sk-ant-api03-SECRET",
        "help sk-ant-api03-SECRET",
    ] {
        let err = fail(text);
        assert!(err.is_secret_bearing(), "{text:?}");
        assert!(!err.to_string().contains("SECRET"), "{text:?}");
    }
}

#[test]
fn errors_without_a_secret_are_not_secret_bearing() {
    for text in [
        "",
        "help",
        "help login",
        "help admin",
        "frobnicate",
        "logn abc123",
        "logn #general",
        "logn abc#",
        "logn a#b#c",
        "skill add Bad-Name https://x.io/r#main",
        "skill add Bad-Name https://x.io/r?ref=main#main",
        "skill add Bad-Name HTTPS://x.io/r#main",
        "skill add Bad-Name Http://x.io/r#main",
        "logn https://x.io/a?decode=1",
        "allow Bad-Name <#C123|general>",
        "logout now",
        "pause Bad",
        "api-key",
        "api-key set",
        "apikey clear",
        "slack_token",
        "admin apikey",
        "admin api-key set",
        "admin api-key",
        "skill add helper http://x.io/r",
        "persona Bad-Name xox",
        "login-page",
    ] {
        assert!(!fail(text).is_secret_bearing(), "{text:?}");
    }
}

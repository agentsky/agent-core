//! What the owner and the thread are told about a consent: the consent
//! card, its closed form, and the outcomes posted to the thread.
//!
//! A card is one message on every surface, its decision commands
//! included: [`Card::check`] refuses at request time a task whose card
//! wouldn't fit Slack's blocks or one Rocket.Chat message.

use core_types::{ConsentId, MemberKey, SurfaceKind, ThreadKey};
use render::MentionDirectory;
use serde_json::{Value, json};
use store::{Consent, ConsentState};
use time::OffsetDateTime;
use time::macros::format_description;

use crate::commands::Rich;

/// The `block_id` of a Slack consent card's buttons.
pub const BLOCK_ID: &str = "consent";
/// The `action_id` of a Slack consent card's Approve button.
pub const APPROVE_ACTION: &str = "consent_approve";
/// The `action_id` of a Slack consent card's Decline button.
pub const DECLINE_ACTION: &str = "consent_decline";
/// The most UTF-16 code units Slack takes in one text object of a block.
pub const SLACK_TEXT_MAX: usize = 3000;

/// Inserted after each backtick of a task on a Rocket.Chat card.
const ZERO_WIDTH_SPACE: char = '\u{200B}';

/// What the thread is told when a private task's owner declined it, for
/// the consent `id` an agent named `agent` asked for.
pub fn declined_text(id: ConsentId, agent: &str) -> String {
    format!("Private task `{id}`: the owner of {agent} declined it, so it didn't run.")
}

/// What the thread is told when a private task's owner didn't answer
/// within `ttl`.
pub fn expired_text(id: ConsentId, agent: &str, ttl: time::Duration) -> String {
    format!(
        "Private task `{id}`: the owner of {agent} didn't answer within {}, so it expired \
         without running.",
        span(ttl)
    )
}

/// What the thread is told when a private task's consent card never
/// reached the owner.
pub fn unreachable_text(id: ConsentId, agent: &str) -> String {
    format!(
        "Private task `{id}`: the consent card couldn't reach the owner of {agent}, so it \
         expired without running."
    )
}

/// What the thread is told when a private task couldn't be run, however
/// often it was tried.
pub fn failed_text(id: ConsentId) -> String {
    format!("Private task `{id}`: it couldn't be run. Ask again later.")
}

/// What the thread is told when a private task's run was cut short after
/// it reached the model, so it isn't run again.
pub fn interrupted_text(id: ConsentId) -> String {
    format!(
        "Private task `{id}`: it was interrupted while running, so it wasn't run again. Ask \
         again if it's still needed."
    )
}

/// What the thread is told when a private task's agent stayed paused
/// until the task expired.
pub fn paused_text(id: ConsentId, agent: &str) -> String {
    format!("Private task `{id}`: {agent} was paused until the task expired, so it didn't run.")
}

/// What the thread is told when a private task didn't run because of a
/// limit, which `refusal` explains.
pub fn limited_text(id: ConsentId, refusal: &str) -> String {
    format!("Private task `{id}`: it didn't run. {refusal}")
}

/// The heading of a private task's result in the thread.
pub fn result_heading(id: ConsentId) -> String {
    format!("*Private task `{id}`:*")
}

/// `ttl` in its largest whole unit, such as `24 hours`.
fn span(ttl: time::Duration) -> String {
    let secs = ttl.whole_seconds().max(0);
    let (count, unit) = [(86_400, "day"), (3_600, "hour"), (60, "minute")]
        .into_iter()
        .find(|(size, _)| secs >= *size && secs % size == 0)
        .map_or((secs, "second"), |(size, unit)| (secs / size, unit));
    format!("{count} {unit}{}", if count == 1 { "" } else { "s" })
}

/// A consent card, for the owner of the agent `agent` whose consent it
/// is, with the names of the files the task is handed.
#[derive(Debug, Clone)]
pub struct Card<'a> {
    /// The consent.
    pub consent: &'a Consent,
    /// The agent's name.
    pub agent: &'a str,
    /// The names of the files handed to the task.
    pub files: &'a [String],
    /// Whether the requester is the agent's owner, so the task runs on the
    /// owner's side if the owner approves it.
    pub owners: bool,
    /// Whether the agent is paused, so an approved task waits for it.
    pub paused: bool,
}

impl Card<'_> {
    /// Whether the card fits where it is shown, before it is asked for: on
    /// Slack each block's text within [`SLACK_TEXT_MAX`] UTF-16 code units,
    /// and on Rocket.Chat the whole card, as the surface renders it, in
    /// one message of the server's default limit.
    ///
    /// # Errors
    ///
    /// What is too long, to tell the agent.
    pub fn check(&self) -> Result<(), String> {
        let open = self.open();
        let texts = open
            .blocks
            .as_ref()
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .flat_map(block_texts);
        if texts
            .into_iter()
            .any(|text| utf16_len(text) > SLACK_TEXT_MAX)
        {
            return Err(format!(
                "the task, or the list of its files' names, is over {SLACK_TEXT_MAX} UTF-16 \
                 code units, more than a consent card holds"
            ));
        }
        let rendered = render::rocketchat::to_markdown(&open.markdown, &NoDirectory);
        if utf16_len(&rendered) > render::rocketchat::DEFAULT_MESSAGE_LIMIT.max {
            return Err(
                "the task and its files' names are too long for one consent card message; \
                 shorten them"
                    .to_owned(),
            );
        }
        Ok(())
    }

    /// The card while it waits for the owner: Markdown that says how to
    /// answer with `approve` and `decline`, and on Slack Block Kit with
    /// Approve and Decline buttons.
    pub fn open(&self) -> Rich {
        let id = self.consent.id;
        let markdown = format!(
            "{}\n\nSend `approve {id}` or `decline {id}` to me here. It expires at {}.",
            self.markdown(),
            expiry(self.consent.expires_at)
        );
        let mut blocks = self.blocks();
        blocks.push(json!({
            "type": "context",
            "elements": [{
                "type": "plain_text",
                "text": format!("It expires at {}.", expiry(self.consent.expires_at)),
                "emoji": false,
            }],
        }));
        blocks.push(json!({
            "type": "actions",
            "block_id": BLOCK_ID,
            "elements": [
                button("Approve", APPROVE_ACTION, "primary", id),
                button("Decline", DECLINE_ACTION, "danger", id),
            ],
        }));
        Rich {
            markdown,
            fallback: self.fallback(),
            blocks: Some(Value::Array(blocks)),
        }
    }

    /// The card once decided or expired: the same, with the outcome in
    /// place of the buttons.
    pub fn closed(&self) -> Rich {
        let outcome = outcome(self.consent);
        let markdown = format!("{}\n\n{outcome}", self.markdown());
        let mut blocks = self.blocks();
        blocks.push(json!({
            "type": "context",
            "elements": [{"type": "mrkdwn", "text": outcome}],
        }));
        Rich {
            markdown,
            fallback: format!("{} {outcome}", self.fallback()),
            blocks: Some(Value::Array(blocks)),
        }
    }

    /// The notification's text.
    fn fallback(&self) -> String {
        format!("Private task request for {}", self.agent)
    }

    fn requester(&self) -> String {
        let key = &self.consent.requester.key;
        match key.surface {
            SurfaceKind::Slack => format!("<@{}>", key.user),
            SurfaceKind::RocketChat => inline_code(key.user.as_str()),
        }
    }

    /// Who asked where, and what the owner should know of how.
    fn heading(&self, bold: &str) -> String {
        let mut text = format!(
            "{bold}Private task request{bold} for *{}* from {}, in {}.",
            self.agent,
            self.requester(),
            place(&self.consent.thread),
        );
        let hop = self.consent.hop.0;
        if hop > 0 {
            text.push_str(&format!(
                " It was asked for in a turn another agent's message started (hop {hop}), so \
                 its text may not be what the requester wrote."
            ));
        }
        if self.paused {
            text.push_str(&format!(
                " {} is paused: if you approve, the task runs once it is resumed, if that is \
                 before it expires.",
                self.agent
            ));
        }
        text
    }

    /// What approving means: the owner's side for the owner's own task,
    /// read-only `shared/` and no memory for anyone else's.
    fn terms(&self) -> &'static str {
        if self.owners { OWNER_TERMS } else { TERMS }
    }

    /// The Markdown both surfaces' cards share, before how to answer. The
    /// task is in a code block fenced with exactly three backticks, the
    /// only fence Rocket.Chat's parser knows, with a zero-width space after
    /// each backtick in it so none of its own can end the block.
    fn markdown(&self) -> String {
        let task = &self.consent.task;
        let broken = task.contains('`');
        let task: String = task
            .chars()
            .flat_map(|c| [Some(c), (c == '`').then_some(ZERO_WIDTH_SPACE)])
            .flatten()
            .collect();
        let mut text = format!(
            "{}\n\nThe task, exactly as written{}:\n```\n{task}\n```\n",
            self.heading("**"),
            if broken {
                ", with a zero-width space after each backtick so the block can't be ended \
                 early"
            } else {
                ""
            },
        );
        if !self.files.is_empty() {
            let names: Vec<String> = self.files.iter().map(|name| inline_code(name)).collect();
            text.push_str(&format!("Files handed to it: {}.\n", names.join(", ")));
        }
        text.push_str(self.terms());
        text
    }

    /// The Block Kit both states of a Slack card share: who asked where,
    /// the task as plain text, the files and what approving means.
    fn blocks(&self) -> Vec<Value> {
        let mut blocks = vec![
            json!({
                "type": "section",
                "text": {"type": "mrkdwn", "text": self.heading("*")},
            }),
            json!({
                "type": "section",
                "text": {"type": "plain_text", "text": self.consent.task, "emoji": false},
            }),
        ];
        if !self.files.is_empty() {
            blocks.push(json!({
                "type": "context",
                "elements": [{
                    "type": "plain_text",
                    "text": format!("Files handed to it: {}.", self.files.join(", ")),
                    "emoji": false,
                }],
            }));
        }
        blocks.push(json!({
            "type": "context",
            "elements": [{"type": "mrkdwn", "text": self.terms()}],
        }));
        blocks
    }
}

/// What approving a task someone other than the owner asked for means.
const TERMS: &str = "If you approve, it runs once in a new private session on your Claude \
     account. It can read your agent's shared files but not change them, and doesn't see its \
     memory. Only its reply and the files it attaches are posted to the thread.";

/// What approving a task the owner asked for, in a turn another agent's
/// message started, means.
const OWNER_TERMS: &str = "If you approve, it runs once in a new private session on your \
     Claude account, on your side: it can read and change your agent's shared files and its \
     memory. Only its reply and the files it attaches are posted to the thread, where everyone \
     in it can read them.";

/// The text objects of a Slack block.
fn block_texts(block: &Value) -> Vec<&str> {
    let mut texts: Vec<&str> = block
        .pointer("/text/text")
        .and_then(Value::as_str)
        .into_iter()
        .collect();
    if let Some(elements) = block.get("elements").and_then(Value::as_array) {
        texts.extend(
            elements
                .iter()
                .filter_map(|element| element.get("text").and_then(Value::as_str)),
        );
    }
    texts
}

/// The length of `text` in UTF-16 code units, as Slack and Rocket.Chat
/// count it.
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `text` as Rocket.Chat inline code, its backticks left out.
fn inline_code(text: &str) -> String {
    format!("`{}`", text.replace('`', ""))
}

/// A directory that resolves no mention: a card's Markdown is rendered as
/// it is.
struct NoDirectory;

impl MentionDirectory for NoDirectory {
    fn resolve(&self, _name: &str) -> Option<String> {
        None
    }
}

fn button(text: &str, action: &str, style: &str, id: ConsentId) -> Value {
    json!({
        "type": "button",
        "action_id": action,
        "text": {"type": "plain_text", "text": text},
        "style": style,
        "value": id.to_string(),
    })
}

/// How the card names the thread the task was asked for in: on Slack a
/// link to it, elsewhere its room and thread ids.
fn place(thread: &ThreadKey) -> String {
    let conv = &thread.conv;
    match conv.surface {
        SurfaceKind::Slack => match surface_slack::surface::thread_link(thread) {
            Some(link) if thread.root.is_some() => {
                format!("<#{}> (<{link}|the thread>)", conv.conversation)
            }
            _ => format!("<#{}>", conv.conversation),
        },
        SurfaceKind::RocketChat => match &thread.root {
            Some(root) => format!(
                "room {}, thread {}",
                inline_code(conv.conversation.as_str()),
                inline_code(root.as_str())
            ),
            None => format!("room {}", inline_code(conv.conversation.as_str())),
        },
    }
}

/// The outcome line of a closed card.
fn outcome(consent: &Consent) -> String {
    let by = |by: &Option<MemberKey>| match by {
        Some(key) if key.surface == SurfaceKind::Slack => format!(" by <@{}>", key.user),
        _ => String::new(),
    };
    match consent.state {
        ConsentState::Approved => format!("Approved{}.", by(&consent.decided_by)),
        ConsentState::Declined => format!("Declined{}.", by(&consent.decided_by)),
        ConsentState::Expired => "Expired: nobody answered in time.".to_owned(),
        ConsentState::Pending => "Waiting for an answer.".to_owned(),
    }
}

/// `at` as the card shows it, in UTC.
fn expiry(at: OffsetDateTime) -> String {
    at.format(format_description!(
        "[year]-[month]-[day] [hour]:[minute] UTC"
    ))
    .unwrap_or_else(|_| at.unix_timestamp().to_string())
}

#[cfg(test)]
mod tests {
    use core_types::{AgentId, ConvRef, Hop, Requester, SessionId};

    use super::*;

    fn consent(surface: SurfaceKind, task: &str) -> Consent {
        let key = MemberKey {
            surface,
            team: "T0TEAM001".into(),
            user: "U0BOB".into(),
        };
        Consent {
            id: ConsentId::new_v4(),
            agent: AgentId::new_v4(),
            requester: Requester { member: None, key },
            hop: Hop::ZERO,
            task: task.to_owned(),
            attachments_json: "[]".to_owned(),
            state: ConsentState::Pending,
            approval: None,
            thread: ThreadKey {
                conv: ConvRef {
                    surface,
                    team: "T0TEAM001".into(),
                    conversation: "C0CHAN001".into(),
                },
                root: Some("1727697600.000100".into()),
            },
            origin_session: SessionId::new_v4(),
            private_session: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: OffsetDateTime::UNIX_EPOCH + time::Duration::days(1),
            decided_by: None,
            decided_at: None,
            card: None,
            card_closed_at: None,
            work_attempts: 0,
            work_failures: 0,
            finished_at: None,
        }
    }

    #[test]
    fn a_slack_card_shows_the_exact_task_as_plain_text_with_buttons() {
        let mut consent = consent(SurfaceKind::Slack, "Summarize *my* <!channel> notes");
        let files = ["a.csv".to_owned()];
        let card = Card {
            consent: &consent,
            agent: "helper",
            files: &files,
            owners: false,
            paused: false,
        };
        let open = card.open();
        let blocks = open.blocks.unwrap();
        let text = blocks.to_string();
        assert_eq!(
            blocks[1]["text"],
            json!({"type": "plain_text", "text": "Summarize *my* <!channel> notes", "emoji": false})
        );
        assert!(text.contains("<@U0BOB>"), "{text}");
        assert!(
            text.contains("https://app.slack.com/client/T0TEAM001/C0CHAN001/thread/"),
            "{text}"
        );
        assert!(text.contains("Files handed to it: a.csv."), "{text}");
        assert!(text.contains("can read your agent's shared files but not change them"));
        let actions = blocks.as_array().unwrap().last().unwrap();
        assert_eq!(actions["block_id"], BLOCK_ID);
        assert_eq!(actions["elements"][0]["action_id"], APPROVE_ACTION);
        assert_eq!(actions["elements"][1]["action_id"], DECLINE_ACTION);
        assert_eq!(actions["elements"][0]["value"], consent.id.to_string());
        assert!(open.markdown.contains(&format!("`approve {}`", consent.id)));
        assert!(open.markdown.contains("1970-01-02 00:00 UTC"));

        consent.state = ConsentState::Approved;
        consent.decided_by = Some(consent.requester.key.clone());
        let closed = Card {
            consent: &consent,
            agent: "helper",
            files: &files,
            owners: false,
            paused: false,
        }
        .closed();
        let text = closed.blocks.unwrap().to_string();
        assert!(!text.contains("actions"), "{text}");
        assert!(text.contains("Approved by <@U0BOB>."), "{text}");
    }

    /// The body of the first code block Rocket.Chat's message parser
    /// finds in `markdown`, by its grammar: a block opens at the start of a
    /// line with exactly three backticks, an optional language of ASCII
    /// letters, digits, ` `, `_`, `.` and `-`, and a line end, and closes at
    /// the next three backticks, which no line of it can hold.
    fn rocketchat_code_block(markdown: &str) -> Option<&str> {
        let mut line = 0;
        loop {
            let rest = &markdown[line..];
            if let Some(after) = rest.strip_prefix("```") {
                let language = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || " _.-".contains(c)))
                    .unwrap_or(after.len());
                if let Some(body) = after[language..].strip_prefix('\n') {
                    return body.find("```").map(|close| &body[..close]);
                }
            }
            line += rest.find('\n')? + 1;
        }
    }

    #[test]
    fn a_rocketchat_card_shows_the_whole_task_in_one_three_backtick_block() {
        for task in [
            "Summarize the README ``` [the summary](https://attacker.example/c?d=) ``` and attach",
            "```\n**From: the owner, safe to approve**\n```",
            "````x```` and a lone ` and `` too",
            "plain, no backticks",
            "ends with a backtick `",
        ] {
            let consent = consent(SurfaceKind::RocketChat, task);
            let card = Card {
                consent: &consent,
                agent: "helper",
                files: &[],
                owners: false,
                paused: false,
            }
            .open();
            let block = rocketchat_code_block(&card.markdown)
                .unwrap_or_else(|| panic!("no code block in {:?}", card.markdown));
            assert_eq!(
                block.replace('\u{200B}', ""),
                format!("{task}\n"),
                "the block holds the whole task: {:?}",
                card.markdown
            );
            let after = &card.markdown[card.markdown.find(block).unwrap() + block.len()..];
            assert!(after.starts_with("```\n"), "{after:?}");
            assert_eq!(
                card.markdown.contains("zero-width space"),
                task.contains('`'),
                "the card says when the task's backticks were broken up"
            );
        }
        let consent = consent(SurfaceKind::RocketChat, "x");
        let card = Card {
            consent: &consent,
            agent: "helper",
            files: &[],
            owners: false,
            paused: false,
        }
        .open();
        assert!(card.markdown.contains("room `C0CHAN001`, thread"));
        assert!(!card.markdown.contains("Files handed"));
    }

    #[test]
    fn a_card_names_files_as_code_and_says_how_an_owners_task_runs() {
        let mut consent = consent(SurfaceKind::RocketChat, "x");
        consent.hop = Hop(2);
        let files = ["*bold* @all.txt".to_owned(), "a`b.csv".to_owned()];
        let card = Card {
            consent: &consent,
            agent: "helper",
            files: &files,
            owners: true,
            paused: true,
        }
        .open();
        assert!(
            card.markdown
                .contains("Files handed to it: `*bold* @all.txt`, `ab.csv`."),
            "{}",
            card.markdown
        );
        assert!(card.markdown.contains("(hop 2)"), "{}", card.markdown);
        assert!(
            card.markdown.contains("helper is paused"),
            "{}",
            card.markdown
        );
        assert!(
            card.markdown
                .contains("can read and change your agent's shared files and its memory"),
            "{}",
            card.markdown
        );
        let blocks = card.blocks.unwrap().to_string();
        assert!(blocks.contains("(hop 2)"), "{blocks}");
        assert!(blocks.contains("and its memory"), "{blocks}");
    }

    #[test]
    fn a_card_that_wouldnt_fit_one_message_is_refused() {
        let card = |task: &str, files: &[String]| {
            let consent = consent(SurfaceKind::Slack, task);
            Card {
                consent: &consent,
                agent: "helper",
                files,
                owners: false,
                paused: false,
            }
            .check()
        };
        assert_eq!(card(&"a".repeat(3000), &[]), Ok(()));
        assert!(
            card(&"\u{1F600}".repeat(1501), &[]).is_err(),
            "emoji count twice, as Slack counts them"
        );
        assert!(
            card(&"`".repeat(2600), &[]).is_err(),
            "a task whose backticks are broken up must still fit one Rocket.Chat message"
        );
        let long: Vec<String> = (0..10).map(|n| format!("{n}{}", "x".repeat(250))).collect();
        assert_eq!(card("x", &long), Ok(()));
        assert!(card(&"a".repeat(2500), &long).is_err());
    }

    #[test]
    fn outcomes_name_the_consent_and_spans_read_naturally() {
        let id = ConsentId::new_v4();
        assert!(declined_text(id, "helper").contains(&id.to_string()));
        assert!(
            expired_text(id, "helper", time::Duration::days(1)).contains("within 1 day,"),
            "{}",
            expired_text(id, "helper", time::Duration::days(1))
        );
        for (secs, text) in [
            (86_400 * 2, "2 days"),
            (3_600 * 5, "5 hours"),
            (90, "90 seconds"),
            (120, "2 minutes"),
            (1, "1 second"),
        ] {
            assert_eq!(span(time::Duration::seconds(secs)), text);
        }
    }
}

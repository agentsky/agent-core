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
use crate::commands::reply::MAX_NAME_CHARS;

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
    /// The surface the card is shown on: the owner's identity it goes to.
    pub surface: SurfaceKind,
    /// The requester's name on their surface, when it could be looked up.
    pub requester_name: Option<&'a str>,
}

impl Card<'_> {
    /// Whether the card fits wherever it may be shown, before it is asked
    /// for, in its longest form (for a paused agent): on Slack each block's
    /// text within [`SLACK_TEXT_MAX`] UTF-16 code units, and on
    /// Rocket.Chat the whole card, as the surface renders it, in one
    /// message of the server's default limit. The requester's name, looked
    /// up only when the card is sent, is counted at its longest.
    ///
    /// # Errors
    ///
    /// What is too long, to tell the agent.
    pub fn check(&self) -> Result<(), String> {
        let name = "\u{1D54E}".repeat(MAX_NAME_CHARS);
        let longest = |surface| Card {
            surface,
            paused: true,
            requester_name: Some(&name),
            ..self.clone()
        };
        let open = longest(SurfaceKind::Slack).open();
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
                "the task is over {SLACK_TEXT_MAX} UTF-16 code units, more than a consent card \
                 holds"
            ));
        }
        let markdown = longest(SurfaceKind::RocketChat).open().markdown;
        let rendered = render::rocketchat::to_markdown(&markdown, &NoDirectory);
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

    /// Who asked, by a handle that stays the same: a mention on Slack for
    /// someone on Slack, which Slack shows as their display name, with
    /// their user id; and otherwise their name, if it isn't their id, and
    /// their id, as code, with their surface when it isn't the card's.
    fn requester(&self) -> String {
        let key = &self.consent.requester.key;
        let id = key.user.as_str();
        if key.surface == SurfaceKind::Slack && self.surface == SurfaceKind::Slack {
            return format!("<@{id}> ({})", self.code(id));
        }
        let who = match self.requester_name.filter(|name| *name != id) {
            Some(name) => format!("{} ({})", self.code(name), self.code(id)),
            None => self.code(id),
        };
        if key.surface == self.surface {
            who
        } else {
            format!("{who} on {}", surface_name(key.surface))
        }
    }

    /// `text` as inline code on the card's surface, escaped for Slack's
    /// mrkdwn there.
    fn code(&self, text: &str) -> String {
        slack_safe(self.surface, &inline_code(text))
    }

    /// Who asked where, and what the owner should know of how.
    fn heading(&self, bold: &str) -> String {
        let who = if self.owners {
            format!("you, as {}", self.requester())
        } else {
            format!("someone other than you: {}", self.requester())
        };
        let mut text = format!(
            "{bold}Private task request{bold} for *{}* from {who}, in {}.",
            slack_safe(self.surface, self.agent),
            place(&self.consent.thread, self.surface),
        );
        let hop = self.consent.hop.0;
        if hop > 0 {
            text.push_str(&format!(
                " It was asked for in a turn another agent's message started (hop {hop}), so \
                 its text may not be what the requester wrote."
            ));
        } else if self.owners {
            text.push_str(
                " You asked for it outside your own DM with the agent, so others' messages in \
                 the thread may have steered it.",
            );
        }
        if self.paused {
            text.push_str(&format!(
                " {} is paused: if you approve, the task runs once it is resumed, if that is \
                 before it expires.",
                slack_safe(self.surface, self.agent)
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
    /// the task under a label, boxed as preformatted literal text, the
    /// files under a label, each its own element of one `context` block
    /// (Slack takes at most 10, as many as [`MAX_FILES`](super::MAX_FILES)),
    /// and what approving means.
    fn blocks(&self) -> Vec<Value> {
        let mut blocks = vec![
            json!({
                "type": "section",
                "text": {"type": "mrkdwn", "text": self.heading("*")},
            }),
            json!({
                "type": "context",
                "elements": [{
                    "type": "plain_text",
                    "text": "The task, exactly as written:",
                    "emoji": false,
                }],
            }),
            json!({
                "type": "rich_text",
                "elements": [{
                    "type": "rich_text_preformatted",
                    "elements": [{"type": "text", "text": self.consent.task}],
                }],
            }),
        ];
        if !self.files.is_empty() {
            blocks.push(json!({
                "type": "context",
                "elements": [{
                    "type": "plain_text",
                    "text": "Files handed to it:",
                    "emoji": false,
                }],
            }));
            let names: Vec<Value> = self
                .files
                .iter()
                .map(|name| json!({"type": "plain_text", "text": name, "emoji": false}))
                .collect();
            blocks.push(json!({"type": "context", "elements": names}));
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
     memory. The files handed to it aren't shown here, and their contents can direct it like \
     its text. Only its reply and the files it attaches are posted to the thread.";

/// What approving a task the owner's identity asked for, outside the
/// owner's own DM with the agent, means.
const OWNER_TERMS: &str = "If you approve, it runs once in a new private session on your \
     Claude account, on your side: it can read and change your agent's shared files and its \
     memory. The files handed to it aren't shown here, and their contents can direct it like \
     its text. Only its reply and the files it attaches are posted to the thread, where \
     everyone in it can read them.";

/// The text objects of a Slack block, rich text's nested elements
/// included.
fn block_texts(block: &Value) -> Vec<&str> {
    let mut texts: Vec<&str> = block
        .pointer("/text/text")
        .and_then(Value::as_str)
        .into_iter()
        .collect();
    for element in block
        .get("elements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        texts.extend(element.get("text").and_then(Value::as_str));
        texts.extend(block_texts(element));
    }
    texts
}

/// `text` escaped for Slack's mrkdwn when it is shown on Slack, so `&`,
/// `<` and `>` in it can't form a link, a mention or a broadcast.
fn slack_safe(surface: SurfaceKind, text: &str) -> String {
    if surface == SurfaceKind::Slack {
        render::slack::escape(text)
    } else {
        text.to_owned()
    }
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

/// How a card shown on `surface` names the thread the task was asked for
/// in: a link to it on Slack, and otherwise its conversation and thread
/// ids, naming its surface when it isn't the card's.
fn place(thread: &ThreadKey, surface: SurfaceKind) -> String {
    let conv = &thread.conv;
    let link = surface_slack::surface::thread_link(thread).filter(|_| thread.root.is_some());
    match (conv.surface, surface) {
        (SurfaceKind::Slack, SurfaceKind::Slack) => match link {
            Some(link) => format!("<#{}> (<{link}|the thread>)", conv.conversation),
            None => format!("<#{}>", conv.conversation),
        },
        (SurfaceKind::Slack, SurfaceKind::RocketChat) => {
            let channel = format!("Slack channel {}", inline_code(conv.conversation.as_str()));
            match link {
                Some(link) => format!("{channel} ([the thread]({link}))"),
                None => channel,
            }
        }
        (SurfaceKind::RocketChat, _) => {
            let label = if surface == SurfaceKind::RocketChat {
                "room"
            } else {
                "Rocket.Chat room"
            };
            let code = |text: &str| slack_safe(surface, &inline_code(text));
            match &thread.root {
                Some(root) => format!(
                    "{label} {}, thread {}",
                    code(conv.conversation.as_str()),
                    code(root.as_str())
                ),
                None => format!("{label} {}", code(conv.conversation.as_str())),
            }
        }
    }
}

/// A surface's name, as people know it.
fn surface_name(surface: SurfaceKind) -> &'static str {
    match surface {
        SurfaceKind::Slack => "Slack",
        SurfaceKind::RocketChat => "Rocket.Chat",
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
            requester: Requester {
                member: None,
                key,
                outside: None,
            },
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
    fn a_slack_card_boxes_the_exact_task_as_literal_text_with_buttons() {
        let mut consent = consent(SurfaceKind::Slack, "Summarize *my* <!channel> notes");
        let files = ["a.csv".to_owned()];
        let card = Card {
            consent: &consent,
            agent: "helper",
            files: &files,
            owners: false,
            paused: false,
            surface: consent.thread.conv.surface,
            requester_name: None,
        };
        let open = card.open();
        let blocks = open.blocks.unwrap();
        let text = blocks.to_string();
        assert_eq!(
            blocks[1]["elements"][0]["text"],
            "The task, exactly as written:"
        );
        assert_eq!(
            blocks[2],
            json!({"type": "rich_text", "elements": [{
                "type": "rich_text_preformatted",
                "elements": [{"type": "text", "text": "Summarize *my* <!channel> notes"}],
            }]})
        );
        assert!(
            text.contains("from someone other than you: <@U0BOB> (`U0BOB`)"),
            "{text}"
        );
        assert!(
            text.contains("https://app.slack.com/client/T0TEAM001/C0CHAN001/thread/"),
            "{text}"
        );
        assert_eq!(blocks[3]["elements"][0]["text"], "Files handed to it:");
        assert_eq!(
            blocks[4],
            json!({"type": "context", "elements": [
                {"type": "plain_text", "text": "a.csv", "emoji": false},
            ]})
        );
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
            surface: consent.thread.conv.surface,
            requester_name: None,
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
                surface: consent.thread.conv.surface,
                requester_name: None,
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
            surface: consent.thread.conv.surface,
            requester_name: None,
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
            surface: consent.thread.conv.surface,
            requester_name: None,
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
    fn a_card_names_the_requester_and_thread_for_the_surface_it_is_shown_on() {
        let slack = consent(SurfaceKind::Slack, "x");
        let on_rocketchat = Card {
            consent: &slack,
            agent: "helper",
            files: &[],
            owners: false,
            paused: false,
            surface: SurfaceKind::RocketChat,
            requester_name: Some("bob.smith"),
        }
        .open()
        .markdown;
        assert!(
            on_rocketchat.contains("from someone other than you: `bob.smith` (`U0BOB`) on Slack, in Slack channel `C0CHAN001` ([the thread](https://app.slack.com/client/T0TEAM001/C0CHAN001/thread/"),
            "{on_rocketchat}"
        );
        assert!(!on_rocketchat.contains("<@"), "{on_rocketchat}");

        let rocketchat = consent(SurfaceKind::RocketChat, "x");
        let card = |surface, requester_name| {
            Card {
                consent: &rocketchat,
                agent: "helper",
                files: &[],
                owners: false,
                paused: false,
                surface,
                requester_name,
            }
            .open()
        };
        let named = card(SurfaceKind::RocketChat, Some("bob")).markdown;
        assert!(
            named.contains("from someone other than you: `bob` (`U0BOB`), in room `C0CHAN001`"),
            "{named}"
        );
        let unnamed = card(SurfaceKind::RocketChat, None).markdown;
        assert!(
            unnamed.contains("from someone other than you: `U0BOB`, in room"),
            "{unnamed}"
        );
        let on_slack = card(SurfaceKind::Slack, Some("<!here|a&b>"))
            .blocks
            .unwrap()
            .to_string();
        assert!(
            on_slack.contains(
                "from someone other than you: `&lt;!here|a&amp;b&gt;` (`U0BOB`) on Rocket.Chat, \
                 in Rocket.Chat room `C0CHAN001`"
            ),
            "Slack-bound names are escaped: {on_slack}"
        );
        let owners = Card {
            consent: &rocketchat,
            agent: "helper",
            files: &[],
            owners: true,
            paused: false,
            surface: SurfaceKind::RocketChat,
            requester_name: Some("alice"),
        }
        .open()
        .markdown;
        assert!(
            owners.contains("from you, as `alice` (`U0BOB`)"),
            "{owners}"
        );
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
                surface: consent.thread.conv.surface,
                requester_name: None,
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
        let longest = longest_names();
        assert_eq!(card("x", &longest), Ok(()), "ten 255-byte names fit");
        let err = card(&"a".repeat(2500), &longest).unwrap_err();
        assert!(err.contains("one consent card message"), "{err}");
    }

    /// Ten file names of 255 bytes, the most a task may be handed.
    fn longest_names() -> Vec<String> {
        (0..crate::consents::MAX_FILES)
            .map(|n| format!("{n}{}", "x".repeat(254)))
            .collect()
    }

    #[test]
    fn a_slack_card_gives_each_file_its_own_context_element() {
        let consent = consent(SurfaceKind::Slack, "Summarize these");
        let files = longest_names();
        let blocks = Card {
            consent: &consent,
            agent: "helper",
            files: &files,
            owners: false,
            paused: true,
            surface: consent.thread.conv.surface,
            requester_name: None,
        }
        .open()
        .blocks
        .unwrap();
        let blocks = blocks.as_array().unwrap();
        let label = blocks
            .iter()
            .position(|block| block["elements"][0]["text"] == "Files handed to it:")
            .expect("the files' label");
        assert_eq!(blocks[label]["type"], "context");
        assert_eq!(blocks[label]["elements"].as_array().unwrap().len(), 1);
        let names = &blocks[label + 1];
        assert_eq!(names["type"], "context");
        let elements = names["elements"].as_array().unwrap();
        assert_eq!(elements.len(), files.len());
        for (element, name) in elements.iter().zip(&files) {
            assert_eq!(
                *element,
                json!({"type": "plain_text", "text": name, "emoji": false})
            );
        }
        for block in blocks.iter().filter(|block| block["type"] == "context") {
            let elements = block["elements"].as_array().unwrap();
            assert!(elements.len() <= 10, "{block}");
            for element in elements {
                let text = element["text"].as_str().unwrap();
                assert!(utf16_len(text) < 2000, "{block}");
            }
        }
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

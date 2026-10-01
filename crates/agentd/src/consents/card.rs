//! What the owner and the thread are told about a consent: the consent
//! card, its closed form, and the outcomes posted to the thread.

use core_types::{ConsentId, MemberKey, SurfaceKind, ThreadKey};
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

/// What the thread is told when a private task couldn't be run, however
/// often it was tried.
pub fn failed_text(id: ConsentId) -> String {
    format!("Private task `{id}`: it couldn't be run. Ask again later.")
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
}

impl Card<'_> {
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
            SurfaceKind::RocketChat => format!("`{}`", key.user.as_str().replace('`', "")),
        }
    }

    fn files_line(&self) -> Option<String> {
        (!self.files.is_empty()).then(|| format!("Files handed to it: {}.", self.files.join(", ")))
    }

    /// The Markdown both surfaces' cards share, before how to answer.
    fn markdown(&self) -> String {
        let task = &self.consent.task;
        let fence = "`".repeat(longest_run(task, '`').max(2) + 1);
        let mut text = format!(
            "**Private task request** for *{}* from {}, in {}.\n\nThe task, exactly as \
             written:\n{fence}\n{task}\n{fence}\n",
            self.agent,
            self.requester(),
            place(&self.consent.thread),
        );
        if let Some(files) = self.files_line() {
            text.push_str(&format!("{}\n", files.replace('`', "")));
        }
        text.push_str(TERMS);
        text
    }

    /// The Block Kit both states of a Slack card share: who asked where,
    /// the task as plain text, the files and what approving means.
    fn blocks(&self) -> Vec<Value> {
        let mut blocks = vec![
            json!({
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!(
                        "*Private task request* for *{}* from {}, in {}.",
                        self.agent,
                        self.requester(),
                        place(&self.consent.thread),
                    ),
                },
            }),
            json!({
                "type": "section",
                "text": {"type": "plain_text", "text": self.consent.task, "emoji": false},
            }),
        ];
        if let Some(files) = self.files_line() {
            blocks.push(json!({
                "type": "context",
                "elements": [{"type": "plain_text", "text": files, "emoji": false}],
            }));
        }
        blocks.push(json!({
            "type": "context",
            "elements": [{"type": "mrkdwn", "text": TERMS}],
        }));
        blocks
    }
}

/// What approving a card means. A card is only sent for someone other
/// than the owner, whose task never gets the owner's side.
const TERMS: &str = "If you approve, it runs once in a new private session on your Claude \
     account. It can read your agent's shared files but not change them, and doesn't see its \
     memory. Only its reply and the files it attaches are posted to the thread.";

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
                "room `{}`, thread `{}`",
                conv.conversation.as_str().replace('`', ""),
                root.as_str().replace('`', "")
            ),
            None => format!("room `{}`", conv.conversation.as_str().replace('`', "")),
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

/// The longest run of `c` in `text`.
fn longest_run(text: &str, c: char) -> usize {
    text.split(|other| other != c)
        .map(str::len)
        .max()
        .unwrap_or(0)
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
        }
        .closed();
        let text = closed.blocks.unwrap().to_string();
        assert!(!text.contains("actions"), "{text}");
        assert!(text.contains("Approved by <@U0BOB>."), "{text}");
    }

    #[test]
    fn a_rocketchat_card_fences_the_task_past_its_own_backticks() {
        let consent = consent(SurfaceKind::RocketChat, "run ```rm``` and ````x````");
        let card = Card {
            consent: &consent,
            agent: "helper",
            files: &[],
        }
        .open();
        assert!(
            card.markdown
                .contains("\n`````\nrun ```rm``` and ````x````\n`````\n")
        );
        assert!(card.markdown.contains("room `C0CHAN001`, thread"));
        assert!(!card.markdown.contains("Files handed"));
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
        assert_eq!(longest_run("a``b```", '`'), 3);
        assert_eq!(longest_run("none", '`'), 0);
    }
}

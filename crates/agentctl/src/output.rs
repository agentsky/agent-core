//! What agentctl prints: plain text for the model.

use std::fmt::Write as _;

use core_types::{AttachResponse, Msg, PrivateResponse};
use time::format_description::well_known::Rfc3339;

/// What `post` prints.
pub const POSTED: &str = "Queued. The message is posted after this turn.\n";
/// What `ask-agent` prints.
pub const ASKED: &str = "Queued. The task is posted in this thread after this turn, and the other agent may answer there.\n";

/// `s` on one line: control characters, line breaks included, become
/// spaces.
pub fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// What `attach` prints.
pub fn attached(response: &AttachResponse) -> String {
    format!(
        "Staged {} ({} bytes). It is uploaded with this turn's reply.\n",
        one_line(&response.name),
        response.size
    )
}

/// What `react` prints.
pub fn reacted(emoji: &str) -> String {
    format!(
        "Queued :{}:. The reaction is added after this turn.\n",
        one_line(emoji.trim().trim_matches(':'))
    )
}

/// What `private` prints.
pub fn private(response: &PrivateResponse) -> String {
    format!(
        "Asked for consent {}. The result is posted to this thread when the task finishes.\n",
        response.consent
    )
}

/// What `history` prints: each message's header line, then its text and
/// files, with a blank line between messages.
pub fn history(messages: &[Msg]) -> String {
    if messages.is_empty() {
        return "No messages.\n".to_owned();
    }
    let mut out = String::new();
    for (i, msg) in messages.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let sent = msg
            .sent_at
            .format(&Rfc3339)
            .unwrap_or_else(|_| msg.sent_at.unix_timestamp().to_string());
        let bot = if msg.sender_is_bot { " (bot)" } else { "" };
        let _ = writeln!(
            out,
            "[{}] {}{bot} at {sent}:",
            one_line(msg.id.as_str()),
            one_line(msg.sender.user.as_str())
        );
        if !msg.text.is_empty() {
            out.push_str(&msg.text);
            if !msg.text.ends_with('\n') {
                out.push('\n');
            }
        }
        for file in &msg.files {
            let _ = writeln!(out, "(file: {})", one_line(&file.name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use core_types::{ConsentId, InFile, MemberKey, MessageId, SurfaceKind};
    use time::macros::datetime;

    use super::*;

    fn msg(id: &str, text: &str, bot: bool) -> Msg {
        Msg {
            id: MessageId::new(id),
            sender: MemberKey {
                surface: SurfaceKind::Slack,
                team: "T1".into(),
                user: "U1".into(),
            },
            sender_is_bot: bot,
            text: text.into(),
            files: vec![],
            sent_at: datetime!(2026-09-30 10:00 UTC),
        }
    }

    #[test]
    fn one_line_flattens_control_characters() {
        assert_eq!(one_line(" a\nb\r\tc\u{1b}[2J "), "a b  c [2J");
    }

    #[test]
    fn history_prints_headers_text_and_files() {
        let mut second = msg("2", "line one\nline two\n", true);
        second.files.push(InFile {
            id: "F1".into(),
            name: "plot.png".into(),
            mime_type: None,
            size: None,
            url: "https://example.com/plot.png".into(),
        });
        assert_eq!(
            history(&[msg("1", "hello", false), second, msg("3", "", false)]),
            "[1] U1 at 2026-09-30T10:00:00Z:\nhello\n\n\
             [2] U1 (bot) at 2026-09-30T10:00:00Z:\nline one\nline two\n(file: plot.png)\n\n\
             [3] U1 at 2026-09-30T10:00:00Z:\n"
        );
        assert_eq!(history(&[]), "No messages.\n");
    }

    #[test]
    fn results_are_one_sentence_each() {
        assert_eq!(
            attached(&AttachResponse {
                name: "a\nb".into(),
                size: 3
            }),
            "Staged a b (3 bytes). It is uploaded with this turn's reply.\n"
        );
        assert_eq!(
            reacted(":eyes:"),
            "Queued :eyes:. The reaction is added after this turn.\n"
        );
        let consent = ConsentId::new_v4();
        assert!(private(&PrivateResponse { consent }).contains(&consent.to_string()));
    }
}

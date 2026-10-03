//! Slack request signing and payload fixtures, for tests of the Slack
//! ingress.
//!
//! [`sign`] computes Slack's `v0` signature independently of
//! `surface-slack`, so a test that signs with it checks the verifier against
//! a second implementation. [`signed_headers`] gives the two headers a
//! request needs.
//!
//! The fixtures are in `crates/testkit/fixtures/slack/`. They are written by
//! hand in the shapes Slack sends, taken from the payloads in Slack's own
//! SDK test suites (`slackapi/bolt-python`, `slackapi/bolt-js`) and Slack's
//! documentation, with made-up ids. Every Events API fixture is from team
//! [`TEAM`]; the agent app receiving them has the bot user [`BOT_USER`].
//! None of them holds a real token.
//!
//! # Slack Connect
//!
//! The Slack Connect fixtures ([`MESSAGE_CONNECT_NO_ACTOR_TEAM`],
//! [`MESSAGE_CONNECT_THEIR_TEAM`], [`MESSAGE_CONNECT_HOME`],
//! [`MESSAGE_HOME_ORG`], [`MESSAGE_WITHOUT_AUTHORIZATIONS`],
//! [`BLOCK_ACTIONS_WITHOUT_USER_TEAM`], [`BLOCK_ACTIONS_OUTSIDE`]) are
//! still written by hand: the first after `slackapi/bolt-python`'s
//! `slack_connect_events_api_no_actor_team_requests`, the rest after the
//! design's reading of Slack's documentation. T36e replaces them with
//! redacted captures from a real shared channel. An outside member is
//! [`OUTSIDE_USER`], of [`OUTSIDE_TEAM`]; the home workspace's Enterprise
//! Grid organization is [`HOME_ORG`].

use hmac::{Hmac, KeyInit as _, Mac as _};
use sha2::Sha256;

/// The team every fixture comes from.
pub const TEAM: &str = "T0TEAM001";
/// The bot user of the agent app the Events API fixtures are sent to.
pub const BOT_USER: &str = "U0BOT0001";
/// The human who sends most fixtures.
pub const USER: &str = "U0HUMAN01";
/// A second human.
pub const OTHER_USER: &str = "U0HUMAN02";
/// The public channel most fixtures are in.
pub const CHANNEL: &str = "C0CHAN001";
/// A member of another organization, in a channel shared with it.
pub const OUTSIDE_USER: &str = "U0OUTSID1";
/// [`OUTSIDE_USER`]'s own workspace.
pub const OUTSIDE_TEAM: &str = "T0THEIRS1";
/// The Enterprise Grid organization [`TEAM`] belongs to, in
/// [`MESSAGE_HOME_ORG`].
pub const HOME_ORG: &str = "E0HOMEORG";
/// The externally shared channel the Slack Connect fixtures are in.
pub const SHARED_CHANNEL: &str = "C0SHARED1";

/// A `url_verification` request, whose challenge is [`CHALLENGE`].
pub const URL_VERIFICATION: &str = include_str!("../fixtures/slack/url_verification.json");
/// The challenge in [`URL_VERIFICATION`].
pub const CHALLENGE: &str = "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P";

/// A top-level channel message (`message.channels`) from [`USER`] that
/// mentions [`BOT_USER`] and then [`OTHER_USER`], in the text and in a
/// `rich_text` block.
pub const MESSAGE_MENTION: &str = include_str!("../fixtures/slack/message_mention.json");
/// A top-level channel message that mentions only [`OTHER_USER`].
pub const MESSAGE_PLAIN: &str = include_str!("../fixtures/slack/message_plain.json");
/// A thread reply in a channel, with `thread_ts` and `parent_user_id`,
/// that mentions no one.
pub const MESSAGE_THREAD_REPLY: &str = include_str!("../fixtures/slack/message_thread_reply.json");
/// A thread reply also sent to the channel: subtype `thread_broadcast`,
/// with the thread's `root`.
pub const MESSAGE_THREAD_BROADCAST: &str =
    include_str!("../fixtures/slack/message_thread_broadcast.json");
/// A DM to the bot (`message.im`, `channel_type: im`).
pub const MESSAGE_IM: &str = include_str!("../fixtures/slack/message_im.json");
/// A group DM (`message.mpim`, `channel_type: mpim`) that mentions no one.
pub const MESSAGE_MPIM: &str = include_str!("../fixtures/slack/message_mpim.json");
/// A private channel message (`message.groups`, `channel_type: group`)
/// that mentions [`BOT_USER`].
pub const MESSAGE_GROUP: &str = include_str!("../fixtures/slack/message_group.json");
/// Another app's bot mentioning [`BOT_USER`], as a current Slack app posts:
/// no subtype, with `bot_id`, `bot_profile` and its bot user in `user`, and
/// the mention in a `mrkdwn` section block too.
pub const MESSAGE_BOT: &str = include_str!("../fixtures/slack/message_bot.json");
/// A bot's thread reply with `bot_id` and `bot_profile` but no `user`,
/// mentioning [`BOT_USER`] in its text.
pub const MESSAGE_BOT_WITHOUT_USER: &str =
    include_str!("../fixtures/slack/message_bot_without_user.json");
/// A DM with a file: subtype `file_share` and one entry in `files`.
pub const MESSAGE_FILE_SHARE: &str = include_str!("../fixtures/slack/message_file_share.json");
/// An edit of [`MESSAGE_MENTION`]: subtype `message_changed`.
pub const MESSAGE_CHANGED: &str = include_str!("../fixtures/slack/message_changed.json");
/// A `user_change` event for a deleted member, as the manager app receives
/// it.
pub const USER_CHANGE: &str = include_str!("../fixtures/slack/user_change.json");
/// An `app_rate_limited` notice.
pub const APP_RATE_LIMITED: &str = include_str!("../fixtures/slack/app_rate_limited.json");
/// A form-encoded `/agent create helper You are terse.` from [`USER`] in
/// [`CHANNEL`], as the manager app receives it.
pub const SLASH_COMMAND: &str = include_str!("../fixtures/slack/slash_command.txt");
/// A `block_actions` interactivity payload (a consent card's Approve
/// button), as JSON. [`interactivity_body`] form-encodes it the way Slack
/// sends it.
pub const BLOCK_ACTIONS: &str = include_str!("../fixtures/slack/block_actions.json");

/// [`OUTSIDE_USER`] mentioning [`BOT_USER`] in [`SHARED_CHANNEL`], with
/// the envelope's `team_id` and the event's `team` both the installing
/// team, and [`OUTSIDE_TEAM`] only in `user_team`, `source_team` and
/// `user_profile.team`, as in bolt-python's
/// `slack_connect_events_api_no_actor_team_requests`.
pub const MESSAGE_CONNECT_NO_ACTOR_TEAM: &str =
    include_str!("../fixtures/slack/message_connect_no_actor_team.json");
/// [`OUTSIDE_USER`] mentioning [`BOT_USER`] in [`SHARED_CHANNEL`], with
/// [`OUTSIDE_TEAM`] in the envelope's `team_id` and in every team field of
/// the event, and the installation, [`TEAM`], only in `authorizations`.
pub const MESSAGE_CONNECT_THEIR_TEAM: &str =
    include_str!("../fixtures/slack/message_connect_their_team.json");
/// [`USER`], a home member, mentioning [`BOT_USER`] in [`SHARED_CHANNEL`],
/// with every team field [`TEAM`].
pub const MESSAGE_CONNECT_HOME: &str = include_str!("../fixtures/slack/message_connect_home.json");
/// [`USER`] mentioning [`BOT_USER`] from an Enterprise Grid workspace,
/// with [`HOME_ORG`] in `user_team` and `user_profile.team`.
pub const MESSAGE_HOME_ORG: &str = include_str!("../fixtures/slack/message_home_org.json");
/// [`MESSAGE_MENTION`] without `authorizations`, under its own event id.
pub const MESSAGE_WITHOUT_AUTHORIZATIONS: &str =
    include_str!("../fixtures/slack/message_without_authorizations.json");
/// [`BLOCK_ACTIONS`] without `user.team_id`.
pub const BLOCK_ACTIONS_WITHOUT_USER_TEAM: &str =
    include_str!("../fixtures/slack/block_actions_without_user_team.json");
/// [`BLOCK_ACTIONS`] clicked by [`OUTSIDE_USER`], whose `user.team_id` is
/// [`OUTSIDE_TEAM`].
pub const BLOCK_ACTIONS_OUTSIDE: &str =
    include_str!("../fixtures/slack/block_actions_outside.json");

/// Every fixture, by file name.
pub const ALL: [(&str, &str); 23] = [
    ("url_verification.json", URL_VERIFICATION),
    ("message_mention.json", MESSAGE_MENTION),
    ("message_plain.json", MESSAGE_PLAIN),
    ("message_thread_reply.json", MESSAGE_THREAD_REPLY),
    ("message_thread_broadcast.json", MESSAGE_THREAD_BROADCAST),
    ("message_im.json", MESSAGE_IM),
    ("message_mpim.json", MESSAGE_MPIM),
    ("message_group.json", MESSAGE_GROUP),
    ("message_bot.json", MESSAGE_BOT),
    ("message_bot_without_user.json", MESSAGE_BOT_WITHOUT_USER),
    ("message_file_share.json", MESSAGE_FILE_SHARE),
    ("message_changed.json", MESSAGE_CHANGED),
    ("user_change.json", USER_CHANGE),
    ("app_rate_limited.json", APP_RATE_LIMITED),
    ("slash_command.txt", SLASH_COMMAND),
    ("block_actions.json", BLOCK_ACTIONS),
    (
        "message_connect_no_actor_team.json",
        MESSAGE_CONNECT_NO_ACTOR_TEAM,
    ),
    (
        "message_connect_their_team.json",
        MESSAGE_CONNECT_THEIR_TEAM,
    ),
    ("message_connect_home.json", MESSAGE_CONNECT_HOME),
    ("message_home_org.json", MESSAGE_HOME_ORG),
    (
        "message_without_authorizations.json",
        MESSAGE_WITHOUT_AUTHORIZATIONS,
    ),
    (
        "block_actions_without_user_team.json",
        BLOCK_ACTIONS_WITHOUT_USER_TEAM,
    ),
    ("block_actions_outside.json", BLOCK_ACTIONS_OUTSIDE),
];

/// Slack's `v0` signature of `body` sent at `timestamp` (Unix seconds),
/// with the app's signing `secret`: `v0=` and the lowercase hex
/// HMAC-SHA256 of `v0:{timestamp}:{body}`.
pub fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .expect("HMAC-SHA256 takes a key of any length");
    mac.update(format!("v0:{timestamp}:").as_bytes());
    mac.update(body);
    let tag = mac.finalize().into_bytes();
    let mut signature = String::with_capacity(3 + tag.len() * 2);
    signature.push_str("v0=");
    for byte in tag {
        signature.push_str(&format!("{byte:02x}"));
    }
    signature
}

/// The current time in Unix seconds, for signing a fresh request.
pub fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// The two headers of a request signed at `timestamp`:
/// `x-slack-request-timestamp` and `x-slack-signature`.
pub fn signed_headers(secret: &str, timestamp: i64, body: &[u8]) -> [(&'static str, String); 2] {
    [
        ("x-slack-request-timestamp", timestamp.to_string()),
        ("x-slack-signature", sign(secret, timestamp, body)),
    ]
}

/// An interactivity request body: `payload=` and the form-encoded JSON.
pub fn interactivity_body(payload: &str) -> String {
    serde_urlencoded::to_string([("payload", payload)]).expect("a string pair always encodes")
}

/// A fixture with its `event_id` replaced, for a second delivery of the same
/// message under a new id.
pub fn with_event_id(fixture: &str, event_id: &str) -> String {
    let mut value: serde_json::Value = serde_json::from_str(fixture).expect("the fixture is JSON");
    value["event_id"] = event_id.into();
    value.to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn json(fixture: &str) -> Value {
        serde_json::from_str(fixture).unwrap()
    }

    #[test]
    fn sign_matches_slacks_documented_example() {
        let body = "token=xyzz0WbapA4vBCDEFasx0q6G&team_id=T1DC2JH3J&team_domain=testteamnow&channel_id=G8PSS9T3V&channel_name=foobar&user_id=U2CERLKJA&user_name=roadrunner&command=%2Fwebhook-collect&text=&response_url=https%3A%2F%2Fhooks.slack.com%2Fcommands%2FT1DC2JH3J%2F397700885554%2F96rGlfmibIGlgcZRskXaIFfN&trigger_id=398738663015.47445629121.803a0bc887a14d10d2c447fce8b6703c";
        assert_eq!(
            sign(
                "8f742231b10e8888abcd99yyyzzz85a5",
                1_531_420_618,
                body.as_bytes()
            ),
            "v0=a2114d57b48eac39b9ad189dd8316235a7b4a8d21a10bd27519666489c69b503"
        );
    }

    #[test]
    fn signed_headers_carry_the_timestamp_and_signature() {
        let [(ts_name, ts), (sig_name, sig)] = signed_headers("s", 42, b"{}");
        assert_eq!((ts_name, ts.as_str()), ("x-slack-request-timestamp", "42"));
        assert_eq!(sig_name, "x-slack-signature");
        assert_eq!(sig, sign("s", 42, b"{}"));
        assert!(now() > 1_700_000_000);
    }

    #[test]
    fn every_fixture_parses_and_holds_no_real_token() {
        for (name, fixture) in ALL {
            if name.ends_with(".json") {
                json(fixture);
            }
            for needle in ["xoxb-", "xoxp-", "xapp-", "xoxe"] {
                assert!(!fixture.contains(needle), "{name} contains {needle}");
            }
        }
    }

    #[test]
    fn event_fixtures_are_event_callbacks_from_the_team() {
        for (name, fixture) in ALL {
            if !name.starts_with("message_") && name != "user_change.json" {
                continue;
            }
            let value = json(fixture);
            assert_eq!(value["type"], "event_callback", "{name}");
            assert!(value["event_id"].is_string(), "{name}");
            if fixture == MESSAGE_WITHOUT_AUTHORIZATIONS {
                assert!(value.get("authorizations").is_none());
                continue;
            }
            assert_eq!(value["authorizations"][0]["team_id"], TEAM, "{name}");
            let envelope = if fixture == MESSAGE_CONNECT_THEIR_TEAM {
                OUTSIDE_TEAM
            } else {
                TEAM
            };
            assert_eq!(value["team_id"], envelope, "{name}");
        }
        assert_eq!(json(URL_VERIFICATION)["challenge"], CHALLENGE);
        assert_eq!(json(MESSAGE_MENTION)["event"]["channel"], CHANNEL);
        assert_eq!(json(MESSAGE_MENTION)["event"]["user"], USER);
        assert_eq!(json(MESSAGE_MPIM)["event"]["user"], OTHER_USER);
    }

    #[test]
    fn the_slash_command_and_interactivity_bodies_decode() {
        let form: Vec<(String, String)> = serde_urlencoded::from_str(SLASH_COMMAND).unwrap();
        let get = |key: &str| form.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        assert_eq!(get("command"), Some("/agent"));
        assert_eq!(get("text"), Some("create helper You are terse."));
        assert_eq!(get("user_id"), Some(USER));

        let body = interactivity_body(BLOCK_ACTIONS);
        let form: Vec<(String, String)> = serde_urlencoded::from_str(&body).unwrap();
        assert_eq!(form.len(), 1);
        assert_eq!(form[0].0, "payload");
        assert_eq!(json(&form[0].1)["type"], "block_actions");
    }

    #[test]
    fn the_slack_connect_fixtures_name_the_teams_they_say() {
        let event = |fixture: &str| json(fixture)["event"].clone();
        let no_actor = event(MESSAGE_CONNECT_NO_ACTOR_TEAM);
        assert_eq!(no_actor["user"], OUTSIDE_USER);
        assert_eq!(no_actor["team"], TEAM);
        for field in [
            &no_actor["user_team"],
            &no_actor["source_team"],
            &no_actor["user_profile"]["team"],
        ] {
            assert_eq!(field, OUTSIDE_TEAM);
        }
        assert_eq!(event(MESSAGE_CONNECT_THEIR_TEAM)["team"], OUTSIDE_TEAM);
        let home = event(MESSAGE_CONNECT_HOME);
        assert_eq!(home["channel"], SHARED_CHANNEL);
        assert_eq!(home["user_team"], TEAM);
        assert_eq!(event(MESSAGE_HOME_ORG)["user_team"], HOME_ORG);
        assert_eq!(
            json(MESSAGE_HOME_ORG)["authorizations"][0]["enterprise_id"],
            HOME_ORG
        );
        assert!(
            json(BLOCK_ACTIONS_WITHOUT_USER_TEAM)["user"]
                .get("team_id")
                .is_none()
        );
        assert_eq!(json(BLOCK_ACTIONS)["user"]["team_id"], TEAM);
        assert_eq!(json(BLOCK_ACTIONS_OUTSIDE)["user"]["team_id"], OUTSIDE_TEAM);
    }

    #[test]
    fn with_event_id_replaces_only_the_id() {
        let replaced = json(&with_event_id(MESSAGE_MENTION, "Ev0OTHER"));
        let original = json(MESSAGE_MENTION);
        assert_eq!(replaced["event_id"], "Ev0OTHER");
        assert_eq!(replaced["event"], original["event"]);
    }
}

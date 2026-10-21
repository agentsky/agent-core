//! Agent apps: the manifest each agent's Slack app is created from, and the
//! link that installs it.
//!
//! Every agent is its own Slack app, created with `apps.manifest.create`
//! ([`SlackClient::create_app`](crate::SlackClient::create_app)) from
//! [`agent_manifest`]. The app's request URLs carry agentd's binding id,
//! which exists before Slack assigns the app one. The app declares no slash
//! command, since only the manager app may own `/agent`, and hears every
//! message in the conversations its bot user is in through the
//! `message.*` events rather than `app_mention` (see
//! [`normalize`](crate::normalize)), and `channel_id_changed`, which says
//! a private channel it is in got a new id when it was shared with another
//! organization.
//!
//! An app keeps the manifest it was made from until agentd updates it: it
//! reads the app's manifest with `apps.manifest.export`
//! ([`SlackClient::export_app`](crate::SlackClient::export_app)), adds the
//! [`ADDED_BOT_EVENTS`] it lacks ([`add_bot_events`]) and writes it back
//! with `apps.manifest.update`
//! ([`SlackClient::update_app`](crate::SlackClient::update_app)), so
//! nothing else in it changes, events its owner removed included. Each new
//! bot event an existing app should get goes into [`ADDED_BOT_EVENTS`] and
//! raises [`MANIFEST_VERSION`], and agentd records the version each app
//! has. A change of scopes would take a new install, which an update can't
//! do, so a version never adds one.
//!
//! A member installs the app from [`install_url`]: Slack's OAuth consent
//! page, which redirects to agentd's [`OAUTH_CALLBACK_PATH`] with a code
//! that `oauth.v2.access`
//! ([`SlackClient::install_app`](crate::SlackClient::install_app)) turns into
//! the bot token.

use core_types::BindingId;
use serde_json::{Value, json};

/// Where the OAuth consent page for an install is.
pub const OAUTH_AUTHORIZE_URL: &str = "https://slack.com/oauth/v2/authorize";

/// The path of agentd's OAuth callback on its public listener, which every
/// agent app's manifest names as its redirect URL.
pub const OAUTH_CALLBACK_PATH: &str = "/slack/oauth/callback";

/// The bot events every agent app subscribes to: every message in public
/// and private channels, DMs and group DMs its bot user is in, and a
/// channel it is in getting a new id.
pub const BOT_EVENTS: [&str; 5] = [
    "message.channels",
    "message.groups",
    "message.im",
    "message.mpim",
    "channel_id_changed",
];

/// The version of [`agent_manifest`]: 0 for the manifest without
/// `channel_id_changed`, 1 since. An app made, or updated, from an older
/// version doesn't have what it added.
pub const MANIFEST_VERSION: u32 = 1;

/// The bot events [`agent_manifest`] gained since version 0, which an
/// update adds to an older app: only those, so an event its owner removed
/// stays removed.
pub const ADDED_BOT_EVENTS: [&str; 1] = ["channel_id_changed"];

/// Adds to `manifest`, an app's manifest as `apps.manifest.export` gives
/// it, the [`ADDED_BOT_EVENTS`] its `settings.event_subscriptions.bot_events`
/// lacks, after the ones it has, and says whether it added any. `None`,
/// changing nothing, when the manifest has no such list of strings: an app
/// that subscribes to no events isn't one agentd should change.
pub fn add_bot_events(manifest: &mut Value) -> Option<bool> {
    let events = manifest
        .get_mut("settings")?
        .get_mut("event_subscriptions")?
        .get_mut("bot_events")?
        .as_array_mut()?;
    if !events.iter().all(Value::is_string) {
        return None;
    }
    let missing: Vec<&str> = ADDED_BOT_EVENTS
        .into_iter()
        .filter(|event| !events.iter().any(|known| known == event))
        .collect();
    events.extend(missing.iter().map(|event| json!(event)));
    Some(!missing.is_empty())
}

/// The bot scopes every agent app asks for. `chat:write.public` is added
/// only when [`AgentApp::public_posting`] is on. The `*:read` scopes let
/// `conversations.info` tell what kind of conversation a message is in
/// when it is confirmed.
pub const BOT_SCOPES: [&str; 15] = [
    "chat:write",
    "channels:history",
    "groups:history",
    "im:history",
    "mpim:history",
    "channels:read",
    "groups:read",
    "im:read",
    "mpim:read",
    "im:write",
    "reactions:write",
    "files:read",
    "files:write",
    "users:read",
    "channels:join",
];

/// The scope that lets a bot post in public channels it isn't in.
pub const PUBLIC_POSTING_SCOPE: &str = "chat:write.public";

/// What an agent's app is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentApp<'a> {
    /// The agent's name: the app's name and its bot user's display name.
    pub name: &'a str,
    /// agentd's public HTTPS URL, without a trailing slash, as Slack
    /// reaches the public listener.
    pub public_url: &'a str,
    /// The binding the app belongs to, which its request URLs name.
    pub binding: BindingId,
    /// Whether the app asks for `chat:write.public`.
    pub public_posting: bool,
}

impl AgentApp<'_> {
    /// The bot scopes the app asks for, in [`BOT_SCOPES`]' order, with
    /// `chat:write.public` after `chat:write` when public posting is on.
    pub fn scopes(&self) -> Vec<&'static str> {
        let mut scopes = BOT_SCOPES.to_vec();
        if self.public_posting {
            scopes.insert(1, PUBLIC_POSTING_SCOPE);
        }
        scopes
    }

    /// Where Slack sends the install's code: `{public_url}` and
    /// [`OAUTH_CALLBACK_PATH`].
    pub fn redirect_url(&self) -> String {
        format!("{}{OAUTH_CALLBACK_PATH}", self.public_url)
    }

    fn request_url(&self, kind: &str) -> String {
        format!("{}/slack/b/{}/{kind}", self.public_url, self.binding)
    }
}

/// agentd's public URL as agent apps' manifests need it, checked: an
/// `https://` URL with a host and no user info, query or fragment, returned
/// without trailing slashes so paths can be appended. `None` for anything
/// else. Slack only takes `https` request URLs.
pub fn public_url(text: &str) -> Option<String> {
    let url = reqwest::Url::parse(text).ok()?;
    let usable = url.scheme() == "https"
        && url.host_str().is_some_and(|host| !host.is_empty())
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none();
    usable.then(|| url.as_str().trim_end_matches('/').to_owned())
}

/// The manifest of an agent's app, as `apps.manifest.create` takes it.
///
/// The app's name and its bot user's display name are the agent's name. It
/// declares no slash command, subscribes to [`BOT_EVENTS`], asks for
/// [`AgentApp::scopes`], and sends events and interactions to
/// `/slack/b/{binding}/events` and `…/interactivity`. Its messages tab is
/// on, so members can DM the agent. Socket Mode, token rotation and org-wide
/// deployment are off.
pub fn agent_manifest(app: &AgentApp<'_>) -> Value {
    json!({
        "display_information": {
            "name": app.name,
            "description": "A Claude Code agent. Mention it in a channel it is in, or DM it.",
        },
        "features": {
            "app_home": {
                "home_tab_enabled": false,
                "messages_tab_enabled": true,
                "messages_tab_read_only_enabled": false,
            },
            "bot_user": {
                "display_name": app.name,
                "always_online": true,
            },
        },
        "oauth_config": {
            "redirect_urls": [app.redirect_url()],
            "scopes": {"bot": app.scopes()},
        },
        "settings": {
            "event_subscriptions": {
                "request_url": app.request_url("events"),
                "bot_events": BOT_EVENTS,
            },
            "interactivity": {
                "is_enabled": true,
                "request_url": app.request_url("interactivity"),
            },
            "org_deploy_enabled": false,
            "socket_mode_enabled": false,
            "token_rotation_enabled": false,
        },
    })
}

/// The link that installs an app: Slack's OAuth consent page for
/// `client_id` with `scopes`, redirecting to `redirect_url` with `state`.
///
/// `state` comes first and `redirect_uri` last, so the link ends with the
/// letters of [`OAUTH_CALLBACK_PATH`]: a renderer that trims trailing
/// punctuation off a bare URL can't cut a `state` ending in `_` or `-`.
pub fn install_url(client_id: &str, scopes: &[&str], redirect_url: &str, state: &str) -> String {
    let scope = scopes.join(",");
    let query = serde_urlencoded::to_string([
        ("state", state),
        ("client_id", client_id),
        ("scope", &scope),
        ("redirect_uri", redirect_url),
    ])
    .unwrap_or_default();
    format!("{OAUTH_AUTHORIZE_URL}?{query}")
}

#[cfg(test)]
mod tests {
    use reqwest::Url;

    use super::*;

    fn app(public_posting: bool) -> AgentApp<'static> {
        AgentApp {
            name: "helper",
            public_url: "https://agentd.example.com",
            binding: "0f6a1c9e-2d3b-4c5d-8e7f-0123456789ab".parse().unwrap(),
            public_posting,
        }
    }

    #[test]
    fn the_manifest_names_the_agent_and_its_binding_urls() {
        assert_eq!(
            agent_manifest(&app(false)),
            json!({
                "display_information": {
                    "name": "helper",
                    "description": "A Claude Code agent. Mention it in a channel it is in, or DM it.",
                },
                "features": {
                    "app_home": {
                        "home_tab_enabled": false,
                        "messages_tab_enabled": true,
                        "messages_tab_read_only_enabled": false,
                    },
                    "bot_user": {"display_name": "helper", "always_online": true},
                },
                "oauth_config": {
                    "redirect_urls": ["https://agentd.example.com/slack/oauth/callback"],
                    "scopes": {"bot": [
                        "chat:write", "channels:history", "groups:history", "im:history",
                        "mpim:history", "channels:read", "groups:read", "im:read",
                        "mpim:read", "im:write", "reactions:write", "files:read",
                        "files:write", "users:read", "channels:join",
                    ]},
                },
                "settings": {
                    "event_subscriptions": {
                        "request_url": "https://agentd.example.com/slack/b/0f6a1c9e-2d3b-4c5d-8e7f-0123456789ab/events",
                        "bot_events": [
                            "message.channels", "message.groups", "message.im", "message.mpim",
                            "channel_id_changed",
                        ],
                    },
                    "interactivity": {
                        "is_enabled": true,
                        "request_url": "https://agentd.example.com/slack/b/0f6a1c9e-2d3b-4c5d-8e7f-0123456789ab/interactivity",
                    },
                    "org_deploy_enabled": false,
                    "socket_mode_enabled": false,
                    "token_rotation_enabled": false,
                },
            })
        );
    }

    #[test]
    fn agent_apps_declare_no_slash_command_and_no_app_mention() {
        let manifest = agent_manifest(&app(true));
        assert!(manifest["features"].get("slash_commands").is_none());
        let events = manifest["settings"]["event_subscriptions"]["bot_events"].to_string();
        assert!(!events.contains("app_mention"), "{events}");
    }

    #[test]
    fn public_posting_is_a_switch() {
        assert!(!app(false).scopes().contains(&PUBLIC_POSTING_SCOPE));
        let scopes = app(true).scopes();
        assert_eq!(&scopes[..2], ["chat:write", PUBLIC_POSTING_SCOPE]);
        assert_eq!(scopes.len(), BOT_SCOPES.len() + 1);
    }

    #[test]
    fn a_public_url_is_https_without_extras_and_loses_its_trailing_slash() {
        assert_eq!(
            public_url("https://agentd.example.com/").as_deref(),
            Some("https://agentd.example.com")
        );
        assert_eq!(
            public_url("https://example.com/agentd//").as_deref(),
            Some("https://example.com/agentd")
        );
        assert_eq!(
            public_url("https://example.com:8443").as_deref(),
            Some("https://example.com:8443")
        );
        for bad in [
            "http://agentd.example.com",
            "https://user:pw@agentd.example.com",
            "https://agentd.example.com/?x=1",
            "https://agentd.example.com/#x",
            "agentd.example.com",
            "",
        ] {
            assert_eq!(public_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn only_the_bot_events_added_since_are_added_and_nothing_else_changes() {
        assert!(
            ADDED_BOT_EVENTS
                .iter()
                .all(|event| BOT_EVENTS.contains(event))
        );
        let mut current = agent_manifest(&app(false));
        let unchanged = current.clone();
        assert_eq!(add_bot_events(&mut current), Some(false));
        assert_eq!(current, unchanged);

        let mut older = unchanged.clone();
        older["settings"]["event_subscriptions"]["bot_events"] =
            json!(["message.im", "app_mention", "message.channels"]);
        older["display_information"]["description"] = json!("Edited by its owner.");
        let mut expected = older.clone();
        expected["settings"]["event_subscriptions"]["bot_events"] = json!([
            "message.im",
            "app_mention",
            "message.channels",
            "channel_id_changed",
        ]);
        assert_eq!(add_bot_events(&mut older), Some(true));
        assert_eq!(older, expected);

        for mut odd in [
            json!({}),
            json!({"settings": {}}),
            json!({"settings": {"event_subscriptions": {"request_url": "x"}}}),
            json!({"settings": {"event_subscriptions": {"bot_events": "message.im"}}}),
            json!({"settings": {"event_subscriptions": {"bot_events": [7]}}}),
            json!([]),
        ] {
            let before = odd.clone();
            assert_eq!(add_bot_events(&mut odd), None, "{before}");
            assert_eq!(odd, before);
        }
    }

    #[test]
    fn the_install_url_encodes_every_parameter() {
        let url = install_url(
            "123.456",
            &["chat:write", "im:history"],
            "https://agentd.example.com/slack/oauth/callback",
            "a.b+c/d",
        );
        let parsed = Url::parse(&url).unwrap();
        assert_eq!(parsed.as_str().split('?').next(), Some(OAUTH_AUTHORIZE_URL));
        let pairs: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
        assert_eq!(
            pairs,
            [
                ("state".to_owned(), "a.b+c/d".to_owned()),
                ("client_id".to_owned(), "123.456".to_owned()),
                ("scope".to_owned(), "chat:write,im:history".to_owned()),
                (
                    "redirect_uri".to_owned(),
                    "https://agentd.example.com/slack/oauth/callback".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_state_ending_in_punctuation_survives_rendering_the_link() {
        let redirect = "https://agentd.example.com/slack/oauth/callback";
        for state in ["abc.def_", "abc.def-", "abc.de_."] {
            let url = install_url("1.2", &["chat:write"], redirect, state);
            for markdown in [
                format!("Install it:\n{url}"),
                format!("[Install it]({url})"),
            ] {
                let rendered = render::slack::to_mrkdwn(&markdown, &NoNames);
                let linked = rendered
                    .split('<')
                    .nth(1)
                    .and_then(|rest| rest.split(['>', '|']).next())
                    .unwrap()
                    .replace("&amp;", "&");
                let parsed = Url::parse(&linked).unwrap();
                let pairs: Vec<(String, String)> = parsed.query_pairs().into_owned().collect();
                assert_eq!(
                    pairs[0],
                    ("state".to_owned(), state.to_owned()),
                    "{rendered}"
                );
                assert_eq!(pairs[3].1, redirect, "{rendered}");
            }
        }
    }

    struct NoNames;

    impl render::MentionDirectory for NoNames {
        fn resolve(&self, _name: &str) -> Option<String> {
            None
        }
    }
}

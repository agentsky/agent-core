//! `deploy/slack/manager-manifest.yaml`: it parses as YAML, and with the
//! public URL substituted it is exactly the manifest below.

use serde_json::{Value, json};

const TEMPLATE: &str = include_str!("../../../deploy/slack/manager-manifest.yaml");
const PLACEHOLDER: &str = "${PUBLIC_URL}";

fn parse(text: &str) -> Value {
    serde_norway::from_str(text).expect("the manifest is valid YAML")
}

fn substituted(public_url: &str) -> Value {
    parse(&TEMPLATE.replace(PLACEHOLDER, public_url))
}

#[test]
fn the_template_parses_as_yaml_and_has_one_kind_of_placeholder() {
    let template = parse(TEMPLATE);
    assert!(template.is_object());
    let body: String = TEMPLATE
        .lines()
        .filter(|line| !line.trim_start().starts_with('#'))
        .collect();
    assert_eq!(body.matches('$').count(), 3);
    assert_eq!(body.matches(PLACEHOLDER).count(), 3);
}

#[test]
fn the_substituted_manifest_matches_its_snapshot() {
    assert_eq!(
        substituted("https://agentd.example.com"),
        json!({
            "display_information": {
                "name": "agent-core",
                "description": "Link your Claude account and manage your Claude Code agents.",
                "long_description": "agent-core runs Claude Code agents that members mention in \
                    channels. This app is its manager: /agent links your Claude account, \
                    registers the Slack configuration token agent-core creates your agents' apps \
                    with, and manages your agents. Its replies are visible only to you. Send \
                    /agent help to see every command.",
            },
            "features": {
                "app_home": {
                    "home_tab_enabled": false,
                    "messages_tab_enabled": true,
                    "messages_tab_read_only_enabled": false,
                },
                "bot_user": {"display_name": "agent-core", "always_online": true},
                "slash_commands": [{
                    "command": "/agent",
                    "url": "https://agentd.example.com/slack/b/manager/commands",
                    "description": "Link your Claude account and manage your agents",
                    "usage_hint": "help | login | me | slack-token <token> <refresh token>",
                    "should_escape": true,
                }],
            },
            "oauth_config": {
                "scopes": {
                    "bot": [
                        "commands",
                        "chat:write",
                        "im:write",
                        "im:history",
                        "users:read",
                        "files:read",
                    ],
                },
            },
            "settings": {
                "event_subscriptions": {
                    "request_url": "https://agentd.example.com/slack/b/manager/events",
                    "bot_events": ["message.im", "user_change"],
                },
                "interactivity": {
                    "is_enabled": true,
                    "request_url": "https://agentd.example.com/slack/b/manager/interactivity",
                },
                "org_deploy_enabled": false,
                "socket_mode_enabled": false,
                "token_rotation_enabled": false,
            },
        })
    );
}

#[test]
fn every_request_url_is_one_the_manager_binding_serves() {
    let manifest = substituted("https://agentd.example.com");
    let urls = [
        &manifest["features"]["slash_commands"][0]["url"],
        &manifest["settings"]["event_subscriptions"]["request_url"],
        &manifest["settings"]["interactivity"]["request_url"],
    ];
    for (url, kind) in urls
        .into_iter()
        .zip(["commands", "events", "interactivity"])
    {
        let path = url
            .as_str()
            .unwrap()
            .strip_prefix("https://agentd.example.com")
            .unwrap();
        assert_eq!(
            path,
            format!(
                "/slack/b/{}/{kind}",
                surface_slack::BindingRef::MANAGER_SEGMENT
            )
        );
    }
}

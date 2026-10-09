//! [`testkit::fake_anthropic`] on a fixed address, with Claude's OAuth
//! endpoints added, for `scripts/ci/rocketchat-e2e.sh`:
//!
//! - `POST /v1/oauth/token` answers every code and refresh token with
//!   [`ACCESS_TOKEN`] and [`REFRESH_TOKEN`], granting
//!   `user:profile user:inference`.
//! - `POST /v1/oauth/token/revoke` answers 200.
//! - `GET /api/oauth/profile` names a Claude Max plan.
//!
//! Once listening it prints `listening on <base URL>`, then one line per
//! request: its method, its path, and what its `authorization` and
//! `x-api-key` headers held, each `issued` (the bearer [`ACCESS_TOKEN`]),
//! `community` (the `--community-key`), `other` or `absent`. It never prints
//! a header's value.

use std::net::{SocketAddr, TcpListener};
use std::time::Duration;

use clap::Parser;
use serde_json::json;
use testkit::FakeAnthropic;
use testkit::anthropic::Request;
use wiremock::matchers::{method, path};
use wiremock::{Mock, ResponseTemplate};

const ACCESS_TOKEN: &str = "sk-ant-oat01-fake-anthropic-access";
const REFRESH_TOKEN: &str = "sk-ant-ort01-fake-anthropic-refresh";
const POLL: Duration = Duration::from_millis(250);

#[derive(Debug, Parser)]
#[command(
    name = "fake-anthropic",
    about = "The Anthropic API and Claude OAuth, faked."
)]
struct Args {
    /// The address to listen on.
    #[arg(long)]
    listen: SocketAddr,
    /// The community API key, which request lines name `community`.
    #[arg(long)]
    community_key: Option<String>,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let args = Args::parse();
    let fake = FakeAnthropic::start_on(TcpListener::bind(args.listen)?).await;
    for mock in oauth() {
        fake.register(mock).await;
    }
    println!("listening on {}", fake.uri());
    let mut printed = 0;
    loop {
        let requests = fake.requests().await;
        for request in requests.iter().skip(printed) {
            println!("{}", line(request, args.community_key.as_deref()));
        }
        printed = printed.max(requests.len());
        tokio::time::sleep(POLL).await;
    }
}

fn oauth() -> [Mock; 3] {
    [
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token_type": "Bearer",
                "access_token": ACCESS_TOKEN,
                "refresh_token": REFRESH_TOKEN,
                "expires_in": 28800,
                "scope": "user:profile user:inference",
            }))),
        Mock::given(method("POST"))
            .and(path("/v1/oauth/token/revoke"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({}))),
        Mock::given(method("GET"))
            .and(path("/api/oauth/profile"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "organization": {
                    "organization_type": "claude_max",
                    "rate_limit_tier": "default_claude_max_20x",
                },
            }))),
    ]
}

fn line(request: &Request, community_key: Option<&str>) -> String {
    let header = |name: &str| {
        request
            .headers
            .get(name)
            .map(|value| value.to_str().unwrap_or_default())
    };
    let bearer = format!("Bearer {ACCESS_TOKEN}");
    let authorization = match header("authorization") {
        None => "absent",
        Some(value) if value == bearer => "issued",
        Some(_) => "other",
    };
    let api_key = match header("x-api-key") {
        None => "absent",
        Some(value) if Some(value) == community_key => "community",
        Some(_) => "other",
    };
    format!(
        "{} {} authorization={authorization} x-api-key={api_key}",
        request.method,
        request.url.path()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "sk-ant-api03-community";

    async fn served() -> FakeAnthropic {
        let fake = FakeAnthropic::start_on(TcpListener::bind("127.0.0.1:0").unwrap()).await;
        for mock in oauth() {
            fake.register(mock).await;
        }
        fake
    }

    async fn lines(fake: &FakeAnthropic) -> Vec<String> {
        fake.requests()
            .await
            .iter()
            .map(|request| line(request, Some(KEY)))
            .collect()
    }

    #[tokio::test]
    async fn oauth_links_with_the_scopes_agentd_accepts() {
        let fake = served().await;
        let client = reqwest::Client::new();
        let token: serde_json::Value = client
            .post(format!("{}/v1/oauth/token", fake.uri()))
            .json(&json!({"grant_type": "authorization_code", "code": "c"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(token["access_token"], ACCESS_TOKEN);
        assert_eq!(token["scope"], "user:profile user:inference");
        let profile: serde_json::Value = client
            .get(format!("{}/api/oauth/profile", fake.uri()))
            .bearer_auth(ACCESS_TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(profile["organization"]["organization_type"], "claude_max");
        let revoke = client
            .post(format!("{}/v1/oauth/token/revoke", fake.uri()))
            .send()
            .await
            .unwrap();
        assert_eq!(revoke.status(), 200);
        assert_eq!(
            lines(&fake).await,
            [
                "POST /v1/oauth/token authorization=absent x-api-key=absent",
                "GET /api/oauth/profile authorization=issued x-api-key=absent",
                "POST /v1/oauth/token/revoke authorization=absent x-api-key=absent",
            ]
        );
    }

    #[tokio::test]
    async fn lines_name_the_credential_without_its_value() {
        let fake = served().await;
        let client = reqwest::Client::new();
        let messages = format!("{}/v1/messages", fake.uri());
        let body = json!({"model": "m", "messages": []});
        for request in [
            client.post(&messages).bearer_auth(ACCESS_TOKEN),
            client.post(&messages).header("x-api-key", KEY),
            client
                .post(&messages)
                .header("x-api-key", "sk-ant-placeholder"),
            client
                .post(&messages)
                .bearer_auth("sk-ant-oat01-placeholder"),
        ] {
            assert_eq!(request.json(&body).send().await.unwrap().status(), 200);
        }
        let lines = lines(&fake).await;
        assert_eq!(
            lines,
            [
                "POST /v1/messages authorization=issued x-api-key=absent",
                "POST /v1/messages authorization=absent x-api-key=community",
                "POST /v1/messages authorization=absent x-api-key=other",
                "POST /v1/messages authorization=other x-api-key=absent",
            ]
        );
        for line in &lines {
            assert!(!line.contains("sk-ant"), "{line}");
        }
    }
}

//! `fake-claude` talks to `fake_anthropic()` through the proxy, as the real
//! CLI in a sandbox talks to Anthropic, with only a placeholder in its
//! environment.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use auth::{AuthError, TokenSource};
use core_types::{CredentialRef, MemberId, SessionId};
use cred_proxy::{CredProxy, FixedKey, Placeholder, Registry};
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::Value;
use testkit::claude::{API_KEY_BETA, OAUTH_BETA, SCRIPT_ENV};
use testkit::{FakeAnthropic, TempDir, Turn, fake_anthropic, fake_claude_path, write_script};
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
const OAUTH_TOKEN: &str = "sk-ant-oat01-real-member-token";
const COMMUNITY_KEY: &str = "sk-ant-api03-real-community-key";
const REPLY: &str = "Hello through the proxy.";
const WAIT: Duration = Duration::from_secs(60);

struct OneMember(MemberId);

#[async_trait]
impl TokenSource for OneMember {
    async fn access_token(&self, member: MemberId) -> Result<SecretString, AuthError> {
        if member == self.0 {
            Ok(SecretString::from(OAUTH_TOKEN))
        } else {
            Err(AuthError::NotLinked)
        }
    }
}

struct Stack {
    fake: FakeAnthropic,
    registry: Registry,
    member: MemberId,
    base_url: String,
}

impl Stack {
    async fn start() -> Self {
        let fake = fake_anthropic().await;
        let registry = Registry::new();
        let member = MemberId::new_v4();
        let router = CredProxy::new(
            &fake.uri(),
            registry.clone(),
            Arc::new(OneMember(member)),
            Arc::new(FixedKey::new(SecretString::from(COMMUNITY_KEY))),
        )
        .unwrap()
        .into_router();
        let listener = TcpListener::bind((LOCAL, 0)).await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        Self {
            fake,
            registry,
            member,
            base_url,
        }
    }

    /// Runs one `fake-claude` turn for `session` with `placeholder` as its
    /// only credential, and returns its `result` line.
    async fn turn(&self, session: SessionId, placeholder: &Placeholder) -> Value {
        let dir = TempDir::new("cred-proxy-e2e");
        for sub in ["claude", "work"] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
        }
        let persona = dir.join("persona.md");
        std::fs::write(&persona, "You are a test agent.\n").unwrap();
        let script = dir.join("script.json");
        write_script(&script, &[Turn::reply(REPLY)]).unwrap();
        let mut command = tokio::process::Command::new(fake_claude_path());
        command
            .args([
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
                "--tools",
                "Bash,Read,Edit,Write,Glob,Grep,Skill",
                "--strict-mcp-config",
                "--setting-sources",
                "user",
                "--permission-mode",
                "bypassPermissions",
                "--append-system-prompt-file",
            ])
            .arg(&persona)
            .arg("--session-id")
            .arg(session.to_string())
            .current_dir(dir.join("work"))
            .env_clear()
            .env("CLAUDE_CONFIG_DIR", dir.join("claude"))
            .env("CLAUDE_CODE_PROJECT_DIR_NAME", session.to_string())
            .env("ANTHROPIC_BASE_URL", &self.base_url)
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env(SCRIPT_ENV, &script)
            .env(placeholder.env_var(), placeholder.expose_secret())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
            command.env("LLVM_PROFILE_FILE", profile);
        }
        let mut child = command.spawn().unwrap();
        let line = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": "hi"},
        });
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        drop(stdin);
        let output = tokio::time::timeout(WAIT, child.wait_with_output())
            .await
            .expect("fake-claude timed out")
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|line| line["type"] == "result")
            .unwrap_or_else(|| {
                panic!(
                    "no result line:\n{stdout}\n{}",
                    String::from_utf8_lossy(&output.stderr)
                )
            })
    }
}

fn header<'a>(request: &'a testkit::anthropic::Request, name: &str) -> Option<&'a str> {
    request
        .headers
        .get(name)
        .map(|value| value.to_str().unwrap())
}

#[tokio::test]
async fn fake_claude_reaches_fake_anthropic_with_the_real_credentials() {
    fake_claude_path();
    let stack = Stack::start().await;
    let cases = [
        (
            CredentialRef::Member(stack.member),
            "authorization",
            format!("Bearer {OAUTH_TOKEN}"),
            OAUTH_BETA,
        ),
        (
            CredentialRef::Community,
            "x-api-key",
            COMMUNITY_KEY.to_owned(),
            API_KEY_BETA,
        ),
    ];
    for (index, (credential, name, expected, beta)) in cases.into_iter().enumerate() {
        let session = SessionId::new_v4();
        let placeholder = stack
            .registry
            .mint(session, LOCAL, credential.kind())
            .unwrap();
        stack.registry.point(placeholder.id(), credential).unwrap();
        let result = stack.turn(session, &placeholder).await;
        assert_eq!(result["is_error"], false, "{result}");
        assert_eq!(result["result"], REPLY);

        let requests = stack.fake.message_requests().await;
        assert_eq!(requests.len(), index + 1);
        let sent = &requests[index];
        assert_eq!(header(sent, name), Some(expected.as_str()));
        let other = if name == "x-api-key" {
            "authorization"
        } else {
            "x-api-key"
        };
        assert_eq!(header(sent, other), None);
        assert_eq!(header(sent, "anthropic-beta"), Some(beta));
        assert_eq!(header(sent, "anthropic-version"), Some("2023-06-01"));
        assert_eq!(header(sent, "x-app"), Some("cli"));
        assert_eq!(
            header(sent, "x-claude-code-session-id"),
            Some(session.to_string().as_str())
        );
        assert_eq!(sent.url.path(), "/v1/messages");
        assert_eq!(sent.url.query(), Some("beta=true"));
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body["stream"], true);
        let text = placeholder.expose_secret();
        for (header_name, value) in &sent.headers {
            assert!(
                !value.to_str().unwrap_or_default().contains(text),
                "the placeholder reached the upstream in {header_name}"
            );
        }
        stack.registry.revoke_session(session);
    }
}

#[tokio::test]
async fn a_revoked_placeholder_fails_the_turn_with_an_auth_error() {
    fake_claude_path();
    let stack = Stack::start().await;
    let session = SessionId::new_v4();
    let placeholder = stack
        .registry
        .mint(session, LOCAL, CredentialRef::Community.kind())
        .unwrap();
    let revoked = stack
        .registry
        .mint(session, LOCAL, CredentialRef::Community.kind())
        .unwrap();
    stack
        .registry
        .point(placeholder.id(), CredentialRef::Community)
        .unwrap();
    assert!(stack.registry.revoke(revoked.id()));
    let result = stack.turn(session, &revoked).await;
    assert_eq!(result["is_error"], true, "{result}");
    assert_eq!(result["api_error_status"], 401);
    assert!(stack.fake.requests().await.is_empty());
}

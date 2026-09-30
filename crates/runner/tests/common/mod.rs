use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use core_types::{AgentId, CredentialKind, ScopeKey, SessionId, VolumeKey};
use runner::{LaunchSpec, ProcessConfig, SessionStart};
use sandbox::{Container, ProcessSandbox, Sandbox, SessionSpec};
use secrecy::SecretString;
use store::{Sealer, Store};
use testkit::{FakeAnthropic, Turn};

pub const PLACEHOLDER: &str = "agentd-placeholder-7f3a";

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("runner-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub struct Harness {
    pub _dir: TempDir,
    pub sandbox: ProcessSandbox,
    pub anthropic: FakeAnthropic,
    pub container: Container,
    pub script: PathBuf,
    pub config: ProcessConfig,
}

impl Harness {
    pub async fn new(turns: &[Turn]) -> Self {
        let bin = testkit::fake_claude_path();
        let dir = TempDir::new();
        let sealer = Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap();
        let store = Store::open_in_memory(sealer).await.unwrap();
        let sandbox = ProcessSandbox::new(store, dir.0.clone()).unwrap();
        let agent = AgentId::new_v4();
        assert!(
            runner::write_persona(&dir.0, agent, "You are a test agent.\n")
                .await
                .unwrap()
        );
        let volume = sandbox
            .ensure_volume(&VolumeKey {
                agent,
                scope: ScopeKey::Private,
            })
            .await
            .unwrap();
        let spec = SessionSpec::new(
            SessionId::new_v4(),
            volume,
            "unused",
            runner::persona_dir(&dir.0, agent),
        );
        let container = sandbox.start(&spec).await.unwrap();
        let script = dir.0.join("script.json");
        testkit::write_script(&script, turns).unwrap();
        let anthropic = testkit::fake_anthropic().await;
        let config = ProcessConfig {
            claude_bin: bin.to_str().unwrap().to_owned(),
            anthropic_base_url: anthropic.uri(),
            turn_timeout_secs: 60,
        };
        Self {
            _dir: dir,
            sandbox,
            anthropic,
            container,
            script,
            config,
        }
    }

    pub fn launch(&self, start: SessionStart) -> LaunchSpec {
        self.launch_with(start, CredentialKind::Subscription)
    }

    pub fn launch_with(&self, start: SessionStart, credential: CredentialKind) -> LaunchSpec {
        LaunchSpec {
            start,
            model: None,
            credential,
            placeholder: SecretString::from(PLACEHOLDER),
            env: BTreeMap::from([
                (
                    testkit::claude::SCRIPT_ENV.to_string(),
                    self.script.to_str().unwrap().to_owned(),
                ),
                (
                    "AGENTCTL_TOKEN".to_string(),
                    "ctl-token-for-test".to_string(),
                ),
            ]),
        }
    }

    pub async fn start(&self, spec: LaunchSpec) -> runner::ClaudeProcess {
        runner::ClaudeProcess::start(&self.sandbox, &self.container, &self.config, spec)
            .await
            .unwrap()
    }

    pub fn transcript(&self) -> PathBuf {
        let session = self.container.session().to_string();
        self.container
            .paths()
            .claude_config
            .join("projects")
            .join(&session)
            .join(format!("{session}.jsonl"))
    }

    pub fn transcript_user_messages(&self) -> Vec<String> {
        read_user_messages(&self.transcript())
    }
}

fn read_user_messages(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|entry| entry["type"] == "user")
        .map(|entry| {
            entry["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

//! The `agentd` binary end to end: `serve` until SIGTERM, `migrate` and
//! `gen-key`.

mod common;

use std::io::{BufRead, BufReader};
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

use common::{CONFIG, TempDir, master_key};

fn agentd() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_agentd"));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("AGENTD_") {
            command.env_remove(name);
        }
    }
    command
}

fn write_config(dir: &Path, text: &str) -> std::path::PathBuf {
    let path = dir.join("agentd.toml");
    std::fs::write(&path, text).unwrap();
    path
}

fn wait(child: &mut Child, limit: Duration) -> ExitStatus {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("agentd did not exit within {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[test]
fn serve_answers_healthz_and_exits_cleanly_on_sigterm() {
    let dir = TempDir::new();
    let db = dir.path().join("agentd.db");
    let text = CONFIG.replace("sqlite::memory:", &format!("sqlite://{}", db.display()));
    let config = write_config(dir.path(), &text);
    let mut child = agentd()
        .args(["serve", "--config"])
        .arg(&config)
        .env("AGENTD_MASTER_KEY", master_key())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let (lines_tx, lines) = mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if lines_tx.send(line).is_err() {
                break;
            }
        }
    });
    let next_log = |message: &str| -> Value {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no {message:?} log line"));
            let log: Value =
                serde_json::from_str(&line).unwrap_or_else(|_| panic!("not JSON: {line}"));
            if log["fields"]["message"] == message {
                return log;
            }
        }
    };

    let listening = next_log("listening");
    let public: SocketAddr = listening["fields"]["public"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let health = common::get(public, "/healthz").unwrap();
    assert_eq!((health.status, health.body.as_str()), (200, "ok\n"));
    assert!(db.exists());

    let status = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    next_log("received a shutdown signal");
    next_log("stopped");
    let status = wait(&mut child, Duration::from_secs(20));
    assert!(status.success(), "{status}");
    assert!(common::get(public, "/healthz").is_none());
}

#[test]
fn serve_refuses_a_bad_config_and_names_the_key() {
    let dir = TempDir::new();
    let config = write_config(
        dir.path(),
        &CONFIG.replacen("127.0.0.1:0", "0.0.0.0:8443", 1),
    );
    let output = agentd()
        .args(["serve", "--config"])
        .arg(&config)
        .env("AGENTD_MASTER_KEY", master_key())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.starts_with("agentd: server.listen: "), "{stderr}");
}

#[test]
fn migrate_creates_and_migrates_the_database() {
    let dir = TempDir::new();
    let db = dir.path().join("agentd.db");
    let text = CONFIG.replace("sqlite::memory:", &format!("sqlite://{}", db.display()));
    let config = write_config(dir.path(), &text);
    let key = master_key();
    for _ in 0..2 {
        let output = agentd()
            .args(["migrate", "--config"])
            .arg(&config)
            .env("AGENTD_MASTER_KEY", &key)
            .env("AGENTD_PORT", "tcp://10.0.0.11:8443")
            .env("AGENTD_SERVICE_HOST", "10.0.0.11")
            .env("AGENTD_UNUSED_SETTING", "value")
            .output()
            .unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{stderr}");
        assert!(stderr.contains("the store is migrated"), "{stderr}");
        let warnings: Vec<&str> = stderr
            .lines()
            .filter(|line| line.contains("ignoring an unknown AGENTD_ environment variable"))
            .collect();
        assert_eq!(warnings.len(), 1, "{stderr}");
        assert!(warnings[0].contains("AGENTD_UNUSED_SETTING"), "{stderr}");
        assert!(!warnings[0].contains("value"), "{stderr}");
    }
    assert!(db.exists());
}

#[test]
fn migrate_needs_the_master_key() {
    let dir = TempDir::new();
    let config = write_config(dir.path(), CONFIG);
    let output = agentd()
        .args(["migrate", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("AGENTD_MASTER_KEY: is not set"), "{stderr}");
}

#[test]
fn gen_key_prints_a_key_the_config_accepts() {
    let output = agentd().arg("gen-key").output().unwrap();
    assert!(output.status.success());
    let key = String::from_utf8(output.stdout).unwrap();
    agentd::Config::parse(CONFIG, [("AGENTD_MASTER_KEY", key.trim())]).unwrap();
}

use std::collections::HashMap;

use bollard::models::EventActor;
use core_types::{AgentId, ScopeKey};

use super::*;
use crate::test_util::{awkward_channel, volume};

const DATA: &str = "/var/lib/agentd";

fn spec_for(scope: ScopeKey) -> SessionSpec {
    SessionSpec::new(
        SessionId::new_v4(),
        volume(Path::new(DATA), AgentId::new_v4(), scope),
        "agent-sandbox:2.1.285",
        format!("{DATA}/agents/a1"),
    )
}

fn build(config: &SandboxConfig, spec: &SessionSpec) -> ContainerCreateBody {
    container_config(config, Path::new(DATA), spec).unwrap()
}

fn host(body: &ContainerCreateBody) -> &HostConfig {
    body.host_config.as_ref().unwrap()
}

/// `(source, target, read_only)` for every mount, in order.
fn mounts(body: &ContainerCreateBody) -> Vec<(String, String, bool)> {
    host(body)
        .mounts
        .as_ref()
        .unwrap()
        .iter()
        .map(|mount| {
            assert_eq!(mount.typ, Some(MountType::BIND));
            (
                mount.source.clone().unwrap(),
                mount.target.clone().unwrap(),
                mount.read_only.unwrap(),
            )
        })
        .collect()
}

fn session_target(spec: &SessionSpec) -> String {
    format!("/volume/sessions/{}", spec.session)
}

#[test]
fn a_channel_session_mounts_its_directory_shared_and_the_persona() {
    let config = SandboxConfig::new("unused");
    let spec = spec_for(awkward_channel());
    let body = build(&config, &spec);
    let volume = spec.volume.path().to_str().unwrap().to_string();
    assert_eq!(
        mounts(&body),
        [
            (
                format!("{volume}/sessions/{}", spec.session),
                session_target(&spec),
                false
            ),
            (
                format!("{volume}/shared"),
                "/volume/shared".to_string(),
                true
            ),
            (format!("{DATA}/agents/a1"), "/agent".to_string(), true),
        ]
    );
}

#[test]
fn a_scope_key_with_colon_and_percent_yields_mounts_and_no_binds() {
    let config = SandboxConfig::new("unused");
    let spec = spec_for(awkward_channel());
    let scope = spec.volume.key().scope.to_string();
    assert!(scope.contains(':') && scope.contains('%'), "{scope}");
    let body = build(&config, &spec);
    let expected = format!(
        "{DATA}/volumes/{}/{}",
        spec.volume.key().agent,
        crate::scope_dir_name(&spec.volume.key().scope)
    );
    for (source, _, _) in mounts(&body).iter().take(2) {
        assert!(source.starts_with(&expected), "{source}");
        assert!(!source.contains(':') && !source.contains('%'), "{source}");
    }
    assert_eq!(host(&body).binds, None);
    assert_eq!(body.volumes, None);
    let json = serde_json::to_value(&body).unwrap();
    assert!(json["HostConfig"].get("Binds").is_none(), "{json}");
    assert_eq!(json["HostConfig"]["Mounts"][0]["Type"], "bind");
    assert_eq!(json["HostConfig"]["Mounts"][0]["ReadOnly"], false);
    assert_eq!(json["HostConfig"]["Mounts"][1]["ReadOnly"], true);
    assert_eq!(body.labels.as_ref().unwrap()[LABEL_SCOPE], scope);
}

#[test]
fn shared_is_read_write_only_when_the_spec_says_so() {
    let config = SandboxConfig::new("unused");
    let mut spec = spec_for(ScopeKey::Private);
    spec.shared = SharedAccess::ReadWrite;
    let body = build(&config, &spec);
    assert_eq!(
        mounts(&body)[1],
        (
            format!("{}/shared", spec.volume.path().display()),
            "/volume/shared".to_string(),
            false
        )
    );
}

#[test]
fn memory_is_mounted_only_when_asked_for() {
    let config = SandboxConfig::new("unused");
    let mut spec = spec_for(ScopeKey::Private);
    let targets = |body: &ContainerCreateBody| -> Vec<String> {
        mounts(body)
            .into_iter()
            .map(|(_, target, _)| target)
            .collect()
    };
    assert!(!targets(&build(&config, &spec)).contains(&"/volume/memory".to_string()));
    spec.memory = true;
    let body = build(&config, &spec);
    assert_eq!(
        mounts(&body)[2],
        (
            format!("{}/memory", spec.volume.path().display()),
            "/volume/memory".to_string(),
            false
        )
    );
    assert_eq!(
        container_paths(&spec).memory,
        Some(PathBuf::from("/volume/memory"))
    );
    let mut channel = spec_for(awkward_channel());
    channel.memory = true;
    assert!(matches!(
        container_config(&config, Path::new(DATA), &channel),
        Err(SandboxError::InvalidSpec(_))
    ));
}

#[test]
fn skills_are_mounted_read_only_in_the_claude_config_dir() {
    let config = SandboxConfig::new("unused");
    let mut spec = spec_for(awkward_channel());
    spec.skills_dir = Some(format!("{DATA}/skills/a1").into());
    let body = build(&config, &spec);
    let all = mounts(&body);
    assert_eq!(
        all[2],
        (
            format!("{DATA}/skills/a1"),
            format!("{}/claude/skills", session_target(&spec)),
            true
        )
    );
    assert_eq!(all[3].1, "/agent");
    assert!(all[3].2);
}

#[test]
fn the_user_environment_and_working_directory() {
    let mut config = SandboxConfig::new("unused");
    let mut spec = spec_for(ScopeKey::Private);
    spec.env
        .insert("ANTHROPIC_BASE_URL".into(), "http://proxy".into());
    let body = build(&config, &spec);
    assert_eq!(body.user.as_deref(), Some("10001:10001"));
    assert_eq!(body.image.as_deref(), Some("agent-sandbox:2.1.285"));
    let session = session_target(&spec);
    assert_eq!(
        body.env.as_deref().unwrap(),
        [
            format!("HOME={session}/home"),
            format!("TMPDIR={session}/tmp"),
            "ANTHROPIC_BASE_URL=http://proxy".to_string(),
        ]
    );
    assert_eq!(body.working_dir, Some(format!("{session}/work")));
    assert_eq!(
        body.cmd.as_deref().unwrap(),
        ["sleep".to_string(), "infinity".to_string()]
    );
    assert_eq!(body.tty, Some(false));
    assert_eq!(body.open_stdin, Some(false));

    config.uid = 1001;
    config.gid = 121;
    assert_eq!(build(&config, &spec).user.as_deref(), Some("1001:121"));
}

#[test]
fn network_limits_capabilities_and_root_filesystem() {
    let mut config = SandboxConfig::new("unused");
    config.network = "sandbox-net".into();
    config.memory_mb = 2048;
    config.cpus = 1.5;
    config.pids_limit = 256;
    config.tmp_size_mb = 64;
    config.stop_timeout_secs = 7;
    let body = build(&config, &spec_for(awkward_channel()));
    let host = host(&body);
    assert_eq!(host.network_mode.as_deref(), Some("sandbox-net"));
    assert_eq!(host.readonly_rootfs, Some(true));
    assert_eq!(host.privileged, Some(false));
    assert_eq!(
        host.security_opt.as_deref().unwrap(),
        ["no-new-privileges".to_string()]
    );
    assert_eq!(host.cap_drop.as_deref().unwrap(), ["ALL".to_string()]);
    assert_eq!(host.cap_add, None);
    assert_eq!(host.memory, Some(2048 * 1024 * 1024));
    assert_eq!(host.memory_swap, Some(2048 * 1024 * 1024));
    assert_eq!(host.nano_cpus, Some(1_500_000_000));
    assert_eq!(host.pids_limit, Some(256));
    assert_eq!(host.init, Some(true));
    assert_eq!(host.port_bindings, None);
    assert_eq!(
        host.tmpfs.as_ref().unwrap(),
        &HashMap::from([(
            "/tmp".to_string(),
            "rw,exec,nosuid,nodev,size=64m,mode=1777".to_string()
        )])
    );
    assert_eq!(body.stop_timeout, Some(7));
}

#[test]
fn labels_name_the_session_agent_scope_and_instance() {
    let mut config = SandboxConfig::new("unused");
    config.instance = "blue".into();
    let mut spec = spec_for(awkward_channel());
    spec.labels.insert("team".into(), "x".into());
    let body = build(&config, &spec);
    let key = spec.volume.key();
    assert_eq!(
        body.labels.unwrap(),
        HashMap::from([
            ("team".to_string(), "x".to_string()),
            (LABEL_SESSION.to_string(), spec.session.to_string()),
            (LABEL_AGENT.to_string(), key.agent.to_string()),
            (LABEL_SCOPE.to_string(), key.scope.to_string()),
            (LABEL_INSTANCE.to_string(), "blue".to_string()),
        ])
    );
    spec.labels.insert(LABEL_SESSION.into(), "forged".into());
    assert!(container_config(&config, Path::new(DATA), &spec).is_err());
}

#[test]
fn host_data_dir_rewrites_every_source() {
    let mut config = SandboxConfig::new("unused");
    config.host_data_dir = Some("/srv/agentd".into());
    let mut spec = spec_for(ScopeKey::Private);
    spec.skills_dir = Some(format!("{DATA}/skills/a1").into());
    spec.memory = true;
    let body = build(&config, &spec);
    for (source, _, _) in mounts(&body) {
        assert!(source.starts_with("/srv/agentd/"), "{source}");
    }
    spec.persona_dir = "/elsewhere/agents/a1".into();
    assert!(matches!(
        container_config(&config, Path::new(DATA), &spec),
        Err(SandboxError::InvalidSpec(_))
    ));
    config.host_data_dir = None;
    let body = build(&config, &spec);
    assert_eq!(mounts(&body).last().unwrap().0, "/elsewhere/agents/a1");
}

#[test]
fn bad_specs_are_refused() {
    let config = SandboxConfig::new("unused");
    let mut spec = spec_for(ScopeKey::Private);
    spec.image = " ".into();
    assert!(container_config(&config, Path::new(DATA), &spec).is_err());
    let mut spec = spec_for(ScopeKey::Private);
    spec.env.insert("HOME".into(), "/root".into());
    assert!(container_config(&config, Path::new(DATA), &spec).is_err());
    let mut spec = spec_for(ScopeKey::Private);
    spec.persona_dir = format!("{DATA}/agents/../../etc").into();
    assert!(container_config(&config, Path::new(DATA), &spec).is_err());
}

#[test]
fn container_paths_are_under_volume_and_agent() {
    let spec = spec_for(ScopeKey::Private);
    let session = PathBuf::from(session_target(&spec));
    assert_eq!(
        container_paths(&spec),
        SessionPaths {
            work: session.join("work"),
            claude_config: session.join("claude"),
            home: session.join("home"),
            tmp: session.join("tmp"),
            persona_file: PathBuf::from("/agent/persona.md"),
            shared: PathBuf::from("/volume/shared"),
            memory: None,
        }
    );
}

#[test]
fn managed_filters_select_this_instance() {
    let mut config = SandboxConfig::new("unused");
    config.instance = "green".into();
    assert_eq!(
        managed_filters(&config),
        HashMap::from([(
            "label".to_string(),
            vec![
                "agentd.session".to_string(),
                "agentd.instance=green".to_string()
            ]
        )])
    );
}

fn event(action: &str, instance: &str, session: &str) -> EventMessage {
    EventMessage {
        action: Some(action.into()),
        actor: Some(EventActor {
            id: Some("c1".into()),
            attributes: Some(HashMap::from([
                (LABEL_INSTANCE.to_string(), instance.to_string()),
                (LABEL_SESSION.to_string(), session.to_string()),
            ])),
        }),
        ..Default::default()
    }
}

#[test]
fn die_events_of_this_instance_become_container_events() {
    let config = SandboxConfig::new("unused");
    let session = SessionId::new_v4();
    assert_eq!(
        died_event(&config, event("die", "agentd", &session.to_string())),
        Some(ContainerEvent::Died {
            container: ContainerId("c1".into()),
            session: Some(session),
        })
    );
    assert_eq!(
        died_event(&config, event("die", "agentd", "not-a-uuid")),
        Some(ContainerEvent::Died {
            container: ContainerId("c1".into()),
            session: None,
        })
    );
    assert_eq!(
        died_event(&config, event("start", "agentd", &session.to_string())),
        None
    );
    assert_eq!(
        died_event(&config, event("die", "other", &session.to_string())),
        None
    );
    assert_eq!(died_event(&config, EventMessage::default()), None);
}

#[test]
fn the_pid_line_is_split_off_stdout() {
    let mut splitter = PidSplitter::default();
    let (pid, rest) = splitter.feed(b"12");
    assert_eq!((pid, rest.as_ref()), (None, &b""[..]));
    let (pid, rest) = splitter.feed(b"34\n{\"type\"");
    assert_eq!((pid, rest.as_ref()), (Some(1234), &b"{\"type\""[..]));
    let (pid, rest) = splitter.feed(b"5\n");
    assert_eq!((pid, rest.as_ref()), (None, &b"5\n"[..]));

    let mut splitter = PidSplitter::default();
    let (pid, rest) = splitter.feed(b"not a pid\nmore");
    assert_eq!((pid, rest.as_ref()), (None, &b"not a pid\nmore"[..]));

    let mut splitter = PidSplitter::default();
    let long = [b'x'; 30];
    let (pid, rest) = splitter.feed(&long);
    assert_eq!((pid, rest.as_ref()), (None, &long[..]));
}

#[test]
fn docker_errors_drop_the_message_for_exec() {
    let server = || bollard::errors::Error::DockerResponseServerError {
        status_code: 500,
        message: "env A=secret".into(),
    };
    let kept = docker_err("create container", true)(server());
    assert_eq!(
        kept.to_string(),
        "docker create container failed (HTTP 500): env A=secret"
    );
    let dropped = docker_err("create exec", false)(server());
    assert_eq!(dropped.to_string(), "docker create exec failed (HTTP 500)");
    let timeout = docker_err("stop", true)(bollard::errors::Error::RequestTimeoutError);
    assert_eq!(timeout.to_string(), "docker stop failed: timed out");
    let json = docker_err("list", true)(bollard::errors::Error::JsonDataError {
        message: "near secret".into(),
        column: 1,
    });
    assert_eq!(json.to_string(), "docker list failed: unexpected response");
    assert_eq!(status_of(&server()), Some(500));
    assert_eq!(
        status_of(&bollard::errors::Error::RequestTimeoutError),
        None
    );
}

#[tokio::test]
async fn a_docker_sandbox_validates_its_inputs() {
    let docker =
        Docker::connect_with_http("http://127.0.0.1:1", 1, bollard::API_DEFAULT_VERSION).unwrap();
    let store = crate::test_util::memory_store().await;
    let mut config = SandboxConfig::new("img");
    config.uid = 0;
    assert!(matches!(
        DockerSandbox::new(docker.clone(), store.clone(), DATA, config),
        Err(SandboxError::Config(_))
    ));
    assert!(matches!(
        DockerSandbox::new(docker, store, "relative", SandboxConfig::new("img")),
        Err(SandboxError::InvalidSpec(_))
    ));
}

#[test]
fn events_start_from_the_time_they_are_asked_for() {
    let at = std::time::UNIX_EPOCH + Duration::new(1_790_000_000, 5);
    assert_eq!(since(at), "1790000000.000000005");
}

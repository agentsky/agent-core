use std::process::Command;

use core_types::{ConvRef, ScopeKey, SurfaceKind, TeamId, ThreadKey};
use cred_proxy::EgressPolicy;
use store::{AgentCreation, NewAgent, Sealer, Visibility};

use super::*;

const PREFIX: &str = "https://git.test/";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("agentd-skills-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn skill_md(name: &str, hosts: &[&str]) -> String {
    let mut text = format!("---\nname: {name}\ndescription: The {name} skill.\n");
    if !hosts.is_empty() {
        text.push_str("allowed-hosts:\n");
        for host in hosts {
            text.push_str(&format!("  - '{host}'\n"));
        }
    }
    text.push_str("---\nUse it well.\n");
    text
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A Git repository `repos/<repo>` holding `files`, committed on `main`.
fn repo(repos: &Path, repo: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = repos.join(repo);
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "--quiet"]);
    for (path, text) in files {
        let file = dir.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }
    git(&dir, &["add", "--all"]);
    git(&dir, &["commit", "--quiet", "-m", "skill"]);
    dir
}

struct Harness {
    store: Store,
    skills: Skills,
    agent: AgentId,
    owner: MemberId,
    data: PathBuf,
    repos: PathBuf,
    _dir: TempDir,
}

async fn harness() -> Harness {
    let dir = TempDir::new();
    let data = dir.0.join("data");
    let repos = dir.0.join("repos");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&repos).unwrap();
    let store =
        Store::open_in_memory(Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap())
            .await
            .unwrap();
    let now = OffsetDateTime::now_utc();
    let owner = store
        .ensure_member(
            &core_types::MemberKey {
                surface: SurfaceKind::RocketChat,
                team: TeamId::new("T1"),
                user: "alice".into(),
            },
            "alice",
            now,
        )
        .await
        .unwrap();
    let team = TeamId::new("T1");
    let AgentCreation::Created(agent, _) = store
        .create_agent(
            &NewAgent {
                owner,
                name: "helper",
                persona: "p",
                visibility: Visibility::Public,
                surface: SurfaceKind::RocketChat,
                team: &team,
            },
            10,
            now,
        )
        .await
        .unwrap()
    else {
        panic!("created");
    };
    let git = Git::new(EgressPolicy::new(Vec::new(), Vec::new()))
        .serving_prefix_from_directory_for_tests(PREFIX, &repos);
    let skills = Skills::new(store.clone(), data.clone(), git);
    Harness {
        store,
        skills,
        agent: agent.id,
        owner,
        data,
        repos,
        _dir: dir,
    }
}

impl Harness {
    fn live(&self, name: &str) -> PathBuf {
        runner::skills_dir(&self.data, self.agent).join(name)
    }

    fn pending(&self, name: &str) -> PathBuf {
        self.data
            .join(PENDING_DIR)
            .join(self.agent.to_string())
            .join(name)
    }

    async fn add(&self, source: Source<'_>) -> Result<Added, Refused> {
        self.skills
            .add(self.agent, source, self.owner)
            .await
            .unwrap()
    }

    async fn upload(&self, name: &str, text: &str) -> Result<Added, Refused> {
        self.add(Source::Upload {
            name,
            bytes: text.as_bytes(),
        })
        .await
    }

    async fn hosts(&self) -> Vec<String> {
        let thread = ThreadKey {
            conv: ConvRef {
                surface: SurfaceKind::RocketChat,
                team: TeamId::new("T1"),
                conversation: "C1".into(),
            },
            root: None,
        };
        let session = self
            .store
            .session_for_thread(
                self.agent,
                &thread,
                &ScopeKey::Private,
                OffsetDateTime::now_utc(),
            )
            .await
            .unwrap()
            .session
            .id;
        SkillHosts(self.store.clone())
            .rules(session)
            .await
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn work_is_empty(&self) -> bool {
        std::fs::read_dir(self.data.join(WORK_DIR))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true)
    }
}

#[tokio::test]
async fn a_skill_is_added_from_a_local_git_fixture_repo() {
    let h = harness().await;
    repo(
        &h.repos,
        "pdf.git",
        &[
            ("SKILL.md", &skill_md("pdf-tools", &[])),
            ("scripts/fill.sh", "#!/bin/sh\necho filled\n"),
        ],
    );
    let added = h
        .add(Source::Git("https://git.test/pdf.git"))
        .await
        .unwrap();
    let Added::Active(manifest) = added else {
        panic!("{added:?}");
    };
    assert_eq!(manifest.name.as_str(), "pdf-tools");
    let live = h.live("pdf-tools");
    assert_eq!(
        std::fs::read_to_string(live.join("SKILL.md")).unwrap(),
        skill_md("pdf-tools", &[])
    );
    assert!(live.join("scripts/fill.sh").is_file());
    assert!(!live.join(".git").exists(), "the clone's .git is gone");
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].source, "https://git.test/pdf.git");
    assert_eq!(rows[0].state, SkillState::Active);
    assert!(h.work_is_empty());
}

#[tokio::test]
async fn a_ref_picks_the_branch_and_a_missing_repo_or_ref_fails() {
    let h = harness().await;
    let dir = repo(&h.repos, "r.git", &[("SKILL.md", &skill_md("one", &[]))]);
    git(&dir, &["checkout", "--quiet", "-b", "v2"]);
    std::fs::write(dir.join("SKILL.md"), skill_md("two", &[])).unwrap();
    git(&dir, &["commit", "--quiet", "-am", "v2"]);
    git(&dir, &["checkout", "--quiet", "main"]);

    let Added::Active(manifest) = h
        .add(Source::Git("https://git.test/r.git#v2"))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(manifest.name.as_str(), "two");
    for source in [
        "https://git.test/r.git#nope",
        "https://git.test/missing.git",
    ] {
        assert_eq!(
            h.add(Source::Git(source)).await,
            Err(Refused::Clone(CloneError::Failed)),
            "{source}"
        );
    }
    assert!(h.work_is_empty());
}

#[tokio::test]
async fn a_symlink_in_a_repo_is_checked_out_as_a_plain_file() {
    let h = harness().await;
    let dir = repo(&h.repos, "l.git", &[("SKILL.md", &skill_md("links", &[]))]);
    std::os::unix::fs::symlink("/etc/passwd", dir.join("passwd")).unwrap();
    git(&dir, &["add", "passwd"]);
    git(&dir, &["commit", "--quiet", "-m", "link"]);
    h.add(Source::Git("https://git.test/l.git")).await.unwrap();
    let passwd = h.live("links").join("passwd");
    let meta = std::fs::symlink_metadata(&passwd).unwrap();
    assert!(meta.is_file(), "not a symlink");
    assert_eq!(std::fs::read_to_string(passwd).unwrap(), "/etc/passwd");
}

#[tokio::test]
async fn skills_are_added_from_an_uploaded_file_or_zip() {
    let h = harness().await;
    let Added::Active(manifest) = h.upload("SKILL.md", &skill_md("notes", &[])).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(manifest.name.as_str(), "notes");
    assert!(h.live("notes").join("SKILL.md").is_file());
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    assert_eq!(rows[0].source, "upload:SKILL.md");

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file("sheets/SKILL.md", zip::write::SimpleFileOptions::default())
        .unwrap();
    std::io::Write::write_all(&mut zip, skill_md("sheets", &[]).as_bytes()).unwrap();
    let bytes = zip.finish().unwrap().into_inner();
    let Added::Active(manifest) = h
        .add(Source::Upload {
            name: "Sheets.ZIP",
            bytes: &bytes,
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(manifest.name.as_str(), "sheets");
    assert!(h.live("sheets").join("SKILL.md").is_file());

    assert_eq!(h.upload("skill.txt", "x").await, Err(Refused::FileKind));
    assert_eq!(
        h.upload("SKILL.md", "no front matter").await,
        Err(Refused::Problem(Problem::NoFrontMatter))
    );
    assert_eq!(
        h.upload("x.zip", "not a zip").await,
        Err(Refused::Problem(Problem::Archive))
    );
    assert!(h.work_is_empty());
}

#[tokio::test]
async fn adding_a_skill_again_replaces_it() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("notes", &[])).await.unwrap();
    std::fs::write(h.live("notes").join("old.txt"), "old").unwrap();
    let newer = skill_md("notes", &[]).replace("Use it well.", "Use it better.");
    h.upload("notes.md", &newer).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(h.live("notes").join("SKILL.md")).unwrap(),
        newer
    );
    assert!(!h.live("notes").join("old.txt").exists());
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap().len(), 1);
}

#[tokio::test]
async fn declared_hosts_wait_for_confirmation_then_extend_the_allowlist() {
    let h = harness().await;
    let text = skill_md("gh", &["api.github.com", "*.githubusercontent.com"]);
    let Added::Pending(manifest) = h.upload("SKILL.md", &text).await.unwrap() else {
        panic!()
    };
    assert_eq!(manifest.hosts.len(), 2);
    assert!(!h.live("gh").exists(), "not mounted while it waits");
    assert!(h.pending("gh").join("SKILL.md").is_file());
    assert!(h.hosts().await.is_empty(), "no hosts while it waits");

    assert_eq!(
        h.skills.confirm(h.agent, "other").await.unwrap(),
        Confirmed::NotPending
    );
    let Confirmed::Active(row) = h.skills.confirm(h.agent, "gh").await.unwrap() else {
        panic!()
    };
    assert_eq!(row.hosts, ["api.github.com", "*.githubusercontent.com"]);
    assert!(h.live("gh").join("SKILL.md").is_file());
    assert!(!h.pending("gh").exists());
    assert_eq!(
        h.hosts().await,
        ["*.githubusercontent.com", "api.github.com"]
    );
    assert_eq!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::NotPending,
        "confirmed once"
    );

    assert!(h.skills.remove(h.agent, "gh").await.unwrap());
    assert!(!h.live("gh").exists());
    assert!(
        h.hosts().await.is_empty(),
        "removing it takes its hosts away"
    );
    assert!(!h.skills.remove(h.agent, "gh").await.unwrap());
}

#[tokio::test]
async fn a_confirmation_after_the_wait_finds_it_expired() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let hosts = vec!["api.github.com".to_owned()];
    let old = OffsetDateTime::now_utc() - PENDING_TTL - time::Duration::minutes(1);
    h.store
        .put_skill(
            &NewSkill {
                agent: h.agent,
                name: "gh",
                source: "upload:SKILL.md",
                hosts: &hosts,
                added_by: h.owner,
            },
            SkillState::Pending,
            old,
        )
        .await
        .unwrap();
    assert_eq!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::Expired
    );
    assert!(!h.pending("gh").exists());
    assert!(h.store.agent_skills(h.agent).await.unwrap().is_empty());
    assert!(h.hosts().await.is_empty());
}

#[tokio::test]
async fn a_pending_skill_without_its_files_cant_be_confirmed() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    std::fs::remove_dir_all(h.pending("gh")).unwrap();
    assert_eq!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::NotPending
    );
    assert!(h.store.agent_skills(h.agent).await.unwrap().is_empty());
}

#[tokio::test]
async fn an_agent_has_at_most_max_skills() {
    let h = harness().await;
    for i in 0..MAX_SKILLS {
        h.upload("SKILL.md", &skill_md(&format!("s{i}"), &[]))
            .await
            .unwrap();
    }
    assert_eq!(
        h.upload("SKILL.md", &skill_md("one-more", &[])).await,
        Err(Refused::TooMany)
    );
    assert!(
        h.upload("SKILL.md", &skill_md("s0", &[])).await.is_ok(),
        "replacing one still works"
    );
}

#[tokio::test]
async fn startup_clears_work_expired_and_unrecorded_pending_skills() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("keep", &["api.github.com"]))
        .await
        .unwrap();
    let stray_work = h.data.join(WORK_DIR).join("left-over");
    std::fs::create_dir_all(&stray_work).unwrap();
    let stray_pending = h.pending("nobody");
    std::fs::create_dir_all(&stray_pending).unwrap();
    let stray_agent = h.data.join(PENDING_DIR).join("not-an-agent");
    std::fs::create_dir_all(&stray_agent).unwrap();
    h.skills.purge().await.unwrap();
    assert!(!stray_work.exists());
    assert!(!stray_pending.exists());
    assert!(!stray_agent.exists());
    assert!(h.pending("keep").join("SKILL.md").is_file());
}

#[tokio::test]
async fn the_bundled_skill_is_written_once_and_kept_current() {
    let dir = TempDir::new();
    let agent = AgentId::new_v4();
    assert!(write_bundled(&dir.0, agent).await.unwrap());
    let file = runner::skills_dir(&dir.0, agent).join("agentctl/SKILL.md");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), BUNDLED_SKILL);
    assert!(!write_bundled(&dir.0, agent).await.unwrap());
    std::fs::write(&file, "stale").unwrap();
    assert!(write_bundled(&dir.0, agent).await.unwrap());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), BUNDLED_SKILL);
}

#[test]
fn the_bundled_skill_documents_every_agentctl_command() {
    let manifest = package::parse_skill_file(BUNDLED_SKILL).unwrap_err();
    assert_eq!(manifest, Problem::Reserved, "only agentd may use the name");
    for command in [
        "agentctl attach",
        "agentctl post",
        "agentctl react",
        "agentctl history",
        "agentctl lock",
        "agentctl ask-agent",
        "agentctl private",
    ] {
        assert!(BUNDLED_SKILL.contains(command), "{command}");
    }
    assert!(BUNDLED_SKILL.contains("returns a consent id at once"));
    assert!(BUNDLED_SKILL.contains("Refused"));
}

#[tokio::test]
async fn a_failing_store_allows_no_hosts() {
    let h = harness().await;
    h.store.close().await;
    assert!(
        SkillHosts(h.store.clone())
            .rules(SessionId::new_v4())
            .await
            .is_empty()
    );
}

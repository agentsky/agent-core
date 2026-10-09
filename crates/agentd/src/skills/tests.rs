use std::process::Command;

use core_types::{ConvRef, ScopeKey, SurfaceKind, TeamId, ThreadKey};
use cred_proxy::EgressPolicy;
use sqlx::Connection as _;
use store::{AgentCreation, NewAgent, Sealer, Visibility};
use testkit::TempDir;

use super::*;

const PREFIX: &str = "https://git.test/";

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
    db_url: String,
    _dir: TempDir,
}

async fn harness() -> Harness {
    let dir = TempDir::new("agentd-skills");
    let data = dir.join("data");
    let repos = dir.join("repos");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&repos).unwrap();
    let db_url = dir.db_url();
    let store = Store::open(
        &db_url,
        Sealer::from_base64(&Sealer::generate_key().unwrap()).unwrap(),
    )
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
        db_url,
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
        SkillHosts(self.skills.clone())
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
    let text = skill_md("gh", &["api.github.com", "raw.githubusercontent.com"]);
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
    assert_eq!(row.hosts, ["api.github.com", "raw.githubusercontent.com"]);
    assert!(h.live("gh").join("SKILL.md").is_file());
    assert!(!h.pending("gh").exists());
    assert_eq!(
        h.hosts().await,
        ["api.github.com", "raw.githubusercontent.com"]
    );
    assert_eq!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::NotPending,
        "confirmed once"
    );

    assert_eq!(
        h.skills.remove(h.agent, "gh").await.unwrap(),
        Removed::Active { had_hosts: true }
    );
    assert!(!h.live("gh").exists());
    assert!(
        h.hosts().await.is_empty(),
        "removing it takes its hosts away"
    );
    assert_eq!(
        h.skills.remove(h.agent, "gh").await.unwrap(),
        Removed::NotFound
    );
}

#[tokio::test]
async fn a_skill_still_waiting_is_removed_as_unconfirmed() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    assert!(h.pending("gh").join("SKILL.md").is_file());
    assert_eq!(
        h.skills.remove(h.agent, "gh").await.unwrap(),
        Removed::Unconfirmed
    );
    assert!(!h.pending("gh").exists());
    assert!(h.store.agent_skills(h.agent).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_bundled_skill_is_never_removed() {
    let h = harness().await;
    write_bundled(&h.data, h.agent).await.unwrap();
    assert_eq!(
        h.skills.remove(h.agent, BUNDLED_NAME).await.unwrap(),
        Removed::Bundled
    );
    assert!(h.live(BUNDLED_NAME).join("SKILL.md").is_file());
}

#[tokio::test]
async fn a_confirmation_after_the_wait_finds_it_expired() {
    let h = harness().await;
    h.expired("gh").await;
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    assert_eq!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::Expired
    );
    assert_eq!(
        h.store.agent_skills(h.agent).await.unwrap(),
        rows,
        "left for the sweeper"
    );
    assert!(h.pending("gh").join("SKILL.md").is_file());
    assert!(h.hosts().await.is_empty());

    h.skills.drop_expired().await.unwrap();
    assert!(!h.pending("gh").exists());
    assert!(h.store.agent_skills(h.agent).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_confirmation_before_the_files_are_in_deletes_nothing() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    let files = h.data.join("added");
    std::fs::rename(h.pending("gh"), &files).unwrap();
    assert_eq!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::NotPending
    );
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap(), rows);
    assert!(h.hosts().await.is_empty());

    std::fs::rename(&files, h.pending("gh")).unwrap();
    assert!(matches!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::Active(_)
    ));
    assert_eq!(h.hosts().await, ["api.github.com"]);
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

impl Harness {
    /// Records `name` as a pending skill added `ago`, with files waiting.
    async fn waiting_since(&self, name: &str, ago: Duration) {
        self.upload("SKILL.md", &skill_md(name, &["api.github.com"]))
            .await
            .unwrap();
        let hosts = vec!["api.github.com".to_owned()];
        let old = OffsetDateTime::now_utc() - ago;
        let digest = package::tree_digest(&self.pending(name)).unwrap();
        let new = NewSkill {
            agent: self.agent,
            name,
            source: "upload:SKILL.md",
            hosts: &hosts,
            digest: &digest,
            added_by: self.owner,
        };
        let lease = self.lease(name).await;
        self.store
            .put_skill(&new, SkillState::Pending, MAX_SKILLS, old, lease)
            .await
            .unwrap();
        self.release(name, lease).await;
    }

    /// Takes the lease on the skill `name`, as another change would.
    async fn lease(&self, name: &str) -> LeaseId {
        self.store
            .acquire_skill_lease(self.agent, name, OffsetDateTime::now_utc(), LEASE_TTL)
            .await
            .unwrap()
            .unwrap()
    }

    async fn release(&self, name: &str, lease: LeaseId) {
        assert!(
            self.store
                .release_skill_lease(self.agent, name, lease)
                .await
                .unwrap()
        );
    }

    /// Records `name` as a pending skill the sweeper drops, with files
    /// waiting.
    async fn expired(&self, name: &str) {
        self.waiting_since(name, PENDING_TTL + SWEEP_INTERVAL + Duration::from_secs(60))
            .await;
    }

    /// Puts a file where the agent's skills directory goes, so moving a
    /// skill into it fails.
    fn block_live(&self) -> PathBuf {
        let dir = runner::skills_dir(&self.data, self.agent);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
        std::fs::write(&dir, "in the way").unwrap();
        dir
    }
}

#[tokio::test]
async fn expired_pending_skills_are_dropped_with_their_files() {
    let h = harness().await;
    h.expired("old").await;
    h.upload("SKILL.md", &skill_md("update", &[]))
        .await
        .unwrap();
    h.expired("update").await;
    h.upload("SKILL.md", &skill_md("new", &["api.github.com"]))
        .await
        .unwrap();
    h.waiting_since("late", PENDING_TTL + SWEEP_INTERVAL / 2)
        .await;
    h.skills.drop_expired().await.unwrap();
    assert!(!h.pending("old").exists());
    assert!(
        !h.pending("update").exists(),
        "an active skill keeps only its own files"
    );
    assert_eq!(
        std::fs::read_to_string(h.live("update").join("SKILL.md")).unwrap(),
        skill_md("update", &[])
    );
    assert!(h.pending("new").join("SKILL.md").is_file());
    assert!(
        h.pending("late").join("SKILL.md").is_file(),
        "kept for a sweep past the deadline"
    );
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.name.as_str(), r.state))
            .collect::<Vec<_>>(),
        [
            ("late", SkillState::Pending),
            ("new", SkillState::Pending),
            ("update", SkillState::Active)
        ]
    );
    assert_eq!(
        h.skills.confirm(h.agent, "late").await.unwrap(),
        Confirmed::Expired
    );
}

#[tokio::test]
async fn expired_files_that_cant_be_removed_leave_the_others_dropped() {
    let h = harness().await;
    h.expired("stuck").await;
    h.expired("old").await;
    std::fs::remove_dir_all(h.pending("stuck")).unwrap();
    std::fs::write(h.pending("stuck"), "not a directory").unwrap();
    h.skills.drop_expired().await.unwrap();
    assert!(!h.pending("old").exists());
    assert!(h.pending("stuck").is_file());
    assert!(h.store.agent_skills(h.agent).await.unwrap().is_empty());
}

#[tokio::test]
async fn startup_clears_work_expired_and_unrecorded_skills() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("keep", &["api.github.com"]))
        .await
        .unwrap();
    h.upload("SKILL.md", &skill_md("live", &[])).await.unwrap();
    h.expired("old").await;
    write_bundled(&h.data, h.agent).await.unwrap();
    let stray_work = h.data.join(WORK_DIR).join("left-over");
    std::fs::create_dir_all(&stray_work).unwrap();
    let stray_pending = h.pending("nobody");
    std::fs::create_dir_all(&stray_pending).unwrap();
    let stray_agent = h.data.join(PENDING_DIR).join("not-an-agent");
    std::fs::create_dir_all(&stray_agent).unwrap();
    let stray_live = h.live("removed");
    std::fs::create_dir_all(&stray_live).unwrap();
    let strays = [&stray_work, &stray_pending, &stray_agent, &stray_live];
    std::fs::create_dir_all(h.live("keep")).unwrap();
    std::fs::create_dir_all(h.pending("live")).unwrap();

    h.skills.purge().await.unwrap();
    assert!(!h.pending("old").exists(), "expired skills go at once");
    for stray in strays {
        assert!(
            stray.exists(),
            "{} is too recent to be left over",
            stray.display()
        );
    }

    h.skills.purge_older_than(Duration::ZERO).await.unwrap();
    for stray in strays {
        assert!(!stray.exists(), "{}", stray.display());
    }
    assert!(h.pending("keep").join("SKILL.md").is_file());
    assert!(h.live("live").join("SKILL.md").is_file());
    assert!(
        h.live("keep").is_dir() && h.pending("live").is_dir(),
        "a row of either state keeps both directories of its name"
    );
    assert!(h.live(BUNDLED_NAME).join("SKILL.md").is_file());
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["keep", "live"]
    );
}

#[tokio::test]
async fn replacing_a_skill_takes_its_hosts_away_even_if_its_files_cant_move() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    h.skills.confirm(h.agent, "gh").await.unwrap();
    assert_eq!(h.hosts().await, ["api.github.com"]);
    h.block_live();
    assert!(
        h.skills
            .add(
                h.agent,
                Source::Upload {
                    name: "SKILL.md",
                    bytes: skill_md("gh", &[]).as_bytes(),
                },
                h.owner,
            )
            .await
            .is_err()
    );
    assert!(h.hosts().await.is_empty());
    assert!(h.work_is_empty());
}

#[tokio::test]
async fn a_confirmation_whose_files_cant_move_leaves_the_skill_waiting() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let blocked = h.block_live();
    assert!(h.skills.confirm(h.agent, "gh").await.is_err());
    assert!(h.hosts().await.is_empty());
    assert!(h.pending("gh").join("SKILL.md").is_file());
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    assert_eq!(rows[0].state, SkillState::Pending);

    std::fs::remove_file(blocked).unwrap();
    assert!(matches!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::Active(_)
    ));
    assert_eq!(h.hosts().await, ["api.github.com"]);
}

#[tokio::test]
async fn a_failed_replacement_puts_the_old_skill_back() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("notes", &[])).await.unwrap();
    let work = h.data.join(WORK_DIR).join("w");
    std::fs::create_dir_all(&work).unwrap();
    let missing = h.data.join("missing");
    assert!(move_into(&missing, &h.live("notes"), &work).await.is_err());
    assert!(h.live("notes").join("SKILL.md").is_file());
}

#[tokio::test]
async fn a_stale_confirmation_restores_the_active_skill_and_its_row() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &[])).await.unwrap();
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    let mut stale = rows
        .iter()
        .find(|row| row.state == SkillState::Pending)
        .unwrap()
        .clone();
    stale.added_at -= time::Duration::seconds(1);
    let lease = h.lease("gh").await;
    assert_eq!(
        h.skills.confirm_row(&stale, lease).await.unwrap(),
        Confirmed::NotPending
    );
    h.release("gh", lease).await;
    assert_eq!(
        std::fs::read_to_string(h.live("gh").join("SKILL.md")).unwrap(),
        skill_md("gh", &[])
    );
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap(), rows);
    assert!(h.hosts().await.is_empty());
    assert!(h.work_is_empty());
    assert!(h.pending("gh").join("SKILL.md").is_file());
    assert!(matches!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::Active(_)
    ));
    assert_eq!(h.hosts().await, ["api.github.com"]);
}

#[tokio::test]
async fn a_stale_confirmation_without_an_active_skill_puts_the_files_back() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    let mut stale = rows[0].clone();
    stale.added_at -= time::Duration::seconds(1);
    let lease = h.lease("gh").await;
    assert_eq!(
        h.skills.confirm_row(&stale, lease).await.unwrap(),
        Confirmed::NotPending
    );
    h.release("gh", lease).await;
    assert!(!h.live("gh").exists());
    assert!(h.pending("gh").join("SKILL.md").is_file());
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap(), rows);
    assert!(h.work_is_empty());
    assert!(matches!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::Active(_)
    ));
    assert_eq!(h.hosts().await, ["api.github.com"]);
}

#[tokio::test]
async fn a_confirmation_the_store_fails_puts_back_the_files_and_the_old_skill() {
    let h = harness().await;
    let old = skill_md("gh", &["a.example"]);
    h.upload("SKILL.md", &old).await.unwrap();
    h.skills.confirm(h.agent, "gh").await.unwrap();
    let new = skill_md("gh", &["b.example"]);
    h.upload("SKILL.md", &new).await.unwrap();
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    let mut db = sqlx::SqliteConnection::connect(&h.db_url).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER confirming_fails BEFORE UPDATE OF state ON agent_skills \
         BEGIN SELECT RAISE(ABORT, 'confirming fails'); END",
    )
    .execute(&mut db)
    .await
    .unwrap();

    assert!(h.skills.confirm(h.agent, "gh").await.is_err());
    assert_eq!(
        std::fs::read_to_string(h.live("gh").join("SKILL.md")).unwrap(),
        old
    );
    assert_eq!(
        std::fs::read_to_string(h.pending("gh").join("SKILL.md")).unwrap(),
        new
    );
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap(), rows);
    assert_eq!(h.hosts().await, ["a.example"]);
    assert!(h.work_is_empty());

    sqlx::query("DROP TRIGGER confirming_fails")
        .execute(&mut db)
        .await
        .unwrap();
    assert!(matches!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::Active(_)
    ));
    assert_eq!(
        std::fs::read_to_string(h.live("gh").join("SKILL.md")).unwrap(),
        new
    );
    assert_eq!(h.hosts().await, ["b.example"]);
}

#[tokio::test]
async fn a_change_to_a_skill_another_holds_waits_then_changes_nothing() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    let held = h
        .store
        .acquire_skill_lease(h.agent, "gh", OffsetDateTime::now_utc(), LEASE_TTL)
        .await
        .unwrap()
        .unwrap();
    let again = skill_md("gh", &["evil.example"]);
    let started = std::time::Instant::now();
    let (confirmed, removed, added) = tokio::join!(
        h.skills.confirm(h.agent, "gh"),
        h.skills.remove(h.agent, "gh"),
        h.upload("SKILL.md", &again),
    );
    assert!(started.elapsed() >= LEASE_WAIT);
    assert_eq!(confirmed.unwrap(), Confirmed::Busy);
    assert_eq!(removed.unwrap(), Removed::Busy);
    assert_eq!(added, Err(Refused::Busy));
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap(), rows);
    assert_eq!(
        std::fs::read_to_string(h.pending("gh").join("SKILL.md")).unwrap(),
        skill_md("gh", &["api.github.com"])
    );
    assert!(!h.live("gh").exists());
    assert!(h.hosts().await.is_empty());

    let release = async {
        tokio::time::sleep(LEASE_WAIT / 4).await;
        assert!(
            h.store
                .release_skill_lease(h.agent, "gh", held)
                .await
                .unwrap()
        );
    };
    let (confirmed, ()) = tokio::join!(h.skills.confirm(h.agent, "gh"), release);
    assert!(matches!(confirmed.unwrap(), Confirmed::Active(_)));
    assert_eq!(h.hosts().await, ["api.github.com"]);
    assert!(
        h.store
            .acquire_skill_lease(h.agent, "gh", OffsetDateTime::now_utc(), LEASE_TTL)
            .await
            .unwrap()
            .is_some(),
        "released when done"
    );
}

#[tokio::test]
async fn the_sweeper_skips_an_expired_skill_another_holds() {
    let h = harness().await;
    h.expired("gh").await;
    let held = h
        .store
        .acquire_skill_lease(h.agent, "gh", OffsetDateTime::now_utc(), LEASE_TTL)
        .await
        .unwrap()
        .unwrap();
    h.skills.drop_expired().await.unwrap();
    assert!(h.pending("gh").join("SKILL.md").is_file());
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap().len(), 1);

    h.store
        .release_skill_lease(h.agent, "gh", held)
        .await
        .unwrap();
    h.skills.drop_expired().await.unwrap();
    assert!(!h.pending("gh").exists());
    assert!(h.store.agent_skills(h.agent).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_confirmation_stopped_after_its_move_is_finished_by_the_next() {
    let h = harness().await;
    let old = skill_md("gh", &["a.example"]);
    h.upload("SKILL.md", &old).await.unwrap();
    h.skills.confirm(h.agent, "gh").await.unwrap();
    let new = skill_md("gh", &["b.example"]);
    h.upload("SKILL.md", &new).await.unwrap();
    let work = h.skills.work_dir().await.unwrap();
    move_into(&h.pending("gh"), &h.live("gh"), &work.0)
        .await
        .unwrap();
    drop(work);
    assert!(
        h.hosts().await.is_empty(),
        "the old row's hosts don't cover the new files"
    );

    let Confirmed::Active(row) = h.skills.confirm(h.agent, "gh").await.unwrap() else {
        panic!()
    };
    assert_eq!(row.hosts, ["b.example"]);
    assert_eq!(
        std::fs::read_to_string(h.live("gh").join("SKILL.md")).unwrap(),
        new
    );
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap().len(), 1);
    assert_eq!(h.hosts().await, ["b.example"]);
}

#[tokio::test]
async fn an_undo_whose_files_cant_go_back_still_restores_the_old_skill() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("notes", &[])).await.unwrap();
    let moved = h.data.join("moved");
    std::fs::create_dir_all(&moved).unwrap();
    std::fs::write(moved.join("SKILL.md"), "unconfirmed").unwrap();
    let work = h.skills.work_dir().await.unwrap();
    move_into(&moved, &h.live("notes"), &work.0).await.unwrap();
    let blocked = h.data.join("blocked");
    std::fs::write(&blocked, "not a directory").unwrap();
    assert!(
        put_back(&h.live("notes"), &blocked.join("notes"), &work.0)
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(h.live("notes").join("SKILL.md")).unwrap(),
        skill_md("notes", &[])
    );
    assert_eq!(
        std::fs::read_to_string(work.0.join("new").join("SKILL.md")).unwrap(),
        "unconfirmed"
    );
    drop(work);
    assert!(h.work_is_empty());
}

#[tokio::test]
async fn an_expiry_that_deletes_no_row_leaves_the_files() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let lease = h.lease("gh").await;
    let before = OffsetDateTime::now_utc() - PENDING_TTL;
    h.skills
        .drop_expired_skill(h.agent, "gh", before, lease)
        .await
        .unwrap();
    h.release("gh", lease).await;
    assert!(h.pending("gh").join("SKILL.md").is_file());
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_change_whose_caller_is_aborted_still_runs_to_the_end() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let held = h.lease("gh").await;
    let (skills, agent) = (h.skills.clone(), h.agent);
    let caller = tokio::spawn(async move { skills.confirm(agent, "gh").await });
    tokio::time::sleep(LEASE_RETRY).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    h.release("gh", held).await;
    let deadline = std::time::Instant::now() + LEASE_WAIT;
    while h.hosts().await.is_empty() {
        assert!(std::time::Instant::now() < deadline, "never confirmed");
        tokio::time::sleep(LEASE_RETRY).await;
    }
    assert!(h.live("gh").join("SKILL.md").is_file());
    assert!(!h.pending("gh").exists());
}

#[tokio::test]
async fn hosts_are_granted_only_while_the_files_in_use_declare_them() {
    let h = harness().await;
    for (name, host) in [("gh", "a.example"), ("py", "b.example")] {
        h.upload("SKILL.md", &skill_md(name, &[host]))
            .await
            .unwrap();
        h.skills.confirm(h.agent, name).await.unwrap();
    }
    assert_eq!(h.hosts().await, ["a.example", "b.example"]);
    std::fs::write(
        h.live("gh").join("SKILL.md"),
        skill_md("gh", &["a.example", "c.example"]),
    )
    .unwrap();
    assert_eq!(h.hosts().await, ["b.example"]);
    std::fs::remove_file(h.live("gh").join("SKILL.md")).unwrap();
    assert_eq!(h.hosts().await, ["b.example"]);
    std::fs::write(
        h.live("gh").join("SKILL.md"),
        skill_md("gh", &["a.example"]),
    )
    .unwrap();
    assert_eq!(h.hosts().await, ["a.example", "b.example"]);
}

#[tokio::test]
async fn a_confirmation_that_lost_its_lease_leaves_its_files_for_the_next() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    assert!(matches!(
        h.skills.confirm_row(&rows[0], LeaseId::new_v4()).await,
        Err(SkillError::Store(StoreError::SkillLeaseLost))
    ));
    assert!(h.live("gh").join("SKILL.md").is_file());
    assert!(!h.pending("gh").exists());
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap(), rows);
    assert!(h.hosts().await.is_empty());

    assert!(matches!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::Active(_)
    ));
    assert_eq!(h.hosts().await, ["api.github.com"]);
}

#[tokio::test]
async fn the_sweeper_never_finishes_a_confirmation_nobody_made() {
    let h = harness().await;
    h.expired("gh").await;
    let work = h.skills.work_dir().await.unwrap();
    move_into(&h.pending("gh"), &h.live("gh"), &work.0)
        .await
        .unwrap();
    drop(work);
    h.skills.drop_expired().await.unwrap();
    assert!(h.store.agent_skills(h.agent).await.unwrap().is_empty());
    assert!(h.hosts().await.is_empty());
}

/// The front matter of `skill_md(name, hosts)` without its closing `---`
/// line, padded with a YAML comment to exactly `size` bytes.
fn front_of_size(name: &str, hosts: &[&str], size: usize) -> String {
    let text = skill_md(name, hosts);
    let mut front = text[..text.rfind("---\n").unwrap()].to_owned();
    let pad = size - front.len() - 2;
    front.push('#');
    front.push_str(&"x".repeat(pad));
    front.push('\n');
    assert_eq!(front.len(), size);
    front
}

#[tokio::test]
async fn a_skill_md_add_accepts_reads_the_same_for_its_hosts() {
    let h = harness().await;
    let limit = package::MAX_FRONT_MATTER_BYTES;
    let hosts = ["api.github.com"];
    let body = "Use it well.\n".repeat(limit / 4);
    let at_limit = format!("{}---\n{body}", front_of_size("gh", &hosts, limit - 4));
    assert!(matches!(
        h.upload("SKILL.md", &at_limit).await,
        Ok(Added::Pending(_))
    ));
    assert_eq!(
        declared_hosts(&h.pending("gh")).await,
        Some(vec!["api.github.com".to_owned()])
    );
    assert!(matches!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::Active(_)
    ));
    assert_eq!(h.hosts().await, ["api.github.com"]);

    let over = format!("{}---\n{body}", front_of_size("over", &hosts, limit - 3));
    let spaced = format!(
        "{}---{}\n{body}",
        front_of_size("spaced", &hosts, 200),
        " ".repeat(limit)
    );
    for text in [over, spaced] {
        assert_eq!(
            h.upload("SKILL.md", &text).await,
            Err(Refused::Problem(Problem::NoFrontMatter))
        );
    }
}

#[tokio::test]
async fn hosts_are_read_from_no_more_than_the_front_matter() {
    let h = harness().await;
    let limit = package::MAX_FRONT_MATTER_BYTES;
    let hosts = ["api.github.com"];
    let mut past = format!("{}---\n", front_of_size("gh", &hosts, limit - 4)).into_bytes();
    past.extend(std::iter::repeat_n(0xff, limit));
    let mut cut = format!("{}---\n", front_of_size("gh", &hosts, limit / 2)).into_bytes();
    cut.resize(limit - 1, b'a');
    cut.extend("\u{e9}".as_bytes());
    cut.extend(std::iter::repeat_n(0xff, limit));
    let mut stub = front_of_size("gh", &hosts, limit - 3).into_bytes();
    stub.extend(b"---abc\n---\n");
    assert!(package::parse_skill_file(std::str::from_utf8(&stub).unwrap()).is_err());
    let declared = Some(vec!["api.github.com".to_owned()]);
    for (name, bytes, expected) in [
        ("past", past, declared.clone()),
        ("cut", cut, declared),
        ("stub", stub, None),
    ] {
        let dir = h.data.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), bytes).unwrap();
        assert_eq!(declared_hosts(&dir).await, expected, "{name}");
    }
}

#[tokio::test]
async fn an_update_whose_files_never_arrived_isnt_taken_for_the_skill_in_use() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    h.skills.confirm(h.agent, "gh").await.unwrap();
    let update = format!("{}Updated.\n", skill_md("gh", &["api.github.com"]));
    h.upload("SKILL.md", &update).await.unwrap();
    std::fs::remove_dir_all(h.pending("gh")).unwrap();
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    assert_eq!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::NotPending
    );
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap(), rows);
    assert_eq!(
        std::fs::read_to_string(h.live("gh").join("SKILL.md")).unwrap(),
        skill_md("gh", &["api.github.com"])
    );
}

async fn panicking() -> Result<(), SkillError> {
    panic!("a skill change panicked")
}

#[tokio::test]
async fn a_change_that_panics_releases_the_lease() {
    let h = harness().await;
    assert!(matches!(
        h.skills
            .leased(h.agent, "gh", Duration::ZERO, |_, _| panicking())
            .await,
        Err(SkillError::Task(_))
    ));
    let lease = h.lease("gh").await;
    h.release("gh", lease).await;
}

#[tokio::test]
async fn shutdown_waits_for_a_change_running_until_its_timeout() {
    let h = harness().await;
    let gate = Arc::new(tokio::sync::Notify::new());
    let opened = gate.clone();
    let change = tokio::spawn({
        let skills = h.skills.clone();
        let agent = h.agent;
        async move {
            skills
                .leased(agent, "gh", Duration::ZERO, move |_, _| async move {
                    opened.notified().await;
                    Ok(())
                })
                .await
        }
    });
    tokio::time::sleep(LEASE_RETRY).await;
    assert!(
        !h.skills.drain(LEASE_RETRY).await,
        "gives up at its timeout"
    );
    let drain = tokio::spawn({
        let skills = h.skills.clone();
        async move { skills.drain(LEASE_WAIT).await }
    });
    tokio::time::sleep(LEASE_RETRY).await;
    assert!(!drain.is_finished(), "waits for the change");
    gate.notify_one();
    assert!(drain.await.unwrap());
    assert_eq!(change.await.unwrap().unwrap(), Some(()));
}

#[tokio::test]
async fn files_declaring_other_hosts_than_the_row_are_left_waiting() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let other = skill_md("gh", &["evil.example"]);
    std::fs::write(h.pending("gh").join("SKILL.md"), &other).unwrap();
    assert_eq!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::NotPending
    );
    assert_eq!(
        std::fs::read_to_string(h.pending("gh").join("SKILL.md")).unwrap(),
        other
    );
    assert!(!h.live("gh").exists());
    let rows = h.store.agent_skills(h.agent).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, SkillState::Pending);
    assert!(h.hosts().await.is_empty());
    assert!(h.work_is_empty());
}

#[tokio::test]
async fn a_clone_of_a_highly_compressible_blob_stops_at_the_cap() {
    let h = harness().await;
    let blob = "0".repeat(8 * 1024 * 1024);
    repo(
        &h.repos,
        "big.git",
        &[("SKILL.md", &skill_md("big", &[])), ("blob", &blob)],
    );
    let git = Git::new(EgressPolicy::new(Vec::new(), Vec::new()))
        .with_limits(Path::new("git"), git::CLONE_TIMEOUT, 1024 * 1024)
        .serving_prefix_from_directory_for_tests(PREFIX, &h.repos);
    let dest = h.data.join("clone/src");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    assert_eq!(
        git.clone_into("https://git.test/big.git", &dest).await,
        Err(CloneError::TooLarge)
    );
}

#[tokio::test]
async fn the_bundled_skill_is_written_once_and_kept_current() {
    let dir = TempDir::new("agentd-skills");
    let agent = AgentId::new_v4();
    assert!(write_bundled(dir.path(), agent).await.unwrap());
    let file = runner::skills_dir(dir.path(), agent).join("agentctl/SKILL.md");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), BUNDLED_SKILL);
    assert!(!write_bundled(dir.path(), agent).await.unwrap());
    std::fs::write(&file, "stale").unwrap();
    assert!(write_bundled(dir.path(), agent).await.unwrap());
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
        SkillHosts(h.skills.clone())
            .rules(SessionId::new_v4())
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_failed_replacement_of_a_waiting_skill_leaves_no_new_hosts_to_confirm() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let agent_dir = h.pending("gh").parent().unwrap().to_path_buf();
    let stash = h.data.join("stash");
    std::fs::rename(&agent_dir, &stash).unwrap();
    std::fs::write(&agent_dir, "in the way").unwrap();
    assert!(
        h.skills
            .add(
                h.agent,
                Source::Upload {
                    name: "SKILL.md",
                    bytes: skill_md("gh", &["evil.example"]).as_bytes(),
                },
                h.owner,
            )
            .await
            .is_err()
    );
    std::fs::remove_file(&agent_dir).unwrap();
    std::fs::rename(&stash, &agent_dir).unwrap();

    assert_eq!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::NotPending
    );
    assert!(h.hosts().await.is_empty());
    assert!(h.work_is_empty());
}

#[test]
fn debug_shows_the_upload_and_description_lengths_not_their_content() {
    let upload = Source::Upload {
        name: "SKILL.md",
        bytes: b"the secret plan",
    };
    let manifest =
        package::parse_skill_file("---\nname: pdf-tools\ndescription: the secret plan\n---\n")
            .unwrap();
    for debug in [
        format!("{upload:?}"),
        format!("{:?}", Added::Active(manifest.clone())),
        format!("{:?}", Added::Pending(manifest)),
    ] {
        assert!(!debug.contains("secret plan"), "{debug}");
        assert!(debug.contains("_len: 15"), "{debug}");
    }
}

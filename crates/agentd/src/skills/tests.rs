use std::process::Command;

use core_types::{ConvRef, ScopeKey, SurfaceKind, TeamId, ThreadKey};
use cred_proxy::EgressPolicy;
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
    _dir: TempDir,
}

async fn harness() -> Harness {
    let dir = TempDir::new("agentd-skills");
    let data = dir.join("data");
    let repos = dir.join("repos");
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
            MAX_SKILLS,
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

    h.waiting_since("gh", PENDING_TTL + Duration::from_secs(60))
        .await;
    let expired = h.store.agent_skills(h.agent).await.unwrap().remove(0);
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    let Confirmed::Active(row) = h.skills.confirm_row(&expired).await.unwrap() else {
        panic!("confirms the skill added again");
    };
    assert!(row.added_at > expired.added_at);
    assert!(h.live("gh").join("SKILL.md").is_file());
    assert!(!h.pending("gh").exists());
    assert_eq!(h.hosts().await, ["api.github.com"]);
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

impl Harness {
    /// Records `name` as a pending skill added `ago`, with files waiting.
    async fn waiting_since(&self, name: &str, ago: Duration) {
        self.upload("SKILL.md", &skill_md(name, &["api.github.com"]))
            .await
            .unwrap();
        let hosts = vec!["api.github.com".to_owned()];
        let old = OffsetDateTime::now_utc() - ago;
        let new = NewSkill {
            agent: self.agent,
            name,
            source: "upload:SKILL.md",
            hosts: &hosts,
            added_by: self.owner,
        };
        self.store
            .put_skill(&new, SkillState::Pending, MAX_SKILLS, old)
            .await
            .unwrap();
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
async fn a_skill_added_again_while_its_expired_row_is_dropped_keeps_its_files() {
    let h = harness().await;
    h.expired("gh").await;
    let before = OffsetDateTime::now_utc() - PENDING_TTL - SWEEP_INTERVAL;
    assert_eq!(
        h.store.delete_pending_skills_before(before).await.unwrap(),
        [(h.agent, "gh".to_owned())]
    );
    h.upload("SKILL.md", &skill_md("gh", &["api.github.com"]))
        .await
        .unwrap();
    h.skills.drop_expired_files(h.agent, "gh").await.unwrap();
    assert!(h.pending("gh").join("SKILL.md").is_file());
    assert!(matches!(
        h.skills.confirm(h.agent, "gh").await.unwrap(),
        Confirmed::Active(_)
    ));
    assert!(h.live("gh").join("SKILL.md").is_file());
    assert_eq!(h.hosts().await, ["api.github.com"]);
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
async fn a_confirmation_whose_row_went_puts_back_only_what_it_moved() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("notes", &[])).await.unwrap();
    let moved = h.data.join("moved");
    std::fs::create_dir_all(&moved).unwrap();
    std::fs::write(moved.join("SKILL.md"), "unconfirmed").unwrap();
    let work = h.skills.work_dir().await.unwrap();
    let ino = inode(&moved).await.unwrap();
    move_into(&moved, &h.live("notes"), &work.0).await.unwrap();
    h.skills
        .put_back(&h.live("notes"), ino, &work.0)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(h.live("notes").join("SKILL.md")).unwrap(),
        skill_md("notes", &[])
    );
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap().len(), 1);
    drop(work);

    std::fs::create_dir_all(&moved).unwrap();
    let work = h.skills.work_dir().await.unwrap();
    let ino = inode(&moved).await.unwrap();
    move_into(&moved, &h.live("new"), &work.0).await.unwrap();
    h.skills
        .put_back(&h.live("new"), ino, &work.0)
        .await
        .unwrap();
    assert!(!h.live("new").exists());
    assert!(h.live("notes").join("SKILL.md").is_file());
    drop(work);
    assert!(h.work_is_empty());
}

#[tokio::test]
async fn a_confirmation_leaves_files_that_replaced_the_ones_it_moved() {
    let h = harness().await;
    h.upload("SKILL.md", &skill_md("notes", &[])).await.unwrap();
    let moved = h.data.join("moved");
    std::fs::create_dir_all(&moved).unwrap();
    std::fs::write(moved.join("SKILL.md"), "unconfirmed").unwrap();
    let work = h.skills.work_dir().await.unwrap();
    let ino = inode(&moved).await.unwrap();
    move_into(&moved, &h.live("notes"), &work.0).await.unwrap();
    let newer = h.data.join("newer");
    std::fs::create_dir_all(&newer).unwrap();
    std::fs::write(newer.join("SKILL.md"), "newer").unwrap();
    let other = h.skills.work_dir().await.unwrap();
    move_into(&newer, &h.live("notes"), &other.0).await.unwrap();
    h.skills
        .put_back(&h.live("notes"), ino, &work.0)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(h.live("notes").join("SKILL.md")).unwrap(),
        "newer"
    );
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
    assert_eq!(
        h.skills.confirm_row(&stale).await.unwrap(),
        Confirmed::NotPending
    );
    assert_eq!(
        std::fs::read_to_string(h.live("gh").join("SKILL.md")).unwrap(),
        skill_md("gh", &[])
    );
    assert_eq!(h.store.agent_skills(h.agent).await.unwrap(), rows);
    assert!(h.hosts().await.is_empty());
    assert!(h.work_is_empty());
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
        SkillHosts(h.store.clone())
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

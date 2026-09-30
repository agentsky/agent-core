//! Where an agent's persona file and skills live, and writing files there.

use std::path::{Path, PathBuf};

use core_types::AgentId;
use tokio::io::AsyncWriteExt;

use crate::{Result, RunnerError};

/// The directory under agentd's data directory holding each agent's
/// persona directory.
pub const AGENTS_DIR: &str = "agents";

/// The directory under agentd's data directory holding each agent's
/// skills directory.
pub const SKILLS_DIR: &str = "skills";

/// The agent's skills directory, `<data_dir>/skills/<agent>`, with one
/// directory per skill: what
/// [`SessionSpec::skills_dir`](sandbox::SessionSpec::skills_dir) names, so
/// every session of the agent sees it read-only at
/// `$CLAUDE_CONFIG_DIR/skills`. A session mounts it only if it exists when
/// the session's container starts.
pub fn skills_dir(data_dir: &Path, agent: AgentId) -> PathBuf {
    data_dir.join(SKILLS_DIR).join(agent.to_string())
}

/// The agent's persona directory, `<data_dir>/agents/<agent>`: what
/// [`SessionSpec::persona_dir`](sandbox::SessionSpec::persona_dir) names.
/// It holds [`sandbox::PERSONA_FILE`].
pub fn persona_dir(data_dir: &Path, agent: AgentId) -> PathBuf {
    data_dir.join(AGENTS_DIR).join(agent.to_string())
}

/// Writes the agent's persona file, `<data_dir>/agents/<agent>/persona.md`,
/// creating the directory if needed. Returns whether the file changed.
///
/// The file is written only when its bytes differ, and then atomically: to
/// a new file in the same directory that is renamed over the old one, so a
/// process starting meanwhile reads either the old persona or the new one,
/// never part of one. A process reads the file when it starts, so an edit
/// reaches a session when its process next starts, and the file stays
/// byte-identical across restarts until then, which keeps prompt caching
/// working.
///
/// # Errors
///
/// [`RunnerError::Io`] if the directory or the file can't be written.
pub async fn write_persona(data_dir: &Path, agent: AgentId, persona: &str) -> Result<bool> {
    write_if_changed(
        &persona_dir(data_dir, agent),
        sandbox::PERSONA_FILE,
        persona.as_bytes(),
    )
    .await
}

/// Writes `bytes` to the file `name` in `dir`, creating the directory if
/// needed, and returns whether the file changed: only when its bytes
/// differ, and then atomically, through a new file in `dir` renamed over
/// the old one, so a reader sees either the old bytes or the new ones.
///
/// # Errors
///
/// [`RunnerError::Io`] if the directory or the file can't be written.
pub async fn write_if_changed(dir: &Path, name: &str, bytes: &[u8]) -> Result<bool> {
    let io = |what| move |source| RunnerError::Io { what, source };
    let path = dir.join(name);
    match tokio::fs::read(&path).await {
        Ok(existing) if existing == bytes => return Ok(false),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(io("reading a file to replace")(err)),
    }
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(io("creating a file's directory"))?;
    let temp = dir.join(format!(".{name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp, &path).await
    }
    .await;
    if let Err(err) = written {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(io("writing a file")(err));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("runner-persona-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_skills_directory_is_under_skills() {
        let agent = AgentId::new_v4();
        assert_eq!(
            skills_dir(Path::new("/data"), agent),
            Path::new("/data/skills").join(agent.to_string())
        );
    }

    #[test]
    fn the_persona_directory_is_under_agents() {
        let agent = AgentId::new_v4();
        assert_eq!(
            persona_dir(Path::new("/data"), agent),
            Path::new("/data/agents").join(agent.to_string())
        );
    }

    #[tokio::test]
    async fn write_persona_writes_only_changes_and_leaves_no_temp_files() {
        let dir = TempDir::new();
        let agent = AgentId::new_v4();
        let file = persona_dir(&dir.0, agent).join("persona.md");
        assert!(
            write_persona(&dir.0, agent, "You are Ada.\n")
                .await
                .unwrap()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "You are Ada.\n");
        let inode = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&file).unwrap());

        assert!(
            !write_persona(&dir.0, agent, "You are Ada.\n")
                .await
                .unwrap()
        );
        let same = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&file).unwrap());
        assert_eq!(inode, same, "an unchanged persona is not rewritten");

        assert!(
            write_persona(&dir.0, agent, "You are Grace.")
                .await
                .unwrap()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "You are Grace.");
        let names: Vec<_> = std::fs::read_dir(persona_dir(&dir.0, agent))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["persona.md"]);
    }

    #[tokio::test]
    async fn a_failed_write_is_an_io_error() {
        let dir = TempDir::new();
        let agent = AgentId::new_v4();
        std::fs::write(dir.0.join(AGENTS_DIR), "a file, not a directory").unwrap();
        let err = write_persona(&dir.0, agent, "x").await.unwrap_err();
        assert!(matches!(err, RunnerError::Io { .. }), "{err:?}");

        let dir = TempDir::new();
        let file = persona_dir(&dir.0, agent).join("persona.md");
        std::fs::create_dir_all(&file).unwrap();
        let err = write_persona(&dir.0, agent, "x").await.unwrap_err();
        assert!(matches!(err, RunnerError::Io { .. }), "{err:?}");
    }
}

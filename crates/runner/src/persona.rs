//! Where an agent's persona file lives, and writing it.

use std::path::{Path, PathBuf};

use core_types::AgentId;
use tokio::io::AsyncWriteExt;

use crate::{Result, RunnerError};

/// The directory under agentd's data directory holding each agent's
/// persona directory.
pub const AGENTS_DIR: &str = "agents";

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
    let io = |what| move |source| RunnerError::Io { what, source };
    let dir = persona_dir(data_dir, agent);
    let path = dir.join(sandbox::PERSONA_FILE);
    match tokio::fs::read(&path).await {
        Ok(existing) if existing == persona.as_bytes() => return Ok(false),
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(io("reading the persona file")(err)),
    }
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(io("creating the persona directory"))?;
    let temp = dir.join(format!(
        ".{}.{}.tmp",
        sandbox::PERSONA_FILE,
        uuid::Uuid::new_v4().simple()
    ));
    let written = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await?;
        file.write_all(persona.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temp, &path).await
    }
    .await;
    if let Err(err) = written {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(io("writing the persona file")(err));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use testkit::TempDir;

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
        let dir = TempDir::new("runner-test");
        let agent = AgentId::new_v4();
        let file = persona_dir(dir.path(), agent).join("persona.md");
        assert!(
            write_persona(dir.path(), agent, "You are Ada.\n")
                .await
                .unwrap()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "You are Ada.\n");
        let inode = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&file).unwrap());

        assert!(
            !write_persona(dir.path(), agent, "You are Ada.\n")
                .await
                .unwrap()
        );
        let same = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&file).unwrap());
        assert_eq!(inode, same, "an unchanged persona is not rewritten");

        assert!(
            write_persona(dir.path(), agent, "You are Grace.")
                .await
                .unwrap()
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "You are Grace.");
        let names: Vec<_> = std::fs::read_dir(persona_dir(dir.path(), agent))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["persona.md"]);
    }

    #[tokio::test]
    async fn a_failed_write_is_an_io_error() {
        let dir = TempDir::new("runner-test");
        let agent = AgentId::new_v4();
        std::fs::write(dir.join(AGENTS_DIR), "a file, not a directory").unwrap();
        let err = write_persona(dir.path(), agent, "x").await.unwrap_err();
        assert!(matches!(err, RunnerError::Io { .. }), "{err:?}");

        let dir = TempDir::new("runner-test");
        let file = persona_dir(dir.path(), agent).join("persona.md");
        std::fs::create_dir_all(&file).unwrap();
        let err = write_persona(dir.path(), agent, "x").await.unwrap_err();
        assert!(matches!(err, RunnerError::Io { .. }), "{err:?}");
    }
}

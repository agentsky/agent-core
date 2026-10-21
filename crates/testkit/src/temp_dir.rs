//! [`TempDir`]: a directory for one test, removed when the test ends.

use std::path::{Path, PathBuf};

use uuid::Uuid;

/// A new directory under the system temp directory, removed with
/// everything in it on drop, so also when the test panics.
#[derive(Debug)]
pub struct TempDir(PathBuf);

impl TempDir {
    /// Creates `<temp>/<prefix>-<uuid>`. The prefix names the crate or
    /// test, so a directory left behind by a killed run says where it came
    /// from. Bind the result to a name: a `TempDir` dropped at once, as
    /// `let _ =` or a temporary does, removes its directory at once.
    #[must_use = "dropping a TempDir removes its directory"]
    pub fn new(prefix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(std::env::temp_dir())
            .and_then(|()| std::fs::create_dir(&dir))
            .unwrap_or_else(|e| panic!("creating {}: {e}", dir.display()));
        Self(dir)
    }

    /// The directory.
    pub fn path(&self) -> &Path {
        &self.0
    }

    /// `path` inside the directory.
    pub fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.0.join(path)
    }

    /// A SQLite URL for `agentd.db` in the directory.
    pub fn db_url(&self) -> String {
        format!("sqlite://{}", self.join("agentd.db").display())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_directory_and_its_contents_go_on_drop() {
        let dir = TempDir::new("testkit-temp-dir");
        let name = dir.path().file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with("testkit-temp-dir-"), "{name}");
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/file"), "x").unwrap();
        let path = dir.path().to_owned();
        drop(dir);
        assert!(!path.exists());
    }

    #[test]
    fn the_directory_goes_when_the_test_panics() {
        let mut path = None;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let dir = TempDir::new("testkit-temp-dir");
            std::fs::write(dir.join("file"), "x").unwrap();
            path = Some(dir.path().to_owned());
            panic!("panic injected to check cleanup");
        }));
        assert!(result.is_err());
        assert!(!path.unwrap().exists());
    }

    #[test]
    fn db_url_names_agentd_db_in_the_directory() {
        let dir = TempDir::new("testkit-temp-dir");
        assert_eq!(
            dir.db_url(),
            format!("sqlite://{}/agentd.db", dir.path().display())
        );
    }
}

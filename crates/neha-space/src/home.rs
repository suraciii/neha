//! The Space's home directory.
//!
//! `NEHA_HOME` names one Space. The home holds the record, secrets, File
//! bytes, Computer workspaces, logs, and the scratch directories a running
//! Space needs.

use std::fs;
use std::path::{Path, PathBuf};

use neha::id::Id;
use thiserror::Error;

/// The schema version this build writes.
pub use crate::schema::SCHEMA_VERSION;

const DIRECTORIES: [&str; 8] = [
    "db",
    "secrets",
    "state",
    "tmp",
    "cache",
    "logs",
    "computers",
    "blobs",
];

/// One Space's home directory.
#[derive(Clone, Debug)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// Reads `NEHA_HOME` and opens the Space it names.
    pub fn from_env() -> Result<Self, HomeError> {
        let root = std::env::var_os("NEHA_HOME").ok_or(HomeError::MissingEnv)?;

        Self::open(root)
    }

    /// Opens a home, creating the directories it needs.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, HomeError> {
        let home = Self { root: root.into() };
        home.prepare()?;
        let root = fs::canonicalize(home.root()).unwrap_or(home.root);

        Ok(Self { root })
    }

    fn prepare(&self) -> Result<(), HomeError> {
        fs::create_dir_all(&self.root)?;
        for name in DIRECTORIES {
            fs::create_dir_all(self.root.join(name))?;
        }
        for path in [
            &self.root,
            &self.root.join("db"),
            &self.root.join("secrets"),
        ] {
            restrict(path)?;
        }
        clear_dir(&self.root.join("tmp"))?;

        Ok(())
    }

    /// Returns the root of the home.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the record's database file.
    #[must_use]
    pub fn db_path(&self) -> PathBuf {
        self.root
            .join("db")
            .join(format!("neha-{}.sqlite3", crate::schema::SCHEMA_VERSION))
    }

    /// Returns the directory that holds the bytes of Files.
    #[must_use]
    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    /// Returns the directory for a Computer's workspace.
    #[must_use]
    pub fn workspace_dir(&self, computer: Id) -> PathBuf {
        self.root
            .join("computers")
            .join(computer.to_string())
            .join("workspace")
    }
}

fn restrict(path: &Path) -> Result<(), HomeError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

fn clear_dir(path: &Path) -> Result<(), HomeError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let target = entry.path();
        if target.is_dir() {
            fs::remove_dir_all(target)?;
        } else {
            fs::remove_file(target)?;
        }
    }

    Ok(())
}

/// A home that cannot be opened.
#[derive(Debug, Error)]
pub enum HomeError {
    /// `NEHA_HOME` is not set.
    #[error("NEHA_HOME is not set")]
    MissingEnv,
    /// The directory cannot be read or written.
    #[error("cannot use the home directory: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::Home;

    #[test]
    fn opens_the_directories_and_clears_scratch() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        std::fs::write(home.root().join("tmp").join("leftover"), b"x").unwrap();

        let home = Home::open(dir.path()).unwrap();

        assert!(home.root().join("db").is_dir());
        assert!(home.root().join("secrets").is_dir());
        assert!(home.root().join("computers").is_dir());
        assert!(home.blobs_dir().is_dir());
        assert!(!home.root().join("tmp").join("leftover").exists());
        assert!(home.db_path().starts_with(home.root().join("db")));
    }

    #[test]
    fn a_computer_has_its_own_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let computer = neha::id::Id::new();

        let workspace = home.workspace_dir(computer);

        assert!(workspace.ends_with("workspace"));
        assert!(workspace.starts_with(home.root().join("computers")));
    }
}

#[cfg(test)]
mod canonical_tests {
    use super::Home;

    #[test]
    fn a_relative_home_becomes_an_absolute_canonical_path() {
        let relative = format!("target/neha-relative-home-{}", std::process::id());
        let home = Home::open(&relative).unwrap();
        assert!(home.root().is_absolute());
        assert_eq!(home.root(), std::fs::canonicalize(&relative).unwrap());
        std::fs::remove_dir_all(home.root()).ok();
    }
}

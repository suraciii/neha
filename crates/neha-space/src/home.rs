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

/// The file name of the Space's database inside `db`.
const DATABASE_FILE: &str = "neha.sqlite3";

/// The database file an older build wrote, renamed when the home opens.
const LEGACY_DATABASE_FILE: &str = "neha-1.sqlite3";

const DIRECTORIES: [&str; 10] = [
    "db",
    "secrets",
    "state",
    "tmp",
    "cache",
    "logs",
    "workspaces",
    "blobs",
    "plugins",
    "plugin-data",
];

/// One Space's home directory.
#[derive(Clone, Debug)]
pub struct Home {
    root: PathBuf,
    /// Where the local ticket lives, when a caller names it.
    ///
    /// The Space resolves the ticket's configuration directory from the
    /// environment; a test names one so its Spaces never share the User's
    /// real `~/.config/neha`.
    ticket_directory: Option<PathBuf>,
}

impl Home {
    /// Reads `NEHA_HOME` and opens the Space it names.
    pub fn from_env() -> Result<Self, HomeError> {
        let root = std::env::var_os("NEHA_HOME").ok_or(HomeError::MissingEnv)?;

        Self::open(root)
    }

    /// Opens a home, creating the directories it needs.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, HomeError> {
        let home = Self {
            root: root.into(),
            ticket_directory: None,
        };
        home.prepare()?;
        let root = fs::canonicalize(home.root()).unwrap_or(home.root);

        Ok(Self {
            root,
            ticket_directory: None,
        })
    }

    /// Opens a home whose local ticket lives at `ticket_directory`.
    ///
    /// Tests use this to keep the configuration directory inside their own
    /// temporary home; production resolves it from `XDG_CONFIG_HOME`/`HOME`.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn open_with_ticket_directory(
        root: impl Into<PathBuf>,
        ticket_directory: impl Into<PathBuf>,
    ) -> Result<Self, HomeError> {
        let mut home = Self::open(root)?;
        home.ticket_directory = Some(ticket_directory.into());

        Ok(home)
    }

    /// Returns the ticket directory a caller named, if any.
    #[must_use]
    pub(crate) fn ticket_directory(&self) -> Option<&Path> {
        self.ticket_directory.as_deref()
    }

    fn prepare(&self) -> Result<(), HomeError> {
        fs::create_dir_all(&self.root)?;
        for name in DIRECTORIES {
            fs::create_dir_all(self.root.join(name))?;
        }
        rename_legacy_database(&self.root)?;
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
        self.root.join("db").join(DATABASE_FILE)
    }

    /// Returns the directory that holds the bytes of Files.
    #[must_use]
    pub fn blobs_dir(&self) -> PathBuf {
        self.root.join("blobs")
    }

    /// Returns the directory that holds installed packages.
    #[must_use]
    pub fn plugins_dir(&self) -> PathBuf {
        self.root.join("plugins")
    }

    /// Returns the directory that holds a package's data.
    #[must_use]
    pub fn plugin_data_dir(&self, name: &str) -> PathBuf {
        self.root.join("plugin-data").join(name)
    }

    /// Returns the root of a Computer's Workspace.
    #[must_use]
    pub fn workspace_dir(&self, computer: Id) -> PathBuf {
        self.root.join("workspaces").join(computer.to_string())
    }

    /// Returns the directory one Bot works in inside its Computer's Workspace.
    #[must_use]
    pub fn bot_workspace_dir(&self, computer: Id, bot: Id) -> PathBuf {
        self.workspace_dir(computer).join(bot.to_string())
    }
}

fn rename_legacy_database(root: &Path) -> Result<(), HomeError> {
    let db = root.join("db");
    let current = db.join(DATABASE_FILE);
    let legacy = db.join(LEGACY_DATABASE_FILE);
    if current.exists() || !legacy.exists() {
        return Ok(());
    }

    // SQLite finds a write-ahead log and its shared memory by the database
    // file's name. Move them first so a crash between the renames leaves both
    // halves under the new name.
    for suffix in ["-wal", "-shm"] {
        let from = db.join(format!("{LEGACY_DATABASE_FILE}{suffix}"));
        if from.exists() {
            fs::rename(&from, db.join(format!("{DATABASE_FILE}{suffix}")))?;
        }
    }
    fs::rename(&legacy, &current)?;

    Ok(())
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
        assert!(home.root().join("workspaces").is_dir());
        assert!(home.blobs_dir().is_dir());
        assert!(!home.root().join("tmp").join("leftover").exists());
        assert_eq!(home.db_path(), home.root().join("db").join("neha.sqlite3"));
    }

    #[test]
    fn renames_the_version_one_database_and_its_logs() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        std::fs::create_dir_all(&db).unwrap();
        std::fs::write(db.join("neha-1.sqlite3"), b"record").unwrap();
        std::fs::write(db.join("neha-1.sqlite3-wal"), b"log").unwrap();
        std::fs::write(db.join("neha-1.sqlite3-shm"), b"shared").unwrap();

        let home = Home::open(dir.path()).unwrap();

        assert_eq!(
            std::fs::read(home.db_path()).unwrap(),
            b"record",
            "the record moved under the fixed name"
        );
        assert_eq!(
            std::fs::read(home.root().join("db").join("neha.sqlite3-wal")).unwrap(),
            b"log"
        );
        assert!(home.root().join("db").join("neha.sqlite3-shm").exists());
        assert!(!db.join("neha-1.sqlite3").exists());
        assert!(!db.join("neha-1.sqlite3-wal").exists());
    }

    #[test]
    fn keeps_both_files_when_the_fixed_name_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("db");
        std::fs::create_dir_all(&db).unwrap();
        std::fs::write(db.join("neha.sqlite3"), b"current").unwrap();
        std::fs::write(db.join("neha-1.sqlite3"), b"legacy").unwrap();

        let home = Home::open(dir.path()).unwrap();

        assert_eq!(std::fs::read(home.db_path()).unwrap(), b"current");
        assert_eq!(std::fs::read(db.join("neha-1.sqlite3")).unwrap(), b"legacy");
    }

    #[test]
    fn a_computer_has_its_own_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::open(dir.path()).unwrap();
        let computer = neha::id::Id::new();

        let workspace = home.workspace_dir(computer);

        assert!(workspace.starts_with(home.root().join("workspaces")));
        assert!(workspace.ends_with(computer.to_string()));

        let bot = neha::id::Id::new();
        let folder = home.bot_workspace_dir(computer, bot);
        assert_eq!(folder, workspace.join(bot.to_string()));
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

//! Folder paths.
//!
//! A granted folder and the directory a run names are both write roots the
//! Space hands to a Computer, so the Space and the CLI must read them the same
//! way. One rule lives here: the lexical form folds repeated separators and
//! `.` components but keeps `..` for a caller to refuse, and a folder the
//! Space names is trimmed, absolute, and contains no `..` or NUL.

/// Why a path is not a folder the Space can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FolderError {
    /// The path is empty or only whitespace.
    #[error("a folder path cannot be empty")]
    Empty,
    /// The path is not absolute.
    #[error("a folder path must be absolute")]
    Relative,
    /// The path contains a `..` component.
    #[error("a folder path cannot contain `..`")]
    ParentDir,
    /// The path contains a NUL byte.
    #[error("a folder path cannot contain NUL")]
    Nul,
}

/// Returns the one lexical spelling of a path.
///
/// Repeated separators are folded, `.` components dropped, and a trailing
/// separator dropped. `..` is kept so callers can refuse it, and an absolute
/// path stays absolute. Component names are otherwise preserved exactly:
/// `logs ` and `logs` are different directories.
#[must_use]
pub fn lexical(path: &str) -> String {
    let absolute = path.starts_with('/');
    let components: Vec<&str> = path
        .split('/')
        .filter(|component| !component.is_empty() && *component != ".")
        .collect();
    let joined = components.join("/");
    if absolute {
        if joined.is_empty() {
            "/".to_owned()
        } else {
            format!("/{joined}")
        }
    } else if joined.is_empty() {
        // The relative root: `.` names the Workspace root.
        ".".to_owned()
    } else {
        joined
    }
}

/// Normalizes one absolute folder path the Space or the CLI names.
///
/// The whitespace around the path is not part of it. The path is absolute and
/// carries no `..` component, so the folder it names is the folder the Space
/// records and shows; repeated separators and `.` components fold away.
pub fn normalize_folder(path: &str) -> Result<String, FolderError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(FolderError::Empty);
    }
    if trimmed.contains('\0') {
        return Err(FolderError::Nul);
    }
    let path = std::path::Path::new(trimmed);
    if !path.is_absolute() {
        return Err(FolderError::Relative);
    }
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(FolderError::ParentDir);
    }

    Ok(lexical(trimmed))
}

#[cfg(test)]
mod tests {
    use super::{FolderError, lexical, normalize_folder};

    #[test]
    fn a_path_folds_to_one_lexical_spelling() {
        assert_eq!(lexical("/data//shared/./notes/"), "/data/shared/notes");
        assert_eq!(lexical("/"), "/");
        assert_eq!(lexical("//"), "/");
        assert_eq!(lexical("."), ".");
        assert_eq!(lexical("./logs//x"), "logs/x");
        assert_eq!(lexical("logs "), "logs ");
        assert_eq!(
            lexical("/data/../etc"),
            "/data/../etc",
            "`..` is kept for a caller to refuse"
        );
    }

    #[test]
    fn a_folder_is_trimmed_absolute_and_free_of_dot_dot() {
        assert_eq!(
            normalize_folder("  /data//shared/./notes  "),
            Ok("/data/shared/notes".to_owned())
        );
        assert_eq!(normalize_folder("/"), Ok("/".to_owned()));
        for (path, error) in [
            ("", FolderError::Empty),
            ("   ", FolderError::Empty),
            ("relative/path", FolderError::Relative),
            ("./relative", FolderError::Relative),
            ("/data/../etc", FolderError::ParentDir),
            ("/data\u{0}notes", FolderError::Nul),
        ] {
            assert_eq!(normalize_folder(path), Err(error), "{path:?}");
        }
    }
}

//! Folder paths.
//!
//! A granted folder and the directory a run names are both write roots the
//! Space hands to a Computer, so the Space and the CLI must read them the same
//! way. One rule lives here: the lexical form folds repeated separators and
//! `.` components but keeps `..` for a caller to refuse, and a folder the
//! Space names is absolute and contains no `..` or NUL.
//!
//! A raw path is trimmed once at the edge where a User typed it
//! ([`normalize_folder`]). A path a client already normalized keeps every
//! component exactly ([`normalize_recorded`]): trimming a normalized name
//! whose last component ends in a space would name another folder, and the
//! Computer is promised the folder the record shows.

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

/// Normalizes one folder path a client already normalized.
///
/// The value keeps every component exactly, including a last component that
/// ends in a space: the CLI trims the raw path once, and the Space records the
/// folder that trim left. Trimming here again would hand the Computer another
/// folder than the record names.
pub fn normalize_recorded(path: &str) -> Result<String, FolderError> {
    if path.trim().is_empty() {
        return Err(FolderError::Empty);
    }
    if path.contains('\0') {
        return Err(FolderError::Nul);
    }
    let path = std::path::Path::new(path);
    if !path.is_absolute() {
        return Err(FolderError::Relative);
    }
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(FolderError::ParentDir);
    }

    Ok(lexical(path.to_string_lossy().as_ref()))
}

#[cfg(test)]
mod tests {
    use super::{FolderError, lexical, normalize_folder, normalize_recorded};

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
    fn a_recorded_folder_keeps_its_component_names() {
        assert_eq!(
            normalize_recorded("/var/tmp/logs "),
            Ok("/var/tmp/logs ".to_owned()),
            "a last component that ends in a space names its own folder"
        );
        assert_eq!(
            normalize_recorded("/var//tmp/./logs /x"),
            Ok("/var/tmp/logs /x".to_owned())
        );
        assert_eq!(
            normalize_recorded("  /var/tmp/logs  "),
            Err(FolderError::Relative),
            "a client path is normalized before it crosses the wire"
        );
    }

    #[test]
    fn a_recorded_folder_is_still_absolute_and_free_of_dot_dot() {
        for (path, error) in [
            ("", FolderError::Empty),
            ("relative/path", FolderError::Relative),
            ("/var/tmp/../etc", FolderError::ParentDir),
            ("/var/tmp\u{0}notes", FolderError::Nul),
        ] {
            assert_eq!(normalize_recorded(path), Err(error), "{path:?}");
        }
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

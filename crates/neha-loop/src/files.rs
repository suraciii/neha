//! Moving Files between the Space and a Bot's Computer.
//!
//! A File belongs to a conversation and lives in the Space; a Bot takes one
//! into its Workspace or delivers one from it. The Space implements
//! [`FileHost`]; the loop only needs these two moves.

use neha::id::Id;

/// A File the loop fetched from the Space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TakenFile {
    /// The File's name, which is also its path in the Workspace.
    pub name: String,
    /// The File's media type.
    pub media_type: String,
    /// The File's bytes.
    pub bytes: Vec<u8>,
}

/// A File the loop delivered to the Space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveredFile {
    /// The File's identifier.
    pub id: Id,
    /// The File's name.
    pub name: String,
}

/// What a run needs from the Space to move Files in and out of the Computer.
pub trait FileHost {
    /// Reads one File of the run's conversation.
    fn take(&self, file: Id) -> impl std::future::Future<Output = Result<TakenFile, String>> + Send;

    /// Delivers bytes as a File of the run's conversation.
    fn put(
        &self,
        name: String,
        bytes: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<DeliveredFile, String>> + Send;
}

/// Returns the name a File takes in the Workspace.
///
/// A name that would escape the Workspace falls back to the File's identifier,
/// so a delivered name is never a path.
#[must_use]
pub fn workspace_name(name: &str, fallback: &Id) -> String {
    let name = name.trim();
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains('/')
        && !name.contains('\\')
        && !name.contains('\0');
    if safe {
        name.to_owned()
    } else {
        fallback.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::workspace_name;
    use neha::id::Id;

    #[test]
    fn a_name_is_kept_when_it_cannot_escape_the_workspace() {
        let id = Id::new();
        assert_eq!(workspace_name("report.txt", &id), "report.txt");
        assert_eq!(workspace_name("a b.md", &id), "a b.md");
    }

    #[test]
    fn an_escaping_name_falls_back_to_the_identifier() {
        let id = Id::new();
        for name in ["../secret", "a/b", "a\\b", "", ".", ".."] {
            assert_eq!(workspace_name(name, &id), id.to_string());
        }
    }
}

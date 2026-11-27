//! Agent Plugins packages.
//!
//! A package is data: a `plugin.json` manifest, skills under `skills/`, and
//! MCP servers in `mcp.json`. This module reads a package without running
//! anything from it, validates it against the Agent Plugins 1.0.0 rules, and
//! returns what the Space may register.
//!
//! Two readers produce the same thing — a map of package-relative paths to
//! their bytes — so one validator serves every source: [`fetch_archive`] reads
//! the tarball of a repository pinned to a commit, and
//! [`read_authorized_directory`] reads a directory the Space is allowed to
//! install from. [`extract_archive`] unpacks bytes a caller fetched itself. Each
//! applies [`MAX_PACKAGE_FILES`] regular files, [`MAX_PACKAGE_ENTRIES`]
//! entries, and [`MAX_PACKAGE_FILE_BYTES`] and [`MAX_PACKAGE_BYTES`] while it
//! reads, so a package cannot cost more than [`MAX_PACKAGE_BYTES`] however it
//! arrives and however it compresses.
//!
//! [`authorize_source`] decides which source the Space may install from: a
//! directory source must resolve to an allowed directory or below it, and a
//! repository source must share scheme, host, and port with an allowed URL and
//! name a path at or below that URL's. [`fetch_archive`] applies the same rule
//! to the archive address and to every redirect before following it.
//!
//! On Linux, [`read_authorized_directory`] binds the bytes it reads to the
//! object the authorization resolved: it opens the canonical root once as a
//! directory, without following a final symbolic link and without waiting on
//! one, and then walks it through the opened descriptor — `/proc/self/fd` —
//! opening every child relative to its parent's descriptor with `O_NOFOLLOW`
//! (and `O_DIRECTORY` for a directory). A path component swapped after the
//! check is therefore not followed, and a FIFO at the root or in place of a
//! file is refused instead of waited on. The root-identity check reads the
//! opened descriptor back to the path of the object it holds, which `/proc`
//! provides; macOS names descriptors under `/dev/fd` but devfs answers no
//! `readlink` for those entries, so the check cannot be made there. A
//! directory source is therefore supported on Linux only: every other
//! platform, macOS among them, refuses one with [`PluginError::Read`] before
//! opening anything, and a repository source installs the same package.
//!
//! [`validate`] reads that map against the Agent Plugins 1.0.0 specification at
//! <https://agent-plugins.org/>. The manifest is fatal: a `plugin.json` that
//! breaks the closed schema of §5 rejects the whole package. Whatever the
//! specification isolates to one component — a `mcp.json` that breaks its
//! top-level rules, an invalid skill, an invalid or unsupported MCP server
//! entry — is reported in [`Package::skipped`] and the rest of the package
//! still loads, as §7.2.2 and §11.3 require.
//!
//! Two readings of the specification are narrowed here:
//!
//! - A repository source names one git commit object id, never a branch or a
//!   tag: what a package holds may not change under the digest the record
//!   keeps. A branch or tag name is refused when the source is parsed.
//! - §4.1 allows a symlink that resolves inside the plugin root. A symlink in a
//!   package directory is reported in [`Package::skipped`] and not followed
//!   instead: what it resolves to is not package data, and a package is
//!   inspectable without following one.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt as _;
use neha::record::{MAX_SKILL_BODY_BYTES, valid_script_name};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// The most regular files one package carries.
pub const MAX_PACKAGE_FILES: usize = 200;
/// The most entries one package tree may hold: every file, directory, symlink,
/// and skipped entry counts toward it.
pub const MAX_PACKAGE_ENTRIES: usize = 1024;
/// The most bytes one file in a package carries.
pub const MAX_PACKAGE_FILE_BYTES: u64 = 100 * 1024;
/// The most bytes one package carries.
pub const MAX_PACKAGE_BYTES: u64 = 4 * 1024 * 1024;
/// The Agent Plugins version this Space reads.
pub const PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
/// The MCP configuration schema of the Agent Plugins version this Space reads.
pub const MCP_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/mcp.schema.json";

/// The most redirects one archive fetch follows.
const MAX_REDIRECTS: usize = 3;
/// How long one archive fetch may take to connect.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one archive fetch may take in total.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The most scripts one skill carries.
const MAX_SKILL_SCRIPTS: usize = 64;
/// The characters a commit object id holds: git's SHA-1 and SHA-256 hex.
const COMMIT_LENGTHS: [usize; 2] = [40, 64];
/// The fields `plugin.json` may carry: the closed schema of §5.2.
const MANIFEST_FIELDS: [&str; 10] = [
    "$schema",
    "name",
    "version",
    "description",
    "author",
    "homepage",
    "repository",
    "license",
    "keywords",
    "extensions",
];
/// The fields a Streamable HTTP server entry may carry: §7.2.1.
const HTTP_SERVER_FIELDS: [&str; 3] = ["type", "url", "headers"];

/// One install source: a repository pinned to a commit, or a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A repository read at one commit.
    Repository {
        /// The repository URL, without a trailing slash.
        url: String,
        /// The commit the package is read at.
        commit: String,
    },
    /// A directory that already holds the package.
    Directory {
        /// The plugin root.
        path: PathBuf,
    },
}

/// What a package holds, read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The plugin name, as `plugin.json` spells it.
    pub name: String,
    /// The `version` field, when the manifest carries one.
    pub version: Option<String>,
    /// The `description` field, when the manifest carries one.
    pub description: Option<String>,
}

/// One skill of a package, as `skills/` holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageSkill {
    /// The directory the skill was read from, one component of `skills/`.
    pub directory: String,
    /// The `name` field of its frontmatter.
    pub name: String,
    /// The `description` field of its frontmatter.
    pub description: String,
    /// The body of its `SKILL.md`, without the frontmatter.
    pub body: String,
    /// The scripts it carries under `scripts/`.
    pub scripts: Vec<PackageScript>,
}

/// One script of a skill.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageScript {
    /// The file name within the skill, one path component.
    pub name: String,
    /// The bytes of the script.
    pub content: String,
}

/// One MCP server a package configures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageServer {
    /// The key the server is configured under in `mcp.json`.
    pub key: String,
    /// The name the Space registers the server under.
    pub name: String,
    /// The server's endpoint.
    pub url: String,
    /// The headers the package configures, by name.
    pub headers: BTreeMap<String, String>,
}

/// One package, read and validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    /// The manifest at the plugin root.
    pub manifest: Manifest,
    /// The skills the package carries, by directory.
    pub skills: Vec<PackageSkill>,
    /// The MCP servers the package configures, by key.
    pub servers: Vec<PackageServer>,
    /// What was reported and ignored while loading, one line each.
    pub skipped: Vec<String>,
}

/// Why a package cannot be read.
///
/// A message names the cause and never the contents of a package file: a
/// failure is shown to whoever installed the plugin, and a skill body or a
/// script is not theirs to read from an error. A message never repeats a URL
/// either, because a URL may carry user information the reader must not see.
#[derive(Debug, Error)]
pub enum PluginError {
    /// The install source is neither a repository pinned to a commit nor a
    /// directory.
    #[error("an install source is a repository pinned to a commit, or a directory: {0}")]
    InvalidSource(String),
    /// The source is a directory, which no archive fetch reads.
    #[error("a directory source is read from disk, not fetched")]
    NotFetched,
    /// The source is not one the Admin allowed.
    #[error("the package source is not one the Admin allows: {0}")]
    NotAllowed(String),
    /// The archive could not be fetched.
    #[error("the package archive could not be fetched: {0}")]
    Fetch(String),
    /// A package file could not be read from disk.
    #[error("a package file could not be read: {0}")]
    Read(String),
    /// A package exceeds one of the size limits.
    #[error("a package is too large: {0}")]
    TooLarge(String),
    /// The bytes are not a readable gzipped tarball.
    #[error("the package archive is not a gzipped tarball: {0}")]
    Archive(String),
    /// A package path leaves the plugin root.
    #[error("a package path leaves the plugin root: {0}")]
    Escape(String),
    /// The manifest breaks the closed `plugin.json` schema.
    #[error("the plugin manifest is invalid: {0}")]
    Manifest(String),
}

/// Parses one install source.
///
/// A repository source is `<url>@<commit>`: the URL is an absolute
/// `https://` (or `http://`) address with no user information, query string,
/// or fragment, and the commit is a git object id — 40 lowercase hexadecimal
/// characters (SHA-1) or 64 (SHA-256). A branch or tag name is not a commit,
/// and is refused: what the record pins is a commit, so what the package holds
/// cannot change under it. Anything else that holds no `://` is a directory
/// path, read as it is written.
///
/// # Errors
///
/// Returns [`PluginError::InvalidSource`] when the text names neither, or when
/// the repository URL or the commit does not meet those rules.
pub fn parse_source(raw: &str) -> Result<Source, PluginError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(PluginError::InvalidSource(
            "a source is not empty".to_owned(),
        ));
    }
    if !raw.contains("://") {
        return Ok(Source::Directory {
            path: PathBuf::from(raw),
        });
    }

    if raw.chars().any(char::is_whitespace) {
        return Err(PluginError::InvalidSource(
            "a source holds no whitespace".to_owned(),
        ));
    }
    let Some((url, commit)) = raw.rsplit_once('@') else {
        return Err(PluginError::InvalidSource(
            "a repository source is <url>@<commit>, and names a commit".to_owned(),
        ));
    };
    if url.is_empty() || commit.is_empty() {
        return Err(PluginError::InvalidSource(
            "a repository source is <url>@<commit>, and names both".to_owned(),
        ));
    }
    if !valid_commit(commit) {
        return Err(PluginError::InvalidSource(
            "a repository source names a commit object id: 40 or 64 lowercase hexadecimal \
             characters"
                .to_owned(),
        ));
    }
    let url = url.trim_end_matches('/');
    let address = reqwest::Url::parse(url).ok().filter(|address| {
        address.fragment().is_none() && address.query().is_none() && is_absolute_address(address)
    });
    let Some(address) = address else {
        return Err(PluginError::InvalidSource(
            "a repository URL is an absolute https:// or http:// address with no credentials, \
             query, or fragment"
                .to_owned(),
        ));
    };

    Ok(Source::Repository {
        url: address.to_string().trim_end_matches('/').to_owned(),
        commit: commit.to_owned(),
    })
}

/// Whether the text is a git commit object id: a SHA-1 or SHA-256 in hex.
fn valid_commit(commit: &str) -> bool {
    COMMIT_LENGTHS.contains(&commit.len())
        && commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether the address is an absolute http(s) address with no credentials.
fn is_absolute_address(address: &reqwest::Url) -> bool {
    matches!(address.scheme(), "http" | "https")
        && address.has_host()
        && address.username().is_empty()
        && address.password().is_none()
}

/// Returns whether the Admin allows a package to be installed from a source.
///
/// A directory source must resolve — after symlinks and `.`/`..` components —
/// to one of the allowed directories or to a path below one. A missing allowed
/// entry is skipped and the other entries may still match. A repository source
/// must share scheme, host, and port with an allowed URL, and name a path at or
/// below that URL's path on a path-component boundary; a trailing `/` names the
/// same prefix. Both sides reject user information, query strings, and
/// fragments.
///
/// # Errors
///
/// Returns [`PluginError::NotAllowed`] when no allowed entry covers the source,
/// and [`PluginError::Read`] when a directory source cannot be resolved.
pub fn authorize_source(source: &Source, allowed: &[String]) -> Result<(), PluginError> {
    match source {
        Source::Directory { path } => {
            let resolved = resolve_directory(path)?;
            authorize_resolved_directory(&resolved, allowed)
        }
        Source::Repository { url, .. } => {
            let address = parse_repository_url(url).ok_or_else(|| {
                PluginError::NotAllowed(
                    "a repository source is an absolute https address, or http on a loopback \
                     address, with no user information, query, or fragment"
                        .to_owned(),
                )
            })?;
            if allowed
                .iter()
                .filter_map(|entry| parse_repository_url(entry))
                .any(|prefix| names_source_below(&address, &prefix))
            {
                Ok(())
            } else {
                Err(PluginError::NotAllowed(
                    "a repository source must be at or below an allowed URL".to_owned(),
                ))
            }
        }
    }
}

/// Resolves a directory source to the path of the filesystem object it names.
fn resolve_directory(path: &Path) -> Result<PathBuf, PluginError> {
    path.canonicalize()
        .map_err(|error| PluginError::Read(format!("{}: {error}", path.display())))
}

/// Returns whether an already resolved directory is allowed by `allowed`.
fn authorize_resolved_directory(resolved: &Path, allowed: &[String]) -> Result<(), PluginError> {
    let matched = allowed
        .iter()
        .filter(|entry| !entry.contains("://"))
        .filter_map(|entry| Path::new(entry).canonicalize().ok())
        .any(|prefix| resolved == prefix || resolved.starts_with(&prefix));
    if matched {
        Ok(())
    } else {
        Err(PluginError::NotAllowed(
            "a directory source must be an allowed directory or a path below one".to_owned(),
        ))
    }
}

/// Returns the tarball address a repository source names.
///
/// # Errors
///
/// Returns [`PluginError::NotFetched`] for a directory source, and
/// [`PluginError::InvalidSource`] or [`PluginError::Fetch`] when the source's
/// URL or commit is not one this module fetches.
pub fn repository_archive_url(source: &Source) -> Result<reqwest::Url, PluginError> {
    let Source::Repository { url, commit } = source else {
        return Err(PluginError::NotFetched);
    };
    if !valid_commit(commit) {
        return Err(PluginError::InvalidSource(
            "a repository source names a commit object id: 40 or 64 lowercase hexadecimal \
             characters"
                .to_owned(),
        ));
    }
    let mut address = reqwest::Url::parse(url.trim_end_matches('/')).map_err(|_| {
        PluginError::Fetch("the package archive address could not be built".to_owned())
    })?;
    // The archive path is built from the URL's parsed segments, never by
    // appending text to a serialized URL: a query, a fragment, or a path
    // segment a serialization would place elsewhere cannot move the commit out
    // of the archive path's last segment.
    let mut path = address.path().trim_end_matches('/').to_owned();
    path.push_str("/archive/");
    path.push_str(commit);
    path.push_str(".tar.gz");
    address.set_path(&path);
    address.set_query(None);
    address.set_fragment(None);

    Ok(address)
}

/// Fetches the tarball a repository source names.
///
/// The source, the archive address, and every redirect on the way must be an
/// address the Admin allows (see [`authorize_source`]) and one the Space
/// fetches from — HTTPS anywhere, or HTTP on a loopback address, with no user
/// information, query string, or fragment. At most three redirects are
/// followed, and a non-2xx answer or a body past [`MAX_PACKAGE_BYTES`] refuses
/// the fetch. A [`Source::Directory`] is not fetched;
/// [`read_authorized_directory`] is the caller's path for it.
///
/// # Errors
///
/// Returns [`PluginError::NotFetched`] for a directory source,
/// [`PluginError::NotAllowed`] for a source or redirect the Admin does not
/// allow, and [`PluginError::Fetch`] or [`PluginError::TooLarge`] for a fetch
/// the Space refuses.
pub async fn fetch_archive(source: &Source, allowed: &[String]) -> Result<Vec<u8>, PluginError> {
    fetch_archive_within(source, allowed, REQUEST_TIMEOUT).await
}

/// Fetches one archive with a caller-chosen total timeout.
async fn fetch_archive_within(
    source: &Source,
    allowed: &[String],
    timeout: Duration,
) -> Result<Vec<u8>, PluginError> {
    let Source::Repository { .. } = source else {
        return Err(PluginError::NotFetched);
    };
    // The source the Admin allowed is checked before an address is built, so a
    // refused source costs no network request.
    authorize_source(source, allowed)?;
    let client = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        // A fetch is one answer from one server; a slow server cannot hold the
        // install open past this.
        .timeout(timeout)
        // Every redirect is checked against the address rule here, so the
        // client itself never follows one.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| {
            PluginError::Fetch("the package archive fetch client could not be built".to_owned())
        })?;
    let mut address = repository_archive_url(source)?;
    let mut redirects = 0_usize;
    loop {
        authorize_address(&address, allowed)?;
        require_addressable(&address)?;
        let response = client
            .get(address.clone())
            .send()
            .await
            .map_err(fetch_error)?;
        if response.status().is_success() {
            return read_body(response).await;
        }
        let Some(location) = redirect(&response) else {
            return Err(PluginError::Fetch(format!(
                "the package archive answered {}",
                response.status()
            )));
        };
        redirects = redirects.saturating_add(1);
        if redirects > MAX_REDIRECTS {
            return Err(PluginError::Fetch(format!(
                "the package archive redirected more than {MAX_REDIRECTS} times"
            )));
        }
        // The redirect is checked before it is followed: the initial address
        // and every `Location` answer the same rule.
        let next = response.url().join(location).map_err(|_| {
            PluginError::Fetch("a package archive redirect is not a URL".to_owned())
        })?;
        authorize_address(&next, allowed)?;
        require_addressable(&next)?;
        address = next;
    }
}

/// Returns whether one archive address is at or below an allowed URL.
fn authorize_address(address: &reqwest::Url, allowed: &[String]) -> Result<(), PluginError> {
    if allowed
        .iter()
        .filter_map(|entry| parse_repository_url(entry))
        .any(|prefix| names_source_below(address, &prefix))
    {
        Ok(())
    } else {
        Err(PluginError::NotAllowed(
            "a package archive must be at or below an allowed URL".to_owned(),
        ))
    }
}

/// Parses one allowed repository URL, when it is one this Space reads.
fn parse_repository_url(text: &str) -> Option<reqwest::Url> {
    let address = reqwest::Url::parse(text.trim_end_matches('/')).ok()?;
    (is_absolute_address(&address) && addressable(&address)).then_some(address)
}

/// Whether the address names a path at or below the prefix's path.
///
/// The comparison is on a path-component boundary: `/acme` covers
/// `/acme/reports`, not `/acmex`.
fn names_source_below(address: &reqwest::Url, prefix: &reqwest::Url) -> bool {
    address.scheme() == prefix.scheme()
        && address.host_str() == prefix.host_str()
        && address.port_or_known_default() == prefix.port_or_known_default()
        && path_at_or_below(address.path(), prefix.path())
}

/// Whether one URL path names the prefix path or a path below it.
fn path_at_or_below(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return true;
    }

    path == prefix
        || (path.len() > prefix.len()
            && path.starts_with(prefix)
            && path.as_bytes()[prefix.len()] == b'/')
}

/// Reads a directory the Space is allowed to install from into a package file
/// map.
///
/// The source is authorized first: its canonical root must be one of `allowed`
/// or below one. On Linux the authorized root is then opened once as a
/// directory — without following a final symbolic link, and without waiting on
/// one — and walked through its own descriptor: every child is opened relative
/// to the parent's descriptor (`/proc/self/fd`) with `O_NOFOLLOW`, with
/// `O_DIRECTORY` for a directory, and with `O_NONBLOCK`, so the bytes that are
/// read are the bytes the authorization decided about and a FIFO is refused
/// rather than waited on. On every other platform, macOS among them, a
/// directory source is refused with [`PluginError::Read`] before anything is
/// opened: a package directory is supported only on Linux, and a repository
/// source installs the same package.
///
/// The map holds one entry per regular file, keyed by its path relative to
/// `path`, with `/` separators on every platform. A symlink or another entry
/// that is not a regular file is reported in `skipped` and not followed. One
/// package carries at most [`MAX_PACKAGE_FILES`] regular files and
/// [`MAX_PACKAGE_ENTRIES`] entries: directories, symlinks, and skipped entries
/// count as entries, so a tree of empty entries is bounded too. A file is
/// opened once, its opened size checked before any byte is read, and read
/// through a bounded reader, so a file cannot cost more than
/// [`MAX_PACKAGE_FILE_BYTES`]. The same limits [`fetch_archive`] applies apply
/// here.
///
/// # Errors
///
/// Returns [`PluginError::NotAllowed`] when the directory is not one the Admin
/// allows, [`PluginError::Read`] when the directory or one of its files cannot
/// be read or the platform does not support a directory source,
/// [`PluginError::Escape`] for a path that resolves outside the root, and
/// [`PluginError::TooLarge`] past a limit.
pub fn read_authorized_directory(
    path: &Path,
    allowed: &[String],
    skipped: &mut Vec<String>,
) -> Result<BTreeMap<String, Vec<u8>>, PluginError> {
    let root = resolve_directory(path)?;
    authorize_resolved_directory(&root, allowed)?;

    read_resolved_root(&root, skipped)
}

/// Reads a directory whose containment the caller established itself.
///
/// This reader is for tests and exists only there: it is not the install path,
/// and a build that installs from a source does not compile it. A caller that
/// installs from a source must call [`read_authorized_directory`] instead, so
/// the bytes that are read are the bytes the authorization decided about. Its
/// portable walk canonicalizes each candidate and refuses one that leaves the
/// root or is a symlink; it binds nothing to an opened descriptor, so a caller
/// that has not established containment itself cannot rely on it.
///
/// The map, the entries reported in `skipped`, and the limits are those of
/// [`read_authorized_directory`].
///
/// # Errors
///
/// Returns [`PluginError::Read`] when the directory or one of its files cannot
/// be read, [`PluginError::Escape`] for a path that resolves outside the root,
/// and [`PluginError::TooLarge`] past a limit.
#[cfg(test)]
pub fn read_directory(
    path: &Path,
    skipped: &mut Vec<String>,
) -> Result<BTreeMap<String, Vec<u8>>, PluginError> {
    let root = resolve_directory(path)?;

    read_portable_root(&root, skipped)
}

/// One directory level of the descriptor walk.
#[cfg(target_os = "linux")]
struct Level {
    /// The opened directory this level reads.
    directory: std::fs::File,
    /// The level's path relative to the package root.
    prefix: String,
    /// Its entries, by name, read when the level was opened.
    names: std::vec::IntoIter<std::ffi::OsString>,
}

/// Reads one canonical package root, binding every level to an opened
/// directory descriptor.
///
/// The root is opened once. Every child is opened relative to its parent's
/// descriptor with `O_NOFOLLOW` (and `O_DIRECTORY` for a directory), so no
/// component is resolved by a pathname that could be replaced after the
/// authorization check.
#[cfg(target_os = "linux")]
fn read_resolved_root(
    root: &Path,
    skipped: &mut Vec<String>,
) -> Result<BTreeMap<String, Vec<u8>>, PluginError> {
    let root_directory = open_root_directory(root)?;
    let root_names = list_names(
        &root_directory,
        &root.display().to_string(),
        MAX_PACKAGE_ENTRIES,
    )?;
    let mut levels = vec![Level {
        directory: root_directory,
        prefix: String::new(),
        names: root_names.into_iter(),
    }];
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut total = 0_u64;
    let mut entries = 0_usize;
    let mut regular_files = 0_usize;
    // A stack rather than recursion: the depth of a directory tree the caller
    // points at is not this module's to spend its own stack on, and a level's
    // descriptor closes as soon as its last entry is read.
    while let Some(level) = levels.last_mut() {
        let Some(name) = level.names.next() else {
            levels.pop();
            continue;
        };
        let relative = child_relative(&level.prefix, &name)?;
        count_entry(&mut entries)?;
        // The metadata is read through the parent's descriptor with its final
        // component not followed, so a symlink is seen here before anything
        // would follow it.
        let kind = entry_metadata(&level.directory, &name, &relative)?.file_type();
        if kind.is_symlink() {
            skipped.push(format!("{relative}: a symbolic link, skipped"));
            continue;
        }
        if kind.is_dir() {
            let directory = open_child_directory(&level.directory, &name, &relative)?;
            let names = list_names(
                &directory,
                &relative,
                MAX_PACKAGE_ENTRIES.saturating_sub(entries),
            )?;
            levels.push(Level {
                directory,
                prefix: relative,
                names: names.into_iter(),
            });
            continue;
        }
        if !kind.is_file() {
            skipped.push(format!(
                "{relative}: a package entry is not a regular file, skipped"
            ));
            continue;
        }
        let file = open_child_file(&level.directory, &name, &relative)?;
        let Some(bytes) = read_regular_file(file, &relative, total)? else {
            // What was opened is no longer a regular file: the entry changed
            // between its metadata check and its open, and is not package data.
            skipped.push(format!(
                "{relative}: a package entry changed while it was read, skipped"
            ));
            continue;
        };
        count_regular_file(&mut regular_files)?;
        insert(&mut files, &mut total, relative, bytes)?;
    }

    Ok(files)
}

/// Returns the path that names an opened descriptor.
///
/// `/proc/self/fd` names an opened descriptor under a path that resolves
/// through the descriptor itself, so a child opened through that path is opened
/// relative to the object its parent descriptor holds rather than by a
/// pathname lookup that a rename could redirect. The same path read back with
/// `read_link` names the path of the object the descriptor holds, which is how
/// the opened root is checked against the object the authorization resolved.
/// That readback is what a directory source needs, and it is why one is
/// supported on Linux only.
#[cfg(target_os = "linux")]
fn descriptor_path(directory: &std::fs::File) -> PathBuf {
    use std::os::fd::AsRawFd as _;

    let descriptor = directory.as_raw_fd();
    PathBuf::from(format!("/proc/self/fd/{descriptor}"))
}

/// Opens the package root once and checks it is the resolved directory.
///
/// The open itself enforces the type: `O_DIRECTORY` refuses anything that is
/// not a directory, `O_NOFOLLOW` refuses a symbolic link swapped in for the
/// final component, and `O_NONBLOCK` means a FIFO is refused instead of being
/// waited on. The opened descriptor is then checked to name the directory the
/// authorization resolved, so a root renamed between that check and this open
/// is refused too.
#[cfg(target_os = "linux")]
fn open_root_directory(root: &Path) -> Result<std::fs::File, PluginError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let shown = root.display();
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(root)
        .map_err(|error| PluginError::Read(format!("{shown}: {error}")))?;
    // The flag already refuses a non-directory; the opened handle is checked
    // too, so the type is decided from the object that was opened, not a path.
    let metadata = directory
        .metadata()
        .map_err(|error| PluginError::Read(format!("{shown}: {error}")))?;
    if !metadata.is_dir() {
        return Err(PluginError::Read(format!("{shown} is not a directory")));
    }
    // The descriptor names the object the authorization resolved: a root path
    // swapped after that check opens a different object, which is refused.
    // A descriptor path answers the opened object's current path, so a root
    // renamed between the check and the open is refused too.
    let opened = std::fs::read_link(descriptor_path(&directory))
        .and_then(|path| path.canonicalize())
        .map_err(|error| PluginError::Read(format!("{shown}: {error}")))?;
    if opened != root {
        return Err(PluginError::Read(format!(
            "{shown} changed while it was opened"
        )));
    }

    Ok(directory)
}

/// Reads one opened directory's entry names, without following any entry.
///
/// At most `remaining` names are returned: a directory with more entries than
/// a package may still hold is refused before every one is held in memory.
#[cfg(target_os = "linux")]
fn list_names(
    directory: &std::fs::File,
    shown: &str,
    remaining: usize,
) -> Result<Vec<std::ffi::OsString>, PluginError> {
    let listing = std::fs::read_dir(descriptor_path(directory))
        .map_err(|error| PluginError::Read(format!("{shown}: {error}")))?;
    let mut names = Vec::new();
    for entry in listing {
        let entry = entry.map_err(|error| PluginError::Read(format!("{shown}: {error}")))?;
        names.push(entry.file_name());
        if names.len() > remaining {
            return Err(PluginError::TooLarge(format!(
                "a package carries more than {MAX_PACKAGE_ENTRIES} entries"
            )));
        }
    }
    // Sorted, so a package reports its skipped entries in one order.
    names.sort();

    Ok(names)
}

/// Reads one child's metadata without following it.
#[cfg(target_os = "linux")]
fn entry_metadata(
    directory: &std::fs::File,
    name: &std::ffi::OsStr,
    relative: &str,
) -> Result<std::fs::Metadata, PluginError> {
    std::fs::symlink_metadata(descriptor_path(directory).join(name))
        .map_err(|error| PluginError::Read(format!("{relative}: {error}")))
}

/// Opens one child of an opened directory as a file, never following a link.
#[cfg(target_os = "linux")]
fn open_child_file(
    directory: &std::fs::File,
    name: &std::ffi::OsStr,
    relative: &str,
) -> Result<std::fs::File, PluginError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        // The child is opened relative to the parent's descriptor, and
        // `O_NOFOLLOW` refuses a symlink swapped in after the metadata check.
        // `O_NONBLOCK` refuses to wait on a FIFO swapped in the same way; the
        // opened handle's metadata is checked before a byte is read.
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(descriptor_path(directory).join(name))
        .map_err(|error| PluginError::Read(format!("{relative}: {error}")))
}

/// Opens one child directory of an opened directory, never following a link.
#[cfg(target_os = "linux")]
fn open_child_directory(
    directory: &std::fs::File,
    name: &std::ffi::OsStr,
    relative: &str,
) -> Result<std::fs::File, PluginError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY | libc::O_NONBLOCK)
        .open(descriptor_path(directory).join(name))
        .map_err(|error| PluginError::Read(format!("{relative}: {error}")))
}

/// Refuses a directory source on every platform that is not Linux.
///
/// The install path never falls back to canonicalizing each candidate and then
/// opening it by pathname: that window between the containment check and the
/// open is exactly what a directory source off Linux would leave open, and a
/// package archive has no such window. Nothing is opened here; a repository
/// source installs the same package.
#[cfg(not(target_os = "linux"))]
fn read_resolved_root(
    _root: &Path,
    _skipped: &mut Vec<String>,
) -> Result<BTreeMap<String, Vec<u8>>, PluginError> {
    Err(PluginError::Read(
        "a package directory is supported only on Linux: use a repository source instead"
            .to_owned(),
    ))
}

/// Reads one canonical package root by pathname.
///
/// This is the test reader's portable walk, not the install path: a caller that
/// has not established containment must use [`read_authorized_directory`]
/// instead. Each candidate is canonicalized before it is opened and refused
/// when it leaves the root or is a symlink; that narrows the window between the
/// check and the open but cannot eliminate it.
#[cfg(test)]
fn read_portable_root(
    root: &Path,
    skipped: &mut Vec<String>,
) -> Result<BTreeMap<String, Vec<u8>>, PluginError> {
    if !root.is_dir() {
        return Err(PluginError::Read(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut total = 0_u64;
    let mut entries = 0_usize;
    let mut regular_files = 0_usize;
    // A stack rather than recursion: the depth of a directory tree the caller
    // points at is not this module's to spend its own stack on.
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        let listing = std::fs::read_dir(&directory)
            .map_err(|error| PluginError::Read(format!("{}: {error}", directory.display())))?;
        for entry in listing {
            let entry = entry
                .map_err(|error| PluginError::Read(format!("{}: {error}", directory.display())))?;
            let raw = entry.path();
            let relative = relative_path(root, &raw)?;
            count_entry(&mut entries)?;
            let kind = entry
                .file_type()
                .map_err(|error| PluginError::Read(format!("{relative}: {error}")))?;
            // `file_type` reads the link itself, so a symlink is seen here
            // before anything follows it.
            if kind.is_symlink() {
                skipped.push(format!("{relative}: a symbolic link, skipped"));
                continue;
            }
            if kind.is_dir() {
                ensure_within(root, &raw, &relative)?;
                directories.push(raw);
                continue;
            }
            if !kind.is_file() {
                skipped.push(format!(
                    "{relative}: a package entry is not a regular file, skipped"
                ));
                continue;
            }
            ensure_within(root, &raw, &relative)?;
            let file = std::fs::File::open(&raw)
                .map_err(|error| PluginError::Read(format!("{relative}: {error}")))?;
            let Some(bytes) = read_regular_file(file, &relative, total)? else {
                skipped.push(format!(
                    "{relative}: a package entry changed while it was read, skipped"
                ));
                continue;
            };
            count_regular_file(&mut regular_files)?;
            insert(&mut files, &mut total, relative, bytes)?;
        }
    }

    Ok(files)
}

/// Joins one directory entry name to its level's package-relative path.
#[cfg(target_os = "linux")]
fn child_relative(prefix: &str, name: &std::ffi::OsStr) -> Result<String, PluginError> {
    let Some(name) = name.to_str() else {
        return Err(PluginError::Read("a package path is not text".to_owned()));
    };

    Ok(if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    })
}

/// Counts one walked entry toward [`MAX_PACKAGE_ENTRIES`].
fn count_entry(entries: &mut usize) -> Result<(), PluginError> {
    *entries = entries.saturating_add(1);
    if *entries > MAX_PACKAGE_ENTRIES {
        return Err(PluginError::TooLarge(format!(
            "a package carries more than {MAX_PACKAGE_ENTRIES} entries"
        )));
    }

    Ok(())
}

/// Counts one read regular file toward [`MAX_PACKAGE_FILES`].
fn count_regular_file(files: &mut usize) -> Result<(), PluginError> {
    *files = files.saturating_add(1);
    if *files > MAX_PACKAGE_FILES {
        return Err(PluginError::TooLarge(format!(
            "a package carries more than {MAX_PACKAGE_FILES} files"
        )));
    }

    Ok(())
}

/// Reads one opened file, whose opened metadata must be a regular file.
///
/// The size is checked on the opened handle before any byte is read, so an
/// oversized file is refused without being read whole, and the bytes are read
/// through a bounded reader from the same handle. `None` answers a handle that
/// is not a regular file: the entry changed between its metadata check and its
/// open, and what was opened is not package data.
#[cfg(any(test, target_os = "linux"))]
fn read_regular_file(
    file: std::fs::File,
    relative: &str,
    total: u64,
) -> Result<Option<Vec<u8>>, PluginError> {
    let metadata = file
        .metadata()
        .map_err(|error| PluginError::Read(format!("{relative}: {error}")))?;
    if !metadata.is_file() {
        return Ok(None);
    }
    let size = metadata.len();
    if size > MAX_PACKAGE_FILE_BYTES {
        return Err(PluginError::TooLarge(format!(
            "the package file `{relative}` is more than {MAX_PACKAGE_FILE_BYTES} bytes"
        )));
    }
    // Total bytes are counted before the bytes are read, so a package over its
    // budget is refused without being read whole.
    if total.saturating_add(size) > MAX_PACKAGE_BYTES {
        return Err(PluginError::TooLarge(format!(
            "a package is more than {MAX_PACKAGE_BYTES} bytes"
        )));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
    (&file)
        .take(MAX_PACKAGE_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| PluginError::Read(format!("{relative}: {error}")))?;
    if bytes.len() as u64 > MAX_PACKAGE_FILE_BYTES {
        return Err(PluginError::TooLarge(format!(
            "the package file `{relative}` is more than {MAX_PACKAGE_FILE_BYTES} bytes"
        )));
    }

    Ok(Some(bytes))
}

/// Returns one path relative to the package root, with `/` separators.
#[cfg(test)]
fn relative_path(root: &Path, path: &Path) -> Result<String, PluginError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| PluginError::Escape(format!("{} leaves the plugin root", path.display())))?;
    let mut parts = Vec::new();
    for component in relative.components() {
        let Some(part) = component.as_os_str().to_str() else {
            return Err(PluginError::Read("a package path is not text".to_owned()));
        };
        parts.push(part);
    }
    if parts.is_empty() {
        return Err(PluginError::Escape(
            "a package entry names no file".to_owned(),
        ));
    }

    Ok(parts.join("/"))
}

/// Refuses one path that resolves outside the package root.
#[cfg(test)]
fn ensure_within(root: &Path, path: &Path, relative: &str) -> Result<(), PluginError> {
    let resolved = path
        .canonicalize()
        .map_err(|error| PluginError::Read(format!("{relative}: {error}")))?;
    if !resolved.starts_with(root) {
        return Err(PluginError::Escape(format!(
            "{relative} leaves the plugin root"
        )));
    }

    Ok(())
}

/// Unpacks a `.tar.gz` into a package file map.
///
/// Regular files are read and counted toward [`MAX_PACKAGE_FILES`]; every
/// entry — a file, a directory, a symlink, a hard link, or a header-only
/// entry — is counted toward [`MAX_PACKAGE_ENTRIES`], and an entry that is not
/// a regular file is reported in `skipped`, because what it names is not
/// package data. The gzip stream is read through a cap of [`MAX_PACKAGE_BYTES`]
/// plus room for the headers of [`MAX_PACKAGE_ENTRIES`] entries, so a
/// decompression bomb is refused without being held in memory; every declared
/// entry size is checked before its bytes are read. When every file shares one
/// leading directory component it is stripped, which is how a repository
/// archive wraps its package. An absolute path or a `..` component is refused.
///
/// # Errors
///
/// Returns [`PluginError::Archive`] for bytes that are not a gzipped tarball,
/// [`PluginError::Escape`] for a path that leaves the root, and
/// [`PluginError::TooLarge`] past a limit.
pub fn extract_archive(
    bytes: &[u8],
    skipped: &mut Vec<String>,
) -> Result<BTreeMap<String, Vec<u8>>, PluginError> {
    // Each entry costs a tar header of at most a few blocks; the room is a
    // coarse bound, so a valid archive of many long names is not refused for
    // its headers alone.
    let overhead = (MAX_PACKAGE_ENTRIES as u64).saturating_mul(4096);
    let limit = MAX_PACKAGE_BYTES.saturating_add(overhead);
    let decoder = flate2::read::GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(CappedReader::new(decoder, limit));
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    let mut total = 0_u64;
    let mut entries_seen = 0_usize;
    let mut regular_files = 0_usize;
    let entries = archive.entries().map_err(archive_error)?;
    for entry in entries {
        let mut entry = entry.map_err(archive_error)?;
        // Every entry counts, including a directory, a link, and an entry that
        // carries no data: a tarball of empty entries still costs its headers.
        count_entry(&mut entries_seen)?;
        let kind = entry.header().entry_type();
        if kind.is_dir() {
            continue;
        }
        let named = entry.path().map_err(archive_error)?;
        let Some(name) = named.to_str() else {
            return Err(PluginError::Archive(
                "a package path is not text".to_owned(),
            ));
        };
        if !kind.is_file() {
            skipped.push(format!(
                "{name}: a package entry is not a regular file, skipped"
            ));
            continue;
        }
        count_regular_file(&mut regular_files)?;
        // The key is owned here, so the entry is free to be read from.
        let key = normalize(name)?;
        // A declared size past the limit is refused before any byte of it is
        // read, so an oversized entry is never held whole in memory.
        let size = entry.header().size().map_err(archive_error)?;
        if size > MAX_PACKAGE_FILE_BYTES {
            return Err(PluginError::TooLarge(format!(
                "the package file `{key}` is more than {MAX_PACKAGE_FILE_BYTES} bytes"
            )));
        }
        if total.saturating_add(size) > MAX_PACKAGE_BYTES {
            return Err(PluginError::TooLarge(format!(
                "a package is more than {MAX_PACKAGE_BYTES} bytes"
            )));
        }
        let mut entry_bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        (&mut entry)
            .take(MAX_PACKAGE_FILE_BYTES.saturating_add(1))
            .read_to_end(&mut entry_bytes)
            .map_err(archive_error)?;
        if entry_bytes.len() as u64 > MAX_PACKAGE_FILE_BYTES {
            return Err(PluginError::TooLarge(format!(
                "the package file `{key}` is more than {MAX_PACKAGE_FILE_BYTES} bytes"
            )));
        }
        insert(&mut files, &mut total, key, entry_bytes)?;
    }

    Ok(strip_root(files))
}

/// A reader that refuses to yield more bytes than its cap.
///
/// It is wrapped around a decompressor, so bytes that expand past the cap are
/// never produced. The reader probes one byte past the cap so a stream that
/// ends exactly at it is not refused.
struct CappedReader<R> {
    inner: R,
    remaining: u64,
}

/// The marker a capped read fails with.
#[derive(Debug)]
struct ArchiveTooLarge;

impl std::fmt::Display for ArchiveTooLarge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the package archive expands past its limit")
    }
}

impl std::error::Error for ArchiveTooLarge {}

impl<R> CappedReader<R> {
    /// Caps how many bytes `inner` may yield.
    fn new(inner: R, cap: u64) -> Self {
        Self {
            inner,
            remaining: cap,
        }
    }
}

impl<R: std::io::Read> std::io::Read for CappedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let mut probe = [0_u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(archive_limit_error()),
            };
        }
        let wanted = usize::try_from(self.remaining)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = self.inner.read(&mut buffer[..wanted])?;
        self.remaining = self.remaining.saturating_sub(read as u64);

        Ok(read)
    }
}

/// Returns the error a capped read fails with.
fn archive_limit_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, ArchiveTooLarge)
}

/// Maps one archive read error, naming a cap overflow as too large.
fn archive_error(error: std::io::Error) -> PluginError {
    if error
        .get_ref()
        .is_some_and(|cause| cause.is::<ArchiveTooLarge>())
    {
        return PluginError::TooLarge(
            "the package archive expands to more bytes than a package may hold".to_owned(),
        );
    }

    PluginError::Archive(error.to_string())
}

/// Returns the digest of a package.
///
/// The digest is the SHA-256, in hex, of every entry in sorted order: the path
/// bytes, a NUL, the length in decimal, a NUL, then the bytes. A `BTreeMap`
/// already iterates in sorted order, so two maps with the same entries answer
/// the same digest however they were built. One pass over one buffer: the
/// digest is never hashed twice.
#[must_use]
pub fn digest(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut hasher = Sha256::new();
    for (path, bytes) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(bytes.len().to_string().as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
    }

    hex(&hasher.finalize())
}

/// Returns bytes as lowercase hexadecimal text.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut text = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        // Writing to a `String` cannot fail.
        let _ = write!(text, "{byte:02x}");
    }

    text
}

/// Validates one package.
///
/// The manifest is read first: a `plugin.json` that breaks the closed schema of
/// §5 is fatal and the package is refused whole. Everything the specification
/// isolates to one component type is reported in [`Package::skipped`] and the
/// rest of the package still loads, as §7.2.2 and §11.3 require: a `mcp.json`
/// that breaks the top-level rules of §7.2.1 disables only MCP, and an invalid
/// skill, an invalid or unsupported MCP server entry, or a server name the
/// Space cannot register is skipped.
///
/// # Errors
///
/// Returns [`PluginError::Manifest`] for a `plugin.json` that does not conform.
pub fn validate(files: &BTreeMap<String, Vec<u8>>) -> Result<Package, PluginError> {
    let (manifest, mut skipped) = read_manifest(files)?;
    let skills = read_skills(files, &mut skipped);
    let servers = read_servers(files, &manifest.name, &mut skipped);

    Ok(Package {
        manifest,
        skills,
        servers,
        skipped,
    })
}

/// Reads `plugin.json`: the closed schema of §5, and the name rules of §5.5.
fn read_manifest(
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<(Manifest, Vec<String>), PluginError> {
    let invalid = |cause: String| PluginError::Manifest(format!("plugin.json: {cause}"));
    let Some(bytes) = files.get("plugin.json") else {
        return Err(invalid("a plugin package has no plugin.json".to_owned()));
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err(invalid("plugin.json is not text".to_owned()));
    };
    let document: serde_json::Value = serde_json::from_str(text)
        .map_err(|error| invalid(format!("plugin.json is not JSON: {error}")))?;
    let Some(fields) = document.as_object() else {
        return Err(invalid("plugin.json is not a JSON object".to_owned()));
    };
    let schema = fields.get("$schema").and_then(serde_json::Value::as_str);
    if schema != Some(PLUGIN_SCHEMA) {
        return Err(invalid(format!(
            "`$schema` must be {PLUGIN_SCHEMA}, which is the version this Space reads"
        )));
    }

    let mut skipped = Vec::new();
    for field in fields.keys() {
        // An unknown field is reported and ignored: the schema is closed, but
        // an otherwise valid package is not refused for one (§5.2).
        if !MANIFEST_FIELDS.contains(&field.as_str()) {
            skipped.push(format!("plugin.json: unknown field `{field}`"));
        }
    }
    // A non-object `extensions` field is reported and ignored (§8.1), and the
    // namespaces an object holds are not this client's to validate.
    if let Some(extensions) = fields.get("extensions")
        && !extensions.is_object()
    {
        skipped.push(
            "plugin.json: the `extensions` field is not an object, so it is ignored".to_owned(),
        );
    }

    let name = match fields.get("name") {
        Some(serde_json::Value::String(name)) if valid_plugin_name(name) => name.clone(),
        Some(serde_json::Value::String(name)) if name.is_empty() => {
            return Err(invalid("the `name` field is empty".to_owned()));
        }
        Some(serde_json::Value::String(_)) => {
            return Err(invalid(
                "the `name` field is not 1 to 64 lowercase letters, digits, `.`, or `-`, \
                 beginning and ending with one, with no `--` or `..`"
                    .to_owned(),
            ));
        }
        Some(_) => return Err(invalid("the `name` field is not text".to_owned())),
        None => return Err(invalid("the `name` field is required".to_owned())),
    };
    let version = text_field(fields, "version")
        .map_err(invalid)?
        .map(str::to_owned);
    let description = text_field(fields, "description")
        .map_err(invalid)?
        .map(str::to_owned);
    for field in ["homepage", "repository", "license"] {
        text_field(fields, field).map_err(invalid)?;
    }
    if let Some(author) = fields.get("author") {
        // The author object is closed too: `name`, `email`, and `url` of text,
        // and nothing else (§5.4).
        let Some(author) = author.as_object() else {
            return Err(invalid("the `author` field is not an object".to_owned()));
        };
        for (field, value) in author {
            if !matches!(field.as_str(), "name" | "email" | "url") || !value.is_string() {
                return Err(invalid(format!(
                    "the `author` field names no `{field}` of text"
                )));
            }
        }
    }
    if let Some(keywords) = fields.get("keywords") {
        let is_list = keywords
            .as_array()
            .is_some_and(|list| list.iter().all(serde_json::Value::is_string));
        if !is_list {
            return Err(invalid(
                "the `keywords` field is not a list of text".to_owned(),
            ));
        }
    }

    Ok((
        Manifest {
            name,
            version,
            description,
        },
        skipped,
    ))
}

/// Reads the skills under `skills/`: §7.1.
fn read_skills(files: &BTreeMap<String, Vec<u8>>, skipped: &mut Vec<String>) -> Vec<PackageSkill> {
    let mut skills = Vec::new();
    // A skill is an immediate child directory of `skills/` that holds a
    // `SKILL.md`; a deeper descendant is not searched for another (§7.1).
    let mut directories: BTreeSet<(&str, &[u8])> = BTreeSet::new();
    for (key, bytes) in files {
        let parts: Vec<&str> = key.split('/').collect();
        if parts.len() == 3 && parts[0] == "skills" && parts[2] == "SKILL.md" {
            directories.insert((parts[1], bytes));
        }
    }

    for (directory, bytes) in directories {
        let key = format!("skills/{directory}/SKILL.md");
        let Ok(text) = std::str::from_utf8(bytes) else {
            skipped.push(format!("{key}: a SKILL.md is not text"));
            continue;
        };
        if bytes.len() > MAX_SKILL_BODY_BYTES {
            skipped.push(format!(
                "{key}: a SKILL.md is at most {MAX_SKILL_BODY_BYTES} bytes"
            ));
            continue;
        }
        let document = match crate::skills::parse(text) {
            Ok(document) => document,
            Err(error) => {
                skipped.push(format!("{key}: {}", reason(&error)));
                continue;
            }
        };
        // §7.1 defers the skill format to the Agent Skills specification, whose
        // `name` rules bind the frontmatter to the directory that holds it.
        if !valid_skill_name(&document.name) {
            skipped.push(format!(
                "{key}: a skill name is 1 to 64 lowercase letters, digits, or hyphens, with no \
                 leading, trailing, or doubled hyphen"
            ));
            continue;
        }
        if document.name != directory {
            skipped.push(format!(
                "{key}: the `name` field does not match the skill directory `{directory}`"
            ));
            continue;
        }
        match read_scripts(files, directory) {
            Ok(scripts) => skills.push(PackageSkill {
                directory: directory.to_owned(),
                name: document.name,
                description: document.description,
                body: document.body,
                scripts,
            }),
            Err(reason) => skipped.push(format!("skills/{directory}: {reason}")),
        }
    }

    skills
}

/// Reads the scripts one skill carries under `scripts/`.
fn read_scripts(
    files: &BTreeMap<String, Vec<u8>>,
    directory: &str,
) -> Result<Vec<PackageScript>, String> {
    let prefix = format!("skills/{directory}/scripts/");
    let mut scripts = Vec::new();
    for (key, bytes) in files.range(prefix.clone()..) {
        if !key.starts_with(&prefix) {
            break;
        }
        let name = &key[prefix.len()..];
        // A script name is one path component of the skill, so a name that
        // holds a separator names something outside the skill's own scripts.
        if !valid_script_name(name) {
            return Err(format!(
                "the script `{name}` is not one path component of letters, digits, dots, \
                 hyphens, or underscores"
            ));
        }
        if scripts.len() >= MAX_SKILL_SCRIPTS {
            return Err(format!(
                "a skill carries at most {MAX_SKILL_SCRIPTS} scripts"
            ));
        }
        if bytes.len() > MAX_SKILL_BODY_BYTES {
            return Err(format!(
                "the script `{name}` is at most {MAX_SKILL_BODY_BYTES} bytes"
            ));
        }
        let Ok(content) = std::str::from_utf8(bytes) else {
            return Err(format!("the script `{name}` is not text"));
        };
        scripts.push(PackageScript {
            name: name.to_owned(),
            content: content.to_owned(),
        });
    }

    Ok(scripts)
}

/// Reads `mcp.json`: the closed schema of §7.2.1, one server entry at a time.
///
/// A `mcp.json` that breaks the top-level rules is reported in `skipped` and
/// disables only MCP, as §7.2.2 requires: the rest of the package still loads
/// and [`Package::servers`] stays empty.
fn read_servers(
    files: &BTreeMap<String, Vec<u8>>,
    plugin: &str,
    skipped: &mut Vec<String>,
) -> Vec<PackageServer> {
    match parse_servers(files, plugin, skipped) {
        Ok(servers) => servers,
        Err(reason) => {
            skipped.push(format!("mcp.json: {reason}"));
            Vec::new()
        }
    }
}

/// Parses `mcp.json`, or names why MCP is disabled.
fn parse_servers(
    files: &BTreeMap<String, Vec<u8>>,
    plugin: &str,
    skipped: &mut Vec<String>,
) -> Result<Vec<PackageServer>, String> {
    let Some(bytes) = files.get("mcp.json") else {
        // A missing fixed location is not an error (§6.2).
        return Ok(Vec::new());
    };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Err("it is not text".to_owned());
    };
    let document: serde_json::Value =
        serde_json::from_str(text).map_err(|error| format!("it is not JSON: {error}"))?;
    let Some(fields) = document.as_object() else {
        return Err("it is not a JSON object".to_owned());
    };
    let schema = fields.get("$schema").and_then(serde_json::Value::as_str);
    if schema != Some(MCP_SCHEMA) {
        // §10.1: the MCP configuration must target the version `plugin.json`
        // targets. `plugin.json` is already pinned to the one version this
        // Space reads, so any other value here is a mismatch.
        return Err(format!(
            "its `$schema` is not {MCP_SCHEMA}, the version `plugin.json` names"
        ));
    }
    for field in fields.keys() {
        if !matches!(field.as_str(), "$schema" | "mcpServers") {
            return Err(format!(
                "its top-level field `{field}` is not part of the MCP configuration"
            ));
        }
    }
    let Some(entries) = fields
        .get("mcpServers")
        .and_then(serde_json::Value::as_object)
    else {
        return Err("its `mcpServers` field is not an object of server entries".to_owned());
    };

    let mut servers = Vec::new();
    let mut names = HashSet::new();
    for (key, entry) in entries {
        let server = match read_server(entry) {
            Ok(server) => server,
            // One invalid entry skips one server and leaves the rest (§7.2.2).
            Err(reason) => {
                skipped.push(format!("mcp.json: the server `{key}` {reason}"));
                continue;
            }
        };
        // The name the Space registers a server under is the plugin's name and
        // the entry's key: periods are not registerable, so they become
        // hyphens.
        if !valid_server_key(key) {
            skipped.push(format!(
                "mcp.json: the server `{key}` is keyed by something other than letters, digits, \
                 hyphens, or underscores"
            ));
            continue;
        }
        let name = format!("{}-{}", plugin.replace('.', "-"), key);
        if !valid_server_name(&name) {
            // A name is never truncated to fit: a server the Space cannot
            // name is reported, not silently renamed.
            skipped.push(format!(
                "mcp.json: the server `{key}` is named `{name}`, which is not 1 to 32 letters, \
                 digits, or hyphens"
            ));
        } else if !names.insert(name.clone()) {
            skipped.push(format!(
                "mcp.json: the server `{key}` is named `{name}` twice"
            ));
        } else {
            servers.push(PackageServer {
                key: key.clone(),
                name,
                url: server.url,
                headers: server.headers,
            });
        }
    }

    Ok(servers)
}

/// One server entry that satisfies its variant of §7.2.1.
struct ServerEntry {
    url: String,
    headers: BTreeMap<String, String>,
}

/// Reads one server entry, or reports why it is invalid.
fn read_server(entry: &serde_json::Value) -> Result<ServerEntry, String> {
    let Some(fields) = entry.as_object() else {
        return Err("is not a JSON object".to_owned());
    };
    let transport = fields.get("type").and_then(serde_json::Value::as_str);
    let Some(transport) = transport else {
        return Err("names no `type`".to_owned());
    };
    // This Space connects to a server over Streamable HTTP and launches no
    // subprocess, so the other transports of §7.2.1 are unsupported here and
    // their entries are skipped rather than refused.
    if transport != "streamable-http" {
        return Err(match transport {
            "stdio" | "sse" => {
                format!("uses the {transport} transport, which this Space does not support")
            }
            other => format!("names the unknown transport `{other}`"),
        });
    }
    for field in fields.keys() {
        if !HTTP_SERVER_FIELDS.contains(&field.as_str()) {
            return Err(format!("names the unknown field `{field}`"));
        }
    }
    let Some(url) = fields.get("url").and_then(serde_json::Value::as_str) else {
        return Err("names no `url`".to_owned());
    };
    if !valid_server_url(url) {
        // The URL is not repeated: it may carry user information, which a
        // report shown to an admin must not print.
        return Err(
            "names a `url` that is not an absolute https address, or http on a loopback address, \
             and carries no user information, query string, or fragment"
                .to_owned(),
        );
    }
    let mut headers = BTreeMap::new();
    let mut seen = HashSet::new();
    match fields.get("headers") {
        None => {}
        Some(serde_json::Value::Object(configured)) => {
            for (name, value) in configured {
                let Some(value) = value.as_str() else {
                    return Err(format!("the header `{name}` is not text"));
                };
                // Header names are case-insensitive, so the same name under
                // two spellings is one header named twice.
                if !valid_header_name(name)
                    || !valid_header_value(name)
                    || !valid_header_value(value)
                    || !seen.insert(name.to_ascii_lowercase())
                {
                    return Err(format!(
                        "the header `{name}` is not a valid HTTP header field, or is named twice"
                    ));
                }
                headers.insert(name.clone(), value.to_owned());
            }
        }
        Some(_) => return Err("the `headers` field is not a JSON object".to_owned()),
    }

    Ok(ServerEntry {
        url: url.to_owned(),
        headers,
    })
}

/// Whether the name is one the plugin name rules of §5.5 allow.
fn valid_plugin_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.')
        })
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && !name.contains("--")
        && !name.contains("..")
}

/// Whether the name is one the Agent Skills specification allows.
///
/// The name is a skill's directory name: 1 to 64 lowercase letters, digits, or
/// hyphens, not beginning or ending with one and with no doubled hyphen.
fn valid_skill_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
        && !name.contains("--")
}

/// Whether the URL is one a server may be reached at: §7.2.1.
///
/// The package schema validation uses the same rule as the archive fetch: a URL
/// that carries a query string is refused, because a query may carry a
/// credential into the record (an endpoint configured as
/// `?access_token=…`), and a report must never preserve it.
fn valid_server_url(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|address| addressable(&address))
}

/// Whether one address is one the Space may call or fetch from: HTTPS anywhere,
/// or HTTP on a loopback address, with no user information, query string, or
/// fragment.
///
/// This is §7.2.1's transport rule for MCP endpoints, and the same rule admits
/// a package archive address: a bearer token never comes from a URL, and a
/// fragment names nothing a server answers at. A query string is refused too: a
/// URL a package or an Admin wrote may carry a credential in it, and neither a
/// skipped entry nor an error may echo one.
fn addressable(address: &reqwest::Url) -> bool {
    let transport = match address.scheme() {
        "https" => address.has_host(),
        "http" => is_loopback(address),
        _ => false,
    };

    transport
        && address.username().is_empty()
        && address.password().is_none()
        && address.query().is_none()
        && address.fragment().is_none()
}

/// Whether the address is on this machine, so HTTP needs no certificate.
fn is_loopback(address: &reqwest::Url) -> bool {
    match address.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback()),
        None => false,
    }
}

/// Whether the header name is an HTTP token: RFC 9110.
fn valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Whether the text holds no character that would end the header field.
fn valid_header_value(text: &str) -> bool {
    !text.chars().any(char::is_control)
}

/// Whether the key is one a server may be configured under.
fn valid_server_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Whether the name is one the Space registers a server under.
fn valid_server_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Returns why a document is not a skill, without quoting the document.
fn reason(error: &crate::skills::SkillError) -> String {
    match error {
        // A YAML parser quotes what it could not read, and the frontmatter is
        // not the reader's to see in a report.
        crate::skills::SkillError::Yaml(_) => "the frontmatter is not valid YAML".to_owned(),
        other => other.to_string(),
    }
}

/// Reads one optional text field of the manifest, refusing any other type.
fn text_field<'a>(
    fields: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Result<Option<&'a str>, String> {
    match fields.get(field) {
        None => Ok(None),
        Some(serde_json::Value::String(text)) => Ok(Some(text)),
        Some(_) => Err(format!("the `{field}` field is not text")),
    }
}

/// Refuses an address the Space does not fetch a package from: HTTPS anywhere,
/// or HTTP on a loopback address, with no user information, query string, or
/// fragment.
fn require_addressable(address: &reqwest::Url) -> Result<(), PluginError> {
    if addressable(address) {
        return Ok(());
    }

    Err(PluginError::Fetch(
        "a package archive address is https, or http on a loopback address, and carries no user \
         information, query string, or fragment"
            .to_owned(),
    ))
}

/// Returns why one request failed, without repeating the URL it named.
fn fetch_error(error: reqwest::Error) -> PluginError {
    let reason = if error.is_timeout() {
        "the request timed out"
    } else if error.is_connect() {
        "the connection failed"
    } else if error.is_body() || error.is_decode() {
        "the response body could not be read"
    } else if error.is_redirect() {
        "a redirect could not be followed"
    } else if error.is_builder() || error.is_request() {
        "the request could not be sent"
    } else {
        "the request failed"
    };

    PluginError::Fetch(reason.to_owned())
}

/// Returns where a redirect answer points, or `None` when it is not one.
fn redirect(response: &reqwest::Response) -> Option<&str> {
    if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }

    response
        .headers()
        .get(reqwest::header::LOCATION)?
        .to_str()
        .ok()
}

/// Reads a body as it arrives, refusing one past [`MAX_PACKAGE_BYTES`].
async fn read_body(response: reqwest::Response) -> Result<Vec<u8>, PluginError> {
    let mut bytes = Vec::new();
    let mut total = 0_u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(fetch_error)?;
        // The limit is applied to what has arrived, so a body larger than a
        // package may be is refused without ever being held whole.
        total = total.saturating_add(chunk.len() as u64);
        if total > MAX_PACKAGE_BYTES {
            return Err(PluginError::TooLarge(format!(
                "a package is more than {MAX_PACKAGE_BYTES} bytes"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }

    Ok(bytes)
}

/// Adds one read file to a package map, applying the byte limits.
///
/// What a package may hold is counted by its reader: a directory tree, a
/// tarball, and a fetched body are bounded by different walks.
fn insert(
    files: &mut BTreeMap<String, Vec<u8>>,
    total: &mut u64,
    path: String,
    bytes: Vec<u8>,
) -> Result<(), PluginError> {
    if bytes.len() as u64 > MAX_PACKAGE_FILE_BYTES {
        return Err(PluginError::TooLarge(format!(
            "one package file is more than {MAX_PACKAGE_FILE_BYTES} bytes"
        )));
    }
    *total = total.saturating_add(bytes.len() as u64);
    if *total > MAX_PACKAGE_BYTES {
        return Err(PluginError::TooLarge(format!(
            "a package is more than {MAX_PACKAGE_BYTES} bytes"
        )));
    }
    files.insert(path, bytes);

    Ok(())
}

/// Returns one package-relative path, or refuses a path that leaves the root.
///
/// A package path is relative to the plugin root: no component may be `..`,
/// and no component may hold a separator that another platform's filesystem
/// would read as one.
fn normalize(raw: &str) -> Result<String, PluginError> {
    let escape = |cause: &str| PluginError::Escape(format!("{raw} {cause}"));
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(escape("is not a relative path"));
    }
    let mut parts: Vec<&str> = Vec::new();
    for part in raw.split('/') {
        match part {
            "" | "." => {}
            ".." => return Err(escape("leaves the plugin root")),
            part if part.contains('\\') || part.contains('\0') => {
                return Err(escape("holds a separator inside a component"));
            }
            part => parts.push(part),
        }
    }
    if parts.is_empty() {
        return Err(escape("names no file"));
    }

    Ok(parts.join("/"))
}

/// Strips the one leading directory component every entry shares, which is how
/// a repository archive wraps its package.
fn strip_root(files: BTreeMap<String, Vec<u8>>) -> BTreeMap<String, Vec<u8>> {
    let Some(first) = files.keys().next() else {
        return files;
    };
    let Some((root, _)) = first.split_once('/') else {
        return files;
    };
    let shared = files
        .keys()
        .all(|key| key.split_once('/').is_some_and(|(head, _)| head == root));
    if !shared {
        return files;
    }

    files
        .into_iter()
        .map(|(key, bytes)| {
            (
                key.split_once('/')
                    .map_or(key.as_str(), |(_, rest)| rest)
                    .to_owned(),
                bytes,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    #[cfg(target_os = "linux")]
    use super::open_child_file;
    use super::{
        ArchiveTooLarge, CappedReader, MAX_PACKAGE_BYTES, MAX_PACKAGE_ENTRIES,
        MAX_PACKAGE_FILE_BYTES, MAX_PACKAGE_FILES, MCP_SCHEMA, PLUGIN_SCHEMA, PackageScript,
        PackageServer, PluginError, Source, authorize_source, digest, extract_archive,
        fetch_archive, fetch_archive_within, parse_source, read_authorized_directory,
        read_directory, repository_archive_url, validate,
    };

    /// One commit object id: a git SHA-1 in lowercase hexadecimal.
    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    /// Builds a package file map from `(path, text)` pairs.
    fn package(entries: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
        entries
            .iter()
            .map(|(path, text)| ((*path).to_owned(), text.as_bytes().to_vec()))
            .collect()
    }

    /// The `plugin.json` of a valid package.
    fn plugin_json(name: &str) -> String {
        format!("{{\"$schema\":\"{PLUGIN_SCHEMA}\",\"name\":\"{name}\"}}")
    }

    /// One `SKILL.md`.
    fn skill_md(name: &str, description: &str) -> String {
        format!("---\nname: {name}\ndescription: {description}\n---\n\nFollow the steps.\n")
    }

    /// Compresses one tarball.
    fn gzip(bytes: Vec<u8>) -> Vec<u8> {
        use std::io::Write as _;

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(&bytes).unwrap();

        encoder.finish().unwrap()
    }

    /// Builds a `.tar.gz` of regular files.
    fn tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (path, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            builder.append_data(&mut header, path, *bytes).unwrap();
        }
        builder.finish().unwrap();

        gzip(builder.into_inner().unwrap())
    }

    /// Builds a `.tar.gz` whose entry names are written straight into the
    /// header, so a test can name a path the builder itself refuses.
    fn raw_tarball(entries: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in entries {
            assert!(name.len() <= 100, "a header holds 100 name bytes");
            let mut header = tar::Header::new_gnu();
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append(&header, *bytes).unwrap();
        }
        builder.finish().unwrap();

        gzip(builder.into_inner().unwrap())
    }

    /// Builds a `.tar.gz` of a directory and the files under it.
    fn github_tarball(root: &str) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        let mut directory = tar::Header::new_gnu();
        directory.set_entry_type(tar::EntryType::Directory);
        directory.set_size(0);
        directory.set_mode(0o755);
        builder
            .append_data(&mut directory, format!("{root}/"), std::io::empty())
            .unwrap();
        for (path, bytes) in [
            (
                format!("{root}/plugin.json"),
                plugin_json("weekly.tools").into_bytes(),
            ),
            (
                format!("{root}/skills/summarize/SKILL.md"),
                skill_md("summarize", "Write the weekly status report.").into_bytes(),
            ),
            (
                format!("{root}/skills/summarize/scripts/gather.sh"),
                b"#!/bin/sh\n".to_vec(),
            ),
        ] {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(bytes.len() as u64);
            header.set_mode(0o644);
            builder
                .append_data(&mut header, path, bytes.as_slice())
                .unwrap();
        }
        builder.finish().unwrap();

        gzip(builder.into_inner().unwrap())
    }

    #[test]
    fn reads_a_repository_pinned_to_a_commit() {
        let source = parse_source(&format!("https://github.com/acme/weekly-tools@{COMMIT}"))
            .expect("a pinned repository is a source");
        assert_eq!(
            source,
            Source::Repository {
                url: "https://github.com/acme/weekly-tools".to_owned(),
                commit: COMMIT.to_owned(),
            }
        );
        // A trailing slash names the same repository.
        assert_eq!(
            parse_source(&format!("https://github.com/acme/weekly-tools/@{COMMIT}")).unwrap(),
            source
        );
        // A SHA-256 object id is a commit too.
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(
            parse_source(&format!("https://github.com/acme/weekly-tools@{sha256}")).unwrap(),
            Source::Repository {
                url: "https://github.com/acme/weekly-tools".to_owned(),
                commit: sha256.to_owned(),
            }
        );
    }

    #[test]
    fn reads_a_directory_path() {
        assert_eq!(
            parse_source("./plugins/weekly").unwrap(),
            Source::Directory {
                path: std::path::PathBuf::from("./plugins/weekly")
            }
        );
        assert_eq!(
            parse_source("/srv/plugins/weekly").unwrap(),
            Source::Directory {
                path: std::path::PathBuf::from("/srv/plugins/weekly")
            }
        );
        // A path with no scheme is a directory however it is spelled.
        assert_eq!(
            parse_source("plugins/weekly").unwrap(),
            Source::Directory {
                path: std::path::PathBuf::from("plugins/weekly")
            }
        );
    }

    #[test]
    fn refuses_a_source_that_names_no_commit() {
        for raw in [
            "",
            "   ",
            "https://github.com/acme/weekly-tools",
            "https://github.com/acme/weekly-tools@",
            "https://github.com/acme/weekly-tools @0123456789abcdef",
            "https://github.com/acme/weekly-tools@0123456 789",
        ] {
            let error = parse_source(raw).unwrap_err();
            assert!(
                matches!(error, PluginError::InvalidSource(_)),
                "{raw:?} must be refused, answered {error}"
            );
        }
    }

    #[test]
    fn refuses_a_branch_or_tag_in_place_of_a_commit() {
        // A repository source names one immutable object id. A branch or tag
        // may name different bytes tomorrow, so it is not a commit source.
        for name in [
            "main",
            "master",
            "v1.2.3",
            "release/1.0",
            "feature/weekly",
            "HEAD",
            "0123456",
        ] {
            let error =
                parse_source(&format!("https://github.com/acme/weekly@{name}")).unwrap_err();
            assert!(
                matches!(error, PluginError::InvalidSource(_)),
                "{name:?} must be refused, answered {error}"
            );
            assert!(
                error.to_string().contains("commit object id"),
                "the refusal names the commit rule, answered {error}"
            );
        }
    }

    #[test]
    fn refuses_a_commit_outside_its_shape() {
        for commit in [
            "0".repeat(39),
            "0".repeat(41),
            "0".repeat(63),
            "0".repeat(65),
            format!("{}g", "0".repeat(39)),
            "A".repeat(40),
            format!("{};rm", "0".repeat(37)),
            "0123456789abcdef\n0123456789abcdef\n0123456789".to_owned(),
        ] {
            let error =
                parse_source(&format!("https://github.com/acme/weekly@{commit}")).unwrap_err();
            assert!(
                matches!(error, PluginError::InvalidSource(_)),
                "{commit:?} must be refused, answered {error}"
            );
        }
    }

    #[test]
    fn refuses_a_repository_url_that_is_not_an_address() {
        for raw in [
            format!("git://github.com/acme/weekly@{COMMIT}"),
            format!("https://@{COMMIT}"),
            format!("https://someone:secret@github.com/acme/weekly@{COMMIT}"),
            format!("file:///srv/weekly@{COMMIT}"),
            format!("https://github.com/acme/weekly#fragment@{COMMIT}"),
        ] {
            let error = parse_source(&raw).unwrap_err();
            assert!(
                matches!(error, PluginError::InvalidSource(_)),
                "{raw:?} must be refused, answered {error}"
            );
            assert!(
                !error.to_string().contains("secret"),
                "a URL's user information is not repeated, answered {error}"
            );
        }
    }

    #[test]
    fn refuses_a_repository_url_that_carries_a_query() {
        // A query names something other than the repository the commit pins:
        // what the archive URL would drop, the source must not keep.
        let raw = format!("https://github.com/acme/weekly?token=supersecret@{COMMIT}");
        let error = parse_source(&raw).unwrap_err();

        assert!(
            matches!(error, PluginError::InvalidSource(_)),
            "a URL with a query is refused, answered {error}"
        );
        assert!(
            !error.to_string().contains("supersecret"),
            "a URL's query is not repeated, answered {error}"
        );
    }

    #[test]
    fn a_package_digest_is_stable_and_names_its_content() {
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            ("LICENSE", "MIT"),
        ]);
        let same = package(&[
            ("LICENSE", "MIT"),
            ("plugin.json", &plugin_json("weekly.tools")),
        ]);
        let other = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            ("LICENSE", "APACHE"),
        ]);
        let more = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            ("LICENSE", "MIT"),
            ("CHANGELOG.md", "1.0.0"),
        ]);

        assert_eq!(digest(&files), digest(&files));
        assert_eq!(
            digest(&files),
            digest(&same),
            "a map is read in sorted order"
        );
        assert_ne!(digest(&files), digest(&other));
        assert_ne!(digest(&files), digest(&more));
        // An empty package is a digest of nothing, not a panic.
        assert_eq!(digest(&BTreeMap::new()), digest(&BTreeMap::new()));
    }

    #[test]
    fn a_package_digest_is_one_sha256_over_the_canonical_buffer() {
        // The canonical buffer is, per entry in sorted order: the path bytes, a
        // NUL, the length in decimal, a NUL, and the bytes. This SHA-256 was
        // computed over exactly that buffer for these two entries, so a double
        // hash or a different framing would not answer it.
        let files = package(&[("b", "two"), ("a", "one")]);

        assert_eq!(
            digest(&files),
            "d4fcad726c5fe31fb48171feca00869c5522b7f88db0b0fe379309ca927fab7f"
        );
        // The digest of no entries is the SHA-256 of the empty buffer.
        assert_eq!(
            digest(&BTreeMap::new()),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn unpacks_a_gzipped_tarball_and_strips_the_shared_root() {
        let bytes = tarball(&[
            (
                "repo-0123456/plugin.json",
                plugin_json("weekly.tools").as_bytes(),
            ),
            (
                "repo-0123456/skills/summarize/SKILL.md",
                skill_md("summarize", "Report.").as_bytes(),
            ),
        ]);
        let mut skipped = Vec::new();

        let files = extract_archive(&bytes, &mut skipped).expect("a package archive unpacks");

        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["plugin.json", "skills/summarize/SKILL.md"]
        );
    }

    #[test]
    fn a_tarball_of_one_package_root_keeps_every_path() {
        // A package tarball has no wrapping directory, so nothing is stripped.
        let bytes = tarball(&[
            ("plugin.json", plugin_json("weekly.tools").as_bytes()),
            (
                "skills/summarize/SKILL.md",
                skill_md("summarize", "Report.").as_bytes(),
            ),
        ]);
        let mut skipped = Vec::new();

        let files = extract_archive(&bytes, &mut skipped).unwrap();

        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["plugin.json", "skills/summarize/SKILL.md"]
        );
    }

    #[test]
    fn refuses_a_path_that_leaves_the_plugin_root() {
        for name in [
            &b"../plugin.json"[..],
            &b"/etc/plugin.json"[..],
            &b"a/../../b"[..],
        ] {
            let bytes = raw_tarball(&[(name, b"{}")]);
            let mut skipped = Vec::new();

            let error = extract_archive(&bytes, &mut skipped).unwrap_err();

            assert!(
                matches!(error, PluginError::Escape(_)),
                "{} must be refused, answered {error}",
                String::from_utf8_lossy(name)
            );
            assert!(
                !error.to_string().contains("{}"),
                "no file contents in a report"
            );
        }
    }

    #[test]
    fn skips_a_link_and_reports_it() {
        let mut builder = tar::Builder::new(Vec::new());
        let mut regular = tar::Header::new_gnu();
        regular.set_entry_type(tar::EntryType::Regular);
        regular.set_size(2);
        regular.set_mode(0o644);
        builder
            .append_data(&mut regular, "plugin.json", &b"{}"[..])
            .unwrap();
        for (kind, target) in [
            (tar::EntryType::Symlink, "../outside.md"),
            (tar::EntryType::Link, "plugin.json"),
        ] {
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(kind);
            link.set_size(0);
            link.set_mode(0o777);
            builder.append_link(&mut link, "linked.md", target).unwrap();
        }
        builder.finish().unwrap();
        let bytes = gzip(builder.into_inner().unwrap());
        let mut skipped = Vec::new();

        let files = extract_archive(&bytes, &mut skipped).expect("a link is skipped, not fatal");

        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["plugin.json"]
        );
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped.iter().all(|report| report.contains("linked.md")));
    }

    #[test]
    fn refuses_a_package_of_too_many_files() {
        // One file past the limit is under the entry limit: it is the file
        // count, not the entry count, that must refuse this package.
        let entries: Vec<(String, &[u8])> = (0..MAX_PACKAGE_FILES + 1)
            .map(|index| (format!("file-{index}.md"), &b"x"[..]))
            .collect();
        let bytes = tarball(
            &entries
                .iter()
                .map(|(path, bytes)| (path.as_str(), *bytes))
                .collect::<Vec<_>>(),
        );
        let mut skipped = Vec::new();

        let error = extract_archive(&bytes, &mut skipped).unwrap_err();

        assert!(
            matches!(error, PluginError::TooLarge(_)) && error.to_string().contains("files"),
            "answered {error}"
        );
    }

    #[test]
    fn refuses_a_file_larger_than_the_limit() {
        let big = vec![b'x'; MAX_PACKAGE_FILE_BYTES as usize + 1];
        let bytes = tarball(&[("plugin.json", &big)]);
        let mut skipped = Vec::new();

        let error = extract_archive(&bytes, &mut skipped).unwrap_err();

        assert!(
            matches!(error, PluginError::TooLarge(_)),
            "answered {error}"
        );
    }

    #[test]
    fn refuses_a_tarball_that_would_expand_past_the_package_limit() {
        // Five mebibytes of one byte compresses to a few kilobytes, so the
        // uncompressed size is what refuses it, before it is read.
        let big = vec![0_u8; 5 * 1024 * 1024];
        let bytes = tarball(&[("bomb.bin", &big)]);

        assert!(
            bytes.len() < big.len() / 10,
            "the test archive is a compression bomb"
        );
        let mut skipped = Vec::new();
        let error = extract_archive(&bytes, &mut skipped).unwrap_err();

        assert!(
            matches!(error, PluginError::TooLarge(_)),
            "answered {error}"
        );
    }

    #[test]
    fn refuses_a_package_of_too_many_entries() {
        // Empty directories are entries too, so a tarball of them is bounded.
        let mut builder = tar::Builder::new(Vec::new());
        for index in 0..MAX_PACKAGE_ENTRIES + 1 {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_size(0);
            header.set_mode(0o755);
            builder
                .append_data(&mut header, format!("dir-{index}/"), std::io::empty())
                .unwrap();
        }
        builder.finish().unwrap();
        let bytes = gzip(builder.into_inner().unwrap());
        let mut skipped = Vec::new();

        let error = extract_archive(&bytes, &mut skipped).unwrap_err();

        assert!(
            matches!(error, PluginError::TooLarge(_)) && error.to_string().contains("entries"),
            "answered {error}"
        );
    }

    #[test]
    fn a_capped_reader_refuses_to_expand_past_its_cap() {
        use std::io::Read as _;

        let mut reader = CappedReader::new(std::io::repeat(b'x'), 16);
        let mut bytes = Vec::new();

        let error = reader.read_to_end(&mut bytes).unwrap_err();

        assert!(
            error
                .get_ref()
                .is_some_and(|cause| cause.is::<ArchiveTooLarge>()),
            "a read past the cap fails with the marker, answered {error}"
        );
        assert!(bytes.len() <= 16, "at most the cap is read");
    }

    #[test]
    fn refuses_a_package_of_too_many_bytes() {
        // Forty-two files of the most one file may carry.
        let file = vec![b'x'; MAX_PACKAGE_FILE_BYTES as usize];
        let entries: Vec<(String, &[u8])> = (0..42)
            .map(|index| (format!("file-{index}.md"), file.as_slice()))
            .collect();
        let bytes = tarball(
            &entries
                .iter()
                .map(|(path, bytes)| (path.as_str(), *bytes))
                .collect::<Vec<_>>(),
        );
        let mut skipped = Vec::new();

        let error = extract_archive(&bytes, &mut skipped).unwrap_err();

        assert!(
            matches!(error, PluginError::TooLarge(_)) && error.to_string().contains("package"),
            "answered {error}"
        );
    }

    #[test]
    fn refuses_bytes_that_are_not_a_gzipped_tarball() {
        let mut skipped = Vec::new();

        assert!(matches!(
            extract_archive(b"not an archive", &mut skipped),
            Err(PluginError::Archive(_))
        ));
        // A gzipped block that is no tar header fails the header checksum.
        assert!(matches!(
            extract_archive(&gzip(vec![b'a'; 1024]), &mut skipped),
            Err(PluginError::Archive(_))
        ));
    }

    #[test]
    fn reads_a_directory_into_a_package() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("weekly");
        std::fs::create_dir_all(root.join("skills/summarize")).unwrap();
        std::fs::write(root.join("plugin.json"), plugin_json("weekly.tools")).unwrap();
        std::fs::write(
            root.join("skills/summarize/SKILL.md"),
            skill_md("summarize", "Report."),
        )
        .unwrap();
        let mut skipped = Vec::new();

        let files = read_directory(&root, &mut skipped).expect("a package directory reads");

        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["plugin.json", "skills/summarize/SKILL.md"]
        );
    }

    /// One directory path as the only entry the Admin allows.
    fn allowed_entries(directory: &std::path::Path) -> Vec<String> {
        vec![
            directory
                .to_str()
                .expect("a tempdir path is text")
                .to_owned(),
        ]
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_a_directory_only_after_authorizing_its_root() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("packages");
        let root = allowed.join("weekly");
        std::fs::create_dir_all(root.join("skills/summarize")).unwrap();
        std::fs::write(root.join("plugin.json"), plugin_json("weekly.tools")).unwrap();
        std::fs::write(
            root.join("skills/summarize/SKILL.md"),
            skill_md("summarize", "Report."),
        )
        .unwrap();
        let mut skipped = Vec::new();

        let files = read_authorized_directory(&root, &allowed_entries(&allowed), &mut skipped)
            .expect("an allowed package directory reads");

        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["plugin.json", "skills/summarize/SKILL.md"]
        );
    }

    #[test]
    fn refuses_a_directory_source_the_admin_does_not_allow() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("packages");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let entries = allowed_entries(&allowed);
        let mut skipped = Vec::new();

        // A directory outside the allowed root is refused.
        let error = read_authorized_directory(&outside, &entries, &mut skipped).unwrap_err();
        assert!(matches!(error, PluginError::NotAllowed(_)), "{error}");
        // A `..` traversal that resolves outside the allowed root is refused
        // after it is resolved, not by looking at the text.
        let escaped = allowed.join("..").join("outside");
        let error = read_authorized_directory(&escaped, &entries, &mut skipped).unwrap_err();
        assert!(matches!(error, PluginError::NotAllowed(_)), "{error}");
        // A directory that does not exist is a read error, not an allowed one.
        let error = read_authorized_directory(&dir.path().join("absent"), &entries, &mut skipped)
            .unwrap_err();
        assert!(matches!(error, PluginError::Read(_)), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlinked_root_that_leaves_the_allowed_directory() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("packages");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let link = dir.path().join("linked");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let mut skipped = Vec::new();

        let error =
            read_authorized_directory(&link, &allowed_entries(&allowed), &mut skipped).unwrap_err();

        assert!(matches!(error, PluginError::NotAllowed(_)), "{error}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reads_a_root_that_is_a_symlink_inside_the_allowed_directory() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("packages");
        let root = allowed.join("weekly");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("plugin.json"), plugin_json("weekly.tools")).unwrap();
        let link = allowed.join("linked-weekly");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        let mut skipped = Vec::new();

        let files = read_authorized_directory(&link, &allowed_entries(&allowed), &mut skipped)
            .expect("a link inside the allowed directory is read as its target");

        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["plugin.json"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_in_a_package_directory_is_skipped_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.json"), plugin_json("weekly.tools")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("stolen.md")).unwrap();
        let mut skipped = Vec::new();

        let files =
            read_directory(dir.path(), &mut skipped).expect("a symlink is skipped, not fatal");

        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["plugin.json"]
        );
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].contains("stolen.md"), "{skipped:?}");
        assert!(skipped[0].contains("symbolic link"), "{skipped:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_symlinked_component_is_skipped_and_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::fs::write(root.join("real/notes.md"), "notes").unwrap();
        std::os::unix::fs::symlink("real", root.join("link")).unwrap();
        let mut skipped = Vec::new();

        let files = read_authorized_directory(root, &allowed_entries(root), &mut skipped)
            .expect("a symlink is skipped, not followed");

        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["real/notes.md"],
            "the link's target is read once, through its own name"
        );
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].contains("link"), "{skipped:?}");
        assert!(skipped[0].contains("symbolic link"), "{skipped:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_symlink_swapped_in_for_a_file_is_not_opened() {
        let dir = tempfile::tempdir().unwrap();
        let directory = std::fs::File::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("report.md"), "report").unwrap();
        assert!(
            open_child_file(&directory, std::ffi::OsStr::new("report.md"), "report.md").is_ok(),
            "a regular file opens through its parent's descriptor"
        );
        // The metadata check saw a regular file; before it is opened the name
        // is replaced by a symlink. `O_NOFOLLOW` refuses that open instead of
        // reading the link's target.
        std::fs::remove_file(dir.path().join("report.md")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", dir.path().join("report.md")).unwrap();
        let error = open_child_file(&directory, std::ffi::OsStr::new("report.md"), "report.md")
            .unwrap_err();
        assert!(matches!(error, PluginError::Read(_)), "answered {error}");
        assert!(
            error.to_string().contains("symbolic link"),
            "the refusal names the symlink, answered {error}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_fifo_in_a_package_directory_is_skipped_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("plugin.json"), plugin_json("weekly.tools")).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(root.join("pipe"))
            .status()
            .expect("mkfifo runs");
        assert!(status.success(), "mkfifo creates a FIFO");
        let mut skipped = Vec::new();

        let files = read_authorized_directory(root, &allowed_entries(root), &mut skipped)
            .expect("a FIFO is skipped, not fatal");

        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["plugin.json"]
        );
        assert_eq!(skipped.len(), 1, "{skipped:?}");
        assert!(skipped[0].contains("pipe"), "{skipped:?}");
        assert!(skipped[0].contains("not a regular file"), "{skipped:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn refuses_a_fifo_supplied_as_the_source_root() {
        // The root is opened as a directory before anything is read from it, so
        // a FIFO names a refusal rather than blocking for a writer forever.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("pipe");
        let status = std::process::Command::new("mkfifo")
            .arg(&root)
            .status()
            .expect("mkfifo runs");
        assert!(status.success(), "mkfifo creates a FIFO");
        let mut skipped = Vec::new();

        let error = read_authorized_directory(&root, &allowed_entries(dir.path()), &mut skipped)
            .unwrap_err();

        assert!(matches!(error, PluginError::Read(_)), "answered {error}");
        assert!(
            error.to_string().contains("pipe"),
            "the refusal names the source, answered {error}"
        );
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn refuses_a_directory_source_on_a_platform_that_is_not_linux() {
        // Every platform other than Linux, macOS among them: the install path
        // refuses a directory before it opens anything, and points at the
        // repository source that installs the same package.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.json"), plugin_json("weekly.tools")).unwrap();
        let mut skipped = Vec::new();

        let error =
            read_authorized_directory(dir.path(), &allowed_entries(dir.path()), &mut skipped)
                .unwrap_err();

        assert!(matches!(error, PluginError::Read(_)), "answered {error}");
        let report = error.to_string();
        assert!(
            report.contains("supported only on Linux"),
            "the refusal names the platform limit, answered {report}"
        );
        assert!(
            report.contains("repository source"),
            "the refusal names the source that works, answered {report}"
        );
    }

    #[test]
    fn refuses_a_directory_past_a_limit() {
        let files = tempfile::tempdir().unwrap();
        for index in 0..MAX_PACKAGE_FILES + 1 {
            std::fs::write(files.path().join(format!("file-{index}.md")), "x").unwrap();
        }
        let mut skipped = Vec::new();

        // A directory of one file past the file limit is refused for its
        // files, even though its entries are below the entry limit.
        let error = read_directory(files.path(), &mut skipped).unwrap_err();
        assert!(
            matches!(error, PluginError::TooLarge(_)) && error.to_string().contains("files"),
            "answered {error}"
        );

        // Empty directories are entries too, so a tree of them is refused for
        // its entries, even though it holds no file.
        let directories = tempfile::tempdir().unwrap();
        for index in 0..MAX_PACKAGE_ENTRIES + 1 {
            std::fs::create_dir(directories.path().join(format!("dir-{index}"))).unwrap();
        }
        let error = read_directory(directories.path(), &mut skipped).unwrap_err();
        assert!(
            matches!(error, PluginError::TooLarge(_)) && error.to_string().contains("entries"),
            "answered {error}"
        );

        let big = tempfile::tempdir().unwrap();
        for index in 0..42 {
            std::fs::write(
                big.path().join(format!("file-{index}.md")),
                vec![b'x'; MAX_PACKAGE_FILE_BYTES as usize],
            )
            .unwrap();
        }

        assert!(matches!(
            read_directory(big.path(), &mut skipped),
            Err(PluginError::TooLarge(_))
        ));
    }

    #[test]
    fn refuses_a_huge_local_file_without_reading_it() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plugin.json"), plugin_json("weekly.tools")).unwrap();
        std::fs::write(
            dir.path().join("huge.md"),
            vec![b'x'; MAX_PACKAGE_FILE_BYTES as usize + 1],
        )
        .unwrap();
        let mut skipped = Vec::new();

        let error = read_directory(dir.path(), &mut skipped).unwrap_err();

        assert!(
            matches!(error, PluginError::TooLarge(_)),
            "answered {error}"
        );
    }

    #[test]
    fn refuses_a_path_that_is_not_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("plugin.json");
        std::fs::write(&file, plugin_json("weekly.tools")).unwrap();
        let mut skipped = Vec::new();

        assert!(matches!(
            read_directory(&file, &mut skipped),
            Err(PluginError::Read(_))
        ));
        assert!(matches!(
            read_directory(&dir.path().join("absent"), &mut skipped),
            Err(PluginError::Read(_))
        ));
    }

    /// One repository source at the test commit.
    fn repository(url: &str) -> Source {
        Source::Repository {
            url: url.to_owned(),
            commit: COMMIT.to_owned(),
        }
    }

    #[test]
    fn authorizes_a_directory_source_at_or_below_an_allowed_directory() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("packages");
        let package = allowed.join("weekly");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::create_dir(dir.path().join("packages-other")).unwrap();
        std::fs::create_dir(dir.path().join("outside")).unwrap();
        let allowed_entry = allowed.to_str().unwrap().to_owned();

        // The allowed directory itself and a package below it are allowed.
        assert!(
            authorize_source(
                &Source::Directory {
                    path: allowed.clone()
                },
                std::slice::from_ref(&allowed_entry)
            )
            .is_ok()
        );
        assert!(
            authorize_source(
                &Source::Directory {
                    path: package.clone()
                },
                std::slice::from_ref(&allowed_entry)
            )
            .is_ok()
        );
        // A sibling whose name only starts with the allowed path is not below
        // it: the comparison is on a path-component boundary.
        assert!(matches!(
            authorize_source(
                &Source::Directory {
                    path: dir.path().join("packages-other")
                },
                std::slice::from_ref(&allowed_entry)
            ),
            Err(PluginError::NotAllowed(_))
        ));
        // A `..` traversal that resolves outside the allowed directory is
        // refused after it is resolved, not by looking at the text.
        let escaped = Source::Directory {
            path: package.join("..").join("..").join("outside"),
        };
        assert!(matches!(
            authorize_source(&escaped, std::slice::from_ref(&allowed_entry)),
            Err(PluginError::NotAllowed(_))
        ));
        // A missing allowed entry is an error for that entry only; another
        // entry may still match.
        let missing = dir.path().join("absent").to_str().unwrap().to_owned();
        assert!(
            authorize_source(
                &Source::Directory {
                    path: package.clone()
                },
                &[missing.clone(), allowed_entry.clone()]
            )
            .is_ok()
        );
        assert!(matches!(
            authorize_source(
                &Source::Directory {
                    path: package.clone()
                },
                &[missing]
            ),
            Err(PluginError::NotAllowed(_))
        ));
        // A URL entry is a repository entry, never a directory entry.
        assert!(matches!(
            authorize_source(
                &Source::Directory {
                    path: package.clone()
                },
                &["https://github.com/acme/".to_owned()]
            ),
            Err(PluginError::NotAllowed(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_at_the_source_root_is_resolved_before_it_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("packages");
        let package = allowed.join("weekly");
        std::fs::create_dir_all(&package).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let allowed_entry = allowed.to_str().unwrap().to_owned();

        // A root that is a symlink inside the allowed directory is read as its
        // target, which is below the allowed directory.
        let inside_link = dir.path().join("inside-link");
        std::os::unix::fs::symlink(&package, &inside_link).unwrap();
        assert!(
            authorize_source(
                &Source::Directory { path: inside_link },
                std::slice::from_ref(&allowed_entry)
            )
            .is_ok()
        );

        // A root that resolves outside the allowed directory is refused.
        let outside_link = dir.path().join("outside-link");
        std::os::unix::fs::symlink(&outside, &outside_link).unwrap();
        assert!(matches!(
            authorize_source(&Source::Directory { path: outside_link }, &[allowed_entry]),
            Err(PluginError::NotAllowed(_))
        ));
    }

    #[test]
    fn authorizes_a_repository_source_at_or_below_an_allowed_url() {
        let source = repository("https://github.com/acme/reports");
        for allowed in [
            "https://github.com/",
            "https://github.com/acme",
            "https://github.com/acme/",
            "https://github.com/acme/reports",
        ] {
            assert!(
                authorize_source(&source, &[allowed.to_owned()]).is_ok(),
                "{allowed} allows the source"
            );
        }
        for allowed in [
            // A path-component boundary, not a raw prefix.
            "https://github.com/acmex/",
            // A different scheme, host, or port is a different origin.
            "http://github.com/acme/",
            "https://gitlab.com/acme/",
            "https://github.com:8443/acme/",
            // User information, a query string, and a fragment never name an
            // allowed prefix.
            "https://someone:secret@github.com/acme/",
            "https://github.com/acme/?access_token=supersecret",
            "https://github.com/acme/#fragment",
            // A path entry is not a URL entry.
            "/srv/neha-packages",
        ] {
            assert!(
                authorize_source(&source, &[allowed.to_owned()]).is_err(),
                "{allowed} must not allow the source"
            );
        }

        // A source URL that carries user information is refused, and the
        // refusal does not repeat the secret.
        let secret = repository("https://someone:secret@github.com/acme/reports");
        let error =
            authorize_source(&secret, &["https://github.com/acme/".to_owned()]).unwrap_err();
        assert!(matches!(error, PluginError::NotAllowed(_)), "{error}");
        assert!(!error.to_string().contains("secret"), "{error}");
        // A non-loopback HTTP source is not one this Space fetches.
        assert!(
            authorize_source(
                &repository("http://example.com/acme/reports"),
                &["http://example.com/acme/".to_owned()]
            )
            .is_err()
        );
    }

    #[test]
    fn names_the_archive_of_a_repository_source() {
        let source = repository("https://github.com/acme/reports/");
        let address = repository_archive_url(&source).unwrap();

        assert_eq!(
            address.as_str(),
            format!("https://github.com/acme/reports/archive/{COMMIT}.tar.gz")
        );
        // The commit is the archive path's last segment, not text appended to
        // a serialized URL.
        let expected = format!("{COMMIT}.tar.gz");
        assert_eq!(
            address.path_segments().unwrap().next_back(),
            Some(expected.as_str())
        );
        assert!(address.query().is_none());
        assert!(address.fragment().is_none());
        assert!(matches!(
            repository_archive_url(&Source::Directory {
                path: std::path::PathBuf::from("/srv/weekly")
            }),
            Err(PluginError::NotFetched)
        ));
    }

    #[test]
    fn builds_the_archive_path_from_the_urls_segments() {
        // A source built by hand may carry a query; the archive address is
        // built from the URL's parsed path, so the commit stays the last
        // segment and no query survives.
        let source = repository("https://github.com/acme/reports?ref=main");
        let address = repository_archive_url(&source).unwrap();

        assert_eq!(
            address.as_str(),
            format!("https://github.com/acme/reports/archive/{COMMIT}.tar.gz")
        );
        let expected = format!("{COMMIT}.tar.gz");
        assert_eq!(
            address.path_segments().unwrap().next_back(),
            Some(expected.as_str())
        );
        assert!(address.query().is_none());
    }

    #[test]
    fn reads_a_manifest() {
        let files = package(&[(
            "plugin.json",
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"weekly.tools","version":"1.2.0","description":"Weekly reports."}"#,
        )]);

        let loaded = validate(&files).expect("a manifest reads");

        assert_eq!(loaded.manifest.name, "weekly.tools");
        assert_eq!(loaded.manifest.version.as_deref(), Some("1.2.0"));
        assert_eq!(
            loaded.manifest.description.as_deref(),
            Some("Weekly reports.")
        );
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
        assert!(loaded.skills.is_empty());
        assert!(loaded.servers.is_empty());
    }

    #[test]
    fn rejects_a_manifest_that_is_not_a_plugin_json() {
        for files in [
            BTreeMap::new(),
            package(&[("plugin.json", "not json")]),
            package(&[("plugin.json", "[]")]),
            package(&[("plugin.json", "\"a name\"")]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\"}",
            )]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/2.0.0/plugin.schema.json\",\"name\":\"weekly.tools\"}",
            )]),
            package(&[("plugin.json", "{\"name\":\"weekly.tools\"}")]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":42}",
            )]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"\"}",
            )]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"Weekly-Tools\"}",
            )]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"-weekly\"}",
            )]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"weekly--tools\"}",
            )]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"weekly..tools\"}",
            )]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"weekly\",\"version\":null}",
            )]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"weekly\",\"description\":[]}",
            )]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"weekly\",\"keywords\":[\"ok\",3]}",
            )]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"weekly\",\"author\":{\"handle\":\"ada\"}}",
            )]),
            package(&[(
                "plugin.json",
                "{\"$schema\":\"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\"name\":\"weekly\",\"author\":\"Ada\"}",
            )]),
        ] {
            let error = validate(&files).unwrap_err();
            assert!(
                matches!(error, PluginError::Manifest(_)),
                "{:?} must be refused, answered {error}",
                files.get("plugin.json")
            );
        }
    }

    #[test]
    fn reports_an_unknown_field_and_keeps_loading() {
        let files = package(&[
            (
                "plugin.json",
                r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"weekly.tools","x":true,"extensions":{"com.neha.space":{"setting":true}}}"#,
            ),
            (
                "skills/summarize/SKILL.md",
                &skill_md("summarize", "Report."),
            ),
        ]);

        let loaded = validate(&files).expect("an unknown field is reported, not fatal");

        assert_eq!(loaded.skipped, ["plugin.json: unknown field `x`"]);
        assert_eq!(loaded.skills.len(), 1);
    }

    #[test]
    fn reports_an_extensions_field_that_is_not_an_object() {
        let files = package(&[(
            "plugin.json",
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"weekly.tools","extensions":[1,2]}"#,
        )]);

        let loaded = validate(&files).expect("a non-object `extensions` is reported, not fatal");

        assert_eq!(
            loaded.skipped,
            ["plugin.json: the `extensions` field is not an object, so it is ignored"]
        );
    }

    #[test]
    fn a_package_without_mcp_json_has_no_servers() {
        let files = package(&[("plugin.json", &plugin_json("weekly.tools"))]);

        let loaded = validate(&files).unwrap();

        assert!(loaded.servers.is_empty());
        assert!(loaded.skipped.is_empty());
    }

    #[test]
    fn a_bad_mcp_json_disables_only_mcp() {
        // §7.2.2: a `mcp.json` that breaks its top-level rules disables MCP
        // and leaves every other component type loading.
        let manifest = plugin_json("weekly.tools");
        for mcp in [
            "not json",
            "[]",
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json"}"#,
            r#"{"$schema":"https://agent-plugins.org/schemas/2.0.0/mcp.schema.json","mcpServers":{}}"#,
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json","mcpServers":{},"x":1}"#,
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/mcp.schema.json","mcpServers":[]}"#,
            // The MCP version must be the one `plugin.json` names.
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","mcpServers":{}}"#,
        ] {
            let files = package(&[
                ("plugin.json", &manifest),
                ("mcp.json", mcp),
                (
                    "skills/summarize/SKILL.md",
                    &skill_md("summarize", "Report."),
                ),
            ]);
            let loaded = validate(&files).expect("a bad mcp.json disables only MCP");

            assert!(loaded.servers.is_empty(), "{mcp} must load no server");
            assert_eq!(loaded.skills.len(), 1, "{mcp} must not hide a skill");
            assert_eq!(loaded.skipped.len(), 1, "{mcp}: {:?}", loaded.skipped);
            assert!(
                loaded.skipped[0].starts_with("mcp.json: "),
                "{:?}",
                loaded.skipped
            );
        }
    }

    #[test]
    fn a_rejected_server_url_never_repeats_its_user_information() {
        let secret = "s3cret-token";
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            (
                "mcp.json",
                &format!(
                    r#"{{"$schema":"{MCP_SCHEMA}","mcpServers":{{"reports":{{"type":"streamable-http","url":"https://someone:{secret}@deploy.example.com/mcp"}}}}}}"#
                ),
            ),
        ]);

        let loaded = validate(&files).expect("an invalid server entry is skipped");

        assert!(loaded.servers.is_empty());
        assert_eq!(loaded.skipped.len(), 1, "{:?}", loaded.skipped);
        let report = &loaded.skipped[0];
        assert!(report.contains("`reports`"), "{report}");
        assert!(report.contains("`url`"), "{report}");
        assert!(
            !report.contains(secret),
            "a secret is never echoed: {report}"
        );
    }

    #[test]
    fn a_rejected_server_url_never_repeats_its_query() {
        // A query may carry a credential (`?access_token=…`), so it is refused
        // like user information, and the fixed report names the entry and the
        // field, never the URL or what its query holds.
        let secret = "supersecret";
        let url = format!("https://example.com/mcp?access_token={secret}");
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            (
                "mcp.json",
                &format!(
                    r#"{{"$schema":"{MCP_SCHEMA}","mcpServers":{{"reports":{{"type":"streamable-http","url":"{url}"}}}}}}"#
                ),
            ),
        ]);

        let loaded = validate(&files).expect("a server entry with a query is skipped");

        assert!(loaded.servers.is_empty(), "{:?}", loaded.servers);
        assert_eq!(loaded.skipped.len(), 1, "{:?}", loaded.skipped);
        let report = &loaded.skipped[0];
        assert!(report.contains("`reports`"), "{report}");
        assert!(report.contains("`url`"), "{report}");
        assert!(report.contains("query"), "{report}");
        // Nothing a package reports may carry the URL or its query.
        assert!(
            !report.contains(secret),
            "a query is never echoed: {report}"
        );
        assert!(
            !report.contains("access_token"),
            "a query is never echoed: {report}"
        );
        assert!(
            !report.contains("example.com/mcp?"),
            "the URL is never echoed: {report}"
        );
        // The shared address rule refuses the query before a server is built.
        assert!(!super::valid_server_url(&url));
    }

    #[test]
    fn reads_an_empty_server_list() {
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            (
                "mcp.json",
                &format!(r#"{{"$schema":"{MCP_SCHEMA}","mcpServers":{{}}}}"#),
            ),
        ]);

        let loaded = validate(&files).expect("an empty server list is valid");

        assert!(loaded.servers.is_empty());
        assert!(loaded.skipped.is_empty());
    }

    #[test]
    fn reads_a_streamable_http_server() {
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            (
                "mcp.json",
                &format!(
                    r#"{{"$schema":"{MCP_SCHEMA}","mcpServers":{{"reports":{{"type":"streamable-http","url":"https://deploy.example.com/mcp","headers":{{"X-Tenant":"public-tenant","Accept":"application/json"}}}}}}}}"#
                ),
            ),
        ]);

        let loaded = validate(&files).expect("a streamable-http server reads");

        assert_eq!(
            loaded.servers,
            [PackageServer {
                key: "reports".to_owned(),
                name: "weekly-tools-reports".to_owned(),
                url: "https://deploy.example.com/mcp".to_owned(),
                headers: [
                    ("Accept".to_owned(), "application/json".to_owned()),
                    ("X-Tenant".to_owned(), "public-tenant".to_owned()),
                ]
                .into_iter()
                .collect(),
            }]
        );
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
    }

    #[test]
    fn reads_a_server_on_a_loopback_address() {
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            (
                "mcp.json",
                &format!(
                    r#"{{"$schema":"{MCP_SCHEMA}","mcpServers":{{"local":{{"type":"streamable-http","url":"http://127.0.0.1:9000/mcp"}},"v6":{{"type":"streamable-http","url":"http://[::1]:9000/mcp"}},"named":{{"type":"streamable-http","url":"http://localhost:9000/mcp"}}}}}}"#
                ),
            ),
        ]);

        let loaded = validate(&files).expect("loopback http needs no certificate");

        assert_eq!(
            loaded
                .servers
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            [
                "weekly-tools-local",
                "weekly-tools-named",
                "weekly-tools-v6"
            ]
        );
    }

    #[test]
    fn skips_a_transport_the_space_does_not_support() {
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            (
                "mcp.json",
                &format!(
                    r#"{{"$schema":"{MCP_SCHEMA}","mcpServers":{{"validator":{{"type":"stdio","command":"./bin/validator"}},"legacy":{{"type":"sse","url":"https://legacy.example.com/sse"}},"carrier":{{"type":"carrier-pigeon","url":"https://example.com"}}}}}}"#
                ),
            ),
        ]);

        let loaded = validate(&files).expect("an unsupported transport skips one entry");

        assert!(loaded.servers.is_empty());
        // Reports arrive in the order the entries are keyed, which is sorted.
        assert_eq!(
            loaded.skipped,
            [
                "mcp.json: the server `carrier` names the unknown transport `carrier-pigeon`",
                "mcp.json: the server `legacy` uses the sse transport, which this Space does not \
                 support",
                "mcp.json: the server `validator` uses the stdio transport, which this Space does \
                 not support",
            ]
        );
    }

    #[test]
    fn skips_a_server_entry_that_does_not_conform() {
        let manifest = plugin_json("weekly.tools");
        for entry in [
            r#"{"type":"streamable-http","url":"https://deploy.example.com/mcp","args":[]}"#,
            r#"{"type":"streamable-http"}"#,
            r#"{"type":"streamable-http","url":"http://deploy.example.com/mcp"}"#,
            r#"{"type":"streamable-http","url":"ftp://deploy.example.com/mcp"}"#,
            r#"{"type":"streamable-http","url":"https://someone:secret@deploy.example.com/mcp"}"#,
            r#"{"type":"streamable-http","url":"https://deploy.example.com/mcp#fragment"}"#,
            r#"{"type":"streamable-http","url":"https://deploy.example.com/mcp?access_token=secret"}"#,
            r#"{"type":"streamable-http","url":"https://deploy.example.com/mcp","headers":[]}"#,
            r#"{"type":"streamable-http","url":"https://deploy.example.com/mcp","headers":{"X-Tenant":3}}"#,
            r#"{"type":"streamable-http","url":"https://deploy.example.com/mcp","headers":{"X Tenant":"ok"}}"#,
            r#"{"type":"streamable-http","url":"https://deploy.example.com/mcp","headers":{"X-Tenant":"one\nX-Evil: two"}}"#,
            r#"{"type":"streamable-http","url":"https://deploy.example.com/mcp","headers":{"X-Tenant":"a","x-tenant":"b"}}"#,
            r#"{"type":"streamable-http","url":"https://deploy.example.com/mcp","headers":{"":"b"}}"#,
            "42",
            "[]",
        ] {
            let files = package(&[
                ("plugin.json", &manifest),
                (
                    "mcp.json",
                    &format!(r#"{{"$schema":"{MCP_SCHEMA}","mcpServers":{{"reports":{entry}}}}}"#),
                ),
            ]);
            let loaded = validate(&files).expect("an invalid entry skips one server");
            assert!(
                loaded.servers.is_empty(),
                "{entry} must load no server, loaded {:?}",
                loaded.servers
            );
            assert!(!loaded.skipped.is_empty(), "{entry} must be reported");
        }
    }

    #[test]
    fn skips_a_server_the_space_cannot_name() {
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            (
                "mcp.json",
                &format!(
                    r#"{{"$schema":"{MCP_SCHEMA}","mcpServers":{{"re ports":{{"type":"streamable-http","url":"https://deploy.example.com/mcp"}},"under_score":{{"type":"streamable-http","url":"https://deploy.example.com/mcp"}},"0123456789012345678901234567890123":{{"type":"streamable-http","url":"https://deploy.example.com/mcp"}}}}}}"#
                ),
            ),
        ]);

        let loaded = validate(&files).expect("an unnameable server is skipped");

        assert!(loaded.servers.is_empty());
        assert_eq!(loaded.skipped.len(), 3, "{:?}", loaded.skipped);
    }

    #[test]
    fn two_keys_never_name_one_server() {
        // The name a key composes is injective, so the collision report is a
        // guard rather than a case a package can reach.
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            (
                "mcp.json",
                &format!(
                    r#"{{"$schema":"{MCP_SCHEMA}","mcpServers":{{"reports":{{"type":"streamable-http","url":"https://a.example.com/mcp"}},"Reports":{{"type":"streamable-http","url":"https://b.example.com/mcp"}}}}}}"#
                ),
            ),
        ]);

        let loaded = validate(&files).unwrap();

        assert_eq!(loaded.servers.len(), 2);
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
    }

    #[test]
    fn reads_a_skill_and_its_scripts() {
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            (
                "skills/summarize/SKILL.md",
                &skill_md("summarize", "Report."),
            ),
            ("skills/summarize/scripts/gather.sh", "#!/bin/sh\n"),
            ("skills/summarize/references/runbook.md", "Nothing to see."),
            ("skills/summarize/assets/logo.svg", "<svg/>"),
        ]);

        let loaded = validate(&files).expect("a skill reads");

        assert_eq!(loaded.skills.len(), 1);
        let skill = &loaded.skills[0];
        assert_eq!(skill.directory, "summarize");
        assert_eq!(skill.name, "summarize");
        assert_eq!(skill.description, "Report.");
        assert_eq!(skill.body, "\nFollow the steps.\n");
        assert_eq!(
            skill.scripts,
            [PackageScript {
                name: "gather.sh".to_owned(),
                content: "#!/bin/sh\n".to_owned(),
            }]
        );
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
    }

    #[test]
    fn skips_a_skill_whose_frontmatter_is_not_valid() {
        for document in [
            "no frontmatter at all",
            "---\nname: summarize\n---\n",
            "---\nname: Summarize\ndescription: Report.\n---\n",
            "---\nname: -summarize\ndescription: Report.\n---\n",
            "---\nname: summarize\ndescription: ''\n---\n",
            "---\nname: summarize\ndescription: 3\n---\n",
            "---\nname: [unclosed\ndescription: Report.\n---\n",
            "---\n- a list\n---\n",
        ] {
            let files = package(&[
                ("plugin.json", &plugin_json("weekly.tools")),
                ("skills/summarize/SKILL.md", document),
            ]);
            let loaded = validate(&files).expect("an invalid skill is skipped");
            assert!(
                loaded.skills.is_empty(),
                "{document:?} must load no skill, loaded {:?}",
                loaded.skills
            );
            assert!(
                loaded
                    .skipped
                    .iter()
                    .any(|report| report.contains("skills/summarize")),
                "{document:?} must be reported"
            );
        }
    }

    #[test]
    fn skips_a_skill_whose_optional_fields_do_not_match_the_specification() {
        for field in [
            "license: [MIT]",
            "compatibility: [linux]",
            "metadata: [author]",
            "metadata:\n  author: 3",
            "allowed-tools: [read_file]",
        ] {
            let document = format!("---\nname: summarize\ndescription: Report.\n{field}\n---\n");
            let files = package(&[
                ("plugin.json", &plugin_json("weekly.tools")),
                ("skills/summarize/SKILL.md", &document),
            ]);
            let loaded = validate(&files).expect("an invalid skill is skipped");
            assert!(
                loaded.skills.is_empty(),
                "{field:?} must load no skill, loaded {:?}",
                loaded.skills
            );
            assert!(
                loaded
                    .skipped
                    .iter()
                    .any(|report| report.contains("skills/summarize")),
                "{field:?} must be reported"
            );
        }

        let long = "x".repeat(501);
        let document =
            format!("---\nname: summarize\ndescription: Report.\ncompatibility: {long}\n---\n");
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            ("skills/summarize/SKILL.md", &document),
        ]);
        let loaded = validate(&files).unwrap();
        assert!(loaded.skills.is_empty(), "{:?}", loaded.skipped);
    }

    #[test]
    fn skips_a_skill_whose_name_does_not_match_its_directory() {
        for (directory, name) in [
            ("summarize", "summary"),
            ("summarize", "summarize--weekly"),
            ("sum--marize", "sum--marize"),
            ("summarize", "SUMmarize"),
            ("summarize", "summarize."),
        ] {
            let document = skill_md(name, "Report.");
            let key = format!("skills/{directory}/SKILL.md");
            let files = package(&[
                ("plugin.json", &plugin_json("weekly.tools")),
                (key.as_str(), document.as_str()),
            ]);
            let loaded = validate(&files).expect("a misnamed skill is skipped");
            assert!(
                loaded.skills.is_empty(),
                "{directory}/{name} must load no skill, loaded {:?}",
                loaded.skills
            );
            assert!(
                loaded
                    .skipped
                    .iter()
                    .any(|report| report.contains("skills/")),
                "{directory}/{name} must be reported: {:?}",
                loaded.skipped
            );
        }
    }

    #[test]
    fn a_skill_whose_name_matches_its_directory_loads() {
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            (
                "skills/summarize/SKILL.md",
                "---\nname: summarize\ndescription: Report.\nlicense: MIT\ncompatibility: Needs git.\nmetadata:\n  author: ada\n  version: \"1.0\"\nallowed-tools: read_file shell\n---\nBody.",
            ),
        ]);

        let loaded = validate(&files).expect("a conforming skill loads");

        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(loaded.skills[0].name, "summarize");
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
    }

    #[test]
    fn skips_a_skill_whose_script_name_escapes_the_skill() {
        for script in [
            "skills/summarize/scripts/../plugin.json",
            "skills/summarize/scripts/nested/gather.sh",
            "skills/summarize/scripts/.hidden",
        ] {
            let files = package(&[
                ("plugin.json", &plugin_json("weekly.tools")),
                (
                    "skills/summarize/SKILL.md",
                    &skill_md("summarize", "Report."),
                ),
                (script, "#!/bin/sh\n"),
            ]);
            let loaded = validate(&files).expect("an escaping script skips the skill");
            assert!(
                loaded.skills.is_empty(),
                "{script} must load no skill, loaded {:?}",
                loaded.skills
            );
            assert!(
                loaded
                    .skipped
                    .iter()
                    .any(|report| report.contains("skills/summarize")),
                "{script} must be reported"
            );
        }
    }

    #[test]
    fn skips_a_skill_past_a_limit() {
        // A `SKILL.md` past 64 KiB, scripts past 64, and a script past 64 KiB
        // each skip the skill and report the cause.
        let big_body = format!(
            "---\nname: summarize\ndescription: Report.\n---\n{}",
            "x".repeat(neha::record::MAX_SKILL_BODY_BYTES)
        );
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            ("skills/summarize/SKILL.md", &big_body),
        ]);
        let loaded = validate(&files).unwrap();
        assert!(loaded.skills.is_empty(), "{:?}", loaded.skipped);
        assert!(
            loaded
                .skipped
                .iter()
                .any(|report| report.contains("SKILL.md")),
            "{:?}",
            loaded.skipped
        );

        let manifest = plugin_json("weekly.tools");
        let skill = skill_md("summarize", "Report.");
        let script = "#!/bin/sh\n";
        let mut many = package(&[
            ("plugin.json", &manifest),
            ("skills/summarize/SKILL.md", &skill),
        ]);
        for index in 0..65 {
            many.insert(
                format!("skills/summarize/scripts/script-{index}.sh"),
                script.as_bytes().to_vec(),
            );
        }
        let loaded = validate(&many).unwrap();
        assert!(loaded.skills.is_empty(), "{:?}", loaded.skipped);
        assert!(
            loaded
                .skipped
                .iter()
                .any(|report| report.contains("scripts")),
            "{:?}",
            loaded.skipped
        );

        let big_script = "x".repeat(neha::record::MAX_SKILL_BODY_BYTES + 1);
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            ("skills/summarize/SKILL.md", &skill),
            ("skills/summarize/scripts/gather.sh", &big_script),
        ]);
        let loaded = validate(&files).unwrap();
        assert!(loaded.skills.is_empty(), "{:?}", loaded.skipped);
        assert!(
            loaded
                .skipped
                .iter()
                .any(|report| report.contains("gather.sh")),
            "{:?}",
            loaded.skipped
        );
    }

    #[test]
    fn a_skill_is_an_immediate_child_of_skills() {
        let files = package(&[
            ("plugin.json", &plugin_json("weekly.tools")),
            ("skills/SKILL.md", &skill_md("summarize", "Report.")),
            (
                "skills/nested/deeper/SKILL.md",
                &skill_md("summarize", "Report."),
            ),
            (
                "skills/summarize/SKILL.md",
                &skill_md("summarize", "Report."),
            ),
        ]);

        let loaded = validate(&files).unwrap();

        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(loaded.skills[0].directory, "summarize");
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
    }

    #[test]
    fn a_github_tarball_loads_end_to_end() {
        let bytes = github_tarball("weekly-tools-0123456");
        let mut skipped = Vec::new();
        let files = extract_archive(&bytes, &mut skipped).expect("a repository archive unpacks");

        let loaded = validate(&files).expect("a repository archive validates");

        assert!(skipped.is_empty(), "{skipped:?}");
        assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
        assert_eq!(loaded.manifest.name, "weekly.tools");
        assert_eq!(loaded.skills.len(), 1);
        assert_eq!(loaded.skills[0].name, "summarize");
        assert_eq!(loaded.skills[0].scripts.len(), 1);
        assert_eq!(loaded.skills[0].scripts[0].name, "gather.sh");
        assert_eq!(
            digest(&files),
            digest(&read_directory_package()),
            "a package digest names its entries, not its reader"
        );
    }

    /// The same package the GitHub tarball carries, read from a directory.
    fn read_directory_package() -> BTreeMap<String, Vec<u8>> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("weekly-tools-0123456");
        std::fs::create_dir_all(root.join("skills/summarize/scripts")).unwrap();
        std::fs::write(root.join("plugin.json"), plugin_json("weekly.tools")).unwrap();
        std::fs::write(
            root.join("skills/summarize/SKILL.md"),
            skill_md("summarize", "Write the weekly status report."),
        )
        .unwrap();
        std::fs::write(
            root.join("skills/summarize/scripts/gather.sh"),
            "#!/bin/sh\n",
        )
        .unwrap();

        let mut skipped = Vec::new();

        read_directory(&root, &mut skipped).unwrap()
    }

    /// Serves one scripted archive fetch.
    async fn archive_server() -> String {
        use axum::extract::Path;
        use axum::http::header::LOCATION;
        use axum::http::{HeaderValue, StatusCode};
        use axum::response::Response;
        use axum::routing::get;
        use axum::{Router, body::Body};

        let app = Router::new().route(
            "/{*name}",
            get(|Path(name): Path<String>| async move {
                let route = name.split('/').next().unwrap_or_default();
                // One route answers late, so a fetch's total timeout can be
                // observed without a long test.
                if route == "slow" {
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
                let (status, headers) = match route {
                    "ok" | "slow" => (StatusCode::OK, Vec::new()),
                    "one" => (StatusCode::FOUND, vec![(LOCATION, "/two")]),
                    "two" => (StatusCode::SEE_OTHER, vec![(LOCATION, "/three")]),
                    "three" => (StatusCode::TEMPORARY_REDIRECT, vec![(LOCATION, "/ok")]),
                    "loop" => (StatusCode::FOUND, vec![(LOCATION, "/loop")]),
                    "away" => (
                        StatusCode::FOUND,
                        vec![(LOCATION, "http://deploy.example.com/archive/abc.tar.gz")],
                    ),
                    "nowhere" => (StatusCode::FOUND, Vec::new()),
                    "big" => (StatusCode::OK, Vec::new()),
                    _ => (StatusCode::NOT_FOUND, Vec::new()),
                };
                let body = if route == "big" {
                    Body::from(vec![0u8; MAX_PACKAGE_BYTES as usize + 1])
                } else if route == "ok" || route == "slow" {
                    Body::from(tarball(&[(
                        "plugin.json",
                        plugin_json("weekly.tools").as_bytes(),
                    )]))
                } else {
                    Body::empty()
                };
                let mut response = Response::builder().status(status);
                for (name, value) in headers {
                    response = response.header(name, HeaderValue::from_static(value));
                }

                response.body(body).unwrap()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        format!("http://{address}")
    }

    #[tokio::test]
    async fn fetches_the_archive_of_a_repository_source() {
        let server = archive_server().await;
        let source = parse_source(&format!("{server}/ok@{COMMIT}")).unwrap();
        let allowed = [server.clone()];

        let bytes = fetch_archive(&source, &allowed)
            .await
            .expect("an archive fetches");

        let mut skipped = Vec::new();
        let files = extract_archive(&bytes, &mut skipped).unwrap();
        assert_eq!(
            files.keys().map(String::as_str).collect::<Vec<_>>(),
            ["plugin.json"]
        );
    }

    #[tokio::test]
    async fn follows_three_redirects_and_no_more() {
        let server = archive_server().await;
        let allowed = [server.clone()];
        let source = parse_source(&format!("{server}/ok@{COMMIT}")).unwrap();
        let redirected = repository(&format!("{server}/one"));
        let looped = repository(&format!("{server}/loop"));

        assert_eq!(
            fetch_archive(&redirected, &allowed).await.unwrap(),
            fetch_archive(&source, &allowed).await.unwrap(),
            "three redirects are followed"
        );
        assert!(matches!(
            fetch_archive(&looped, &allowed).await,
            Err(PluginError::Fetch(_))
        ));
    }

    #[tokio::test]
    async fn fetches_a_redirect_only_within_the_allowed_urls() {
        let server = archive_server().await;
        // The source and every redirect answer an allowed prefix, so the chain
        // to `/ok` is followed.
        let allowed = [
            format!("{server}/one"),
            format!("{server}/two"),
            format!("{server}/three"),
            format!("{server}/ok"),
        ];
        let bytes = fetch_archive(&repository(&format!("{server}/one")), &allowed)
            .await
            .expect("every redirect is below an allowed URL");
        let mut skipped = Vec::new();
        extract_archive(&bytes, &mut skipped).expect("the fetched archive unpacks");

        // A redirect outside the allowed prefix is refused before it is
        // followed, even on the same origin.
        let error = fetch_archive(
            &repository(&format!("{server}/one")),
            &[format!("{server}/one")],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, PluginError::NotAllowed(_)),
            "answered {error}"
        );

        // A redirect to a different origin is refused as well.
        let error = fetch_archive(&repository(&format!("{server}/away")), &[server])
            .await
            .unwrap_err();
        assert!(
            matches!(error, PluginError::NotAllowed(_) | PluginError::Fetch(_)),
            "answered {error}"
        );
    }

    #[tokio::test]
    async fn refuses_what_a_fetch_may_not_answer() {
        let server = archive_server().await;
        let allowed = [server.clone()];
        for url in [
            format!("{server}/nowhere"),
            format!("{server}/absent"),
            format!("{server}/away"),
            "http://deploy.example.com/weekly".to_owned(),
        ] {
            let error = fetch_archive(&repository(&url), &allowed)
                .await
                .unwrap_err();
            assert!(
                matches!(error, PluginError::Fetch(_) | PluginError::NotAllowed(_)),
                "answered {error}"
            );
        }

        // A source no allowed URL covers is refused before any request.
        let error = fetch_archive(
            &repository(&format!("{server}/ok")),
            &[format!("{server}/absent")],
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, PluginError::NotAllowed(_)),
            "answered {error}"
        );

        let big = repository(&format!("{server}/big"));
        assert!(matches!(
            fetch_archive(&big, &allowed).await,
            Err(PluginError::TooLarge(_))
        ));
    }

    #[tokio::test]
    async fn an_archive_fetch_times_out() {
        let server = archive_server().await;
        let allowed = [server.clone()];
        let source = repository(&format!("{server}/slow"));

        let error = fetch_archive_within(&source, &allowed, Duration::from_millis(50))
            .await
            .unwrap_err();

        assert!(matches!(error, PluginError::Fetch(_)), "answered {error}");
    }

    #[tokio::test]
    async fn a_directory_source_is_not_fetched() {
        let dir = tempfile::tempdir().unwrap();

        assert!(matches!(
            fetch_archive(
                &Source::Directory {
                    path: dir.path().to_owned()
                },
                &[]
            )
            .await,
            Err(PluginError::NotFetched)
        ));
    }
}

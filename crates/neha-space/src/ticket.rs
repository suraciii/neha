//! The one-use ticket that pairs this machine as a Local Computer.
//!
//! `neha serve` writes a ticket to the User's configuration directory;
//! `neha computer pair` reads it and exchanges it for a Computer and its
//! credential. The ticket is short-lived, works once, and binds one start of
//! the Space: the configuration directory is hidden from Bots, the file is
//! written `0600`, and only the ticket's digest is stored.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use neha::id::Id;
use rand::RngCore as _;
use thiserror::Error;

use crate::home::Home;
use crate::identity::{Identity, IdentityError};

/// The file name of the local ticket inside the configuration directory.
pub const TICKET_FILE: &str = "local-computer.ticket";

/// How long an issued ticket stays valid.
pub const TICKET_TTL: Duration = Duration::from_secs(600);

/// The most pairing attempts one address may make per minute.
pub const MAX_PAIR_ATTEMPTS: usize = 10;

/// The most addresses the pairing limit remembers at once.
pub const MAX_PAIR_PEERS: usize = 1024;

/// The window the pairing limit counts.
const PAIR_WINDOW: Duration = Duration::from_secs(60);

/// The entropy of a ticket or a Computer credential.
const SECRET_BYTES: usize = 32;

/// A local ticket could not be issued, read, or used.
#[derive(Debug, Error)]
pub enum TicketError {
    /// The ticket file cannot be written or removed.
    #[error("the ticket file cannot be used: {0}")]
    Io(#[from] std::io::Error),
    /// The Space's identity store failed.
    #[error("identity error: {0}")]
    Identity(#[from] IdentityError),
    /// The ticket file does not hold the expected shape.
    #[error("the ticket file is corrupt: {0}")]
    Corrupt(serde_json::Error),
}

/// The ticket file one start issued, with the digest of its ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartTicket {
    /// Where the ticket file lives.
    pub path: PathBuf,
    /// The digest of the ticket it holds.
    pub hash: [u8; 32],
}

/// Returns the file that carries this start's local ticket.
///
/// The configuration directory is used because the daemon hides it from Bots:
/// a Local Bot's shell and file tools cannot read the plaintext ticket. The
/// Space's own `state` directory is only a fallback for a start with neither
/// `XDG_CONFIG_HOME` nor `HOME`.
#[must_use]
pub fn path(home: &Home) -> PathBuf {
    path_from(
        home,
        config_directory(
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        ),
    )
}

/// Returns the ticket path for a known configuration directory.
fn path_from(home: &Home, config: Option<PathBuf>) -> PathBuf {
    if let Some(directory) = home.ticket_directory() {
        return directory.join(TICKET_FILE);
    }

    config.map_or_else(
        || home.root().join("state").join(TICKET_FILE),
        |directory| directory.join(TICKET_FILE),
    )
}

/// Returns the configuration directory from the variables that may name it.
///
/// A relative `XDG_CONFIG_HOME` is ignored, as the XDG base directory
/// specification requires; without either variable there is no directory and
/// the ticket falls back to the Space's home.
fn config_directory(
    xdg_config_home: Option<PathBuf>,
    user_home: Option<PathBuf>,
) -> Option<PathBuf> {
    xdg_config_home
        .filter(|path| path.is_absolute())
        .or_else(|| user_home.map(|home| home.join(".config")))
        .map(|base| base.join("neha"))
}

/// Returns a fresh secret: a ticket, or a Computer credential.
#[must_use]
pub fn secret() -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Issues a ticket for `owner`.
///
/// The plaintext goes to the caller; the database keeps only its digest.
pub async fn issue(identity: &Identity, owner: Id) -> Result<String, TicketError> {
    let ticket = secret();
    identity
        .remember_local_ticket(&ticket, owner, TICKET_TTL)
        .await?;

    Ok(ticket)
}

/// Issues the ticket a start writes for its Admin.
pub async fn issue_for_start(
    identity: &Identity,
    path: &Path,
    owner: Id,
    url: &str,
) -> Result<StartTicket, TicketError> {
    let ticket = secret();
    let hash = identity
        .remember_local_ticket(&ticket, owner, TICKET_TTL)
        .await?;
    write(path, &ticket, url).await?;

    Ok(StartTicket {
        path: path.to_path_buf(),
        hash,
    })
}

/// Forgets every ticket in the database.
pub async fn clear(identity: &Identity) -> Result<(), TicketError> {
    identity.clear_local_tickets().await?;

    Ok(())
}

/// Claims `ticket` for one pairing, returning its owner.
///
/// A ticket that is unknown, expired, or already used gives `None`. The file
/// is removed only when the claimed ticket is the one this start wrote: a
/// ticket issued to another User through the HTTP endpoint leaves the Admin's
/// start ticket in place.
pub async fn claim(
    identity: &Identity,
    start: Option<&StartTicket>,
    ticket: &str,
) -> Result<Option<Id>, TicketError> {
    let Some((owner, hash)) = identity.claim_local_ticket(ticket).await? else {
        return Ok(None);
    };
    if let Some(start) = start
        && start.hash == hash
    {
        // The ticket is already spent; a file that cannot be removed is left
        // for the next start to clear rather than failing the pairing.
        let _ = remove_file(&start.path).await;
    }

    Ok(Some(owner))
}

/// Deletes a ticket file, if it exists.
pub async fn remove_file(path: &Path) -> Result<(), TicketError> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Writes the ticket file `0600` before any pairing can read it.
async fn write(path: &Path, ticket: &str, url: &str) -> Result<(), TicketError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let body = serde_json::to_vec(&serde_json::json!({"ticket": ticket, "url": url}))
        .map_err(TicketError::Corrupt)?;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    tokio::io::AsyncWriteExt::write_all(&mut file, &body).await?;
    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).await?;
    }

    Ok(())
}

/// Limits how often one address may try to pair.
///
/// Pairing is unauthenticated, so the limit lives in memory: a restart clears
/// it, and a flap of the Space is not a way around it within one start. The
/// map is bounded so an attacker cycling addresses cannot grow it without
/// limit: empty entries are pruned, and past the bound the address that was
/// heard from longest ago is forgotten.
#[derive(Debug, Default)]
pub struct PairLimiter {
    attempts: HashMap<IpAddr, VecDeque<Instant>>,
}

impl PairLimiter {
    /// Records an attempt; false when this address is over its minute quota.
    pub fn allows(&mut self, peer: IpAddr) -> bool {
        let now = Instant::now();
        if self.attempts.len() >= MAX_PAIR_PEERS / 2 {
            self.attempts.retain(|_, attempts| {
                prune(attempts, now);
                !attempts.is_empty()
            });
        }
        while self.attempts.len() >= MAX_PAIR_PEERS && !self.attempts.contains_key(&peer) {
            let Some(oldest) = self
                .attempts
                .iter()
                .min_by_key(|(_, attempts)| attempts.back().copied())
                .map(|(peer, _)| *peer)
            else {
                break;
            };
            self.attempts.remove(&oldest);
        }
        let attempts = self.attempts.entry(peer).or_default();
        prune(attempts, now);
        if attempts.len() >= MAX_PAIR_ATTEMPTS {
            return false;
        }
        attempts.push_back(now);

        true
    }
}

/// Forgets the attempts that fell out of the window.
fn prune(attempts: &mut VecDeque<Instant>, now: Instant) {
    while attempts
        .front()
        .is_some_and(|at| now.duration_since(*at) > PAIR_WINDOW)
    {
        attempts.pop_front();
    }
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::{
        MAX_PAIR_ATTEMPTS, MAX_PAIR_PEERS, PairLimiter, TICKET_FILE, config_directory, path,
        path_from,
    };
    use crate::home::Home;

    #[test]
    fn one_address_gets_ten_attempts_a_minute() {
        let mut limiter = PairLimiter::default();
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        for _ in 0..MAX_PAIR_ATTEMPTS {
            assert!(limiter.allows(peer));
        }
        assert!(
            !limiter.allows(peer),
            "the eleventh attempt in a minute waits"
        );
        assert!(
            limiter.allows(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))),
            "another address has its own quota"
        );
    }

    #[test]
    fn many_addresses_cannot_grow_the_limiter_without_bound() {
        let mut limiter = PairLimiter::default();
        for address in 1..=(MAX_PAIR_PEERS as u32 * 4) {
            assert!(limiter.allows(IpAddr::V4(Ipv4Addr::from(address))));
        }
        assert!(
            limiter.attempts.len() <= MAX_PAIR_PEERS,
            "the map is bounded: {} addresses",
            limiter.attempts.len()
        );

        // An entry whose attempts all left the window is cleaned out by the
        // periodic prune.
        let stale = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        limiter.attempts.insert(
            stale,
            [Instant::now()
                .checked_sub(Duration::from_secs(120))
                .expect("an instant two minutes ago")]
            .into(),
        );
        assert!(limiter.allows(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9))));
        assert!(
            !limiter.attempts.contains_key(&stale),
            "an address with no attempts left in the window is forgotten"
        );
    }

    #[test]
    fn the_ticket_prefers_the_configuration_directory() {
        let directory = tempfile::tempdir().unwrap();
        let home = Home::open(directory.path()).unwrap();

        let config = PathBuf::from("/home/ada/.config/neha");
        assert_eq!(
            path_from(&home, Some(config.clone())),
            config.join(TICKET_FILE)
        );
        assert_eq!(
            path_from(&home, None),
            home.root().join("state").join(TICKET_FILE),
            "without a configuration directory the Space's own state holds it"
        );
        assert_eq!(
            home.ticket_directory(),
            None,
            "a plain home has no named ticket directory"
        );
    }

    #[test]
    fn the_configuration_directory_follows_the_environment_rules() {
        let home = PathBuf::from("/home/ada");
        assert_eq!(
            config_directory(Some(PathBuf::from("/xdg")), Some(home.clone())),
            Some(PathBuf::from("/xdg/neha"))
        );
        assert_eq!(
            config_directory(Some(PathBuf::from("relative")), Some(home.clone())),
            Some(PathBuf::from("/home/ada/.config/neha")),
            "a relative XDG_CONFIG_HOME is ignored"
        );
        assert_eq!(
            config_directory(None, Some(home)),
            Some(PathBuf::from("/home/ada/.config/neha"))
        );
        assert_eq!(
            config_directory(None, None),
            None,
            "without HOME or XDG the Space's state directory is the fallback"
        );
    }

    /// Runs the path resolution in a child process with a chosen environment.
    ///
    /// Tests cannot change the process environment themselves (the workspace
    /// forbids unsafe code, and parallel tests share it), so the fallback and
    /// the variable precedence are checked where the environment is clean.
    #[test]
    fn the_ticket_path_follows_the_environment_and_falls_back() {
        if std::env::var_os("NEHA_TEST_TICKET_CASE").is_some() {
            let root = std::env::var_os("NEHA_TEST_TICKET_HOME").expect("a test home");
            let home = Home::open(root).unwrap();
            println!("TICKET_PATH={}", path(&home).display());

            return;
        }

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("space");
        let xdg = directory.path().join("xdg");
        let user_home = directory.path().join("user-home");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::create_dir_all(&user_home).unwrap();

        let child = |case: &str, extra: &[(&str, &std::path::Path)]| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .arg("--exact")
                .arg("ticket::tests::the_ticket_path_follows_the_environment_and_falls_back")
                .arg("--nocapture")
                .env_clear()
                .env("NEHA_TEST_TICKET_CASE", case)
                .env("NEHA_TEST_TICKET_HOME", &root);
            for (name, value) in extra {
                command.env(name, value);
            }
            let output = command.output().expect("the child test process starts");
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "the child resolution failed for {case}: {stdout}"
            );
            stdout
                .lines()
                .find_map(|line| line.strip_prefix("TICKET_PATH="))
                .unwrap_or_else(|| panic!("no TICKET_PATH for {case}: {stdout}"))
                .to_owned()
        };

        assert!(
            child("fallback", &[]).ends_with("/state/local-computer.ticket"),
            "without HOME or XDG the Space's state directory holds the ticket"
        );
        assert!(
            child("xdg", &[("XDG_CONFIG_HOME", &xdg), ("HOME", &user_home)])
                .ends_with(&format!("{}/neha/local-computer.ticket", xdg.display())),
            "XDG_CONFIG_HOME holds the ticket first"
        );
        assert!(
            child("home", &[("HOME", &user_home)]).ends_with(&format!(
                "{}/.config/neha/local-computer.ticket",
                user_home.display()
            )),
            "HOME's .config holds the ticket without XDG_CONFIG_HOME"
        );
    }

    #[test]
    fn a_named_ticket_directory_wins_over_the_environment() {
        let directory = tempfile::tempdir().unwrap();
        let config = directory.path().join("config");
        let home = Home::open_with_ticket_directory(directory.path(), &config).unwrap();
        assert_eq!(
            path(&home),
            config.join(TICKET_FILE),
            "a test names its own configuration directory"
        );
    }
}

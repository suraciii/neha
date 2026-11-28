//! The CLI's integration tests, one binary.

mod cli;
mod diagnostics;

#[cfg(unix)]
mod cli_grant;
mod client;
mod computer;

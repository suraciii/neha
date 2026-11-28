//! `neha` runs a Space and talks to its Bots from a terminal.
//!
//! The binary is a thin entry point into [`cli::run`]. [`cli`] parses the
//! command line and runs the command; [`client`] is the CLI's connection to one
//! Space; [`computer`] is the machine side of a Local Computer.

pub mod cli;
pub mod client;
pub mod computer;

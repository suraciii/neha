//! Neha's domain library.
//!
//! Capabilities enter this crate as modules, each backed by a contract under
//! `specs/`. `eng/architecture.md` records which crate owns what.
//!
//! This crate holds the record's shared vocabulary: identifiers, sequence
//! numbers, timestamps, stable error codes, event classification, and
//! the bounds a File must fit.

pub mod error;
pub mod event;
pub mod file;
pub mod folder;
pub mod id;
pub mod record;
pub mod seq;
pub mod surface;
pub mod time;
pub mod usage;

mod ts;

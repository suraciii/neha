//! Neha's domain library.
//!
//! Capabilities enter this crate as modules, each backed by a contract under
//! `specs/`. `eng/architecture.md` records which crate owns what.
//!
//! This crate holds the record's shared vocabulary: identifiers, sequence
//! numbers, timestamps, stable error codes, and event classification.

pub mod error;
pub mod event;
pub mod id;
pub mod record;
pub mod seq;
pub mod surface;
pub mod time;
pub mod usage;

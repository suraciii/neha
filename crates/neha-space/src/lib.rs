//! The Space's home directory and its SQLite record.
//!
//! A Space keeps one home directory. The record is its source of truth, and
//! every change enters it as an event whose sequence number is the cursor a
//! reader resumes from.

pub mod home;
pub mod store;

pub use home::{Home, HomeError, SCHEMA_VERSION};
pub use store::{AppendRequest, AppendResult, EventPage, Projection, Store, StoreError};

//! The built-in Bot loop.
//!
//! The loop speaks one provider-neutral Model vocabulary, runs every
//! requested action through a [`neha_computer::ComputerRuntime`] once the
//! Space approves it, moves Files through a [`files::FileHost`], and writes
//! its replies, steps and actions through a [`run::Recorder`]. A run that
//! waits for an approval ends and resumes from the record. The Space supplies
//! the Model adapter, the approvals, the File host, and the record
//! implementation.

pub mod files;
pub mod model;
pub mod run;
pub mod tools;

pub use files::{DeliveredFile, FileHost, TakenFile};
pub use model::{Message, Model, ModelFailure, ModelReply, ModelRequest, Role, ToolCall, ToolSpec};
pub use run::{
    Decision, LoopError, RecordedCall, Recorder, Resume, RunOutcome, RunRequest, WaitingApproval,
    call_id, run,
};

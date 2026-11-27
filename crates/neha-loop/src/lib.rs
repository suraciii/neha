//! The built-in Bot loop.
//!
//! The loop speaks one provider-neutral Model vocabulary, runs every
//! requested action through a [`neha_computer::ComputerRuntime`] once the
//! Space approves it, and writes its replies, steps and actions through a
//! [`run::Recorder`]. A run that waits for an approval ends and resumes from
//! the record. The Space supplies the Model adapter, the approvals, and the
//! record implementation.

pub mod model;
pub mod run;
pub mod tools;

pub use model::{
    Continuation, Message, Model, ModelFailure, ModelReply, ModelRequest, Role, ToolCall, ToolSpec,
};
pub use run::{
    Authorization, Decision, Dispatched, LoopError, McpIdentity, RecordedCall, Recorder, Resume,
    RunOutcome, RunRequest, Runtime, ScriptIdentity, WaitingApproval, call_id, run,
};
pub use tools::SpaceCall;

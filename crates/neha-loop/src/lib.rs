//! The built-in Bot loop.
//!
//! The loop speaks one provider-neutral Model vocabulary, executes every
//! requested action through a [`neha_computer::ComputerRuntime`], moves Files
//! through a [`files::FileHost`], and writes its steps and actions through a
//! [`run::Recorder`]. The Space supplies the Model adapter, the File host,
//! and the record implementation.

pub mod files;
pub mod model;
pub mod run;
pub mod tools;

pub use files::{DeliveredFile, FileHost, TakenFile};
pub use model::{Message, Model, ModelFailure, ModelReply, ModelRequest, Role, ToolCall, ToolSpec};
pub use run::{LoopError, Recorder, RunOutcome, RunRequest, run};

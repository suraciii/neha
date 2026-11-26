//! The Computer's runtime seam and the in-process runtime.
//!
//! A Bot's actions go through [`ComputerRuntime`]; the Space never runs a
//! shell or touches a Bot's files directly. The local runtime is the
//! development and M1 implementation: a workspace directory, POSIX shell and
//! file tools, path containment, output limits, and a timeout that kills the
//! whole process group.

pub mod action;
pub mod local;
pub mod output;

pub use action::{Action, ActionResult, ActionStatus, ComputerRuntime, ToolOutput};
pub use local::{LocalRuntime, RuntimeError};
pub use output::{HEAD_BYTES, TAIL_BYTES, truncate};

//! The Computer's runtime seam and the in-process runtime.
//!
//! A Bot's actions go through [`ComputerRuntime`]; the Space never runs a
//! shell or touches a Bot's files directly. [`LocalRuntime`] has two modes:
//! a managed workspace
//! ([`new`](LocalRuntime::new)) where everything stays inside one directory,
//! and a real machine ([`machine`](LocalRuntime::machine)) where reads reach
//! the whole machine, writes reach one folder, and the shell runs under
//! bubblewrap.
//!
//! [`channel`] holds the JSON frames a real Computer and its Space exchange.

pub mod action;
pub mod channel;
pub mod local;
pub mod output;

pub use action::{Action, ActionResult, ActionStatus, ComputerRuntime, ToolOutput};
pub use channel::{CHANNEL_VERSION, DeviceFrame, HostFrame, MAX_CHANNEL_FRAME_BYTES, WireAction};
pub use local::{LocalRuntime, MachineAccess, RuntimeError};
pub use output::{HEAD_BYTES, TAIL_BYTES, truncate};

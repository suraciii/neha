//! The Computer's runtime seam and the in-process runtime.
//!
//! A Bot's actions go through [`ComputerRuntime`]; the Space never runs a
//! shell or touches a Bot's files directly. [`LocalRuntime`] has two modes:
//! a managed workspace
//! ([`new`](LocalRuntime::new)) where everything stays inside one directory,
//! and a real machine ([`machine`](LocalRuntime::machine)) where reads reach
//! the whole machine, writes reach the Bot's folder, the owner's grants, and
//! the run's own directory, and the shell runs under bubblewrap.
//! [`machine_with_hidden`](LocalRuntime::machine_with_hidden) also hides given
//! root directories from both paths, so a Bot cannot read the Computer's own
//! channel credential. The owner can give a Bot full access instead
//! ([`with_machine_access`](LocalRuntime::with_machine_access)): the shell then
//! runs with no sandbox, and the file tools use the machine user's own rights.
//!
//! A daemon opens its workspace root with
//! [`open_workspace`](LocalRuntime::open_workspace), checks the machine with
//! [`probe_sandbox`](LocalRuntime::probe_sandbox), and opens each Bot's folder
//! under that verified root with [`machine_at`](LocalRuntime::machine_at). A
//! real Bot folder is never a symlink, and a folder holding a hard link, a
//! symlink, or a special file is refused.
//!
//! [`channel`] holds the JSON frames a real Computer and its Space exchange.

pub mod action;
pub mod channel;
pub mod local;
pub mod output;

pub use action::{Action, ActionResult, ActionStatus, ByteError, ComputerRuntime, ToolOutput};
pub use channel::{CHANNEL_VERSION, DeviceFrame, HostFrame, MAX_CHANNEL_FRAME_BYTES, WireAction};
pub use local::{LocalRuntime, MachineAccess, RuntimeError};
pub use output::{HEAD_BYTES, TAIL_BYTES, truncate};

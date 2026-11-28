//! The Space's home directory and its SQLite record.
//!
//! A Space keeps one home directory. The record is its source of truth, and
//! every change enters it as an event whose sequence number is the cursor a
//! reader resumes from.

pub mod blobs;
pub mod catalog;
pub mod commands;
pub mod computer_link;
pub mod config;
pub mod home;
pub mod identity;
pub mod mcp;
pub mod model;
pub mod oauth;
pub mod plugins;
mod proxy;
mod push;
pub mod remote;
pub mod runs;
pub mod schema;
pub mod secrets;
pub mod server;
pub mod skills;
pub mod state;
pub mod stdio;
pub mod store;
pub mod ticket;
pub mod workers;

pub use blobs::{Blobs, BlobsError};
pub use config::{Config, ConfigError, S3Storage, Storage};
pub use home::{Home, HomeError, SCHEMA_VERSION};
pub use identity::{Account, Identity, IdentityError};
pub use model::{ChatMessage, ChatReply, ChatRole, ModelClient, ModelError, Usage};
pub use secrets::{Secrets, SecretsError};
#[cfg(feature = "test-support")]
pub use server::start_with_worker_config;
pub use server::{
    Running, ServerError, SpaceServer, ensure_listener_allowed, start, start_with_ui,
    start_with_ui_public,
};
pub use skills::{SkillDocument, SkillError};
pub use state::{
    BotView, ChatStatus, ChatView, ComputerView, FileView, MessageView, ModelView, ProviderView,
    RunStatus, RunView, State, UserView,
};
pub use store::{AppendRequest, AppendResult, EventPage, Projection, Store, StoreError};

/// What the integration tests need from inside the crate.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    pub use super::commands::test_support as commands;
    pub use super::identity::test_support as identity;
    pub use super::server::test_support as server;
}

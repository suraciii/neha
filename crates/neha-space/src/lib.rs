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
pub mod model;
pub mod remote;
pub mod runs;
pub mod schema;
pub mod secrets;
pub mod server;
pub mod state;
pub mod store;
pub mod ticket;

pub use blobs::{Blobs, BlobsError};
pub use config::{Config, ConfigError, S3Storage, Storage};
pub use home::{Home, HomeError, SCHEMA_VERSION};
pub use identity::{Account, Identity, IdentityError};
pub use model::{ChatMessage, ChatReply, ChatRole, ModelClient, ModelError, Usage};
pub use secrets::{Secrets, SecretsError};
pub use server::{Running, ServerError, SpaceServer, start, start_with_ui};
pub use state::{
    BotView, ChatStatus, ChatView, ComputerView, FileView, MessageView, ModelView, ProviderView,
    RunStatus, RunView, State, UserView,
};
pub use store::{AppendRequest, AppendResult, EventPage, Projection, Store, StoreError};

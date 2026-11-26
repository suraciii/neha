//! The Space's home directory and its SQLite record.
//!
//! A Space keeps one home directory. The record is its source of truth, and
//! every change enters it as an event whose sequence number is the cursor a
//! reader resumes from.

pub mod commands;
pub mod home;
pub mod identity;
pub mod model;
pub mod schema;
pub mod secrets;
pub mod server;
pub mod state;
pub mod store;

pub use home::{Home, HomeError, SCHEMA_VERSION};
pub use identity::{Account, Identity, IdentityError};
pub use model::{ChatMessage, ChatReply, ChatRole, ModelClient, ModelError, Usage};
pub use secrets::{Secrets, SecretsError};
pub use server::{Running, ServerError, SpaceServer, start};
pub use state::{
    BotView, ChatView, ComputerView, MessageView, ModelView, ProviderView, RunStatus, RunView,
    State, UserView,
};
pub use store::{AppendRequest, AppendResult, EventPage, Projection, Store, StoreError};

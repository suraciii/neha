//! Writes the client's wire types from the Rust definitions.
//!
//! `just wire-types` runs this into a fresh directory and diffs it against the
//! committed one, so a removed type cannot leave a stale file behind. One
//! process exports every type: a test binary per crate would export each
//! type's dependencies too, and the second crate's run would truncate the
//! first crate's file with only the types it could see.

use ts_rs::{Config, TS};

/// Exports every listed type to the file its `export_to` attribute names.
macro_rules! export {
    ($config:expr, $($ty:ty),+ $(,)?) => {{
        let config = $config;
        $(
            <$ty as TS>::export(config)
                .unwrap_or_else(|error| panic!("cannot export {}: {error}", stringify!($ty)));
        )+
    }};
}

fn main() {
    let config = Config::from_env();
    export!(
        &config,
        // The record's vocabulary.
        neha::record::ActionFinished,
        neha::record::ActionStarted,
        neha::record::ActionStatus,
        neha::record::ApprovalDecided,
        neha::record::ApprovalReason,
        neha::record::ApprovalRequested,
        neha::record::Author,
        neha::record::BotCreated,
        neha::record::BotMoved,
        neha::record::BotRenamed,
        neha::record::BotUpdated,
        neha::record::ChatCreated,
        neha::record::ComputerCreated,
        neha::record::ComputerSharing,
        neha::record::ComputerUpdated,
        neha::record::Event,
        neha::record::EventKind,
        neha::record::FileAdded,
        neha::record::MessageAdded,
        neha::record::ModelData,
        neha::record::ProviderAdded,
        neha::record::ProviderModelsUpdated,
        neha::record::RecordedCall,
        neha::record::RuleAdded,
        neha::record::RuleEffect,
        neha::record::RuleRemoved,
        neha::record::RunAccess,
        neha::record::RunContext,
        neha::record::RunFinished,
        neha::record::RunQueued,
        neha::record::RunReply,
        neha::record::RunStarted,
        neha::record::RunStatus,
        neha::record::RunStep,
        neha::record::Undoability,
        neha::record::UserJoined,
        // The command surface.
        neha::surface::BotCreate,
        neha::surface::BotUpdate,
        neha::surface::ComputerCreate,
        neha::surface::ComputerUpdate,
        neha::surface::ClientFrame,
        neha::surface::CommandCall,
        neha::surface::FileSummary,
        neha::surface::MessageSend,
        neha::surface::ModelSpec,
        neha::surface::ProviderAdd,
        neha::surface::ProviderConnect,
        neha::surface::ProviderProtocol,
        neha::surface::ServerFrame,
        neha::surface::ThinkingLevel,
        // The rest of the shared vocabulary.
        neha::error::ErrorCode,
        neha::usage::Usage,
        // The Space's projections.
        neha_space::catalog::CatalogRefresh,
        neha_space::catalog::CatalogStatus,
        neha_space::identity::Account,
        neha_space::state::ActionView,
        neha_space::state::ApprovalView,
        neha_space::state::BotView,
        neha_space::state::ChatStatus,
        neha_space::state::ChatView,
        neha_space::state::ComputerView,
        neha_space::state::MessageView,
        neha_space::state::ModelView,
        neha_space::state::ProviderView,
        neha_space::state::RuleView,
        neha_space::state::RunView,
        neha_space::state::StepView,
        neha_space::state::UserView,
    );
}

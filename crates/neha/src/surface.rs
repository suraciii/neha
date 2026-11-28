//! The command surface's wire vocabulary.
//!
//! A client opens one WebSocket, sends `hello`, then commands and
//! subscriptions. Each command is accepted, committed to the record, and
//! confirmed with its result. Events follow the cursor of the record.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::error::ErrorCode;
use crate::id::Id;
use crate::record::{Author, ComputerSharing, Event, RuleEffect, SkillLibrary};
use crate::seq::Seq;
use crate::time::Timestamp;

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: u16 = 3;

/// The largest frame a client or the Space accepts.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// The capability a caller names to receive the memory event kinds.
pub const MEMORY_CAPABILITY: &str = "memory";

/// The capability a caller names to receive the compaction event kind.
pub const COMPACTION_CAPABILITY: &str = "compaction";

/// Returns the capability a caller must name to read one event kind the
/// protocol version does not already carry.
///
/// A kind this function does not name is one every caller of the current
/// protocol version reads; the memory and compaction kinds are the later
/// Presentation kinds a capability admits.
#[must_use]
pub const fn event_capability(kind: crate::record::EventKind) -> Option<&'static str> {
    match kind {
        crate::record::EventKind::MemoryAdded
        | crate::record::EventKind::MemoryRemoved
        | crate::record::EventKind::MemoryAnswered => Some(MEMORY_CAPABILITY),
        crate::record::EventKind::ChatCompacted => Some(COMPACTION_CAPABILITY),
        _ => None,
    }
}

/// A frame a client sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientFrame {
    /// Opens the session and states the protocol version.
    Hello {
        /// The protocol version the client speaks.
        version: u16,
        /// Optional capabilities the client understands.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
    /// Runs one command.
    Command {
        /// The client's identifier for the command.
        id: Id,
        /// The command.
        command: CommandCall,
    },
    /// Reads the record after a cursor.
    Subscribe {
        /// The client's identifier for the subscription.
        id: Id,
        /// The cursor to resume from; the beginning when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<Seq>,
        /// The page size the client asks for.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
}

/// A command and its parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
#[serde(tag = "name", content = "args", rename_all = "snake_case")]
pub enum CommandCall {
    /// Connects a Provider to the Space.
    ProviderAdd(ProviderAdd),
    /// Connects a known vendor whose preset comes from the catalog.
    ProviderConnect(ProviderConnect),
    /// Discovers the Models a Provider offers now.
    ProviderRefresh {
        /// The Provider to refresh.
        provider: Id,
    },
    /// Lists every Provider the catalog knows.
    CatalogList,
    /// Shows the catalog's cache state.
    CatalogStatus,
    /// Fetches the catalog again.
    CatalogRefresh,
    /// Lists the Space's Providers.
    ProviderList,
    /// Lists the Space's Models.
    ModelList,
    /// Makes a Model the Space's default.
    ModelDefault {
        /// The Model the Space uses by default from now on.
        model: Id,
    },
    /// Adds a real Computer the User just paired.
    ComputerAdd {
        /// The pairing code the machine showed.
        code: String,
    },
    /// Lists the caller's Computers.
    ComputerList,
    /// Creates a Hosted Computer.
    ComputerCreate(ComputerCreate),
    /// Changes who may use a Computer.
    ComputerUpdate(ComputerUpdate),
    /// Clears a Hosted Computer's Workspace.
    ComputerReset {
        /// The Hosted Computer to reset.
        computer: Id,
    },
    /// Revokes a Local Computer's credential and disconnects its daemon.
    ComputerRevoke {
        /// The Local Computer to revoke.
        computer: Id,
    },
    /// Approves a CLI's device code as the signed-in User.
    DeviceApprove {
        /// The device code the CLI showed.
        code: String,
    },
    /// Creates a Bot with its Computer and Chat.
    BotCreate(BotCreate),
    /// Changes a Bot's file access.
    BotUpdate(BotUpdate),
    /// Moves a Bot to another Computer its owner owns.
    BotMove {
        /// The Bot.
        bot: Id,
        /// The Computer it should work on.
        computer: Id,
    },
    /// Lists the caller's Bots.
    BotList,
    /// Shows one Bot.
    BotShow {
        /// The Bot.
        bot: Id,
    },
    /// Lists the caller's Chats.
    ChatList,
    /// Lists a Chat's Files.
    FileList {
        /// The Chat.
        chat: Id,
    },
    /// Reads a Chat's Messages.
    ChatRead {
        /// The Chat.
        chat: Id,
        /// Starts after this cursor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<Seq>,
    },
    /// Sends a Message to a Chat.
    MessageSend(MessageSend),
    /// Shows one run.
    RunShow {
        /// The run.
        run: Id,
    },
    /// Lists the approvals the caller can answer.
    ApprovalList {
        /// Only this Chat's approvals, when named.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chat: Option<Id>,
    },
    /// Answers an approval.
    ApprovalDecide {
        /// The approval.
        approval: Id,
        /// Whether the action may run.
        allow: bool,
        /// Whether the answer also makes a lasting rule.
        lasting: bool,
    },
    /// Lists lasting rules the caller made or can manage.
    RuleList {
        /// Only this Bot's rules, when named.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bot: Option<Id>,
    },
    /// Adds a lasting rule.
    RuleAdd {
        /// The Bot it constrains.
        bot: Id,
        /// Whether matching actions ask or run without asking.
        effect: RuleEffect,
        /// The tool name.
        tool: String,
        /// The shell command prefix a matching shell action starts with.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command_prefix: Option<String>,
        /// The path prefix a matching file action's path starts with.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path_prefix: Option<String>,
        /// The skill whose script a `run_skill_script` rule matches, by name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        skill: Option<String>,
        /// The script a `run_skill_script` rule matches, within that skill.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        script: Option<String>,
        /// The MCP server whose tool the rule matches, by name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        server: Option<String>,
    },
    /// Removes a lasting rule.
    RuleRemove {
        /// The rule.
        rule: Id,
    },
    /// Adds a skill to the Space's library or to a Bot's.
    SkillAdd(SkillAdd),
    /// Lists skills: the Space's and the caller's Bots', or one Bot's when named.
    SkillList {
        /// Only this Bot's enabled skills, when named.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bot: Option<Id>,
    },
    /// Removes a skill from its library.
    SkillRemove {
        /// The skill.
        skill: Id,
    },
    /// Lets a Bot use a skill.
    BotSkillEnable {
        /// The Bot.
        bot: Id,
        /// The skill.
        skill: Id,
    },
    /// Stops a Bot using a skill.
    BotSkillDisable {
        /// The Bot.
        bot: Id,
        /// The skill.
        skill: Id,
    },
    /// Connects an MCP server.
    McpAdd(McpAdd),
    /// Lists the MCP servers the caller may see.
    McpList,
    /// Disconnects an MCP server.
    McpRemove {
        /// The server.
        server: Id,
    },
    /// Connects the signed-in User to an existing server with their own
    /// credential.
    McpConnect {
        /// The server.
        server: Id,
        /// The credential to attach for the caller.
        credential: Option<String>,
    },
    /// Starts an MCP sign-in; the answer carries a browser URL to open.
    McpSignInStart(McpSignInStart),
    /// Reports what became of a sign-in.
    McpSignInStatus {
        /// The sign-in the start answered with.
        sign_in: Id,
    },
    /// Finishes a sign-in a command line started with a loopback redirect.
    McpSignInFinish {
        /// The sign-in the start answered with.
        sign_in: Id,
        /// The authorization code the loopback receiver received.
        code: String,
        /// The state token the authorization server returned unchanged.
        state: String,
        /// The path the loopback receiver received the callback at.
        ///
        /// The Space's plan names an issuer-keyed callback path, so a code
        /// that arrived on another issuer's path belongs to another flow: the
        /// finish compares the path the command line received with the path of
        /// the registered redirect and refuses a mismatch before exchanging
        /// the code. The receiver strips the query it got with the callback.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        /// The issuer the authorization server named, when it named one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        iss: Option<String>,
    },
    /// Lets a Bot call an MCP server's tools.
    BotMcpEnable {
        /// The Bot.
        bot: Id,
        /// The server.
        server: Id,
    },
    /// Stops a Bot calling an MCP server's tools.
    BotMcpDisable {
        /// The Bot.
        bot: Id,
        /// The server.
        server: Id,
    },
    /// Installs an Agent Plugins package from an allowed source.
    PluginInstall {
        /// The source: a repository URL pinned to a commit, or a directory.
        source: String,
    },
    /// Lists the installed packages.
    PluginList,
    /// Removes an installed package and what it brought.
    PluginRemove {
        /// The package.
        plugin: Id,
    },
    /// Lets the Space offer a package.
    PluginEnable {
        /// The package.
        plugin: Id,
    },
    /// Stops the Space offering a package.
    PluginDisable {
        /// The package.
        plugin: Id,
    },
    /// Lists a Bot's memory entries and the room each kind has left.
    MemoryList {
        /// The Bot.
        bot: Id,
    },
    /// Removes one memory entry of a Bot.
    MemoryRemove {
        /// The Bot the entry must belong to; the removal is refused when the
        /// entry belongs to another Bot.
        bot: Id,
        /// The entry.
        memory: Id,
    },
}

/// The parameters of `mcp_add`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
#[serde(deny_unknown_fields)]
pub struct McpAdd {
    /// The name the tool names and a User's commands use.
    pub name: String,
    /// The address the server speaks Streamable HTTP at.
    ///
    /// Empty (or absent) names no new server: it attaches the Space's
    /// credential to the existing stdio server of this name, which a package
    /// brought, and which only the Admin may do.
    #[serde(default)]
    pub url: String,
    /// Where the credential touches the record.
    pub scope: crate::record::McpScope,
    /// The credential, which the Space stores encrypted. Absent connects the
    /// server without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
}

/// The parameters of `mcp_sign_in_start`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
#[serde(deny_unknown_fields)]
pub struct McpSignInStart {
    /// An existing server to attach a credential to; `None` registers
    /// `name`/`url` after the sign-in finishes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Id>,
    /// The name a new server takes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The address a new server speaks Streamable HTTP at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Where the credential belongs when the server is new.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<crate::record::McpScope>,
    /// The loopback redirect base a command line owns, when the flow should
    /// finish there rather than in the Space's browser callback.
    ///
    /// The Space appends the discovered issuer's key and `/callback` to this
    /// base, so the command line's receiver must accept any callback path. The
    /// base is plain HTTP on a loopback address with an explicit port, such as
    /// `http://127.0.0.1:4711`; any path the caller writes is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
}

/// The parameters of `skill_add`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
#[serde(deny_unknown_fields)]
pub struct SkillAdd {
    /// The library that holds the skill.
    pub library: SkillLibrary,
    /// The Bot whose library holds it, when it is a Bot's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<Id>,
    /// The text of its `SKILL.md`, frontmatter included.
    pub document: String,
    /// The scripts it carries, each under its file name.
    #[serde(default)]
    pub scripts: Vec<SkillAddScript>,
}

/// A script a `skill_add` command carries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
#[serde(deny_unknown_fields)]
pub struct SkillAddScript {
    /// The script's file name within the skill, one path component.
    pub name: String,
    /// The script's bytes.
    pub content: String,
}

/// The parameters of `provider_add`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
pub struct ProviderAdd {
    /// The provider protocol spoken.
    pub protocol: ProviderProtocol,
    /// The base address of the provider.
    pub base_url: String,
    /// The API key, which the Space stores encrypted.
    pub api_key: String,
    /// The Models this Provider offers, empty only when discovery is asked.
    pub models: Vec<ModelSpec>,
    /// The Model the Space uses by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// The Space's default thinking level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
    /// A catalog vendor, when the Models should come from the catalog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Ask the Provider for its Models instead of naming them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub discover: bool,
}

/// The parameters of `provider_connect`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
pub struct ProviderConnect {
    /// The catalog vendor id, such as `openai`.
    pub vendor: String,
    /// The API key, which the Space stores encrypted.
    pub api_key: String,
    /// A base address that overrides the preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// A protocol that overrides the preset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<ProviderProtocol>,
    /// The Space's default thinking level.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
}

/// A provider protocol the Space can speak.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
#[serde(rename_all = "snake_case")]
pub enum ProviderProtocol {
    /// OpenAI Chat Completions.
    ChatCompletions,
    /// OpenAI Responses.
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// Anthropic Messages.
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
}

/// How much thought a Model should give a reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// Fastest and cheapest.
    Minimal,
    /// A little more thought.
    Low,
    /// The usual balance.
    Medium,
    /// More thought, more time, more cost.
    High,
    /// The most thought the Model offers.
    Max,
}

impl ThinkingLevel {
    /// Every level, cheapest first.
    pub const ALL: [Self; 5] = [
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::Max,
    ];
}

/// A Model as the Admin declares it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
pub struct ModelSpec {
    /// The model name the provider expects.
    pub name: String,
    /// The context window in tokens.
    pub context_limit: u32,
    /// The largest output in tokens.
    pub max_output: u32,
    /// Whether the Model reads images.
    pub reads_images: bool,
    /// Whether the Model can think before it answers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub thinks: bool,
    /// Whether the Model can call tools.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub tools: bool,
    /// The thinking levels the Model supports; empty means every level.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thinking_levels: Vec<ThinkingLevel>,
}

/// The default for capabilities every Model had before they were recorded.
fn default_true() -> bool {
    true
}

/// Whether a capability keeps its broad default on the wire.
fn is_true(value: &bool) -> bool {
    *value
}

/// The parameters of `bot_create`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
pub struct BotCreate {
    /// The Bot's name.
    pub name: String,
    /// Where the Bot works.
    ///
    /// An absent value means the owner's Space Computer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub computer: Option<BotComputer>,
    /// The Bot's thinking level; absent uses the Space's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
    /// The Bot's Model; absent uses the Space's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Id>,
}

/// The parameters of `computer_create`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
pub struct ComputerCreate {
    /// The Computer's name for its owner.
    pub name: String,
    /// Who may use it.
    pub sharing: ComputerSharing,
}

/// The parameters of `computer_update`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
pub struct ComputerUpdate {
    /// The Computer to change.
    pub computer: Id,
    /// Who may use it now.
    pub sharing: ComputerSharing,
}

/// The parameters of `bot_update`.
///
/// An update changes access one way: it replaces the grant list, or adds one
/// grant, or removes one grant, and may set full access alongside. The Space
/// applies a grant change to its current projection, so two owners' commands
/// that arrive together cannot lose one another's change. An update may also
/// rename the Bot; the Space records a rename as its own event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
pub struct BotUpdate {
    /// The Bot to change.
    pub bot: Id,
    /// Its new name.
    ///
    /// The Space trims it and refuses an empty name; absent leaves the name
    /// unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether it may write outside its Workspace fence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_access: Option<bool>,
    /// The version of the owner's answer that allows full access.
    ///
    /// A request that turns full access on must carry the version this build
    /// states: a client written before the answer was versioned never sends
    /// it, so it cannot widen a Bot's access by asking for it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_access_consent: Option<u16>,
    /// The complete list of extra folders it may write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grants: Option<Vec<String>>,
    /// One folder to add to the grants the Bot has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_add: Option<String>,
    /// One folder to take away from the grants the Bot has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_remove: Option<String>,
    /// The Model the Bot uses; absent leaves it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Id>,
    /// The Bot's thinking level; absent leaves it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
    /// Whether it goes back to the Space's default Model.
    ///
    /// The Space refuses a reset together with a Model: a request either
    /// chooses one or restores the default.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset_model: bool,
    /// Whether it goes back to the Space's default thinking level.
    ///
    /// The Space refuses a reset together with a level.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset_thinking: bool,
}

/// The Computer a `bot_create` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BotComputer {
    /// The owner's paired Local Computer.
    Local,
    /// A new exclusive Hosted Computer.
    Hosted,
    /// One Computer the caller already owns.
    Id(Id),
}

impl Serialize for BotComputer {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Local => serializer.serialize_str("local"),
            Self::Hosted => serializer.serialize_str("hosted"),
            Self::Id(id) => id.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for BotComputer {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        match value.as_str() {
            "local" => Ok(Self::Local),
            "hosted" => Ok(Self::Hosted),
            _ => value
                .parse::<Id>()
                .map(Self::Id)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// The parameters of `message_send`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
pub struct MessageSend {
    /// The Chat to post in.
    pub chat: Id,
    /// The Message text, empty only when the Message carries a File.
    pub text: String,
    /// The File the Message carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Id>,
    /// The directory this run works in; absent means the Bot's Workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Whether the User authorizes writing in that directory for this run
    /// alone.
    ///
    /// A request that names a directory must carry `true`: a Client written
    /// before the marker existed never sends it, so the Space refuses the
    /// request instead of recording a directory as a write root the User did
    /// not name for writing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_writable: Option<bool>,
}

/// A frame the Space sends.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerFrame {
    /// Answers `hello`.
    Hello {
        /// The protocol version the Space speaks.
        version: u16,
        /// Capabilities the Space offers.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
    /// The Space accepted a command and will run it.
    Accepted {
        /// The client's command identifier.
        id: Id,
    },
    /// The record holds the command's events.
    Committed {
        /// The client's command identifier.
        id: Id,
        /// The cursor of the command's last event.
        seq: Seq,
    },
    /// The command's work is done.
    Confirmed {
        /// The client's command identifier.
        id: Id,
        /// The command's result.
        #[ts(type = "unknown")]
        result: serde_json::Value,
    },
    /// A command failed.
    Error {
        /// The client's command identifier, when the failure has one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<Id>,
        /// The stable error code.
        code: ErrorCode,
        /// A message for the caller.
        message: String,
    },
    /// The record advanced.
    Event {
        /// The event.
        event: Event,
    },
    /// A page of the record for a subscription.
    Snapshot {
        /// The cursor after the page.
        cursor: Seq,
        /// The events in order.
        events: Vec<Event>,
    },
    /// The connection is alive; the cursor only moves.
    Heartbeat {
        /// The cursor the Space has reached.
        cursor: Seq,
    },
    /// The client's request was not understood.
    ProtocolError {
        /// What the client should do differently.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        BotComputer, BotCreate, BotUpdate, COMPACTION_CAPABILITY, ClientFrame, CommandCall,
        MEMORY_CAPABILITY, McpSignInStart, MessageSend, ModelSpec, PROTOCOL_VERSION, ProviderAdd,
        ProviderConnect, ProviderProtocol, ServerFrame, ThinkingLevel, event_capability,
    };
    use crate::error::ErrorCode;
    use crate::id::Id;
    use crate::record::{Author, EventKind};
    use crate::seq::Seq;
    use crate::time::Timestamp;

    #[test]
    fn a_command_frame_round_trips() {
        let frame = ClientFrame::Command {
            id: Id::new(),
            command: CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: None,

                thinking: None,
                model: None,
            }),
        };

        let text = serde_json::to_string(&frame).unwrap();
        assert_eq!(serde_json::from_str::<ClientFrame>(&text).unwrap(), frame);
    }

    #[test]
    fn computer_commands_round_trip() {
        let add = CommandCall::ComputerAdd {
            code: "ABCD2345".to_owned(),
        };
        let text = serde_json::to_string(&add).unwrap();
        assert!(text.contains("\"name\":\"computer_add\""));
        assert!(text.contains("\"code\":\"ABCD2345\""));
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), add);

        let list = CommandCall::ComputerList;
        let text = serde_json::to_string(&list).unwrap();
        assert_eq!(text, "{\"name\":\"computer_list\"}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), list);
    }

    #[test]
    fn bot_create_keeps_its_old_shape_without_a_computer() {
        let command = CommandCall::BotCreate(BotCreate {
            name: "Scout".to_owned(),
            computer: None,

            thinking: None,
            model: None,
        });
        let text = serde_json::to_string(&command).unwrap();
        assert!(
            !text.contains("\"computer\""),
            "an absent Computer is omitted"
        );

        let old: CommandCall =
            serde_json::from_str("{\"name\":\"bot_create\",\"args\":{\"name\":\"Scout\"}}")
                .unwrap();
        assert_eq!(old, command);
    }

    #[test]
    fn bot_move_round_trips() {
        let (bot, computer) = (Id::new(), Id::new());
        let command = CommandCall::BotMove { bot, computer };
        let text = serde_json::to_string(&command).unwrap();
        assert!(text.contains("\"name\":\"bot_move\""), "{text}");
        assert!(text.contains(&format!("\"bot\":\"{bot}\"")), "{text}");
        assert!(
            text.contains(&format!("\"computer\":\"{computer}\"")),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), command);
    }

    #[test]
    fn bot_create_carries_a_local_or_hosted_choice() {
        for (choice, wire) in [
            (BotComputer::Local, "local"),
            (BotComputer::Hosted, "hosted"),
        ] {
            let command = CommandCall::BotCreate(BotCreate {
                name: "Scout".to_owned(),
                computer: Some(choice),

                thinking: None,
                model: None,
            });
            let text = serde_json::to_string(&command).unwrap();
            assert!(text.contains(&format!("\"computer\":\"{wire}\"")), "{text}");
            assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), command);
        }

        let unknown: Result<CommandCall, _> = serde_json::from_str(
            "{\"name\":\"bot_create\",\"args\":{\"name\":\"Scout\",\"computer\":\"mainframe\"}}",
        );
        assert!(unknown.is_err(), "an unknown choice is refused");
    }

    #[test]
    fn bot_create_carries_a_computer_id() {
        let computer = Id::new();
        let command = CommandCall::BotCreate(BotCreate {
            name: "Scout".to_owned(),
            computer: Some(BotComputer::Id(computer)),

            thinking: None,
            model: None,
        });
        let text = serde_json::to_string(&command).unwrap();
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), command);
    }

    #[test]
    fn a_computer_reset_round_trips() {
        let computer = Id::new();
        let command = CommandCall::ComputerReset { computer };
        let text = serde_json::to_string(&command).unwrap();
        assert!(text.contains("\"computer_reset\""));
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), command);
    }

    #[test]
    fn a_device_approval_round_trips() {
        let command = CommandCall::DeviceApprove {
            code: "ABCD2345".to_owned(),
        };
        let text = serde_json::to_string(&command).unwrap();
        assert!(text.contains("\"device_approve\""));
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), command);
    }

    #[test]
    fn a_computer_revocation_round_trips() {
        let computer = Id::new();
        let command = CommandCall::ComputerRevoke { computer };
        let text = serde_json::to_string(&command).unwrap();
        assert!(text.contains("\"computer_revoke\""));
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), command);
    }

    #[test]
    fn provider_add_carries_its_models() {
        let command = CommandCall::ProviderAdd(ProviderAdd {
            protocol: ProviderProtocol::ChatCompletions,
            base_url: "http://127.0.0.1:9000/v1".to_owned(),
            api_key: "secret".to_owned(),
            models: vec![ModelSpec {
                name: "test-model".to_owned(),
                context_limit: 128_000,
                max_output: 8_192,
                reads_images: false,

                thinks: false,
                tools: true,
                thinking_levels: Vec::new(),
            }],
            default_model: Some("test-model".to_owned()),

            thinking: None,
            vendor: None,
            discover: false,
        });

        let text = serde_json::to_string(&command).unwrap();
        assert!(text.contains("\"name\":\"provider_add\""));
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), command);
    }

    #[test]
    fn provider_connect_names_its_vendor_and_overrides() {
        let command = CommandCall::ProviderConnect(ProviderConnect {
            vendor: "openai".to_owned(),
            api_key: "secret".to_owned(),
            base_url: Some("http://127.0.0.1:9000/v1".to_owned()),
            protocol: Some(ProviderProtocol::OpenAiResponses),
            thinking: Some(ThinkingLevel::High),
        });
        let text = serde_json::to_string(&command).unwrap();
        assert!(text.contains("\"name\":\"provider_connect\""), "{text}");
        assert!(text.contains("\"vendor\":\"openai\""), "{text}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), command);

        let list = CommandCall::CatalogList;
        assert_eq!(
            serde_json::from_str::<CommandCall>("{\"name\":\"catalog_list\"}").unwrap(),
            list
        );
    }

    #[test]
    fn bot_update_carries_a_model_and_thinking() {
        let (bot, model) = (Id::new(), Id::new());
        let update = CommandCall::BotUpdate(BotUpdate {
            bot,
            name: None,
            reset_model: false,
            reset_thinking: false,
            full_access: None,
            full_access_consent: None,
            grants: None,
            grant_add: None,
            grant_remove: None,
            model: Some(model),
            thinking: Some(ThinkingLevel::Low),
        });
        let text = serde_json::to_string(&update).unwrap();
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), update);
        let old: CommandCall = serde_json::from_str(&format!(
            "{{\"name\":\"bot_update\",\"args\":{{\"bot\":\"{bot}\"}}}}"
        ))
        .unwrap();
        assert_eq!(
            old,
            CommandCall::BotUpdate(BotUpdate {
                bot,
                name: None,
                reset_model: false,
                reset_thinking: false,
                full_access: None,
                full_access_consent: None,
                grants: None,
                grant_add: None,
                grant_remove: None,
                model: None,
                thinking: None,
            })
        );
    }

    #[test]
    fn a_bot_update_can_reset_a_choice() {
        let bot = Id::new();
        let reset = CommandCall::BotUpdate(BotUpdate {
            bot,
            name: None,
            reset_model: true,
            reset_thinking: true,
            full_access: None,
            full_access_consent: None,
            grants: None,
            grant_add: None,
            grant_remove: None,
            model: None,
            thinking: None,
        });
        let text = serde_json::to_string(&reset).unwrap();
        assert!(text.contains("\"reset_model\":true"), "{text}");
        assert!(text.contains("\"reset_thinking\":true"), "{text}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), reset);

        // An update written before the flags sends neither: they read false,
        // so such a client never clears a choice.
        let old: CommandCall = serde_json::from_str(&format!(
            "{{\"name\":\"bot_update\",\"args\":{{\"bot\":\"{bot}\",\"model\":\"{}\"}}}}",
            Id::new()
        ))
        .unwrap();
        let CommandCall::BotUpdate(old) = old else {
            panic!("bot_update decodes to its parameters");
        };
        assert!(!old.reset_model);
        assert!(!old.reset_thinking);
    }

    #[test]
    fn a_bot_update_names_its_new_name_when_it_has_one() {
        let bot = Id::new();
        let named = CommandCall::BotUpdate(BotUpdate {
            bot,
            name: Some("Scout".to_owned()),
            reset_model: false,
            reset_thinking: false,
            full_access: None,
            full_access_consent: None,
            grants: None,
            grant_add: None,
            grant_remove: None,
            model: None,
            thinking: None,
        });
        let text = serde_json::to_string(&named).unwrap();
        assert!(text.contains("\"name\":\"Scout\""), "{text}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), named);

        // A client written before the name field sends the old shape: an
        // absent name leaves the Bot's name unchanged.
        let old: CommandCall = serde_json::from_str(&format!(
            "{{\"name\":\"bot_update\",\"args\":{{\"bot\":\"{bot}\"}}}}"
        ))
        .unwrap();
        assert_eq!(
            old,
            CommandCall::BotUpdate(BotUpdate {
                bot,
                name: None,
                reset_model: false,
                reset_thinking: false,
                full_access: None,
                full_access_consent: None,
                grants: None,
                grant_add: None,
                grant_remove: None,
                model: None,
                thinking: None,
            })
        );
    }

    #[test]
    fn model_default_round_trips() {
        let command = CommandCall::ModelDefault { model: Id::new() };
        let text = serde_json::to_string(&command).unwrap();
        assert!(text.contains("\"name\":\"model_default\""), "{text}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), command);
    }

    #[test]
    fn provider_protocols_and_thinking_levels_round_trip() {
        for (protocol, wire) in [
            (ProviderProtocol::ChatCompletions, "chat_completions"),
            (ProviderProtocol::OpenAiResponses, "openai_responses"),
            (ProviderProtocol::AnthropicMessages, "anthropic_messages"),
        ] {
            assert_eq!(
                serde_json::to_value(protocol).unwrap(),
                serde_json::json!(wire)
            );
        }
        for (level, wire) in [
            (ThinkingLevel::Minimal, "minimal"),
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            (ThinkingLevel::Max, "max"),
        ] {
            assert_eq!(
                serde_json::to_value(level).unwrap(),
                serde_json::json!(wire)
            );
        }

        let old: ProviderAdd = serde_json::from_value(serde_json::json!({
            "protocol": "chat_completions",
            "base_url": "http://127.0.0.1:9000/v1",
            "api_key": "secret",
            "models": [{
                "name": "test-model",
                "context_limit": 128000,
                "max_output": 8192,
                "reads_images": false
            }]
        }))
        .unwrap();
        assert!(!old.models[0].thinks);
        assert_eq!(old.thinking, None);
        let old_bot: BotCreate =
            serde_json::from_value(serde_json::json!({"name": "Scout"})).unwrap();
        assert_eq!(old_bot.thinking, None);
    }

    #[test]
    fn a_message_send_round_trips() {
        let chat = Id::new();
        let plain = CommandCall::MessageSend(MessageSend {
            chat,
            text: "hello".to_owned(),
            file: None,
            cwd: None,
            cwd_writable: None,
        });
        let text = serde_json::to_string(&plain).unwrap();
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), plain);
        assert!(
            !text.contains("\"file\""),
            "a Message without a File omits it"
        );
        assert!(
            !text.contains("cwd_writable"),
            "a Message without a directory omits its authorization"
        );

        let file = CommandCall::MessageSend(MessageSend {
            chat,
            text: String::new(),
            file: Some(Id::new()),
            cwd: None,
            cwd_writable: None,
        });
        let text = serde_json::to_string(&file).unwrap();
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), file);

        // A request that names a directory can say it authorizes writing there.
        let directory = CommandCall::MessageSend(MessageSend {
            chat,
            text: "work".to_owned(),
            file: None,
            cwd: Some("/data/project".to_owned()),
            cwd_writable: Some(true),
        });
        let text = serde_json::to_string(&directory).unwrap();
        assert_eq!(
            serde_json::from_str::<CommandCall>(&text).unwrap(),
            directory
        );
        assert!(text.contains("\"cwd_writable\":true"));
        // A request from a Client written before the field existed decodes with
        // no authorization, so the Space can refuse it as an old form.
        let old = serde_json::json!({
            "name": "message_send",
            "args": {
                "chat": chat,
                "text": "work",
                "cwd": "/data/project",
            },
        });
        let CommandCall::MessageSend(sent) = serde_json::from_value(old).unwrap() else {
            panic!("the old form still decodes as a Message");
        };
        assert_eq!(sent.cwd_writable, None);
    }

    #[test]
    fn frames_are_tagged_by_type() {
        let frame = ServerFrame::Hello {
            version: PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        assert_eq!(
            serde_json::to_string(&frame).unwrap(),
            "{\"type\":\"hello\",\"version\":3}"
        );

        let error = ServerFrame::Error {
            id: None,
            code: ErrorCode::Unauthorized,
            message: "sign in".to_owned(),
        };
        let text = serde_json::to_string(&error).unwrap();
        assert!(text.contains("\"type\":\"error\""));
        assert!(text.contains("\"code\":\"unauthorized\""));
    }

    #[test]
    fn an_event_frame_carries_a_record_event() {
        let event = crate::record::Event {
            seq: Seq::from_i64(7).unwrap(),
            kind: EventKind::MessageAdded,
            version: 1,
            at: Timestamp::from_microseconds(1).unwrap(),
            author: Author::User(Id::new()),
            acting: None,
            command: Some(Id::new()),
            data: serde_json::json!({"text": "hi"}),
        };
        let frame = ServerFrame::Event {
            event: event.clone(),
        };
        let text = serde_json::to_string(&frame).unwrap();

        assert!(text.contains("\"type\":\"event\""));
        assert!(text.contains("\"kind\":\"neha.message.added\""));
    }
    #[test]
    fn approval_commands_round_trip() {
        let list = CommandCall::ApprovalList { chat: None };
        let text = serde_json::to_string(&list).unwrap();
        assert_eq!(text, "{\"name\":\"approval_list\",\"args\":{}}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), list);

        let chat = Id::new();
        let list = CommandCall::ApprovalList { chat: Some(chat) };
        let text = serde_json::to_string(&list).unwrap();
        assert!(text.contains(&format!("\"chat\":\"{chat}\"")), "{text}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), list);

        let approval = Id::new();
        let decide = CommandCall::ApprovalDecide {
            approval,
            allow: true,
            lasting: false,
        };
        let text = serde_json::to_string(&decide).unwrap();
        assert!(text.contains("\"name\":\"approval_decide\""), "{text}");
        assert!(text.contains("\"allow\":true"), "{text}");
        assert!(text.contains("\"lasting\":false"), "{text}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), decide);
    }

    #[test]
    fn rule_commands_round_trip() {
        let list = CommandCall::RuleList { bot: None };
        let text = serde_json::to_string(&list).unwrap();
        assert_eq!(text, "{\"name\":\"rule_list\",\"args\":{}}");

        let add = CommandCall::RuleAdd {
            bot: Id::new(),
            effect: crate::record::RuleEffect::Ask,
            tool: "shell".to_owned(),
            command_prefix: Some("git".to_owned()),
            path_prefix: None,
            server: None,
            skill: None,
            script: None,
        };
        let text = serde_json::to_string(&add).unwrap();
        assert!(text.contains("\"name\":\"rule_add\""), "{text}");
        assert!(text.contains("\"effect\":\"ask\""), "{text}");
        assert!(!text.contains("path_prefix"), "an absent prefix is omitted");
        assert!(!text.contains("script"), "an absent script is omitted");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), add);

        let script = CommandCall::RuleAdd {
            bot: Id::new(),
            effect: crate::record::RuleEffect::Allow,
            tool: "run_skill_script".to_owned(),
            command_prefix: None,
            path_prefix: None,
            skill: Some("weekly-report".to_owned()),
            script: Some("collect.sh".to_owned()),
            server: None,
        };
        let text = serde_json::to_string(&script).unwrap();
        assert!(text.contains("\"skill\":\"weekly-report\""), "{text}");
        assert!(text.contains("\"script\":\"collect.sh\""), "{text}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), script);

        let remove = CommandCall::RuleRemove { rule: Id::new() };
        let text = serde_json::to_string(&remove).unwrap();
        assert!(text.contains("\"name\":\"rule_remove\""), "{text}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), remove);
    }

    #[test]
    fn mcp_sign_in_commands_round_trip() {
        let server = Id::new();
        let connect = CommandCall::McpConnect {
            server,
            credential: Some("token".to_owned()),
        };
        let text = serde_json::to_string(&connect).unwrap();
        assert!(text.contains("\"name\":\"mcp_connect\""), "{text}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), connect);

        // A connect without a credential decodes for the Space to refuse.
        let bare: CommandCall = serde_json::from_str(&format!(
            "{{\"name\":\"mcp_connect\",\"args\":{{\"server\":\"{server}\"}}}}"
        ))
        .unwrap();
        assert_eq!(
            bare,
            CommandCall::McpConnect {
                server,
                credential: None,
            }
        );

        let start = CommandCall::McpSignInStart(McpSignInStart {
            server: None,
            name: Some("docs".to_owned()),
            url: Some("http://127.0.0.1:9000/mcp".to_owned()),
            scope: None,
            redirect_uri: None,
        });
        let text = serde_json::to_string(&start).unwrap();
        assert!(text.contains("\"name\":\"mcp_sign_in_start\""), "{text}");
        assert!(!text.contains("\"scope\""), "an absent scope is omitted");
        assert!(
            !text.contains("redirect_uri"),
            "an absent redirect is omitted"
        );
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), start);

        // A command line that receives the callback itself names its loopback
        // redirect.
        let loopback = CommandCall::McpSignInStart(McpSignInStart {
            server: None,
            name: Some("docs".to_owned()),
            url: Some("http://127.0.0.1:9000/mcp".to_owned()),
            scope: None,
            redirect_uri: Some("http://127.0.0.1:4711/api/mcp/signin/callback".to_owned()),
        });
        let text = serde_json::to_string(&loopback).unwrap();
        assert!(
            text.contains("\"redirect_uri\":\"http://127.0.0.1:4711/api/mcp/signin/callback\""),
            "{text}"
        );
        assert_eq!(
            serde_json::from_str::<CommandCall>(&text).unwrap(),
            loopback
        );

        let sign_in = Id::new();
        let status = CommandCall::McpSignInStatus { sign_in };
        let text = serde_json::to_string(&status).unwrap();
        assert!(text.contains("\"name\":\"mcp_sign_in_status\""), "{text}");
        assert!(
            text.contains(&format!("\"sign_in\":\"{sign_in}\"")),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), status);

        let finish = CommandCall::McpSignInFinish {
            sign_in,
            code: "the-code".to_owned(),
            state: "the-state".to_owned(),
            path: Some("/an-issuer-key/callback".to_owned()),
            iss: Some("https://auth.example.com".to_owned()),
        };
        let text = serde_json::to_string(&finish).unwrap();
        assert!(text.contains("\"name\":\"mcp_sign_in_finish\""), "{text}");
        assert!(text.contains("\"code\":\"the-code\""), "{text}");
        assert!(text.contains("\"state\":\"the-state\""), "{text}");
        assert!(
            text.contains("\"path\":\"/an-issuer-key/callback\""),
            "{text}"
        );
        assert!(
            text.contains("\"iss\":\"https://auth.example.com\""),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), finish);

        // An authorization server that omits RFC 9207's `iss`, and a caller
        // that names no path, still decode for the Space to refuse.
        let bare: CommandCall = serde_json::from_str(&format!(
            "{{\"name\":\"mcp_sign_in_finish\",\"args\":{{\"sign_in\":\"{sign_in}\",\
             \"code\":\"the-code\",\"state\":\"the-state\"}}}}"
        ))
        .unwrap();
        assert_eq!(
            bare,
            CommandCall::McpSignInFinish {
                sign_in,
                code: "the-code".to_owned(),
                state: "the-state".to_owned(),
                path: None,
                iss: None,
            }
        );
    }

    #[test]
    fn memory_commands_round_trip() {
        let bot = Id::new();
        let list = CommandCall::MemoryList { bot };
        let text = serde_json::to_string(&list).unwrap();
        assert!(text.contains("\"name\":\"memory_list\""), "{text}");
        assert!(text.contains(&format!("\"bot\":\"{bot}\"")), "{text}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), list);

        let memory = Id::new();
        let remove = CommandCall::MemoryRemove { bot, memory };
        let text = serde_json::to_string(&remove).unwrap();
        assert!(text.contains("\"name\":\"memory_remove\""), "{text}");
        assert!(text.contains(&format!("\"bot\":\"{bot}\"")), "{text}");
        assert!(text.contains(&format!("\"memory\":\"{memory}\"")), "{text}");
        assert_eq!(serde_json::from_str::<CommandCall>(&text).unwrap(), remove);
    }

    #[test]
    fn the_memory_capability_admits_the_memory_kinds() {
        let memory_kinds = [
            crate::record::EventKind::MemoryAdded,
            crate::record::EventKind::MemoryRemoved,
            crate::record::EventKind::MemoryAnswered,
        ];
        for kind in memory_kinds {
            assert_eq!(event_capability(kind), Some(MEMORY_CAPABILITY));
        }
        assert_eq!(
            event_capability(crate::record::EventKind::ChatCompacted),
            Some(COMPACTION_CAPABILITY)
        );
        for kind in [
            crate::record::EventKind::MessageAdded,
            crate::record::EventKind::RunReply,
        ] {
            assert_eq!(
                event_capability(kind),
                None,
                "an older kind needs no capability"
            );
        }
    }
}

/// One File as a client reads it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "surface.ts")]
pub struct FileSummary {
    /// The File.
    pub id: Id,
    /// The Chat it belongs to.
    pub chat: Id,
    /// Its name.
    pub name: String,
    /// Its media type.
    pub media_type: String,
    /// Its size in bytes.
    pub size: u64,
    /// Its lowercase hex SHA-256.
    pub sha256: String,
    /// Who sent it.
    pub author: Author,
    /// The User a Bot acted for, when a Bot sent it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acting: Option<Id>,
    /// When the record holds it.
    pub at: Timestamp,
    /// The cursor of the event that recorded it.
    pub seq: Seq,
}

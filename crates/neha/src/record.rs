//! Record events.
//!
//! Every change to a Space enters the record as an event. An event carries a
//! global sequence number, the actor that wrote it, and the data of the kind
//! it names.

use std::str::FromStr;
use ts_rs::TS;

use serde::{Deserialize, Serialize};

use crate::event::EventClass;
use crate::file::{MAX_FILE_BYTES, is_sha256};
use crate::id::Id;
use crate::seq::Seq;
use crate::time::Timestamp;
use crate::usage::Usage;

/// Who wrote an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Author {
    /// A User.
    User(Id),
    /// A Bot.
    Bot(Id),
}

/// The kinds of event the record carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
pub enum EventKind {
    /// A User joined the Space.
    #[serde(rename = "neha.user.joined")]
    UserJoined,
    /// The Admin connected a Provider.
    #[serde(rename = "neha.provider.added")]
    ProviderAdded,
    /// The Admin refreshed a Provider's Models or chose their default.
    #[serde(rename = "neha.provider.models_updated")]
    ProviderModelsUpdated,
    /// A Bot was created.
    #[serde(rename = "neha.bot.created")]
    BotCreated,
    /// A Bot moved to another Computer.
    #[serde(rename = "neha.bot.moved")]
    BotMoved,
    /// A Bot's owner changed its file access.
    #[serde(rename = "neha.bot.updated")]
    BotUpdated,
    /// A Bot's owner renamed it.
    #[serde(rename = "neha.bot.renamed")]
    BotRenamed,
    /// A Computer was created.
    #[serde(rename = "neha.computer.created")]
    ComputerCreated,
    /// A Chat was created.
    #[serde(rename = "neha.chat.created")]
    ChatCreated,
    /// A Message was added to a Chat or Group.
    #[serde(rename = "neha.message.added")]
    MessageAdded,
    /// A File was added to a Chat or Group.
    #[serde(rename = "neha.file.added")]
    FileAdded,
    /// A run entered its Bot's queue.
    #[serde(rename = "neha.run.queued")]
    RunQueued,
    /// A run started.
    #[serde(rename = "neha.run.started")]
    RunStarted,
    /// The directory a run works in.
    #[serde(rename = "neha.run.context")]
    RunContext,
    /// The file access a run's actions carry, fixed when it starts.
    #[serde(rename = "neha.run.access")]
    RunAccess,
    /// A run finished.
    #[serde(rename = "neha.run.finished")]
    RunFinished,
    /// A run completed one step.
    #[serde(rename = "neha.run.step")]
    RunStep,
    /// A run started one action.
    #[serde(rename = "neha.action.started")]
    ActionStarted,
    /// A run finished one action.
    #[serde(rename = "neha.action.finished")]
    ActionFinished,
    /// The Model's reply for one step, with the calls it asked to run.
    #[serde(rename = "neha.run.reply")]
    RunReply,
    /// A run waits for the acting User to allow or deny an action.
    #[serde(rename = "neha.approval.requested")]
    ApprovalRequested,
    /// The acting User answered an approval.
    #[serde(rename = "neha.approval.decided")]
    ApprovalDecided,
    /// A lasting rule about what a Bot may do without asking.
    #[serde(rename = "neha.rule.added")]
    RuleAdded,
    /// A lasting rule was removed.
    #[serde(rename = "neha.rule.removed")]
    RuleRemoved,
    /// A skill entered a library.
    #[serde(rename = "neha.skill.added")]
    SkillAdded,
    /// A skill left its library.
    #[serde(rename = "neha.skill.removed")]
    SkillRemoved,
    /// A Bot may use a skill.
    #[serde(rename = "neha.bot.skill.enabled")]
    BotSkillEnabled,
    /// A Bot no longer uses a skill.
    #[serde(rename = "neha.bot.skill.disabled")]
    BotSkillDisabled,
    /// An MCP server was connected.
    #[serde(rename = "neha.mcp.server.added")]
    McpServerAdded,
    /// An MCP server's address or credential changed.
    #[serde(rename = "neha.mcp.server.updated")]
    McpServerUpdated,
    /// An MCP server was disconnected.
    #[serde(rename = "neha.mcp.server.removed")]
    McpServerRemoved,
    /// A Bot may call an MCP server's tools.
    #[serde(rename = "neha.bot.mcp.enabled")]
    BotMcpEnabled,
    /// A Bot may no longer call an MCP server's tools.
    #[serde(rename = "neha.bot.mcp.disabled")]
    BotMcpDisabled,
    /// A package was installed.
    #[serde(rename = "neha.plugin.installed")]
    PluginInstalled,
    /// A package was removed.
    #[serde(rename = "neha.plugin.removed")]
    PluginRemoved,
    /// The Space offers a package.
    #[serde(rename = "neha.plugin.enabled")]
    PluginEnabled,
    /// The Space no longer offers a package.
    #[serde(rename = "neha.plugin.disabled")]
    PluginDisabled,
}

impl EventKind {
    /// Returns the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserJoined => "neha.user.joined",
            Self::ProviderAdded => "neha.provider.added",
            Self::ProviderModelsUpdated => "neha.provider.models_updated",
            Self::BotCreated => "neha.bot.created",
            Self::BotMoved => "neha.bot.moved",
            Self::BotUpdated => "neha.bot.updated",
            Self::BotRenamed => "neha.bot.renamed",
            Self::ComputerCreated => "neha.computer.created",
            Self::ChatCreated => "neha.chat.created",
            Self::MessageAdded => "neha.message.added",
            Self::FileAdded => "neha.file.added",
            Self::RunQueued => "neha.run.queued",
            Self::RunStarted => "neha.run.started",
            Self::RunContext => "neha.run.context",
            Self::RunAccess => "neha.run.access",
            Self::RunFinished => "neha.run.finished",
            Self::RunStep => "neha.run.step",
            Self::ActionStarted => "neha.action.started",
            Self::ActionFinished => "neha.action.finished",
            Self::RunReply => "neha.run.reply",
            Self::ApprovalRequested => "neha.approval.requested",
            Self::ApprovalDecided => "neha.approval.decided",
            Self::RuleAdded => "neha.rule.added",
            Self::RuleRemoved => "neha.rule.removed",
            Self::SkillAdded => "neha.skill.added",
            Self::SkillRemoved => "neha.skill.removed",
            Self::BotSkillEnabled => "neha.bot.skill.enabled",
            Self::BotSkillDisabled => "neha.bot.skill.disabled",
            Self::McpServerAdded => "neha.mcp.server.added",
            Self::McpServerUpdated => "neha.mcp.server.updated",
            Self::McpServerRemoved => "neha.mcp.server.removed",
            Self::BotMcpEnabled => "neha.bot.mcp.enabled",
            Self::BotMcpDisabled => "neha.bot.mcp.disabled",
            Self::PluginInstalled => "neha.plugin.installed",
            Self::PluginRemoved => "neha.plugin.removed",
            Self::PluginEnabled => "neha.plugin.enabled",
            Self::PluginDisabled => "neha.plugin.disabled",
        }
    }

    /// Returns how a reader that does not know this kind must treat it.
    #[must_use]
    pub const fn class(self) -> EventClass {
        match self {
            Self::MessageAdded
            | Self::FileAdded
            | Self::RunQueued
            | Self::RunStarted
            | Self::RunFinished => EventClass::Presentation,
            // A rename decides nothing about access or work: a reader that
            // does not know it keeps the name it has and reads on.
            Self::BotRenamed => EventClass::Presentation,
            // Steps and actions decide which work may run again and what a
            // budget counted: a reader that does not know them must stop.
            Self::RunContext
            | Self::RunAccess
            | Self::RunStep
            | Self::ActionStarted
            | Self::ActionFinished
            | Self::RunReply
            | Self::ApprovalRequested
            | Self::ApprovalDecided
            | Self::RuleAdded
            | Self::RuleRemoved
            | Self::SkillAdded
            | Self::SkillRemoved
            | Self::BotSkillEnabled
            | Self::BotSkillDisabled
            | Self::McpServerAdded
            | Self::McpServerUpdated
            | Self::McpServerRemoved
            | Self::BotMcpEnabled
            | Self::BotMcpDisabled
            | Self::PluginInstalled
            | Self::PluginRemoved
            | Self::PluginEnabled
            | Self::PluginDisabled => EventClass::Authority,
            Self::UserJoined
            | Self::ProviderAdded
            | Self::ProviderModelsUpdated
            | Self::BotCreated
            | Self::BotMoved
            | Self::BotUpdated
            | Self::ComputerCreated
            | Self::ChatCreated => EventClass::Authority,
        }
    }
}

impl FromStr for EventKind {
    type Err = UnknownEventKind;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "neha.user.joined" => Ok(Self::UserJoined),
            "neha.provider.added" => Ok(Self::ProviderAdded),
            "neha.provider.models_updated" => Ok(Self::ProviderModelsUpdated),
            "neha.bot.created" => Ok(Self::BotCreated),
            "neha.bot.moved" => Ok(Self::BotMoved),
            "neha.bot.updated" => Ok(Self::BotUpdated),
            "neha.bot.renamed" => Ok(Self::BotRenamed),
            "neha.computer.created" => Ok(Self::ComputerCreated),
            "neha.chat.created" => Ok(Self::ChatCreated),
            "neha.message.added" => Ok(Self::MessageAdded),
            "neha.file.added" => Ok(Self::FileAdded),
            "neha.run.queued" => Ok(Self::RunQueued),
            "neha.run.started" => Ok(Self::RunStarted),
            "neha.run.context" => Ok(Self::RunContext),
            "neha.run.access" => Ok(Self::RunAccess),
            "neha.run.finished" => Ok(Self::RunFinished),
            "neha.run.step" => Ok(Self::RunStep),
            "neha.action.started" => Ok(Self::ActionStarted),
            "neha.action.finished" => Ok(Self::ActionFinished),
            "neha.run.reply" => Ok(Self::RunReply),
            "neha.approval.requested" => Ok(Self::ApprovalRequested),
            "neha.approval.decided" => Ok(Self::ApprovalDecided),
            "neha.rule.added" => Ok(Self::RuleAdded),
            "neha.rule.removed" => Ok(Self::RuleRemoved),
            "neha.skill.added" => Ok(Self::SkillAdded),
            "neha.skill.removed" => Ok(Self::SkillRemoved),
            "neha.bot.skill.enabled" => Ok(Self::BotSkillEnabled),
            "neha.bot.skill.disabled" => Ok(Self::BotSkillDisabled),
            "neha.mcp.server.added" => Ok(Self::McpServerAdded),
            "neha.mcp.server.updated" => Ok(Self::McpServerUpdated),
            "neha.mcp.server.removed" => Ok(Self::McpServerRemoved),
            "neha.bot.mcp.enabled" => Ok(Self::BotMcpEnabled),
            "neha.bot.mcp.disabled" => Ok(Self::BotMcpDisabled),
            "neha.plugin.installed" => Ok(Self::PluginInstalled),
            "neha.plugin.removed" => Ok(Self::PluginRemoved),
            "neha.plugin.enabled" => Ok(Self::PluginEnabled),
            "neha.plugin.disabled" => Ok(Self::PluginDisabled),
            _ => Err(UnknownEventKind(text.to_owned())),
        }
    }
}

/// An event name this version does not know.
#[derive(Debug, thiserror::Error)]
#[error("unknown event kind: {0}")]
pub struct UnknownEventKind(pub String);

/// An event before the record assigns its sequence number and time.
#[derive(Clone, Debug, PartialEq)]
pub struct EventDraft {
    /// The kind.
    pub kind: EventKind,
    /// The kind's version.
    pub version: u16,
    /// Who wrote it.
    pub author: Author,
    /// The User a Bot acts for, when the author is a Bot.
    pub acting: Option<Id>,
    /// The data of its kind.
    pub data: serde_json::Value,
}

impl EventDraft {
    /// Builds a draft of the first version of a kind.
    #[must_use]
    pub fn new(kind: EventKind, author: Author, data: serde_json::Value) -> Self {
        Self {
            kind,
            version: 1,
            author,
            acting: None,
            data,
        }
    }

    /// Records the User a Bot acts for.
    #[must_use]
    pub fn acting(mut self, user: Id) -> Self {
        self.acting = Some(user);
        self
    }
}

/// A recorded event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
pub struct Event {
    /// The cursor of the event.
    pub seq: Seq,
    /// The kind.
    pub kind: EventKind,
    /// The kind's version.
    pub version: u16,
    /// When the Space recorded it.
    pub at: Timestamp,
    /// Who wrote it.
    pub author: Author,
    /// The User a Bot acts for, when the author is a Bot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acting: Option<Id>,
    /// The command that wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Id>,
    /// The data of its kind.
    #[ts(type = "Record<string, unknown>")]
    pub data: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::{
        APP_CARD_KIND, Author, BotMoved, BotRenamed, BotUpdated, CardData, ComputerKind, EventData,
        EventDataError, EventDraft, EventKind, FileAdded, FileError, MAX_CARD_CSP_BYTES,
        MAX_CARD_VALUE_BYTES, MAX_MCP_ARG_BYTES, MAX_MCP_ARGS, MAX_MCP_ARGS_BYTES,
        MAX_MCP_COMMAND_BYTES, MAX_MCP_ENV, MAX_MCP_ENV_VALUE_BYTES, McpScope, McpServerAdded,
        McpTransport, RunAccess, RunQueued, SkillAdded, SkillLibrary, SkillScript,
    };
    use crate::event::EventClass;
    use crate::file::MAX_FILE_BYTES;
    use crate::id::Id;

    #[test]
    fn a_credential_holds_only_what_a_bearer_token_takes() {
        for token in [
            "a",
            "abc",
            "token-1",
            "a.b_c~d+e/f==",
            "A1",
            "x=",
            "x==",
            "x===",
        ] {
            assert!(McpServerAdded::valid_credential(token), "{token}");
        }
        // The token alphabet comes first; padding only ends a token.
        for token in [
            "",
            "=",
            "==",
            "===",
            "a=b",
            "two words",
            "quote\"inside",
            "back\\slash",
            "line\nbreak",
            "###",
        ] {
            assert!(!McpServerAdded::valid_credential(token), "{token}");
        }
    }

    #[test]
    fn a_server_url_names_an_address_this_space_may_call() {
        for url in [
            "https://example.com/mcp",
            "https://example.com/mcp?key=1",
            "https://mcp.example.com:8443",
            "https://[2001:db8::1]/mcp",
            "http://localhost:8080/mcp",
            "http://127.0.0.1/mcp",
            "http://[::1]:9000/mcp",
            "https://single",
        ] {
            assert!(McpServerAdded::valid_url(url), "{url}");
        }
        for url in [
            "",
            "ftp://example.com/mcp",
            "https://",
            "https://user:token@example.com/mcp",
            "https://example.com/mcp#fragment",
            "https://example.com/mcp\\path",
            "http://example.com/mcp",
            "http://localhost:0/mcp",
            "http://localhost:port/mcp",
            "https://exa mple.com/mcp",
            "https://-example.com/mcp",
            "https://example..com/mcp",
        ] {
            assert!(!McpServerAdded::valid_url(url), "{url}");
        }
    }

    #[test]
    fn a_stdio_server_round_trips_and_an_http_record_still_decodes() {
        let server = Id::new();
        let by = Id::new();
        let stdio = McpServerAdded {
            server,
            name: "docs".to_owned(),
            url: None,
            command: Some("./bin/server".to_owned()),
            args: vec!["--data".to_owned(), "${PLUGIN_DATA}/docs".to_owned()],
            env: [("CONFIG".to_owned(), "${PLUGIN_ROOT}/config.json".to_owned())]
                .into_iter()
                .collect(),
            cwd: Some("work".to_owned()),
            data_cwd: true,
            scope: McpScope::Space,
            user: None,
            by,
            plugin: None,
            headers: std::collections::BTreeMap::new(),
        };
        stdio.validate().unwrap();
        assert_eq!(stdio.transport().unwrap(), McpTransport::Stdio);
        // A view names the transport the way `mcp_list` answers it.
        assert_eq!(
            serde_json::to_value(McpTransport::Stdio).unwrap(),
            serde_json::json!("stdio")
        );
        assert_eq!(
            serde_json::to_value(McpTransport::StreamableHttp).unwrap(),
            serde_json::json!("streamable-http")
        );
        assert_eq!(stdio.stdio().unwrap().cwd.as_deref(), Some("work"));
        assert!(stdio.stdio().unwrap().data_cwd);

        let value = serde_json::to_value(&stdio).unwrap();
        assert!(value.get("url").is_none(), "a stdio server carries no url");
        assert_eq!(value["command"], serde_json::json!("./bin/server"));
        assert_eq!(value["data_cwd"], serde_json::json!(true));
        assert_eq!(
            EventKind::McpServerAdded.decode(&value).unwrap(),
            EventData::McpServerAdded(stdio.clone())
        );

        // A record written before stdio servers were supported still decodes
        // and validates: its `url` is the one transport.
        let old = serde_json::json!({
            "server": server,
            "name": "docs",
            "url": "https://example.com/mcp",
            "scope": "space",
            "by": by,
        });
        let EventData::McpServerAdded(old) = EventKind::McpServerAdded.decode(&old).unwrap() else {
            panic!("the data is a server addition");
        };
        assert_eq!(old.url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(old.command, None);
        assert!(old.args.is_empty());
        assert!(old.env.is_empty());
        assert_eq!(old.cwd, None);
        assert!(!old.data_cwd);
        assert_eq!(old.transport().unwrap(), McpTransport::StreamableHttp);

        // A record that names no working directory works in the plugin root.
        let root = serde_json::json!({
            "server": Id::new(),
            "name": "docs",
            "command": "npx",
            "scope": "space",
            "by": by,
        });
        let EventData::McpServerAdded(root) = EventKind::McpServerAdded.decode(&root).unwrap()
        else {
            panic!("the data is a server addition");
        };
        let stdio = root.stdio().unwrap();
        assert_eq!(stdio.cwd, None);
        assert!(!stdio.data_cwd);
    }

    #[test]
    fn a_server_names_exactly_one_transport() {
        let server = Id::new();
        let by = Id::new();
        for fields in [
            // Neither a `url` nor a `command`.
            serde_json::json!({}),
            // Both at once.
            serde_json::json!({"url": "https://example.com/mcp", "command": "npx"}),
            // The fields of the other transport.
            serde_json::json!({"url": "https://example.com/mcp", "args": ["--data"]}),
            serde_json::json!({"url": "https://example.com/mcp", "env": {"A": "b"}}),
            serde_json::json!({"url": "https://example.com/mcp", "cwd": "work"}),
            serde_json::json!({"url": "https://example.com/mcp", "data_cwd": true}),
            serde_json::json!({"command": "npx", "headers": {"X-Tenant": "public"}}),
        ] {
            let mut value = serde_json::json!({
                "server": server,
                "name": "docs",
                "scope": "space",
                "by": by,
            });
            for (field, configured) in fields.as_object().unwrap() {
                value[field] = configured.clone();
            }
            assert!(
                EventKind::McpServerAdded.decode(&value).is_err(),
                "{fields} must name one transport"
            );
        }
    }

    #[test]
    fn a_symbolic_working_directory_may_keep_placeholders_and_parents() {
        let by = Id::new();
        let server = |cwd: &str, data_cwd: bool| McpServerAdded {
            server: Id::new(),
            name: "docs".to_owned(),
            url: None,
            command: Some("npx".to_owned()),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: Some(cwd.to_owned()),
            data_cwd,
            scope: McpScope::Space,
            user: None,
            by,
            plugin: None,
            headers: std::collections::BTreeMap::new(),
        };

        // The record keeps the package's symbolic expression: an install and a
        // launch expand the two placeholders against the real roots, and only
        // that expansion says what a `..` removes. A `..` after a placeholder
        // may remove a component of the root the placeholder stands for, whose
        // component count the text does not carry, so it stays for the install
        // and the launch to judge.
        for cwd in [
            "work",
            "a/../b",
            "a/${PLUGIN_ROOT}/../b",
            "cache${PLUGIN_DATA}",
            "${PLUGIN_ROOT}/../../work",
            "${PLUGIN_DATA}/a/${PLUGIN_ROOT}/../../..",
            "./work",
            "a/./b",
            "a/..",
        ] {
            server(cwd, true)
                .validate()
                .unwrap_or_else(|error| panic!("`{cwd}` is a symbolic working directory: {error}"));
        }
    }

    #[test]
    fn a_stdio_command_is_one_executable_token() {
        let by = Id::new();
        let server = |command: &str| McpServerAdded {
            server: Id::new(),
            name: "docs".to_owned(),
            url: None,
            command: Some(command.to_owned()),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
            data_cwd: false,
            scope: McpScope::Space,
            user: None,
            by,
            plugin: None,
            headers: std::collections::BTreeMap::new(),
        };
        for command in [
            "npx",
            "python3.12",
            "my-tool",
            "tool_x",
            "./bin/server",
            "./server",
        ] {
            server(command)
                .validate()
                .unwrap_or_else(|error| panic!("`{command}` is a command token: {error}"));
        }
        for command in [
            "",
            "bin/server",
            "/bin/server",
            "./",
            "./bin//server",
            "./bin/../server",
            "./../server",
            ".hidden",
            "..",
            "a b",
            "npx;rm -rf /",
            "npx&&cat",
            "npx|cat",
            "$SHELL",
            "a\\b",
        ] {
            assert!(
                server(command).validate().is_err(),
                "`{command}` must not name a command"
            );
        }
    }

    #[test]
    fn a_stdio_server_is_bounded() {
        let by = Id::new();
        let stdio = || McpServerAdded {
            server: Id::new(),
            name: "docs".to_owned(),
            url: None,
            command: Some("npx".to_owned()),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
            data_cwd: false,
            scope: McpScope::Space,
            user: None,
            by,
            plugin: None,
            headers: std::collections::BTreeMap::new(),
        };
        let mut edge = stdio();
        edge.command = Some("a".repeat(MAX_MCP_COMMAND_BYTES));
        edge.args = vec!["a".repeat(MAX_MCP_ARG_BYTES); MAX_MCP_ARGS_BYTES / MAX_MCP_ARG_BYTES];
        edge.validate().unwrap();

        let cases = [
            ("a command above its bound", {
                let mut server = stdio();
                server.command = Some("a".repeat(MAX_MCP_COMMAND_BYTES + 1));
                server
            }),
            ("arguments above their count", {
                let mut server = stdio();
                server.args = vec!["a".to_owned(); MAX_MCP_ARGS + 1];
                server
            }),
            ("an argument above its bound", {
                let mut server = stdio();
                server.args = vec!["a".repeat(MAX_MCP_ARG_BYTES + 1)];
                server
            }),
            ("arguments above their total", {
                let mut server = stdio();
                server.args =
                    vec!["a".repeat(MAX_MCP_ARG_BYTES); MAX_MCP_ARGS_BYTES / MAX_MCP_ARG_BYTES + 1];
                server
            }),
            ("an environment above its count", {
                let mut server = stdio();
                for index in 0..=MAX_MCP_ENV {
                    server.env.insert(format!("A{index}"), "b".to_owned());
                }
                server
            }),
            ("an environment value above its bound", {
                let mut server = stdio();
                server
                    .env
                    .insert("A".to_owned(), "a".repeat(MAX_MCP_ENV_VALUE_BYTES + 1));
                server
            }),
            ("an environment above its total", {
                let mut server = stdio();
                for index in 0..MAX_MCP_ENV {
                    server
                        .env
                        .insert(format!("A{index}"), "a".repeat(MAX_MCP_ENV_VALUE_BYTES));
                }
                server
            }),
            ("an environment name that is not one", {
                let mut server = stdio();
                server.env.insert("A-B".to_owned(), "b".to_owned());
                server
            }),
            ("a working directory that leaves its root", {
                let mut server = stdio();
                server.cwd = Some("../work".to_owned());
                server
            }),
            ("a working directory that pops above its root", {
                let mut server = stdio();
                server.cwd = Some("a/../../b".to_owned());
                server
            }),
            ("a working directory that is not relative", {
                let mut server = stdio();
                server.cwd = Some("/work".to_owned());
                server
            }),
            ("a working directory with a path separator", {
                let mut server = stdio();
                server.cwd = Some("a\\b".to_owned());
                server
            }),
            ("a working directory with a control character", {
                let mut server = stdio();
                server.cwd = Some("a\u{0}b".to_owned());
                server
            }),
            (
                "a working directory with a separator after a placeholder",
                {
                    let mut server = stdio();
                    server.cwd = Some("${PLUGIN_ROOT}/after:bad".to_owned());
                    server
                },
            ),
        ];
        for (reason, server) in cases {
            assert!(server.validate().is_err(), "{reason} is refused");
        }
    }

    #[test]
    fn wire_names_round_trip() {
        let kinds = [
            EventKind::UserJoined,
            EventKind::ProviderAdded,
            EventKind::ProviderModelsUpdated,
            EventKind::BotCreated,
            EventKind::BotMoved,
            EventKind::BotUpdated,
            EventKind::BotRenamed,
            EventKind::ComputerCreated,
            EventKind::ChatCreated,
            EventKind::MessageAdded,
            EventKind::FileAdded,
            EventKind::RunQueued,
            EventKind::RunStarted,
            EventKind::RunAccess,
            EventKind::RunFinished,
            EventKind::RunStep,
            EventKind::ActionStarted,
            EventKind::ActionFinished,
            EventKind::RunReply,
            EventKind::ApprovalRequested,
            EventKind::ApprovalDecided,
            EventKind::RuleAdded,
            EventKind::RuleRemoved,
            EventKind::SkillAdded,
            EventKind::SkillRemoved,
            EventKind::BotSkillEnabled,
            EventKind::BotSkillDisabled,
            EventKind::McpServerAdded,
            EventKind::McpServerUpdated,
            EventKind::McpServerRemoved,
            EventKind::BotMcpEnabled,
            EventKind::BotMcpDisabled,
            EventKind::PluginInstalled,
            EventKind::PluginRemoved,
            EventKind::PluginEnabled,
            EventKind::PluginDisabled,
        ];

        for kind in kinds {
            assert_eq!(kind.as_str().parse::<EventKind>().unwrap(), kind);
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.as_str());
        }
    }

    #[test]
    fn message_and_run_events_are_presentation() {
        assert_eq!(EventKind::MessageAdded.class(), EventClass::Presentation);
        assert_eq!(EventKind::FileAdded.class(), EventClass::Presentation);
        assert_eq!(EventKind::RunQueued.class(), EventClass::Presentation);
        assert_eq!(EventKind::RunStarted.class(), EventClass::Presentation);
        assert_eq!(EventKind::RunFinished.class(), EventClass::Presentation);
        assert_eq!(EventKind::BotRenamed.class(), EventClass::Presentation);
        assert_eq!(EventKind::RunAccess.class(), EventClass::Authority);
        assert_eq!(EventKind::RunStep.class(), EventClass::Authority);
        assert_eq!(EventKind::ActionStarted.class(), EventClass::Authority);
        assert_eq!(EventKind::ActionFinished.class(), EventClass::Authority);
        assert_eq!(EventKind::BotCreated.class(), EventClass::Authority);
        assert_eq!(EventKind::BotMoved.class(), EventClass::Authority);
        assert_eq!(EventKind::RunReply.class(), EventClass::Authority);
        assert_eq!(EventKind::ApprovalRequested.class(), EventClass::Authority);
        assert_eq!(EventKind::ApprovalDecided.class(), EventClass::Authority);
        assert_eq!(EventKind::RuleAdded.class(), EventClass::Authority);
        assert_eq!(EventKind::RuleRemoved.class(), EventClass::Authority);
        assert_eq!(EventKind::SkillAdded.class(), EventClass::Authority);
        assert_eq!(EventKind::SkillRemoved.class(), EventClass::Authority);
        assert_eq!(EventKind::BotSkillEnabled.class(), EventClass::Authority);
        assert_eq!(EventKind::BotSkillDisabled.class(), EventClass::Authority);
    }
    #[test]
    fn a_skill_round_trips_and_rejects_what_it_cannot_hold() {
        let data = SkillAdded {
            skill: Id::new(),
            library: SkillLibrary::Space,
            bot: None,
            name: "weekly-report".to_owned(),
            description: "Write the weekly status report.".to_owned(),
            body: "1. Collect the docs.\n2. Summarize.".to_owned(),
            scripts: vec![SkillScript {
                name: "collect.sh".to_owned(),
                sha256: "a".repeat(64),
            }],
            plugin: None,
        };
        let value = serde_json::to_value(&data).unwrap();
        assert_eq!(
            EventKind::SkillAdded.decode(&value).unwrap(),
            EventData::SkillAdded(data.clone())
        );
        let mut space_with_bot = value.clone();
        space_with_bot["bot"] = serde_json::json!(Id::new());
        assert!(EventKind::SkillAdded.decode(&space_with_bot).is_err());
        let mut bad_name = value.clone();
        bad_name["name"] = serde_json::json!("Weekly Report");
        assert!(EventKind::SkillAdded.decode(&bad_name).is_err());
        let mut bad_script = value.clone();
        bad_script["scripts"] = serde_json::json!([{"name": "../x.sh", "sha256": "a".repeat(64)}]);
        assert!(EventKind::SkillAdded.decode(&bad_script).is_err());
        let mut old_rule = serde_json::json!({
            "rule": Id::new(), "bot": Id::new(), "user": Id::new(),
            "effect": "allow", "tool": "shell", "command_prefix": "ls"
        });
        let EventData::RuleAdded(rule) = EventKind::RuleAdded.decode(&old_rule).unwrap() else {
            unreachable!("the data is a rule");
        };
        assert_eq!((rule.skill, rule.script), (None, None));
        old_rule["skill"] = serde_json::json!(Id::new());
        old_rule["script"] = serde_json::json!("collect.sh");
        let EventData::RuleAdded(rule) = EventKind::RuleAdded.decode(&old_rule).unwrap() else {
            unreachable!("the data is a rule");
        };
        assert!(rule.skill.is_some());
        assert_eq!(rule.script.as_deref(), Some("collect.sh"));
    }

    #[test]
    fn a_shared_change_carries_its_scope_and_an_old_one_still_decodes() {
        let server = Id::new();
        let by = Id::new();
        // A record written before the scope field has none: it still decodes,
        // and the stream keeps it the author's.
        let old = serde_json::json!({
            "server": server,
            "url": null,
            "credential": true,
            "by": by,
        });
        let EventData::McpServerUpdated(updated) =
            EventKind::McpServerUpdated.decode(&old).unwrap()
        else {
            panic!("the data is a server update");
        };
        assert_eq!(updated.scope, None);
        for (scope, expected) in [("space", McpScope::Space), ("user", McpScope::User)] {
            let mut value = old.clone();
            value["scope"] = serde_json::json!(scope);
            let EventData::McpServerUpdated(updated) =
                EventKind::McpServerUpdated.decode(&value).unwrap()
            else {
                panic!("the data is a server update");
            };
            assert_eq!(updated.scope, Some(expected));
        }
        let mut unknown = old.clone();
        unknown["scope"] = serde_json::json!("somewhere");
        assert!(EventKind::McpServerUpdated.decode(&unknown).is_err());

        let removed = serde_json::json!({"server": server});
        let EventData::McpServerRemoved(removal) =
            EventKind::McpServerRemoved.decode(&removed).unwrap()
        else {
            panic!("the data is a server removal");
        };
        assert_eq!(removal.scope, None);
        let mut shared = removed.clone();
        shared["scope"] = serde_json::json!("space");
        let EventData::McpServerRemoved(removal) =
            EventKind::McpServerRemoved.decode(&shared).unwrap()
        else {
            panic!("the data is a server removal");
        };
        assert_eq!(removal.scope, Some(McpScope::Space));

        let skill = Id::new();
        let old_removal = serde_json::json!({"skill": skill});
        let EventData::SkillRemoved(removal) =
            EventKind::SkillRemoved.decode(&old_removal).unwrap()
        else {
            panic!("the data is a skill removal");
        };
        assert_eq!(removal.library, None);
        for (library, expected) in [("space", SkillLibrary::Space), ("bot", SkillLibrary::Bot)] {
            let mut value = old_removal.clone();
            value["library"] = serde_json::json!(library);
            let EventData::SkillRemoved(removal) = EventKind::SkillRemoved.decode(&value).unwrap()
            else {
                panic!("the data is a skill removal");
            };
            assert_eq!(removal.library, Some(expected));
        }
    }

    #[test]
    fn a_run_queued_round_trips() {
        let data = RunQueued {
            run: Id::new(),
            chat: Id::new(),
            bot: Id::new(),
            command: Id::new(),
            message: Id::new(),
        };
        let value = serde_json::json!({
            "run": data.run,
            "chat": data.chat,
            "bot": data.bot,
            "command": data.command,
            "message": data.message,
        });

        assert_eq!(
            EventKind::RunQueued.decode(&value).unwrap(),
            EventData::RunQueued(data)
        );

        let mut unknown = value.clone();
        unknown["position"] = serde_json::json!(1);
        assert!(EventKind::RunQueued.decode(&unknown).is_err());
    }

    #[test]
    fn a_queued_run_takes_the_authors_a_started_run_takes() {
        let user = Author::User(Id::new());
        let bot = Author::Bot(Id::new());
        for kind in [
            EventKind::RunQueued,
            EventKind::RunStarted,
            EventKind::RunAccess,
        ] {
            assert!(kind.validate_author(bot, Some(Id::new())).is_ok());
            assert!(kind.validate_author(user, None).is_err());
            assert!(kind.validate_author(user, Some(Id::new())).is_err());
            assert!(kind.validate_author(bot, None).is_err());
        }
    }

    #[test]
    fn a_run_access_round_trips() {
        let run = Id::new();
        let value = serde_json::json!({
            "run": run,
            "full_access": false,
            "grants": ["/data/shared", "/var/tmp/notes"],
            "cwd_writable": true,
        });

        let EventData::RunAccess(access) = EventKind::RunAccess.decode(&value).unwrap() else {
            panic!("a run's access decodes to its data");
        };
        assert_eq!(
            access,
            RunAccess {
                run,
                full_access: false,
                grants: vec!["/data/shared".to_owned(), "/var/tmp/notes".to_owned()],
                cwd_writable: true,
            }
        );

        let mut unknown = value;
        unknown["write_roots"] = serde_json::json!(["/data/shared"]);
        assert!(
            EventKind::RunAccess.decode(&unknown).is_err(),
            "an access event carries exactly the pinned permission"
        );
    }

    #[test]
    fn a_bot_moved_round_trips() {
        let data = BotMoved {
            bot: Id::new(),
            from: Id::new(),
            to: Id::new(),
        };
        let value = serde_json::json!({
            "bot": data.bot,
            "from": data.from,
            "to": data.to,
        });

        assert_eq!(
            EventKind::BotMoved.decode(&value).unwrap(),
            EventData::BotMoved(data.clone())
        );
        assert_eq!(
            serde_json::from_value::<BotMoved>(value.clone()).unwrap(),
            data
        );

        let mut unknown = value;
        unknown["computer"] = serde_json::json!(Id::new());
        assert!(
            EventKind::BotMoved.decode(&unknown).is_err(),
            "a move names exactly its Bot and two Computers"
        );
    }

    #[test]
    fn a_bot_moved_takes_a_user_author() {
        let user = Author::User(Id::new());
        let bot = Author::Bot(Id::new());
        assert!(EventKind::BotMoved.validate_author(user, None).is_ok());
        assert!(EventKind::BotMoved.validate_author(bot, None).is_err());
        assert!(
            EventKind::BotMoved
                .validate_author(user, Some(Id::new()))
                .is_err()
        );
    }

    #[test]
    fn a_bot_renamed_round_trips() {
        let data = BotRenamed {
            bot: Id::new(),
            name: "Scout".to_owned(),
        };
        let value = serde_json::json!({"bot": data.bot, "name": "Scout"});

        assert_eq!(
            EventKind::BotRenamed.decode(&value).unwrap(),
            EventData::BotRenamed(data.clone())
        );
        assert_eq!(
            serde_json::from_value::<BotRenamed>(value.clone()).unwrap(),
            data
        );

        let mut unknown = value;
        unknown["computer"] = serde_json::json!(Id::new());
        assert!(
            EventKind::BotRenamed.decode(&unknown).is_err(),
            "a rename names exactly its Bot and new name"
        );
    }

    #[test]
    fn a_bot_renamed_takes_a_user_author() {
        let user = Author::User(Id::new());
        let bot = Author::Bot(Id::new());
        assert!(EventKind::BotRenamed.validate_author(user, None).is_ok());
        assert!(EventKind::BotRenamed.validate_author(bot, None).is_err());
        assert!(
            EventKind::BotRenamed
                .validate_author(user, Some(Id::new()))
                .is_err()
        );
    }

    #[test]
    fn a_bot_update_reset_is_additive() {
        let bot = Id::new();
        // An update written before the reset fields names neither: it must
        // replay as not resetting anything.
        let old = serde_json::json!({"bot": bot, "model": null});
        let EventData::BotUpdated(update) = EventKind::BotUpdated.decode(&old).unwrap() else {
            panic!("bot_update decodes to its data");
        };
        assert!(!update.reset_model);
        assert!(!update.reset_thinking);

        let reset = serde_json::json!({"bot": bot, "reset_model": true, "reset_thinking": true});
        let EventData::BotUpdated(update) = EventKind::BotUpdated.decode(&reset).unwrap() else {
            panic!("bot_update decodes to its data");
        };
        assert!(update.reset_model);
        assert!(update.reset_thinking);
        assert_eq!(serde_json::from_value::<BotUpdated>(reset).unwrap(), update);
    }

    #[test]
    fn a_finished_run_is_not_queued() {
        let run = Id::new();
        let queued = serde_json::json!({"run": run, "status": "queued"});
        assert!(EventKind::RunFinished.decode(&queued).is_err());
        let waiting = serde_json::json!({"run": run, "status": "waiting"});
        assert!(
            EventKind::RunFinished.decode(&waiting).is_err(),
            "a finished run cannot be waiting"
        );

        for status in ["running", "succeeded", "failed"] {
            let data = serde_json::json!({"run": run, "status": status});
            assert!(
                EventKind::RunFinished.decode(&data).is_ok(),
                "{status} is a status a finished run may carry"
            );
        }
    }

    #[test]
    fn an_old_model_data_is_still_tool_capable() {
        let provider = Id::new();
        let model = Id::new();
        let value = serde_json::json!({
            "provider": provider,
            "protocol": "chat_completions",
            "base_url": "http://127.0.0.1:9000/v1",
            "models": [{
                "id": model,
                "name": "old-model",
                "context_limit": 8192,
                "max_output": 1024,
                "reads_images": false,
            }],
        });
        let EventData::ProviderAdded(data) = EventKind::ProviderAdded.decode(&value).unwrap()
        else {
            panic!("expected a Provider");
        };
        assert_eq!(data.vendor, None);
        assert!(data.models[0].tools, "old Models could call tools");
        assert!(data.models[0].thinking_levels.is_empty());

        let updated = serde_json::json!({
            "provider": provider,
            "models": [{
                "id": model,
                "name": "new-model",
                "context_limit": 128000,
                "max_output": 8192,
                "reads_images": true,
                "thinks": true,
                "tools": false,
                "thinking_levels": ["low", "high"],
            }],
            "default_model": model,
        });
        let EventData::ProviderModelsUpdated(data) =
            EventKind::ProviderModelsUpdated.decode(&updated).unwrap()
        else {
            panic!("expected refreshed Models");
        };
        assert_eq!(data.provider, provider);
        assert_eq!(data.default_model, Some(model));
        assert!(!data.models[0].tools);
        assert_eq!(data.models[0].thinking_levels.len(), 2);
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        assert!("neha.future.thing".parse::<EventKind>().is_err());
    }

    #[test]
    fn draft_keeps_the_acting_user() {
        let user = crate::id::Id::new();
        let draft = EventDraft::new(
            EventKind::MessageAdded,
            super::Author::Bot(crate::id::Id::new()),
            serde_json::json!({}),
        )
        .acting(user);

        assert_eq!(draft.acting, Some(user));
    }

    fn file(name: &str, media_type: &str, size: u64, sha256: &str) -> FileAdded {
        FileAdded {
            file: Id::new(),
            chat: Id::new(),
            name: name.to_owned(),
            media_type: media_type.to_owned(),
            size,
            sha256: sha256.to_owned(),
        }
    }

    #[test]
    fn a_file_added_round_trips() {
        let data = file("report.csv", "text/csv", 2048, &"ab".repeat(32));
        let value = serde_json::to_value(&data).unwrap();

        assert_eq!(
            EventKind::FileAdded.decode(&value).unwrap(),
            EventData::FileAdded(data)
        );

        let mut unknown = value.clone();
        unknown["path"] = serde_json::json!("/tmp/report.csv");
        assert!(EventKind::FileAdded.decode(&unknown).is_err());
    }

    #[test]
    fn a_file_keeps_the_bounds_a_space_accepts() {
        let digest = "ab".repeat(32);
        let edge = file(&"a".repeat(255), &"m".repeat(127), MAX_FILE_BYTES, &digest);
        assert_eq!(edge.validate(), Ok(()));

        let long_name = file(&"a".repeat(256), "text/plain", 1, &digest);
        assert_eq!(long_name.validate(), Err(FileError::Name));
        for name in ["", "a/b", "a\\b", "a\0b", ".", ".."] {
            assert_eq!(
                file(name, "text/plain", 1, &digest).validate(),
                Err(FileError::Name)
            );
        }

        let long_type = file("a.txt", &"m".repeat(128), 1, &digest);
        assert_eq!(long_type.validate(), Err(FileError::MediaType));
        for media_type in ["", "text/plain\n", "tëxt/plain"] {
            assert_eq!(
                file("a.txt", media_type, 1, &digest).validate(),
                Err(FileError::MediaType)
            );
        }

        let large = file(
            "a.bin",
            "application/octet-stream",
            MAX_FILE_BYTES + 1,
            &digest,
        );
        assert_eq!(large.validate(), Err(FileError::Size));

        for sha256 in ["ab".repeat(31), "AB".repeat(32), "not a digest".to_owned()] {
            assert_eq!(
                file("a.txt", "text/plain", 1, &sha256).validate(),
                Err(FileError::Digest)
            );
        }
    }

    #[test]
    fn a_file_out_of_bounds_never_reaches_the_record() {
        let draft = EventDraft::new(
            EventKind::FileAdded,
            Author::User(Id::new()),
            serde_json::to_value(file("../secret", "text/plain", 1, &"ab".repeat(32))).unwrap(),
        );

        assert!(draft.validate().is_err());
    }

    #[test]
    fn a_file_event_takes_the_authors_a_message_takes() {
        let user = Author::User(Id::new());
        let bot = Author::Bot(Id::new());
        for kind in [EventKind::MessageAdded, EventKind::FileAdded] {
            assert!(kind.validate_author(user, None).is_ok());
            assert!(kind.validate_author(bot, Some(Id::new())).is_ok());
            assert!(kind.validate_author(user, Some(Id::new())).is_err());
            assert!(kind.validate_author(bot, None).is_err());
        }
    }

    #[test]
    fn a_message_may_carry_a_file() {
        let chat = Id::new();
        let file = Id::new();
        let carried = serde_json::json!({"message": Id::new(), "chat": chat, "text": "see attached", "file": file});
        let EventData::MessageAdded(data) = EventKind::MessageAdded.decode(&carried).unwrap()
        else {
            panic!("expected a Message");
        };
        assert_eq!(data.file, Some(file));

        // A record written before a Message could carry a Card still decodes.
        let plain = serde_json::json!({"message": Id::new(), "chat": chat, "text": "hi"});
        let EventData::MessageAdded(data) = EventKind::MessageAdded.decode(&plain).unwrap() else {
            panic!("expected a Message");
        };
        assert_eq!(data.file, None);
        assert_eq!(data.card, None);
    }

    fn app_card(file: Id) -> serde_json::Value {
        serde_json::json!({
            "kind": "app",
            "file": file,
            "input": {"query": "hi"},
            "result": {"content": [{"type": "text", "text": "the answer"}]},
            "csp": "default-src 'none'; connect-src https://example.com",
        })
    }

    #[test]
    fn a_message_may_carry_an_app_card() {
        let chat = Id::new();
        let file = Id::new();
        let value = serde_json::json!({
            "message": Id::new(),
            "chat": chat,
            "text": "",
            "card": app_card(file),
        });

        let EventData::MessageAdded(data) = EventKind::MessageAdded.decode(&value).unwrap() else {
            panic!("expected a Message");
        };
        let card = data.card.as_ref().expect("the Message carries its Card");
        assert_eq!(card.kind, APP_CARD_KIND);
        assert_eq!(card.file, Some(file));
        assert_eq!(card.input, Some(serde_json::json!({"query": "hi"})));
        assert_eq!(
            card.result,
            Some(serde_json::json!({"content": [{"type": "text", "text": "the answer"}]}))
        );
        assert!(card.csp.is_some());
        // A Card of a kind this build does not know still decodes with its
        // fields, so a newer Space's Card replays instead of corrupting the
        // record; a client shows a placeholder.
        let unknown = serde_json::json!({
            "message": Id::new(),
            "chat": chat,
            "text": "",
            "card": {
                "kind": "future",
                "file": Id::new(),
                "input": {"query": "hi"},
                "result": [1, 2],
                "csp": "default-src 'none'",
            },
        });
        let EventData::MessageAdded(data) = EventKind::MessageAdded.decode(&unknown).unwrap()
        else {
            panic!("expected a Message");
        };
        let card = data.card.as_ref().expect("the Message carries its Card");
        assert_eq!(card.kind, "future");
        assert_eq!(card.input, Some(serde_json::json!({"query": "hi"})));
    }

    #[test]
    fn a_card_keeps_the_bounds_a_space_accepts() {
        let file = Id::new();
        let decoded = |card: serde_json::Value| {
            EventKind::MessageAdded.decode(&serde_json::json!({
                "message": Id::new(),
                "chat": Id::new(),
                "text": "",
                "card": card,
            }))
        };

        let fit = CardData {
            kind: APP_CARD_KIND.to_owned(),
            file: Some(file),
            input: Some(serde_json::json!([1, 2])),
            result: Some(serde_json::Value::Null),
            csp: Some("default-src 'none'".to_owned()),
        };
        assert!(fit.validate().is_ok());

        // An app Card names the File that holds its HTML.
        let mut no_file = fit.clone();
        no_file.file = None;
        assert!(no_file.validate().is_err());

        // A kind this build does not know keeps its fields as bounded opaque
        // data, so a newer Space's Card replays instead of a reader reporting
        // the record corrupt. The same bounds apply to those fields.
        let future = serde_json::json!({
            "kind": "future",
            "file": file,
            "input": {"query": "hi"},
            "result": [1, 2],
            "csp": "default-src 'none'",
        });
        let EventData::MessageAdded(data) = decoded(future).unwrap() else {
            panic!("expected a Message");
        };
        let card = data.card.as_ref().expect("the Message carries its Card");
        assert_eq!(card.kind, "future");
        assert_eq!(card.file, Some(file));
        assert_eq!(card.csp.as_deref(), Some("default-src 'none'"));
        assert!(
            decoded(serde_json::json!({"kind": "future", "input": "text"})).is_err(),
            "a Card's input is an object, an array, or null whatever its kind"
        );
        assert!(
            decoded(serde_json::json!({
                "kind": "future",
                "result": {"padding": "a".repeat(MAX_CARD_VALUE_BYTES)},
            }))
            .is_err(),
            "an unknown kind's payload is bounded"
        );
        assert!(
            decoded(serde_json::json!({
                "kind": "future",
                "csp": "a".repeat(MAX_CARD_CSP_BYTES + 1),
            }))
            .is_err(),
            "an unknown kind's policy is bounded"
        );

        // An input or result is an object, an array, or null.
        for value in [
            serde_json::json!("text"),
            serde_json::json!(1),
            serde_json::json!(true),
        ] {
            let mut card = fit.clone();
            card.input = Some(value.clone());
            assert!(card.validate().is_err(), "{value}");
            let mut card = fit.clone();
            card.result = Some(value);
            assert!(card.validate().is_err());
        }

        // A value over the bound is refused.
        let large = serde_json::json!({"padding": "a".repeat(MAX_CARD_VALUE_BYTES)});
        let mut card = fit.clone();
        card.result = Some(large.clone());
        assert!(card.validate().is_err());
        assert!(
            decoded(serde_json::json!({
                "kind": "app",
                "file": file,
                "result": large,
            }))
            .is_err()
        );

        // A policy is printable ASCII within the bound.
        for csp in [
            "a\nb".to_owned(),
            "é".to_owned(),
            "a".repeat(MAX_CARD_CSP_BYTES + 1),
        ] {
            let mut card = fit.clone();
            card.csp = Some(csp.clone());
            assert!(card.validate().is_err(), "{csp:?}");
        }
        let mut edge = fit.clone();
        edge.csp = Some("a".repeat(MAX_CARD_CSP_BYTES));
        assert!(edge.validate().is_ok());

        // An unknown field on the card itself is refused.
        let mut extra = app_card(file);
        extra["text"] = serde_json::json!("no");
        assert!(decoded(extra).is_err());
    }

    fn computer_created_data(kind: &str) -> serde_json::Value {
        serde_json::json!({
            "computer": Id::new(),
            "owner": Id::new(),
            "kind": kind,
            "workspace": "/tmp/workspace",
        })
    }

    #[test]
    fn a_computer_kind_reads_its_old_names_and_writes_the_new_ones() {
        for (recorded, expected) in [
            ("local", ComputerKind::Local),
            ("real", ComputerKind::Local),
            ("hosted", ComputerKind::Hosted),
            ("managed", ComputerKind::Hosted),
        ] {
            let decoded = EventKind::ComputerCreated
                .decode(&computer_created_data(recorded))
                .unwrap();
            let EventData::ComputerCreated(created) = decoded else {
                panic!("a ComputerCreated event decodes to its data")
            };
            assert_eq!(created.kind, expected, "{recorded} reads as {expected:?}");
        }

        assert_eq!(
            serde_json::to_value(ComputerKind::Local).unwrap(),
            serde_json::json!("local"),
            "new events write canonical names"
        );
        assert_eq!(
            serde_json::to_value(ComputerKind::Hosted).unwrap(),
            serde_json::json!("hosted")
        );
    }

    #[test]
    fn an_unknown_computer_kind_is_refused() {
        let error = EventKind::ComputerCreated
            .decode(&computer_created_data("mainframe"))
            .unwrap_err();

        assert!(matches!(error, EventDataError::Data(_)), "{error}");
    }
    #[test]
    fn a_run_reply_round_trips() {
        let run = Id::new();
        let value = serde_json::json!({
            "run": run,
            "step": 2,
            "content": "Let me look",
            "calls": [
                {
                    "call": "call-2-0",
                    "tool": "shell",
                    "arguments": {"command": "pwd"},
                    "refused": null,
                },
            ],
            "usage": {
                "uncached_input": 11,
                "cache_read": 2,
                "cache_write": 0,
                "output": 5,
                "reasoning": 0,
            },
        });

        let EventData::RunReply(reply) = EventKind::RunReply.decode(&value).unwrap() else {
            panic!("a reply decodes to its data");
        };
        assert_eq!(reply.run, run);
        assert_eq!(reply.step, 2);
        assert_eq!(reply.content.as_deref(), Some("Let me look"));
        assert_eq!(reply.calls.len(), 1);
        assert_eq!(reply.calls[0].call, "call-2-0");
        assert_eq!(reply.calls[0].tool, "shell");
        assert!(reply.usage.is_some());

        let mut unknown = value;
        unknown["text"] = serde_json::json!("no");
        assert!(EventKind::RunReply.decode(&unknown).is_err());
    }

    #[test]
    fn an_approval_requested_round_trips() {
        let ids: Vec<Id> = (0..7).map(|_| Id::new()).collect();
        let value = serde_json::json!({
            "approval": ids[0],
            "run": ids[1],
            "bot": ids[2],
            "computer": ids[6],
            "chat": ids[3],
            "acting": ids[4],
            "action": ids[5],
            "index": 0,
            "step": 1,
            "call": "call-1-0",
            "tool": "shell",
            "arguments": {"command": "pwd"},
            "summary": "Run pwd",
            "reason": "first_use_local",
            "ambiguous": false,
            "lasting_scope": null,
            "undoable": "no",
        });

        let EventData::ApprovalRequested(requested) =
            EventKind::ApprovalRequested.decode(&value).unwrap()
        else {
            panic!("an approval request decodes to its data");
        };
        assert_eq!(requested.approval, ids[0]);
        assert_eq!(requested.computer, ids[6]);
        assert_eq!(requested.action, ids[5]);
        assert_eq!(requested.index, 0);
        assert_eq!(requested.step, 1);
        assert_eq!(requested.undoable, super::Undoability::No);
        assert_eq!(requested.lasting_scope, None);
        assert_eq!(requested.call, "call-1-0");
        assert_eq!(requested.tool, "shell");
        assert_eq!(requested.summary, "Run pwd");
        assert_eq!(requested.reason, super::ApprovalReason::FirstUseLocal);
        assert!(!requested.ambiguous);
        assert_eq!(requested.rule, None);

        let mut unknown = value;
        unknown["group"] = serde_json::json!(Id::new());
        assert!(EventKind::ApprovalRequested.decode(&unknown).is_err());
    }

    #[test]
    fn an_approval_decided_round_trips() {
        let (approval, run, user, rule) = (Id::new(), Id::new(), Id::new(), Id::new());
        let value = serde_json::json!({
            "approval": approval,
            "run": run,
            "allow": true,
            "lasting": true,
            "user": user,
            "rule": rule,
        });

        let EventData::ApprovalDecided(decided) =
            EventKind::ApprovalDecided.decode(&value).unwrap()
        else {
            panic!("a decision decodes to its data");
        };
        assert_eq!(decided.approval, approval);
        assert_eq!(decided.run, run);
        assert!(decided.allow);
        assert!(decided.lasting);
        assert_eq!(decided.user, user);
        assert_eq!(decided.rule, Some(rule));
    }

    #[test]
    fn a_rule_added_and_removed_round_trip() {
        let (rule, bot, user) = (Id::new(), Id::new(), Id::new());
        let added = serde_json::json!({
            "rule": rule,
            "bot": bot,
            "user": user,
            "effect": "allow",
            "tool": "shell",
            "command_prefix": "git",
        });
        let EventData::RuleAdded(data) = EventKind::RuleAdded.decode(&added).unwrap() else {
            panic!("a rule decodes to its data");
        };
        assert_eq!(data.rule, rule);
        assert_eq!(data.bot, bot);
        assert_eq!(data.user, user);
        assert_eq!(data.effect, super::RuleEffect::Allow);
        assert_eq!(data.tool, "shell");
        assert_eq!(data.command_prefix.as_deref(), Some("git"));
        assert_eq!(data.path_prefix, None);

        let removed = serde_json::json!({"rule": rule, "bot": bot, "user": user});
        let EventData::RuleRemoved(data) = EventKind::RuleRemoved.decode(&removed).unwrap() else {
            panic!("a removed rule decodes to its data");
        };
        assert_eq!((data.rule, data.bot, data.user), (rule, bot, user));
    }

    #[test]
    fn a_reply_and_an_approval_take_a_bot_author() {
        let user = Author::User(Id::new());
        let bot = Author::Bot(Id::new());
        for kind in [EventKind::RunReply, EventKind::ApprovalRequested] {
            assert!(kind.validate_author(bot, Some(Id::new())).is_ok());
            assert!(kind.validate_author(user, None).is_err());
            assert!(kind.validate_author(user, Some(Id::new())).is_err());
            assert!(kind.validate_author(bot, None).is_err());
        }
    }

    #[test]
    fn a_decision_and_a_rule_take_their_user_as_acting() {
        let owner = Id::new();
        let user = Author::User(owner);
        let bot = Author::Bot(Id::new());
        for kind in [
            EventKind::ApprovalDecided,
            EventKind::RuleAdded,
            EventKind::RuleRemoved,
        ] {
            assert!(kind.validate_author(user, Some(owner)).is_ok());
            assert!(kind.validate_author(user, None).is_err());
            assert!(kind.validate_author(bot, Some(owner)).is_err());
        }
    }

    #[test]
    fn a_skill_takes_a_user_author_who_acts_for_nobody() {
        let user = Author::User(Id::new());
        let bot = Author::Bot(Id::new());
        for kind in [
            EventKind::SkillAdded,
            EventKind::SkillRemoved,
            EventKind::BotSkillEnabled,
            EventKind::BotSkillDisabled,
        ] {
            assert!(kind.validate_author(user, None).is_ok());
            assert!(kind.validate_author(user, Some(Id::new())).is_err());
            assert!(kind.validate_author(bot, None).is_err());
        }
    }
}

/// Where a run is: waiting, working, or how it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// The run waits for its Bot to finish the work before it.
    Queued,
    /// The run is still working.
    Running,
    /// The run waits for the acting User to allow or deny an action.
    Waiting,
    /// The run produced a reply.
    Succeeded,
    /// The run failed.
    Failed,
}

/// The data each event kind carries.
#[derive(Clone, Debug, PartialEq)]
pub enum EventData {
    /// A User joined the Space.
    UserJoined(UserJoined),
    /// A Provider was connected.
    ProviderAdded(ProviderAdded),
    /// A Provider's Models were refreshed.
    ProviderModelsUpdated(ProviderModelsUpdated),
    /// A Bot was created.
    BotCreated(BotCreated),
    /// A Bot moved to another Computer.
    BotMoved(BotMoved),
    /// A Bot's owner changed its file access.
    BotUpdated(BotUpdated),
    /// A Bot's owner renamed it.
    BotRenamed(BotRenamed),
    /// A Computer was created.
    ComputerCreated(ComputerCreated),
    /// A Chat was created.
    ChatCreated(ChatCreated),
    /// A Message was added.
    MessageAdded(MessageAdded),
    /// A File was added.
    FileAdded(FileAdded),
    /// A run entered its Bot's queue.
    RunQueued(RunQueued),
    /// A run started.
    RunStarted(RunStarted),
    /// The directory a run works in.
    RunContext(RunContext),
    /// The file access a run's actions carry.
    RunAccess(RunAccess),
    /// A run finished.
    RunFinished(RunFinished),
    /// A run completed a step.
    RunStep(RunStep),
    /// A run started an action.
    ActionStarted(ActionStarted),
    /// A run finished an action.
    ActionFinished(ActionFinished),
    /// The Model's reply for one step.
    RunReply(RunReply),
    /// A run waits for an approval.
    ApprovalRequested(ApprovalRequested),
    /// The acting User answered an approval.
    ApprovalDecided(ApprovalDecided),
    /// A lasting rule was added.
    RuleAdded(RuleAdded),
    /// A lasting rule was removed.
    RuleRemoved(RuleRemoved),
    SkillAdded(SkillAdded),
    SkillRemoved(SkillRemoved),
    BotSkillEnabled(BotSkillEnabled),
    BotSkillDisabled(BotSkillDisabled),
    /// An MCP server was connected.
    McpServerAdded(McpServerAdded),
    /// An MCP server changed.
    McpServerUpdated(McpServerUpdated),
    /// An MCP server was disconnected.
    McpServerRemoved(McpServerRemoved),
    /// A Bot may use an MCP server.
    BotMcpEnabled(BotMcpEnabled),
    /// A Bot may no longer use an MCP server.
    BotMcpDisabled(BotMcpDisabled),
    /// A package was installed, with what it brought.
    PluginInstalled(PluginInstalled),
    /// A package was removed.
    PluginRemoved(PluginRemoved),
    /// The Space offers a package.
    PluginEnabled(PluginEnabled),
    /// The Space no longer offers a package.
    PluginDisabled(PluginDisabled),
}

/// The first version of every event kind.
pub const KIND_VERSION: u16 = 1;

/// The version of the owner's answer that allows a Bot full access.
///
/// Full access takes the machine user's own rights, so the record keeps the
/// answer that widened it: a record written before this version replays as
/// granted, and the owner turns full access on again with an explicit answer.
pub const FULL_ACCESS_CONSENT_VERSION: u16 = 1;

impl EventKind {
    /// Returns the version of this kind this build writes.
    #[must_use]
    pub const fn supported_version(self) -> u16 {
        KIND_VERSION
    }

    /// Decodes and validates a kind's data.
    pub fn decode(self, data: &serde_json::Value) -> Result<EventData, EventDataError> {
        let decoded = match self {
            Self::UserJoined => EventData::UserJoined(decode(data)?),
            Self::ProviderAdded => EventData::ProviderAdded(decode(data)?),
            Self::ProviderModelsUpdated => EventData::ProviderModelsUpdated(decode(data)?),
            Self::BotCreated => EventData::BotCreated(decode(data)?),
            Self::BotMoved => EventData::BotMoved(decode(data)?),
            Self::BotUpdated => EventData::BotUpdated(decode(data)?),
            Self::BotRenamed => EventData::BotRenamed(decode(data)?),
            Self::ComputerCreated => EventData::ComputerCreated(decode(data)?),
            Self::ChatCreated => EventData::ChatCreated(decode(data)?),
            Self::MessageAdded => {
                let data: MessageAdded = decode(data)?;
                data.validate()?;
                EventData::MessageAdded(data)
            }
            Self::FileAdded => {
                let data: FileAdded = decode(data)?;
                data.validate()?;

                EventData::FileAdded(data)
            }
            Self::RunQueued => EventData::RunQueued(decode(data)?),
            Self::RunStarted => EventData::RunStarted(decode(data)?),
            Self::RunContext => EventData::RunContext(decode(data)?),
            Self::RunAccess => EventData::RunAccess(decode(data)?),
            Self::RunFinished => {
                let data: RunFinished = decode(data)?;
                // Waiting is where a run stands while it asks, not how it
                // ends: a reader that saw it as an end would resume nothing.
                if matches!(data.status, RunStatus::Queued | RunStatus::Waiting) {
                    return Err(EventDataError::Data(
                        "a finished run cannot be queued or waiting".to_owned(),
                    ));
                }

                EventData::RunFinished(data)
            }
            Self::RunStep => EventData::RunStep(decode(data)?),
            Self::ActionStarted => EventData::ActionStarted(decode(data)?),
            Self::ActionFinished => EventData::ActionFinished(decode(data)?),
            Self::RunReply => EventData::RunReply(decode(data)?),
            Self::ApprovalRequested => EventData::ApprovalRequested(decode(data)?),
            Self::ApprovalDecided => EventData::ApprovalDecided(decode(data)?),
            Self::RuleAdded => EventData::RuleAdded(decode(data)?),
            Self::RuleRemoved => EventData::RuleRemoved(decode(data)?),
            Self::SkillAdded => {
                let data: SkillAdded = decode(data)?;
                data.validate()?;
                EventData::SkillAdded(data)
            }
            Self::SkillRemoved => EventData::SkillRemoved(decode(data)?),
            Self::BotSkillEnabled => EventData::BotSkillEnabled(decode(data)?),
            Self::BotSkillDisabled => EventData::BotSkillDisabled(decode(data)?),
            Self::McpServerAdded => {
                let data: McpServerAdded = decode(data)?;
                data.validate()?;
                EventData::McpServerAdded(data)
            }
            Self::McpServerUpdated => EventData::McpServerUpdated(decode(data)?),
            Self::McpServerRemoved => EventData::McpServerRemoved(decode(data)?),
            Self::BotMcpEnabled => EventData::BotMcpEnabled(decode(data)?),
            Self::BotMcpDisabled => EventData::BotMcpDisabled(decode(data)?),
            Self::PluginInstalled => {
                let data: PluginInstalled = decode(data)?;
                data.validate()?;
                EventData::PluginInstalled(data)
            }
            Self::PluginRemoved => EventData::PluginRemoved(decode(data)?),
            Self::PluginEnabled => EventData::PluginEnabled(decode(data)?),
            Self::PluginDisabled => EventData::PluginDisabled(decode(data)?),
        };

        Ok(decoded)
    }

    /// Checks that the author and acting User fit the kind.
    pub fn validate_author(self, author: Author, acting: Option<Id>) -> Result<(), EventDataError> {
        match self {
            Self::UserJoined
            | Self::ProviderAdded
            | Self::ProviderModelsUpdated
            | Self::BotCreated
            | Self::BotMoved
            | Self::BotUpdated
            | Self::BotRenamed
            | Self::ComputerCreated
            | Self::ChatCreated
            | Self::SkillAdded
            | Self::SkillRemoved
            | Self::BotSkillEnabled
            | Self::BotSkillDisabled
            | Self::McpServerAdded
            | Self::McpServerUpdated
            | Self::McpServerRemoved
            | Self::BotMcpEnabled
            | Self::BotMcpDisabled
            | Self::PluginInstalled
            | Self::PluginRemoved
            | Self::PluginEnabled
            | Self::PluginDisabled => {
                if !matches!(author, Author::User(_)) || acting.is_some() {
                    return Err(EventDataError::Author);
                }
            }
            Self::MessageAdded | Self::FileAdded => match (author, acting) {
                (Author::User(_), None) | (Author::Bot(_), Some(_)) => {}
                _ => return Err(EventDataError::Author),
            },
            Self::RunQueued
            | Self::RunStarted
            | Self::RunContext
            | Self::RunAccess
            | Self::RunFinished
            | Self::RunStep
            | Self::ActionStarted
            | Self::ActionFinished
            | Self::RunReply
            | Self::ApprovalRequested => {
                if !matches!(author, Author::Bot(_)) || acting.is_none() {
                    return Err(EventDataError::Author);
                }
            }
            Self::ApprovalDecided | Self::RuleAdded | Self::RuleRemoved => {
                if !matches!(author, Author::User(_)) || acting.is_none() {
                    return Err(EventDataError::Author);
                }
            }
        }

        Ok(())
    }
}

fn decode<T: serde::de::DeserializeOwned>(data: &serde_json::Value) -> Result<T, EventDataError> {
    serde_json::from_value(data.clone()).map_err(|error| EventDataError::Data(error.to_string()))
}

impl EventDraft {
    /// Checks the kind, version, author, and data before the record holds it.
    pub fn validate(&self) -> Result<(), EventDataError> {
        if self.version != self.kind.supported_version() {
            return Err(EventDataError::Version(self.version));
        }
        self.kind.validate_author(self.author, self.acting)?;
        self.kind.decode(&self.data)?;

        Ok(())
    }
}

/// An event does not fit the schema of its kind.
#[derive(Debug, thiserror::Error)]
pub enum EventDataError {
    /// The kind's data does not match its schema.
    #[error("event data does not match its kind: {0}")]
    Data(String),
    /// The author or acting User does not fit the kind.
    #[error("the event's author does not fit its kind")]
    Author,
    /// The version is not one this build writes.
    #[error("event version {0} is not supported")]
    Version(u16),
}

/// The data of `neha.user.joined`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct UserJoined {
    /// The display name.
    pub display_name: String,
    /// Whether the account is the Admin.
    pub is_admin: bool,
}

/// The data of `neha.provider.added`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct ProviderAdded {
    /// The Provider identifier.
    pub provider: Id,
    /// The catalog vendor this Provider came from.
    #[serde(default)]
    pub vendor: Option<String>,
    /// The protocol it speaks.
    pub protocol: String,
    /// Its base address.
    pub base_url: String,
    /// The Models it offers.
    pub models: Vec<ModelData>,
    /// The default Model.
    #[serde(default)]
    pub default_model: Option<Id>,
    /// The Space's default thinking level.
    #[serde(default)]
    pub thinking: Option<crate::surface::ThinkingLevel>,
}

/// One Model a Provider offers.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct ModelData {
    /// The Model identifier.
    pub id: Id,
    /// The name the Provider expects.
    pub name: String,
    /// The context window in tokens.
    pub context_limit: u32,
    /// The largest output in tokens.
    pub max_output: u32,
    /// Whether it reads images.
    pub reads_images: bool,
    /// Whether it can think before it answers.
    #[serde(default)]
    pub thinks: bool,
    /// Whether it can call tools.
    #[serde(default = "model_tools_default")]
    pub tools: bool,
    /// The thinking levels it supports; empty means every level.
    #[serde(default)]
    pub thinking_levels: Vec<crate::surface::ThinkingLevel>,
}

/// Models recorded before tools were a capability could call them.
const fn model_tools_default() -> bool {
    true
}

/// The data of `neha.provider.models_updated`.
///
/// A refresh replaces the Models the Provider offers. Choosing the Space's
/// default Model also names the Provider's whole current list, so every Model
/// keeps its identity and the projection holds one default.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct ProviderModelsUpdated {
    /// The Provider.
    pub provider: Id,
    /// The Models it now offers.
    pub models: Vec<ModelData>,
    /// The Model the Space uses by default now.
    #[serde(default)]
    pub default_model: Option<Id>,
}

/// The data of `neha.bot.created`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct BotCreated {
    /// The Bot identifier.
    pub bot: Id,
    /// The User who owns it.
    pub owner: Id,
    /// Its name.
    pub name: String,
    /// Its Computer.
    pub computer: Id,
    /// Its Chat with the owner.
    pub chat: Id,
    /// Its thinking level; absent uses the Space's default.
    #[serde(default)]
    pub thinking: Option<crate::surface::ThinkingLevel>,
    /// Its Model; absent uses the Space's default.
    #[serde(default)]
    pub model: Option<Id>,
}

/// The data of `neha.bot.moved`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct BotMoved {
    /// The Bot that moved.
    pub bot: Id,
    /// The Computer it worked on.
    pub from: Id,
    /// The Computer it works on now.
    pub to: Id,
}

/// The data of `neha.bot.updated`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct BotUpdated {
    /// The Bot.
    pub bot: Id,
    /// Whether it may write outside its Workspace fence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_access: Option<bool>,
    /// The version of the owner's answer that allows full access.
    ///
    /// Absent in a record written before the answer was versioned; such a
    /// record replays as granted, and only a new explicit answer turns full
    /// access on again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_access_version: Option<u16>,
    /// The complete list of extra folders it may write.
    ///
    /// `Some(empty)` removes every grant; `None` leaves them unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grants: Option<Vec<String>>,
    /// The Model it uses; absent leaves it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Id>,
    /// Its thinking level; absent leaves it unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<crate::surface::ThinkingLevel>,
    /// Whether it goes back to the Space's default Model.
    ///
    /// A record written before the field replays false, so an old update
    /// never clears a Model the owner chose.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset_model: bool,
    /// Whether it goes back to the Space's default thinking level.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reset_thinking: bool,
}

/// The data of `neha.bot.renamed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct BotRenamed {
    /// The Bot.
    pub bot: Id,
    /// Its new name.
    pub name: String,
}

/// What a Computer is: the machine Neha runs on, or the Space itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComputerKind {
    /// The machine the User runs Neha on.
    Local,
    /// A Computer the Space runs itself.
    Hosted,
}

impl ComputerKind {
    /// Returns the canonical recorded name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Hosted => "hosted",
        }
    }

    /// Reads a recorded name.
    ///
    /// Records written before the names settled say `real` and `managed`; a
    /// reading build maps them to the current names instead of failing, so an
    /// old record still replays. Anything else is an error: a reader must not
    /// guess what an unknown kind means.
    #[must_use]
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "local" | "real" => Some(Self::Local),
            "hosted" | "managed" => Some(Self::Hosted),
            _ => None,
        }
    }
}

impl Serialize for ComputerKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ComputerKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let kind = String::deserialize(deserializer)?;
        Self::parse(&kind)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown Computer kind `{kind}`")))
    }
}

/// The data of `neha.computer.created`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct ComputerCreated {
    /// The Computer identifier.
    pub computer: Id,
    /// The User who owns it.
    pub owner: Id,
    /// The kind of Computer.
    pub kind: ComputerKind,
    /// The Computer's workspace directory.
    pub workspace: String,
}

/// The data of `neha.chat.created`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct ChatCreated {
    /// The Chat identifier.
    pub chat: Id,
    /// The User who owns it.
    pub owner: Id,
    /// The Bot the Chat is with.
    pub bot: Id,
}

/// The data of `neha.message.added`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct MessageAdded {
    /// The Message identifier.
    pub message: Id,
    /// The Chat it belongs to.
    pub chat: Id,
    /// Its text.
    pub text: String,
    /// The File it carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Id>,
    /// The run it belongs to.
    #[serde(default)]
    pub run: Option<Id>,
    /// The Card it carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<CardData>,
}

impl MessageAdded {
    /// Checks the Card a Message carries fits what a Space accepts.
    pub fn validate(&self) -> Result<(), EventDataError> {
        if let Some(card) = &self.card {
            card.validate()?;
        }

        Ok(())
    }
}

/// The kind of Card an MCP tool's app makes.
pub const APP_CARD_KIND: &str = "app";

/// The largest JSON a Card carries for its input or its result.
pub const MAX_CARD_VALUE_BYTES: usize = 64 * 1024;

/// The largest Content Security Policy a Card carries.
pub const MAX_CARD_CSP_BYTES: usize = 2 * 1024;

/// A Card a Message shows after its text.
///
/// A Card's kind picks the fields it takes: an `app` Card names the File that
/// holds its HTML and carries the tool's input and result, while a kind this
/// build does not know keeps its fields as bounded opaque data, so a client
/// shows a placeholder and a reader still replays the record.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct CardData {
    /// The Card's kind.
    pub kind: String,
    /// The File that holds an `app` Card's HTML.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Id>,
    /// The tool arguments an `app` Card shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "unknown")]
    pub input: Option<serde_json::Value>,
    /// The tool result an `app` Card shows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(type = "unknown")]
    pub result: Option<serde_json::Value>,
    /// The Content Security Policy a client applies to an `app` Card.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub csp: Option<String>,
}

impl CardData {
    /// Checks the fields a Card's kind takes before the record holds them.
    ///
    /// An `app` Card must name the File that holds its HTML. A kind this build
    /// does not know is kept as opaque bounded data, so a reader replays a Card
    /// a newer Space wrote instead of reporting the whole record corrupt.
    pub fn validate(&self) -> Result<(), EventDataError> {
        if self.kind == APP_CARD_KIND && self.file.is_none() {
            return Err(EventDataError::Data(
                "an app Card must carry the File that holds its HTML".to_owned(),
            ));
        }
        for (what, value) in [("input", &self.input), ("result", &self.result)] {
            let Some(value) = value else {
                continue;
            };
            if !matches!(
                value,
                serde_json::Value::Object(_)
                    | serde_json::Value::Array(_)
                    | serde_json::Value::Null
            ) {
                return Err(EventDataError::Data(format!(
                    "a Card's {what} must be an object, an array, or null"
                )));
            }
            let length = serde_json::to_vec(value)
                .map_err(|error| EventDataError::Data(error.to_string()))?
                .len();
            if length > MAX_CARD_VALUE_BYTES {
                return Err(EventDataError::Data(format!(
                    "a Card's {what} must be at most {} KiB",
                    MAX_CARD_VALUE_BYTES / 1024
                )));
            }
        }
        if let Some(csp) = &self.csp
            && (csp.len() > MAX_CARD_CSP_BYTES
                || !csp
                    .bytes()
                    .all(|byte| byte.is_ascii_graphic() || byte == b' '))
        {
            return Err(EventDataError::Data(format!(
                "a Card's policy must be at most {} KiB of printable ASCII",
                MAX_CARD_CSP_BYTES / 1024
            )));
        }

        Ok(())
    }
}

/// The largest name a File carries.
pub const MAX_FILE_NAME_BYTES: usize = 255;
/// The largest media type a File carries.
const MAX_MEDIA_TYPE_BYTES: usize = 127;

/// The data of `neha.file.added`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct FileAdded {
    /// The File identifier.
    pub file: Id,
    /// The Chat it belongs to.
    pub chat: Id,
    /// Its name, which is never a path.
    pub name: String,
    /// Its media type.
    pub media_type: String,
    /// Its size in bytes.
    pub size: u64,
    /// Its lowercase hex SHA-256.
    pub sha256: String,
}

/// A File does not fit the bounds a Space accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FileError {
    /// The name is empty, too long, or a path.
    #[error("a File name must be 1 to 255 bytes and must not be a path")]
    Name,
    /// The media type is empty, too long, or not printable ASCII.
    #[error("a File's media type must be 1 to 127 bytes of printable ASCII")]
    MediaType,
    /// The size is over the largest File a Space accepts.
    #[error("a File must be at most 32 MiB")]
    Size,
    /// The digest is not a lowercase hex SHA-256.
    #[error("a File's sha256 must be 64 lowercase hexadecimal digits")]
    Digest,
}

impl From<FileError> for EventDataError {
    fn from(error: FileError) -> Self {
        Self::Data(error.to_string())
    }
}

impl FileAdded {
    /// Checks the name of a File before the Space holds its bytes.
    pub fn check_name(name: &str) -> Result<(), FileError> {
        if name.is_empty()
            || name.len() > MAX_FILE_NAME_BYTES
            || name.contains(['/', '\\', '\0'])
            || name == "."
            || name == ".."
        {
            return Err(FileError::Name);
        }

        Ok(())
    }

    /// Checks the media type of a File before the Space holds its bytes.
    pub fn check_media_type(media_type: &str) -> Result<(), FileError> {
        if media_type.is_empty()
            || media_type.len() > MAX_MEDIA_TYPE_BYTES
            || !media_type
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            return Err(FileError::MediaType);
        }

        Ok(())
    }

    /// Checks that the File fits the bounds every Space accepts.
    pub fn validate(&self) -> Result<(), FileError> {
        Self::check_name(&self.name)?;
        Self::check_media_type(&self.media_type)?;
        if self.size > MAX_FILE_BYTES {
            return Err(FileError::Size);
        }
        if !is_sha256(&self.sha256) {
            return Err(FileError::Digest);
        }

        Ok(())
    }
}

/// The data of `neha.run.queued`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct RunQueued {
    /// The run.
    pub run: Id,
    /// The Chat it works in.
    pub chat: Id,
    /// The Bot that works it.
    pub bot: Id,
    /// The command that queued it.
    pub command: Id,
    /// The Message that started it.
    pub message: Id,
}

/// The data of `neha.run.started`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct RunStarted {
    /// The run.
    pub run: Id,
    /// The Chat.
    pub chat: Id,
    /// The Bot.
    pub bot: Id,
    /// The command that started it.
    pub command: Id,
    /// The Message that started it.
    pub message: Id,
}

/// How a Bot's action ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    /// The action did what it was asked.
    Succeeded,
    /// The action returned an error.
    Failed,
    /// The action ran past its time limit and was killed.
    TimedOut,
    /// The action's result never returned.
    Unknown,
}

/// The data of `neha.run.step`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct RunStep {
    /// The run.
    pub run: Id,
    /// The step number, starting at one.
    pub step: u32,
    /// The usage the Model reported.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// How many tool calls the step requested.
    pub tool_calls: u32,
}

/// The data of `neha.action.started`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct ActionStarted {
    /// The run.
    pub run: Id,
    /// The step.
    pub step: u32,
    /// The action.
    pub action: Id,
    /// The tool's name.
    pub tool: String,
    /// The tool's arguments. A refused call keeps them malformed, so the value
    /// is not always an object.
    #[serde(default)]
    #[ts(type = "unknown")]
    pub arguments: serde_json::Value,
    /// The MCP server the action called, when its tool belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Id>,
    /// The skill the action runs, when it is a `run_skill_script` action.
    ///
    /// This is the identity the Space dispatched, resolved before the action
    /// started: it is absent for a call that never runs, and a record written
    /// before it existed has none either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<Id>,
    /// The script's file name within that skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// The digest of the script's bytes the action ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_sha256: Option<String>,
}

/// The data of `neha.action.finished`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct ActionFinished {
    /// The run.
    pub run: Id,
    /// The step.
    pub step: u32,
    /// The action.
    pub action: Id,
    /// The tool's name: the server's original name when it belongs to one.
    pub tool: String,
    /// The MCP server the action called, when its tool belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Id>,
    /// How it ended.
    pub status: ActionStatus,
    /// The text the Model reads.
    pub output: String,
    /// Whether the output dropped bytes.
    #[serde(default)]
    pub truncated: bool,
    /// How many bytes the output dropped.
    #[serde(default)]
    pub omitted_bytes: u64,
    /// The failure, when it failed.
    #[serde(default)]
    pub error: Option<String>,
}

/// The data of `neha.run.context`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct RunContext {
    /// The run.
    pub run: Id,
    /// The directory it works in.
    pub working_dir: String,
    /// Whether the User named that directory for this run, which makes it a
    /// write root for this run alone.
    ///
    /// Absent in a record written before the marker existed: such a run cannot
    /// be restored, because starting it here would widen the old record into a
    /// write root the record never authorized.
    #[serde(default)]
    pub writable: bool,
}

/// The data of `neha.run.access`.
///
/// The Space writes it with `neha.run.started`: the access the Bot's owner had
/// granted when the run began is the access every action of that run carries,
/// and a later change to the Bot applies to the next run.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct RunAccess {
    /// The run.
    pub run: Id,
    /// Whether full access was on when the run started.
    pub full_access: bool,
    /// The folders the owner had granted when the run started.
    pub grants: Vec<String>,
    /// Whether the run's directory was recorded as a write root for it.
    ///
    /// Absent in a record written before the marker existed: a run with a
    /// directory and no marker cannot be restored. A run with no directory
    /// never needs it.
    #[serde(default)]
    pub cwd_writable: bool,
}

/// The data of `neha.run.finished`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct RunFinished {
    /// The run.
    pub run: Id,
    /// How it ended.
    pub status: RunStatus,
    /// The Bot's reply Message.
    #[serde(default)]
    pub reply: Option<Id>,
    /// The usage the Model reported.
    #[serde(default)]
    pub usage: Option<Usage>,
    /// The failure.
    #[serde(default)]
    pub error: Option<String>,
}

/// One tool call the Model asked for in one step.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct RecordedCall {
    /// The tool-call identifier the provider expects in the result.
    pub call: String,
    /// The tool's name: the server's original name when it belongs to one.
    pub tool: String,
    /// The MCP server the call names, when its tool belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Id>,
    /// The name the Model used for this call, when the record names the tool
    /// by its server and original name.
    ///
    /// The loop's history and a replay read the name the Model saw, so a
    /// record whose server no longer offers the tool still resumes with the
    /// same conversation. An older record has none, and the Space forms the
    /// name it would have offered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    /// The arguments the tool runs with; a refused call keeps them malformed.
    #[ts(type = "unknown")]
    pub arguments: serde_json::Value,
    /// Why the loop refused the call before anything ran; `None` when it can run.
    #[serde(default)]
    pub refused: Option<String>,
}

/// The data of `neha.run.reply`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct RunReply {
    /// The run.
    pub run: Id,
    /// The step number, starting at one.
    pub step: u32,
    /// What the Model said, when it said anything.
    #[serde(default)]
    pub content: Option<String>,
    /// The calls the step asked to run.
    pub calls: Vec<RecordedCall>,
    /// The usage the Model reported for the reply.
    #[serde(default)]
    pub usage: Option<Usage>,
}

/// Why a run asks its acting User for an approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReason {
    /// The first use of a Local Computer by this Bot for this User.
    FirstUseLocal,
    /// A lasting rule asks first.
    Rule,
    /// The action writes outside the folders the owner granted.
    Grant,
    /// The first run of a skill's script by this Bot for this User.
    FirstScriptRun,
    /// The first call of an MCP server's tool by this Bot for this User.
    FirstMcpCall,
}

/// Whether the User can undo an action after allowing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(rename_all = "snake_case")]
pub enum Undoability {
    /// The action cannot be undone.
    No,
    /// The action may or may not be undoable; this build cannot tell.
    Unknown,
}

/// What a lasting rule does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(rename_all = "snake_case")]
pub enum RuleEffect {
    /// Ask before a matching action.
    Ask,
    /// Allow matching actions without asking.
    Allow,
}

/// The data of `neha.approval.requested`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequested {
    /// The approval.
    pub approval: Id,
    /// The run that waits.
    pub run: Id,
    /// The Bot.
    pub bot: Id,
    /// The Computer the run works on; a first use counts per machine.
    pub computer: Id,
    /// The Chat.
    pub chat: Id,
    /// The User the Bot acts for; they answer.
    pub acting: Id,
    /// The action that waits.
    pub action: Id,
    /// The call's position among the step's calls, from zero.
    pub index: u32,
    /// The step.
    pub step: u32,
    /// The tool-call identifier.
    pub call: String,
    /// The tool's name.
    pub tool: String,
    /// The arguments the action will run with.
    #[ts(type = "Record<string, unknown>")]
    pub arguments: serde_json::Value,
    /// A short text the User reads.
    pub summary: String,
    /// Why this run asks.
    pub reason: ApprovalReason,
    /// Whether a command form defeated the rules and forced the ask (the
    /// reason is then `rule`, but no rule actually matched).
    #[serde(default)]
    pub ambiguous: bool,
    /// The rule that asked, when a rule did.
    #[serde(default)]
    pub rule: Option<Id>,
    /// What an "Always allow" would allow, in words the User reads.
    #[serde(default)]
    pub lasting_scope: Option<String>,
    /// The MCP server the action calls, when its tool belongs to one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Id>,
    /// The skill the action runs when its tool is `run_skill_script`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<Id>,
    /// The script's file name within that skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// The digest of the skill's script the card was made for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_sha256: Option<String>,
    /// The folder a grant answer covers, when the reason is a grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_folder: Option<String>,
    /// Whether the action can still be undone after it runs.
    pub undoable: Undoability,
}

/// The data of `neha.approval.decided`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct ApprovalDecided {
    /// The approval.
    pub approval: Id,
    /// The run that waited.
    pub run: Id,
    /// Whether the User allowed it.
    pub allow: bool,
    /// Whether this also makes a lasting rule.
    pub lasting: bool,
    /// The User who answered.
    pub user: Id,
    /// The rule the answer added, if any.
    #[serde(default)]
    pub rule: Option<Id>,
}

/// The data of `neha.rule.added`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct RuleAdded {
    /// The rule.
    pub rule: Id,
    /// The Bot it constrains.
    pub bot: Id,
    /// The User who made it.
    pub user: Id,
    /// What matching actions do.
    pub effect: RuleEffect,
    /// The tool name.
    pub tool: String,
    /// The shell command prefix a matching shell action starts with.
    #[serde(default)]
    pub command_prefix: Option<String>,
    /// The path prefix a matching file action's path starts with.
    #[serde(default)]
    pub path_prefix: Option<String>,
    /// The MCP server whose tool a matching call names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<Id>,
    /// The skill whose script a matching `run_skill_script` action runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<Id>,
    /// The script a matching `run_skill_script` action runs, within that skill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
}

/// The data of `neha.rule.removed`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct RuleRemoved {
    /// The rule.
    pub rule: Id,
    /// The Bot it constrained.
    pub bot: Id,
    /// The User who removed it; the Bot's owner may remove another User's
    /// rule, so this is not necessarily the rule's maker.
    pub user: Id,
}

/// The largest body a skill's `SKILL.md` carries, without its frontmatter.
pub const MAX_SKILL_BODY_BYTES: usize = 64 * 1024;

/// The libraries that hold skills.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(rename_all = "snake_case")]
pub enum SkillLibrary {
    /// The Space's library, which every Bot's owner may enable for their Bot.
    Space,
    /// One Bot's own library.
    Bot,
}

/// A script a skill carries, stored in the File store by digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct SkillScript {
    /// The script's file name within the skill, one path component.
    pub name: String,
    /// The digest of its bytes in the File store.
    pub sha256: String,
}

/// The data of `neha.skill.added`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct SkillAdded {
    /// The skill.
    pub skill: Id,
    /// The library that holds it.
    pub library: SkillLibrary,
    /// The Bot whose library holds it, when it is a Bot's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<Id>,
    /// Its name, as the Agent Skills frontmatter spells it.
    pub name: String,
    /// What it does and when to use it.
    pub description: String,
    /// The body of its `SKILL.md`, without the frontmatter.
    pub body: String,
    /// The scripts it carries.
    #[serde(default)]
    pub scripts: Vec<SkillScript>,
    /// The package that brought it, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<Id>,
}

impl SkillAdded {
    /// Whether the name is one the Agent Skills frontmatter allows.
    #[must_use]
    pub fn valid_name(name: &str) -> bool {
        let bytes = name.as_bytes();
        (1..=64).contains(&bytes.len())
            && bytes[0] != b'-'
            && bytes[bytes.len() - 1] != b'-'
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    }

    /// Checks the data against what a skill may hold.
    pub fn validate(&self) -> Result<(), EventDataError> {
        match self.library {
            SkillLibrary::Space if self.bot.is_some() => {
                return Err(EventDataError::Data(
                    "a skill in the Space's library belongs to no Bot".to_owned(),
                ));
            }
            SkillLibrary::Bot if self.bot.is_none() => {
                return Err(EventDataError::Data(
                    "a skill in a Bot's library names its Bot".to_owned(),
                ));
            }
            _ => {}
        }
        if !Self::valid_name(&self.name) {
            return Err(EventDataError::Data(
                "a skill name is 1 to 64 lowercase letters, digits, or hyphens, and does not \
                 begin or end with a hyphen"
                    .to_owned(),
            ));
        }
        if self.description.trim().is_empty() || self.description.chars().count() > 1024 {
            return Err(EventDataError::Data(
                "a skill description has 1 to 1024 characters".to_owned(),
            ));
        }
        if self.body.len() > MAX_SKILL_BODY_BYTES {
            return Err(EventDataError::Data(format!(
                "a skill body is at most {MAX_SKILL_BODY_BYTES} bytes"
            )));
        }
        let mut seen = std::collections::HashSet::new();
        for script in &self.scripts {
            if !valid_script_name(&script.name) {
                return Err(EventDataError::Data(
                    "a script name is one path component of letters, digits, dots, or hyphens"
                        .to_owned(),
                ));
            }
            if !is_sha256(&script.sha256) {
                return Err(EventDataError::Data(
                    "a script is named by the digest of its bytes".to_owned(),
                ));
            }
            if !seen.insert(&script.name) {
                return Err(EventDataError::Data(
                    "a skill carries one script under each name".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

/// Whether a script's file name is one a Computer writes safely.
#[must_use]
pub fn valid_script_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains(['/', '\\'])
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_')
}

/// The data of `neha.skill.removed`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct SkillRemoved {
    /// The skill.
    pub skill: Id,
    /// The library the skill left, when the writer knew it.
    ///
    /// The payload names no person, so the event stream uses this to decide
    /// whether every member may see the removal: only a Space skill's
    /// removal is every member's. A record written before this field has
    /// none and stays its author's.
    #[serde(default)]
    pub library: Option<SkillLibrary>,
}

/// Where an MCP server's credential and tools belong.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(rename_all = "snake_case")]
pub enum McpScope {
    /// The Admin connected it for the whole Space.
    Space,
    /// A User connected it with their own credential.
    User,
}

/// The longest name an MCP server may take.
///
/// A tool's model-facing name is `mcp__<server>__<tool>`, and the Model's tool
/// names are at most 64 characters.
pub const MAX_MCP_NAME_BYTES: usize = 32;

/// The most bytes a stdio server's `command` token may take.
pub const MAX_MCP_COMMAND_BYTES: usize = 1024;
/// The most arguments a stdio server may name.
pub const MAX_MCP_ARGS: usize = 128;
/// The most bytes one argument of a stdio server may take.
pub const MAX_MCP_ARG_BYTES: usize = 4096;
/// The most bytes all arguments of a stdio server together may take.
pub const MAX_MCP_ARGS_BYTES: usize = 16 * 1024;
/// The most environment variables a stdio server may configure.
pub const MAX_MCP_ENV: usize = 64;
/// The most bytes one environment variable's name may take.
pub const MAX_MCP_ENV_NAME_BYTES: usize = 128;
/// The most bytes one environment variable's value may take.
pub const MAX_MCP_ENV_VALUE_BYTES: usize = 4096;
/// The most bytes all environment variables of a stdio server together may take.
pub const MAX_MCP_ENV_BYTES: usize = 64 * 1024;
/// The most bytes a stdio server's working directory may take.
pub const MAX_MCP_CWD_BYTES: usize = 4096;

/// How a Space reaches an MCP server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(rename_all = "kebab-case")]
pub enum McpTransport {
    /// MCP Streamable HTTP: a server that runs elsewhere and is reached over
    /// the network.
    StreamableHttp,
    /// MCP over stdio: a subprocess the Space launches on its own machine.
    Stdio,
}

/// A stdio MCP server's launch configuration, as the record holds it.
///
/// The record keeps the package's symbolic form: `${PLUGIN_ROOT}` and
/// `${PLUGIN_DATA}` in `args`, `env`, and `cwd` stand for the plugin root and
/// its data directory when the Space launches the process. `command` and `cwd`
/// are relative to a root the package chose, so they stay meaningful when the
/// home moves; a `cwd` keeps every `..` its expression needs, because only an
/// install or a launch knows the real roots and can decide what a `..` removes.
/// Nothing is launched from this type alone: the install resolves the directory
/// against the roots it publishes, and a launch resolves the two roots again and
/// checks the resolved directory against the root it names.
#[derive(Clone, Debug, PartialEq, Eq, TS)]
#[ts(export_to = "record.ts")]
pub struct StdioServer {
    /// The executable: a bare name the platform resolves, or a path beginning
    /// with `./` under the plugin root.
    pub command: String,
    /// The arguments, as the package wrote them.
    pub args: Vec<String>,
    /// The environment variables the package configures, as it wrote them.
    pub env: std::collections::BTreeMap<String, String>,
    /// The symbolic working directory under its root, `/`-separated; it may
    /// hold the two recognized placeholders and `..` components, and `None`
    /// names the root itself.
    pub cwd: Option<String>,
    /// Whether the working directory hangs from the plugin data directory
    /// rather than the plugin root.
    pub data_cwd: bool,
}

impl StdioServer {
    /// Checks the launch configuration against what the record holds, and
    /// answers the reason it cannot hold it.
    pub fn validate(&self) -> Result<(), String> {
        if normalize_stdio_command(&self.command).as_deref() != Some(self.command.as_str()) {
            return Err(format!(
                "a stdio server's command is a bare executable name, or a `./` path that stays \
                 in the plugin root, at most {MAX_MCP_COMMAND_BYTES} bytes"
            ));
        }
        if self.args.len() > MAX_MCP_ARGS {
            return Err(format!(
                "a stdio server takes at most {MAX_MCP_ARGS} arguments"
            ));
        }
        let mut args_bytes = 0;
        for argument in &self.args {
            if argument.contains('\0') || argument.len() > MAX_MCP_ARG_BYTES {
                return Err(format!(
                    "a stdio server's argument is text without a NUL, at most \
                     {MAX_MCP_ARG_BYTES} bytes"
                ));
            }
            args_bytes += argument.len();
        }
        if args_bytes > MAX_MCP_ARGS_BYTES {
            return Err(format!(
                "a stdio server's arguments take at most {MAX_MCP_ARGS_BYTES} bytes"
            ));
        }
        if self.env.len() > MAX_MCP_ENV {
            return Err(format!(
                "a stdio server configures at most {MAX_MCP_ENV} environment variables"
            ));
        }
        let mut env_bytes = 0;
        for (name, value) in &self.env {
            if !valid_mcp_env_name(name) || name.len() > MAX_MCP_ENV_NAME_BYTES {
                return Err(format!(
                    "a stdio server's environment variable name is \
                     `[A-Za-z_][A-Za-z0-9_]*`, at most {MAX_MCP_ENV_NAME_BYTES} bytes"
                ));
            }
            if value.contains('\0') || value.len() > MAX_MCP_ENV_VALUE_BYTES {
                return Err(format!(
                    "a stdio server's environment variable value is text without a NUL, at most \
                     {MAX_MCP_ENV_VALUE_BYTES} bytes"
                ));
            }
            env_bytes += name.len() + value.len();
        }
        if env_bytes > MAX_MCP_ENV_BYTES {
            return Err(format!(
                "a stdio server's environment takes at most {MAX_MCP_ENV_BYTES} bytes"
            ));
        }
        if let Some(cwd) = &self.cwd
            && !symbolic_working_directory_stays_inside(cwd)
        {
            return Err(format!(
                "a stdio server's working directory is a `/`-separated relative path that \
                 stays in the root its `data_cwd` bit names, at most {MAX_MCP_CWD_BYTES} bytes"
            ));
        }

        Ok(())
    }
}

/// The placeholders a launch expands in a symbolic working directory.
const SYMBOLIC_ROOT_PLACEHOLDERS: [&str; 2] = ["${PLUGIN_ROOT}", "${PLUGIN_DATA}"];

/// Whether a symbolic working directory is text the record can hold.
///
/// The record and the package loader keep the package's expression, not an
/// expanded result: it may hold the two recognized `${...}` placeholders and
/// `..` components, and only an expansion against the real plugin root and
/// plugin data directory says where it lands. The install expands it with the
/// roots it publishes and skips a directory that leaves the root its `data_cwd`
/// bit names; the launch expands it again with the roots it finds and refuses
/// the same escape. So this check is textual and conservative, and it is not the
/// containment decision. The path is `/`-separated and not absolute, its
/// components are plain text — no backslash, control character, or drive/stream
/// separator, because none is a path component on every platform the record may
/// reach — and a `..` that appears before any placeholder may not remove the
/// root itself: it names the root's parent wherever the root is. A `..` after a
/// placeholder may remove a component of the root the placeholder stands for,
/// and the text does not carry how many, so it stays for the install and the
/// launch to judge.
#[must_use]
pub fn symbolic_working_directory_stays_inside(cwd: &str) -> bool {
    if cwd.is_empty() || cwd.len() > MAX_MCP_CWD_BYTES || cwd.starts_with('/') {
        return false;
    }
    if cwd.chars().any(char::is_control) || cwd.contains('\\') {
        return false;
    }
    let mut depth = 0_usize;
    // Once a component holds a placeholder the record cannot know how many
    // components its expansion contributes, so a later `..` is left for the
    // install and the launch; the text checks still run over every component.
    let mut undecidable = false;
    for component in cwd.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if undecidable {
                    continue;
                }
                let Some(remaining) = depth.checked_sub(1) else {
                    return false;
                };
                depth = remaining;
            }
            component => {
                // A drive or stream separator is not one path component on
                // every platform the record may reach: check it on the whole
                // expression, including the part after a placeholder.
                if component.contains(':') {
                    return false;
                }
                if SYMBOLIC_ROOT_PLACEHOLDERS
                    .iter()
                    .any(|placeholder| component.contains(placeholder))
                {
                    undecidable = true;
                }
                depth += 1;
            }
        }
    }

    true
}

/// Normalizes a `/`-separated path relative to one root.
///
/// Answers the path's components with `.` components dropped and `..`
/// components applied, or `None` when the path is not plain text or a `..`
/// would leave the root. The empty answer names the root itself. A stored
/// command is one this function answers unchanged; a working directory keeps
/// its symbolic form, so it may hold placeholders and `..` components and is
/// checked by [`StdioServer::validate`] instead.
#[must_use]
pub fn normalize_relative(path: &str) -> Option<String> {
    if path.chars().any(char::is_control) || path.contains('\\') {
        return None;
    }
    let mut components: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop()?;
            }
            // A drive or stream separator is not one path component: joining
            // it would name another root on some platform.
            component if component.contains(':') => return None,
            component => components.push(component),
        }
    }

    Some(components.join("/"))
}

/// Whether an environment variable may take this name: `[A-Za-z_][A-Za-z0-9_]*`.
#[must_use]
pub fn valid_mcp_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    match bytes.next() {
        Some(first) if first.is_ascii_alphabetic() || first == b'_' => {}
        _ => return false,
    }

    bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Normalizes a stdio server's command token: a bare executable name, or a
/// path beginning with `./` that stays in the plugin root.
///
/// Answers the token in the one form the record holds, or `None` when it names
/// no executable token: a shell command line, a path without `./`, an escape
/// from the plugin root, or text that is not plain.
///
/// The package loader reads a command into this form, and the record accepts
/// only the form this function answers unchanged.
#[must_use]
pub fn normalize_stdio_command(command: &str) -> Option<String> {
    if command.is_empty()
        || command.len() > MAX_MCP_COMMAND_BYTES
        || command.chars().any(char::is_control)
    {
        return None;
    }
    if let Some(relative) = command.strip_prefix("./") {
        let relative = normalize_relative(relative)?;

        return (!relative.is_empty()).then(|| format!("./{relative}"));
    }
    // A bare executable name is one token the platform's search resolves. Any
    // other character is shell syntax or another path.
    let bare = command
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+'));

    (bare && !command.starts_with('.')).then(|| command.to_owned())
}

/// The data of `neha.mcp.server.added`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct McpServerAdded {
    /// The server.
    pub server: Id,
    /// Its name, unique in the Space.
    pub name: String,
    /// The address it speaks Streamable HTTP at, when it is an HTTP server.
    ///
    /// A record written before stdio servers were supported always carries
    /// one; a stdio server carries none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The executable a stdio server launches, when it is a stdio server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// A stdio server's arguments, with `${PLUGIN_ROOT}` and `${PLUGIN_DATA}`
    /// left for the launch to expand.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// A stdio server's environment, with the placeholders left for the launch
    /// to expand.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// A stdio server's working directory under its root, `/`-separated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Whether a stdio server's working directory hangs from the plugin data
    /// directory rather than the plugin root.
    #[serde(default)]
    pub data_cwd: bool,
    /// Where its credential belongs.
    pub scope: McpScope,
    /// The User it belongs to, when its scope is one User's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<Id>,
    /// The account that connected it.
    pub by: Id,
    /// The package that brought it, when one did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<Id>,
    /// The fixed headers the package configures, which are visible data.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub headers: std::collections::BTreeMap<String, String>,
}

impl McpServerAdded {
    /// Whether a token is one this Space stores as an MCP credential.
    ///
    /// The alphabet is the one a bearer token takes (RFC 6750 `b64token`), so
    /// a token cannot hide from a text search through JSON string escaping.
    #[must_use]
    pub fn valid_credential(token: &str) -> bool {
        let bytes = token.as_bytes();
        if bytes.is_empty() || bytes.len() > 8192 {
            return false;
        }
        // RFC 6750: one or more token-alphabet characters, then zero or more
        // trailing `=` padding characters.
        let body: &[u8] = match bytes.iter().rposition(|byte| *byte != b'=') {
            Some(last) => &bytes[..=last],
            None => return false,
        };
        body.iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/')
        })
    }

    /// Whether the name follows the Plugin naming rule.
    #[must_use]
    pub fn valid_name(name: &str) -> bool {
        let bytes = name.as_bytes();
        (1..=MAX_MCP_NAME_BYTES).contains(&bytes.len())
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
    }

    /// Whether the address is one a Space calls.
    ///
    /// A server runs elsewhere and is reached over HTTPS, or over HTTP only
    /// when the caller is the Space's own loopback.
    #[must_use]
    pub fn valid_url(url: &str) -> bool {
        if url.len() > 2048
            || url
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
            || url.contains('\\')
            || url.contains('#')
        {
            return false;
        }
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            ("https", rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            ("http", rest)
        } else {
            return false;
        };
        // The authority ends where the path or the query begins.
        let authority = rest.split(['/', '?']).next().unwrap_or_default();
        // Userinfo would put a credential in the record.
        if authority.is_empty() || authority.contains('@') {
            return false;
        }
        let (host, port) = if let Some(bracketed) = authority.strip_prefix('[') {
            // A bracketed IPv6 host names its port after the closing bracket.
            let Some((host, rest)) = bracketed.split_once(']') else {
                return false;
            };
            let port = if rest.is_empty() {
                None
            } else if let Some(port) = rest.strip_prefix(':') {
                Some(port)
            } else {
                return false;
            };
            (format!("[{host}]"), port)
        } else if let Some((host, port)) = authority.rsplit_once(':') {
            (host.to_owned(), Some(port))
        } else {
            (authority.to_owned(), None)
        };
        if let Some(port) = port
            && (port.is_empty()
                || port.len() > 5
                || !port.bytes().all(|byte| byte.is_ascii_digit())
                || port.parse::<u16>().is_err()
                || port.parse::<u16>() == Ok(0))
        {
            return false;
        }
        let host = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned();
        let host = host.as_str();
        if host.is_empty() {
            return false;
        }
        let loopback = matches!(host, "localhost" | "127.0.0.1" | "::1");
        if scheme == "http" && !loopback {
            return false;
        }
        if host.contains(':') {
            return host.parse::<std::net::Ipv6Addr>().is_ok();
        }
        let labels: Vec<&str> = host.split('.').collect();
        labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
    }

    /// Returns how this Space reaches the server, or why its data names no one
    /// transport.
    ///
    /// A server is reached one way: a `url` over Streamable HTTP, or a
    /// `command` launched as a subprocess. The fields of the other transport
    /// stay empty.
    pub fn transport(&self) -> Result<McpTransport, EventDataError> {
        match (self.url.is_some(), self.command.is_some()) {
            (true, false) => {
                if !self.args.is_empty()
                    || !self.env.is_empty()
                    || self.cwd.is_some()
                    || self.data_cwd
                {
                    return Err(EventDataError::Data(
                        "an HTTP MCP server names one transport".to_owned(),
                    ));
                }
                Ok(McpTransport::StreamableHttp)
            }
            (false, true) => {
                if !self.headers.is_empty() {
                    return Err(EventDataError::Data(
                        "a stdio MCP server configures no HTTP headers".to_owned(),
                    ));
                }
                Ok(McpTransport::Stdio)
            }
            _ => Err(EventDataError::Data(
                "an MCP server names one transport: a `url`, or a `command`".to_owned(),
            )),
        }
    }

    /// The launch configuration, when this server is a stdio server.
    #[must_use]
    pub fn stdio(&self) -> Option<StdioServer> {
        self.command.as_ref().map(|command| StdioServer {
            command: command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            cwd: self.cwd.clone(),
            data_cwd: self.data_cwd,
        })
    }

    /// Checks the data against what a server may hold.
    pub fn validate(&self) -> Result<(), EventDataError> {
        if !Self::valid_name(&self.name) {
            return Err(EventDataError::Data(format!(
                "an MCP server name is 1 to {MAX_MCP_NAME_BYTES} letters, digits, or hyphens"
            )));
        }
        match self.transport()? {
            McpTransport::StreamableHttp => {
                if !self.url.as_deref().is_some_and(Self::valid_url) {
                    return Err(EventDataError::Data(
                        "an MCP server's address is https, or http on the Space's own loopback"
                            .to_owned(),
                    ));
                }
            }
            McpTransport::Stdio => match self.stdio() {
                Some(stdio) => stdio.validate().map_err(EventDataError::Data)?,
                None => {
                    return Err(EventDataError::Data(
                        "a stdio MCP server names its command".to_owned(),
                    ));
                }
            },
        }
        match self.scope {
            McpScope::Space if self.user.is_some() => {
                return Err(EventDataError::Data(
                    "a server the Space connected belongs to no User".to_owned(),
                ));
            }
            McpScope::User if self.user.is_none() => {
                return Err(EventDataError::Data(
                    "a server a User connected names its User".to_owned(),
                ));
            }
            _ => {}
        }

        Ok(())
    }
}

/// The data of `neha.mcp.server.updated`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct McpServerUpdated {
    /// The server.
    pub server: Id,
    /// Its new address, when the address changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Whether the update replaced its credential.
    pub credential: bool,
    /// The account that changed it.
    pub by: Id,
    /// Where the credential the update names belongs, when the writer knew it.
    ///
    /// The payload names no person, so the event stream uses this to decide
    /// whether every member may see the change: only a Space credential's
    /// change is every member's. A personal credential attached to a Space
    /// server records [`McpScope::User`], so the stream never broadcasts who
    /// attached it. The projection ignores this field: it describes the
    /// credential change, not the server's own scope. A record written before
    /// this field has none and stays its author's.
    #[serde(default)]
    pub scope: Option<McpScope>,
}

/// The data of `neha.mcp.server.removed`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct McpServerRemoved {
    /// The server.
    pub server: Id,
    /// Where the server's credential belonged, when the writer knew it.
    ///
    /// The payload names no person, so the event stream uses this to decide
    /// whether every member may see the removal: only a Space-wide server's
    /// removal is every member's. A record written before this field has none
    /// and stays its author's.
    #[serde(default)]
    pub scope: Option<McpScope>,
}

/// The data of `neha.bot.mcp.enabled`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct BotMcpEnabled {
    /// The Bot.
    pub bot: Id,
    /// The server whose tools it may call.
    pub server: Id,
}

/// The data of `neha.bot.mcp.disabled`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct BotMcpDisabled {
    /// The Bot.
    pub bot: Id,
    /// The server it may no longer call.
    pub server: Id,
}

/// The data of `neha.bot.skill.enabled`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct BotSkillEnabled {
    /// The Bot.
    pub bot: Id,
    /// The skill it may use.
    pub skill: Id,
}

/// The data of `neha.bot.skill.disabled`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct BotSkillDisabled {
    /// The Bot.
    pub bot: Id,
    /// The skill it no longer uses.
    pub skill: Id,
}

/// The most bytes a package's name, version, and source may carry.
pub const MAX_PLUGIN_SOURCE_BYTES: usize = 4096;

/// The data of `neha.plugin.installed`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct PluginInstalled {
    /// The package.
    pub plugin: Id,
    /// Its name from `plugin.json`.
    pub name: String,
    /// Its version from `plugin.json`, when it names one.
    #[serde(default)]
    pub version: Option<String>,
    /// The digest of the package's bytes.
    pub digest: String,
    /// The commit the package was fetched at, when it came from a repository.
    #[serde(default)]
    pub commit: Option<String>,
    /// The source it came from.
    pub source: String,
    /// The MCP servers it brought.
    #[serde(default)]
    pub servers: Vec<Id>,
    /// The skills it brought into the Space's library.
    #[serde(default)]
    pub skills: Vec<Id>,
}

impl PluginInstalled {
    /// Checks the fields a record reader relies on.
    pub fn validate(&self) -> Result<(), EventDataError> {
        let valid = |text: &str, limit: usize| {
            !text.is_empty() && text.len() <= limit && !text.chars().any(char::is_control)
        };

        if !valid(&self.name, 64) {
            return Err(EventDataError::Data("a package names a name".to_owned()));
        }
        if self.digest.len() != 64 || !self.digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(EventDataError::Data(
                "a package digest is a sha256 hex string".to_owned(),
            ));
        }
        if self.version.as_ref().is_some_and(|text| !valid(text, 128)) {
            return Err(EventDataError::Data(
                "a package version is short text".to_owned(),
            ));
        }
        if self.commit.as_ref().is_some_and(|text| !valid(text, 128)) {
            return Err(EventDataError::Data(
                "a package commit is short text".to_owned(),
            ));
        }
        if !valid(&self.source, MAX_PLUGIN_SOURCE_BYTES) {
            return Err(EventDataError::Data(
                "a package names its source".to_owned(),
            ));
        }

        Ok(())
    }
}

/// The data of `neha.plugin.removed`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct PluginRemoved {
    /// The package.
    pub plugin: Id,
}

/// The data of `neha.plugin.enabled`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct PluginEnabled {
    /// The package.
    pub plugin: Id,
}

/// The data of `neha.plugin.disabled`.
#[derive(Clone, Debug, PartialEq, Deserialize, TS)]
#[ts(export_to = "record.ts")]
#[serde(deny_unknown_fields)]
pub struct PluginDisabled {
    /// The package.
    pub plugin: Id,
}

//! The built-in tools and their schemas.
//!
//! A Bot's tools are its Computer tools — `shell`, `read_file`, `write_file`,
//! and `list_files` — the tools the Space serves itself — `skill` and
//! `run_skill_script` — and its record tools — `remember`, `forget`,
//! `list_memories`, and `plan`. A Computer tool becomes an Action the loop
//! asks the Space about; a Space tool is a call the Space answers under the
//! approvals the action names; a record tool is an operation the loop runs
//! directly against the record.

use neha::id::Id;
use neha::memory::{MemoryKind, MemoryScope};
use neha::record::{MAX_PLAN_ENTRIES, PlanEntry, PlanState};
use neha_computer::Action;
use neha_computer::local::{MAX_COMMAND_BYTES, MAX_WRITE_BYTES};
use serde::Deserialize;
use serde_json::json;

use crate::model::ToolSpec;

/// The largest path a file tool accepts.
const MAX_PATH_BYTES: usize = 4 * 1024;

/// The shell tool.
pub const SHELL: &str = "shell";
/// The read file tool.
pub const READ_FILE: &str = "read_file";
/// The write file tool.
pub const WRITE_FILE: &str = "write_file";
/// The list files tool.
pub const LIST_FILES: &str = "list_files";
/// The tool that loads a skill's body.
pub const SKILL: &str = "skill";
/// The action that runs a skill's script on the Computer.
pub const RUN_SKILL_SCRIPT: &str = "run_skill_script";
/// The memory tool that writes an entry.
pub const REMEMBER: &str = "remember";
/// The memory tool that removes entries.
pub const FORGET: &str = "forget";
/// The memory tool that reads the entries that apply.
pub const LIST_MEMORIES: &str = "list_memories";
/// The plan tool that replaces the run's plan.
pub const PLAN: &str = "plan";

/// Every built-in tool, by its wire name.
pub const NAMES: [&str; 10] = [
    SHELL,
    READ_FILE,
    WRITE_FILE,
    LIST_FILES,
    SKILL,
    RUN_SKILL_SCRIPT,
    REMEMBER,
    FORGET,
    LIST_MEMORIES,
    PLAN,
];

/// The memory tools, which run as record operations.
pub const MEMORY_NAMES: [&str; 3] = [REMEMBER, FORGET, LIST_MEMORIES];

/// Returns whether a tool name is one the loop declares itself.
///
/// A server's tool is named by the server, so it is never built in.
#[must_use]
pub fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name)
}

/// Returns whether a tool is a memory tool, which runs no Computer action.
#[must_use]
pub fn is_memory(name: &str) -> bool {
    MEMORY_NAMES.contains(&name)
}

/// Returns whether a tool is the plan tool, which runs no Computer action.
#[must_use]
pub fn is_plan(name: &str) -> bool {
    name == PLAN
}

/// Returns whether a tool is a Computer tool, which a lasting rule can name.
#[must_use]
pub fn is_computer(name: &str) -> bool {
    matches!(name, SHELL | READ_FILE | WRITE_FILE | LIST_FILES)
}

/// Returns the tools every Bot has.
#[must_use]
pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: SHELL.to_owned(),
            description: "Run a POSIX shell command in the Bot's Computer workspace. \
                          The output is the command's exit code, stdout, and stderr."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command line to run."}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: READ_FILE.to_owned(),
            description: "Read a file in the Bot's Computer workspace.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "The path inside the workspace."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: WRITE_FILE.to_owned(),
            description: "Write a file in the Bot's Computer workspace.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "The path inside the workspace."},
                    "content": {"type": "string", "description": "The full file content."}
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: LIST_FILES.to_owned(),
            description: "List a directory in the Bot's Computer workspace.".to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "The directory path inside the workspace."}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: SKILL.to_owned(),
            description: "Load the body of a skill the Bot has, by name. Call it when the \
                          task matches the skill's description, then follow the body."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The skill's name."}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: RUN_SKILL_SCRIPT.to_owned(),
            description: "Run a script a skill carries on the Bot's Computer, inside the \
                          Workspace, and return its output."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "skill": {"type": "string", "description": "The skill's name."},
                    "script": {"type": "string", "description": "The script's file name within the skill."},
                    "arguments": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "The arguments passed after the script."
                    }
                },
                "required": ["skill", "script"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: REMEMBER.to_owned(),
            description:
                "Remember one thing for a later run. A `rule` is a lasting instruction and \
                          only the User's own words may give it; a `note` is data the Bot noticed. \
                          This changes nothing on the Computer, grants no access, and needs no \
                          approval."
                    .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["rule", "note"],
                        "description": "A lasting instruction or the Bot's own note."
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["owner", "user", "group"],
                        "description": "Where it applies: the owner's Chats, this Chat, or the Group. Group memory is not available yet."
                    },
                    "text": {
                        "type": "string",
                        "description": "What to remember."
                    },
                    "replaces": {
                        "type": "array",
                        "items": {"type": "string", "format": "uuid"},
                        "description": "Entries this write replaces."
                    }
                },
                "required": ["kind", "scope", "text"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: FORGET.to_owned(),
            description: "Forget memory entries by id or by exact text. Only the Bot's own notes \
                          and the owner's rules can be forgotten. This changes nothing on the \
                          Computer and needs no approval."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "items": {"type": "string", "format": "uuid"},
                        "description": "The entries to forget."
                    },
                    "text": {
                        "type": "string",
                        "description": "The exact text of one entry to forget."
                    }
                },
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: LIST_MEMORIES.to_owned(),
            description: "List the memory entries that apply in this conversation, and the bytes \
                          each kind uses of its limit. This changes nothing."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: PLAN.to_owned(),
            description: "Set the plan for this request: the flat list of lines the Bot will \
                          work through, oldest first. Every call replaces the whole list, and \
                          an empty list clears it. Update an entry when it finishes; not every \
                          action needs one."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "entries": {
                        "type": "array",
                        "description": "The whole plan, oldest line first; an empty list clears it.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {
                                    "type": "string",
                                    "description": "One line of work."
                                },
                                "state": {
                                    "type": "string",
                                    "enum": ["pending", "doing", "done"],
                                    "description": "Not started, being worked on, or finished."
                                }
                            },
                            "required": ["title", "state"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["entries"],
                "additionalProperties": false
            }),
        },
    ]
}

/// What one tool call asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// An action for the Computer.
    Computer(Action),
    /// A tool the Space serves itself.
    Space(SpaceCall),
    /// A record operation on the Bot's memory.
    Memory(MemoryCall),
    /// A record operation on the run's plan.
    Plan(PlanCall),
}

/// What one memory tool call asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryCall {
    /// Write one entry, replacing the entries it names.
    Remember {
        /// Its kind.
        kind: MemoryKind,
        /// Where it applies.
        scope: MemoryScope,
        /// Its text.
        text: String,
        /// The entries it replaces.
        replaces: Vec<Id>,
    },
    /// Remove the entries an id or a text names.
    Forget {
        /// The entries to remove.
        ids: Vec<Id>,
        /// The exact text of one entry to remove.
        text: Option<String>,
    },
    /// Read the entries that apply in the conversation.
    List,
}

/// What one plan tool call asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanCall {
    /// The whole list; an empty list clears the plan.
    pub entries: Vec<PlanEntry>,
}

/// The text the Model reads for one plan write.
///
/// The plan's list is what the record keeps; the answer is derived from it the
/// same way every time, so a replay returns the result the Model read first.
#[must_use]
pub fn plan_output(entries: &[PlanEntry]) -> String {
    if entries.is_empty() {
        return "Plan cleared.".to_owned();
    }
    let done = entries
        .iter()
        .filter(|entry| entry.state == PlanState::Done)
        .count();

    format!("Plan updated: {done} of {} done.", entries.len())
}

/// A tool call the Space serves without its Computer running it as a plain
/// action: it reads what the Space holds, or runs a script it delivers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpaceCall {
    /// Loads the body of a skill the Bot has.
    Skill { name: String },
    /// Runs a skill's script on the Computer under the Bot's approvals.
    Script {
        skill: String,
        script: String,
        arguments: Vec<String>,
    },
    /// Calls a tool an MCP server gives the Bot, under its approvals.
    Mcp {
        /// The name the Model sees; the Space resolves the server and the
        /// tool's original name before it calls anything.
        name: String,
        /// The arguments the server receives.
        arguments: serde_json::Value,
    },
}

/// Parses the arguments of a `skill` call.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillArguments {
    name: String,
}

/// Parses the arguments of a `run_skill_script` call.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ScriptArguments {
    skill: String,
    script: String,
    #[serde(default)]
    arguments: Vec<String>,
}

/// The largest script call: a skill name, a script name, and 64 arguments of
/// 4 KiB each fit well under this.
const MAX_SCRIPT_CALL_BYTES: usize = 300 * 1024;

/// Turns one raw tool call into what it asks for.
pub fn parse(name: &str, arguments: &str) -> Result<Call, String> {
    match name {
        SKILL => {
            let arguments: SkillArguments =
                serde_json::from_str(arguments).map_err(|error| error.to_string())?;
            Ok(Call::Space(SpaceCall::Skill {
                name: arguments.name,
            }))
        }
        RUN_SKILL_SCRIPT => {
            if arguments.len() > MAX_SCRIPT_CALL_BYTES {
                return Err(format!(
                    "the script call is larger than {MAX_SCRIPT_CALL_BYTES} bytes"
                ));
            }
            let arguments: ScriptArguments =
                serde_json::from_str(arguments).map_err(|error| error.to_string())?;
            if arguments.arguments.len() > 64 {
                return Err("a script takes at most 64 arguments".to_owned());
            }
            Ok(Call::Space(SpaceCall::Script {
                skill: arguments.skill,
                script: arguments.script,
                arguments: arguments.arguments,
            }))
        }
        _ if name.starts_with("mcp__") => {
            let arguments: serde_json::Value =
                serde_json::from_str(arguments).map_err(|error| error.to_string())?;
            Ok(Call::Space(SpaceCall::Mcp {
                name: name.to_owned(),
                arguments,
            }))
        }
        REMEMBER => Ok(Call::Memory(remember(arguments)?)),
        FORGET => Ok(Call::Memory(forget(arguments)?)),
        LIST_MEMORIES => Ok(Call::Memory(list(arguments)?)),
        PLAN => Ok(Call::Plan(plan(arguments)?)),
        _ => action(name, arguments).map(Call::Computer),
    }
}

/// Turns one raw tool call into a Computer action.
pub fn action(name: &str, arguments: &str) -> Result<Action, String> {
    match name {
        SHELL => {
            let arguments: ShellArguments =
                serde_json::from_str(arguments).map_err(|error| error.to_string())?;
            if arguments.command.trim().is_empty() {
                return Err("the shell tool needs a command".to_owned());
            }
            if arguments.command.len() > MAX_COMMAND_BYTES {
                return Err(format!(
                    "the command is larger than {MAX_COMMAND_BYTES} bytes"
                ));
            }
            Ok(Action::Shell {
                command: arguments.command,
            })
        }
        READ_FILE => {
            let arguments: PathArguments =
                serde_json::from_str(arguments).map_err(|error| error.to_string())?;
            check_path(&arguments.path)?;
            Ok(Action::ReadFile {
                path: arguments.path,
            })
        }
        WRITE_FILE => {
            let arguments: WriteArguments =
                serde_json::from_str(arguments).map_err(|error| error.to_string())?;
            check_path(&arguments.path)?;
            if arguments.content.len() > MAX_WRITE_BYTES {
                return Err(format!(
                    "the content is larger than {MAX_WRITE_BYTES} bytes"
                ));
            }
            Ok(Action::WriteFile {
                path: arguments.path,
                content: arguments.content,
            })
        }
        LIST_FILES => {
            let arguments: PathArguments =
                serde_json::from_str(arguments).map_err(|error| error.to_string())?;
            check_path(&arguments.path)?;
            Ok(Action::ListFiles {
                path: arguments.path,
            })
        }
        _ => Err(format!("there is no tool named {name}")),
    }
}

/// Turns one raw `remember` call into what it asks for.
fn remember(arguments: &str) -> Result<MemoryCall, String> {
    let arguments: RememberArguments =
        serde_json::from_str(arguments).map_err(|error| error.to_string())?;
    if arguments.text.trim().is_empty() {
        return Err("the remember tool needs text".to_owned());
    }
    match arguments.scope {
        // The owner's Chats are the default, and a Chat entry is the other
        // scope this build keeps; a Group is not part of it yet.
        MemoryScope::Owner | MemoryScope::User => {}
        MemoryScope::Group => {
            return Err("group memory is not available yet".to_owned());
        }
    }
    Ok(MemoryCall::Remember {
        kind: arguments.kind,
        scope: arguments.scope,
        text: arguments.text,
        replaces: arguments.replaces,
    })
}

/// Turns one raw `forget` call into what it asks for.
fn forget(arguments: &str) -> Result<MemoryCall, String> {
    let arguments: ForgetArguments =
        serde_json::from_str(arguments).map_err(|error| error.to_string())?;
    let text = arguments.text.filter(|text| !text.trim().is_empty());
    if arguments.ids.is_empty() && text.is_none() {
        return Err("the forget tool needs ids or text".to_owned());
    }
    Ok(MemoryCall::Forget {
        ids: arguments.ids,
        text,
    })
}

/// Turns one raw `list_memories` call into what it asks for.
fn list(arguments: &str) -> Result<MemoryCall, String> {
    serde_json::from_str::<EmptyArguments>(arguments).map_err(|error| error.to_string())?;
    Ok(MemoryCall::List)
}

/// Turns one raw `plan` call into the whole list it writes.
fn plan(arguments: &str) -> Result<PlanCall, String> {
    let arguments: PlanArguments =
        serde_json::from_str(arguments).map_err(|error| error.to_string())?;
    if arguments.entries.len() > MAX_PLAN_ENTRIES {
        return Err(format!("a plan holds at most {MAX_PLAN_ENTRIES} entries"));
    }
    let mut entries = Vec::with_capacity(arguments.entries.len());
    for entry in arguments.entries {
        // The record and the tool judge the same text: the title as the Bot
        // wrote it, before the tool trims it, and an over-limit write changes
        // nothing.
        let written = PlanEntry {
            title: entry.title,
            state: entry.state,
        };
        written.validate().map_err(|error| error.to_string())?;
        let title = written.title.trim().to_owned();
        if title.is_empty() {
            return Err("a plan entry needs a title".to_owned());
        }
        entries.push(PlanEntry {
            title,
            state: written.state,
        });
    }

    Ok(PlanCall { entries })
}

fn check_path(path: &str) -> Result<(), String> {
    if path.len() > MAX_PATH_BYTES {
        return Err(format!("the path is larger than {MAX_PATH_BYTES} bytes"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellArguments {
    command: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArguments {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteArguments {
    path: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RememberArguments {
    kind: MemoryKind,
    scope: MemoryScope,
    text: String,
    #[serde(default)]
    replaces: Vec<Id>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForgetArguments {
    #[serde(default)]
    ids: Vec<Id>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanArguments {
    entries: Vec<PlanArgumentsEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlanArgumentsEntry {
    title: String,
    state: PlanState,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArguments {}

#[cfg(test)]
mod tests {
    use neha::id::Id;
    use neha::memory::{MemoryKind, MemoryScope};
    use neha::record::MAX_PLAN_TITLE_BYTES;
    use neha_computer::Action;

    use super::{
        Call, MAX_PLAN_ENTRIES, MemoryCall, NAMES, PlanCall, PlanEntry, PlanState,
        RUN_SKILL_SCRIPT, SKILL, SpaceCall, action, is_builtin, is_computer, is_memory, is_plan,
        parse, plan_output, specs,
    };

    #[test]
    fn every_builtin_name_is_a_declared_tool() {
        let declared: Vec<String> = specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(declared, NAMES, "the rule check reads the declared tools");

        for name in NAMES {
            assert!(is_builtin(name), "{name} is a built-in tool");
        }
        assert!(!is_builtin("reed_file"), "a misspelt tool is not one");
        assert!(
            !is_builtin("mcp__server__tool"),
            "a server's tool is not built in"
        );
        assert!(!is_builtin(""), "an empty name is not a tool");
    }

    #[test]
    fn only_computer_tools_take_a_lasting_rule() {
        for name in NAMES {
            let kinds = [
                is_computer(name),
                is_memory(name),
                is_plan(name),
                matches!(name, SKILL | RUN_SKILL_SCRIPT),
            ];
            assert_eq!(
                kinds.into_iter().filter(|kind| *kind).count(),
                1,
                "{name} is exactly one kind of tool"
            );
        }
        assert!(is_memory("remember"));
        assert!(is_memory("forget"));
        assert!(is_memory("list_memories"));
        assert!(is_plan("plan"));
        assert!(is_computer("shell"));
        assert!(!is_computer("remember"), "a rule never names a memory tool");
        assert!(!is_memory("skill"), "a skill is not a memory tool");
        assert!(!is_memory("plan"), "a plan is not a memory tool");
        assert!(!is_computer("plan"), "a rule never names the plan tool");
    }

    #[test]
    fn every_tool_has_a_strict_schema() {
        let specs = specs();

        assert_eq!(specs.len(), 10);
        for spec in &specs {
            assert_eq!(spec.parameters["additionalProperties"], false);
            assert!(spec.description.len() < 300, "{}", spec.name);
        }
    }

    #[test]
    fn the_skill_tools_become_space_calls() {
        assert_eq!(
            parse("skill", r#"{"name":"weekly-report"}"#).unwrap(),
            Call::Space(SpaceCall::Skill {
                name: "weekly-report".to_owned(),
            })
        );
        assert_eq!(
            parse(
                "run_skill_script",
                r#"{"skill":"weekly-report","script":"collect.sh","arguments":["q4"]}"#
            )
            .unwrap(),
            Call::Space(SpaceCall::Script {
                skill: "weekly-report".to_owned(),
                script: "collect.sh".to_owned(),
                arguments: vec!["q4".to_owned()],
            })
        );
        assert!(parse("skill", r#"{"name":1}"#).is_err());
        assert!(parse("run_skill_script", r#"{"skill":"x"}"#).is_err());
    }

    #[test]
    fn arguments_become_actions() {
        assert_eq!(
            action("shell", r#"{"command":"printf 42"}"#).unwrap(),
            Action::Shell {
                command: "printf 42".to_owned(),
            }
        );
        assert!(action("shell", r#"{"command":"x","surprise":true}"#).is_err());
        assert!(action("shell", "not json").is_err());
        assert!(action("unknown", "{}").is_err());
        let long = serde_json::json!({"path": "x".repeat(5000)});
        assert!(action("read_file", &long.to_string()).is_err());
    }

    #[test]
    fn a_remember_call_carries_its_entry() {
        let replaced = Id::new();
        let arguments = serde_json::json!({
            "kind": "rule",
            "scope": "owner",
            "text": "Keep the deploy notes current.",
            "replaces": [replaced],
        });
        assert_eq!(
            parse("remember", &arguments.to_string()).unwrap(),
            Call::Memory(MemoryCall::Remember {
                kind: MemoryKind::Rule,
                scope: MemoryScope::Owner,
                text: "Keep the deploy notes current.".to_owned(),
                replaces: vec![replaced],
            })
        );

        // A write without text, an unknown field, or a Group scope is refused.
        assert!(parse("remember", r#"{"kind":"note","scope":"user","text":"  "}"#).is_err());
        assert!(
            parse(
                "remember",
                r#"{"kind":"note","scope":"user","text":"x","surprise":1}"#
            )
            .is_err()
        );
        assert!(
            parse("remember", r#"{"kind":"note","scope":"group","text":"x"}"#).is_err(),
            "Group memory is not part of this build"
        );
        assert!(parse("remember", r#"{"kind":"fact","scope":"owner","text":"x"}"#).is_err());
    }

    #[test]
    fn a_forget_call_names_ids_or_text() {
        let memory = Id::new();
        let by_id = serde_json::json!({"ids": [memory]});
        assert_eq!(
            parse("forget", &by_id.to_string()).unwrap(),
            Call::Memory(MemoryCall::Forget {
                ids: vec![memory],
                text: None,
            })
        );

        let by_text = serde_json::json!({"text": "stale note"});
        assert_eq!(
            parse("forget", &by_text.to_string()).unwrap(),
            Call::Memory(MemoryCall::Forget {
                ids: Vec::new(),
                text: Some("stale note".to_owned()),
            })
        );

        assert!(parse("forget", "{}").is_err(), "a forget names its entries");
        assert!(parse("forget", r#"{"text":"  "}"#).is_err());
        assert!(parse("forget", r#"{"ids":[],"other":1}"#).is_err());
    }

    #[test]
    fn a_list_call_takes_nothing() {
        assert_eq!(
            parse("list_memories", "{}").unwrap(),
            Call::Memory(MemoryCall::List)
        );
        assert!(parse("list_memories", r#"{"all":true}"#).is_err());
    }

    #[test]
    fn a_plan_call_carries_the_whole_list() {
        let arguments = serde_json::json!({
            "entries": [
                {"title": "Read the receipts", "state": "done"},
                {"title": "  Buy the pass  ", "state": "doing"},
                {"title": "Email finance", "state": "pending"},
            ]
        });
        assert_eq!(
            parse("plan", &arguments.to_string()).unwrap(),
            Call::Plan(PlanCall {
                entries: vec![
                    PlanEntry {
                        title: "Read the receipts".to_owned(),
                        state: PlanState::Done,
                    },
                    PlanEntry {
                        title: "Buy the pass".to_owned(),
                        state: PlanState::Doing,
                    },
                    PlanEntry {
                        title: "Email finance".to_owned(),
                        state: PlanState::Pending,
                    },
                ],
            })
        );

        // An empty list clears the plan.
        assert_eq!(
            parse("plan", r#"{"entries":[]}"#).unwrap(),
            Call::Plan(PlanCall {
                entries: Vec::new()
            })
        );

        // A bad state, a blank title, an unknown field, or no list is refused.
        assert!(parse("plan", r#"{"entries":[{"title":"x","state":"blocked"}]}"#).is_err());
        assert!(parse("plan", r#"{"entries":[{"title":"  ","state":"done"}]}"#).is_err());
        assert!(
            parse(
                "plan",
                r#"{"entries":[{"title":"x","state":"done","id":"y"}]}"#
            )
            .is_err()
        );
        assert!(parse("plan", r#"{}"#).is_err());
        assert!(parse("plan", r#"{"entries":[{"title":"x"}]}"#).is_err());
    }

    #[test]
    fn a_plan_call_keeps_its_bounds() {
        let long_title = "x".repeat(MAX_PLAN_TITLE_BYTES + 1);
        let over = serde_json::json!({
            "entries": [{"title": long_title, "state": "pending"}]
        });
        assert!(parse("plan", &over.to_string()).is_err());

        let too_many = serde_json::json!({
            "entries": (0..MAX_PLAN_ENTRIES + 1)
                .map(|index| serde_json::json!({"title": format!("line {index}"), "state": "pending"}))
                .collect::<Vec<_>>()
        });
        assert!(parse("plan", &too_many.to_string()).is_err());

        let at_the_bounds = serde_json::json!({
            "entries": (0..MAX_PLAN_ENTRIES)
                .map(|_| serde_json::json!({
                    "title": "x".repeat(MAX_PLAN_TITLE_BYTES),
                    "state": "pending"
                }))
                .collect::<Vec<_>>()
        });
        let call = parse("plan", &at_the_bounds.to_string()).unwrap();
        let Call::Plan(call) = call else {
            panic!("a plan call is a plan");
        };
        assert_eq!(call.entries.len(), MAX_PLAN_ENTRIES);
        assert_eq!(call.entries[0].title.len(), MAX_PLAN_TITLE_BYTES);
    }

    #[test]
    fn a_plan_title_is_judged_as_written() {
        // Trimming must not let a title larger than the bound through: the
        // record and the tool measure the text the Bot wrote.
        let limit = neha::record::MAX_PLAN_TITLE_BYTES;
        let over = format!("{} ", "x".repeat(limit));
        let arguments = serde_json::json!({
            "entries": [{"title": over, "state": "pending"}]
        });
        assert!(parse("plan", &arguments.to_string()).is_err());

        let at_the_bound = serde_json::json!({
            "entries": [{"title": "x".repeat(limit), "state": "pending"}]
        });
        assert!(parse("plan", &at_the_bound.to_string()).is_ok());
    }

    #[test]
    fn a_plan_answer_counts_the_done_lines() {
        let entries = vec![
            PlanEntry {
                title: "a".to_owned(),
                state: PlanState::Done,
            },
            PlanEntry {
                title: "b".to_owned(),
                state: PlanState::Doing,
            },
            PlanEntry {
                title: "c".to_owned(),
                state: PlanState::Pending,
            },
        ];
        assert_eq!(plan_output(&entries), "Plan updated: 1 of 3 done.");
        assert_eq!(plan_output(&[]), "Plan cleared.");
    }
}

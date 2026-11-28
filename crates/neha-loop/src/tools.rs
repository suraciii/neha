//! The built-in tools and their schemas.

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

/// Every built-in tool, by its wire name.
pub const NAMES: [&str; 6] = [
    SHELL,
    READ_FILE,
    WRITE_FILE,
    LIST_FILES,
    SKILL,
    RUN_SKILL_SCRIPT,
];

/// Returns whether a tool name is one the loop declares itself.
///
/// A server's tool is named by the server, so it is never built in.
#[must_use]
pub fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name)
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
    ]
}

/// What one tool call asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// An action for the Computer.
    Computer(Action),
    /// A tool the Space serves itself.
    Space(SpaceCall),
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

#[cfg(test)]
mod tests {
    use neha_computer::Action;

    use super::{Call, NAMES, SpaceCall, action, is_builtin, parse, specs};

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
    fn every_tool_has_a_strict_schema() {
        let specs = specs();

        assert_eq!(specs.len(), 6);
        for spec in &specs {
            assert_eq!(spec.parameters["additionalProperties"], false);
            assert!(spec.description.len() < 300);
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
}

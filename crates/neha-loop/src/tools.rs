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

/// Every built-in tool, by its wire name.
pub const NAMES: [&str; 4] = [SHELL, READ_FILE, WRITE_FILE, LIST_FILES];

/// Returns whether a tool name is one every Bot has.
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
    ]
}

/// What one tool call asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Call {
    /// An action for the Computer.
    Computer(Action),
}

/// Turns one raw tool call into what it asks for.
pub fn parse(name: &str, arguments: &str) -> Result<Call, String> {
    action(name, arguments).map(Call::Computer)
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

    use super::{NAMES, action, is_builtin, specs};

    #[test]
    fn every_builtin_name_is_a_declared_tool() {
        let declared: Vec<String> = specs().into_iter().map(|spec| spec.name).collect();
        assert_eq!(declared, NAMES, "the rule check reads the declared tools");

        for name in NAMES {
            assert!(is_builtin(name), "{name} is a built-in tool");
        }
        assert!(!is_builtin("reed_file"), "a misspelt tool is not one");
        assert!(!is_builtin(""), "an empty name is not a tool");
    }

    #[test]
    fn every_tool_has_a_strict_schema() {
        let specs = specs();

        assert_eq!(specs.len(), 4);
        for spec in &specs {
            assert_eq!(spec.parameters["additionalProperties"], false);
            assert!(spec.description.len() < 300);
        }
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

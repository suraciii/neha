//! The built-in tools and their schemas.

use neha::id::Id;
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
/// The take file tool.
pub const TAKE_FILE: &str = "take_file";
/// The put file tool.
pub const PUT_FILE: &str = "put_file";

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
            name: TAKE_FILE.to_owned(),
            description: "Put a File from the Chat into the Bot's Computer workspace under its name. \
                          A Message that carries a File includes a `[File id=<uuid> ...]` line; pass that id."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": {"type": "string", "description": "The id of the File's `[File id=...]` line."}
                },
                "required": ["file"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: PUT_FILE.to_owned(),
            description: "Deliver a file from the Bot's Computer workspace to the Chat as a File."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "The path of the file inside the workspace."},
                    "name": {"type": "string", "description": "The name the File takes in the Chat. Defaults to the file's name."}
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
    /// A File to take into the Workspace.
    TakeFile {
        /// The File's identifier.
        file: Id,
    },
    /// A Workspace file to deliver as a File.
    PutFile {
        /// The path inside the workspace.
        path: String,
        /// The name the File takes in the Chat; the path's file name when absent.
        name: Option<String>,
    },
}

/// Turns one raw tool call into what it asks for.
///
/// The two File tools move bytes between the Space and the Computer, so the
/// loop runs them; every other tool becomes a Computer action.
pub fn parse(name: &str, arguments: &str) -> Result<Call, String> {
    match name {
        TAKE_FILE => {
            let arguments: TakeArguments =
                serde_json::from_str(arguments).map_err(|error| error.to_string())?;
            Ok(Call::TakeFile {
                file: arguments.file,
            })
        }
        PUT_FILE => {
            let arguments: PutArguments =
                serde_json::from_str(arguments).map_err(|error| error.to_string())?;
            check_path(&arguments.path)?;
            Ok(Call::PutFile {
                path: arguments.path,
                name: arguments.name,
            })
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TakeArguments {
    file: Id,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutArguments {
    path: String,
    #[serde(default)]
    name: Option<String>,
}

#[cfg(test)]
mod tests {
    use neha::id::Id;
    use neha_computer::Action;
    use serde_json::json;

    use super::{Call, PUT_FILE, TAKE_FILE, action, parse, specs};

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
    fn file_arguments_become_calls() {
        let file = Id::new();
        assert_eq!(
            parse(TAKE_FILE, &json!({"file": file}).to_string()).unwrap(),
            Call::TakeFile { file }
        );
        assert_eq!(
            parse(PUT_FILE, r#"{"path":"out/report.txt"}"#).unwrap(),
            Call::PutFile {
                path: "out/report.txt".to_owned(),
                name: None,
            }
        );
        assert_eq!(
            parse(PUT_FILE, r#"{"path":"out/report.txt","name":"answer.txt"}"#).unwrap(),
            Call::PutFile {
                path: "out/report.txt".to_owned(),
                name: Some("answer.txt".to_owned()),
            }
        );
    }

    #[test]
    fn file_arguments_are_bounded_and_complete() {
        let file = Id::new();
        for arguments in [
            "{}".to_owned(),
            json!({"file": "not-an-id"}).to_string(),
            json!({"file": file, "surprise": true}).to_string(),
        ] {
            assert!(parse(TAKE_FILE, &arguments).is_err(), "{arguments}");
        }
        for arguments in [
            "{}".to_owned(),
            json!({"path": "x".repeat(5000)}).to_string(),
            json!({"path": "a.txt", "extra": 1}).to_string(),
        ] {
            assert!(parse(PUT_FILE, &arguments).is_err(), "{arguments}");
        }
        assert_eq!(
            parse("write_file", r#"{"path":"a.txt","content":"a"}"#).unwrap(),
            Call::Computer(Action::WriteFile {
                path: "a.txt".to_owned(),
                content: "a".to_owned(),
            })
        );
        assert!(parse("no_such_tool", "{}").is_err());
    }
}

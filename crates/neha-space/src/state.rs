//! The in-process projection of the record.
//!
//! A Space rebuilds this state by replaying its record. Commands append
//! events, then apply the same events to keep the projection current.

use std::collections::HashMap;

use neha::id::Id;
use neha::record::{ActionStatus, Author, Event, EventData};
use neha::seq::Seq;
use neha::time::Timestamp;
use serde::{Deserialize, Serialize};

pub use neha::record::RunStatus;
/// A File, as the projection holds it and a client reads it.
pub use neha::surface::FileSummary as FileView;

use crate::model::Usage;
use crate::store::Projection;

/// A person in the Space.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UserView {
    /// The account identifier.
    pub id: Id,
    /// The display name.
    pub display_name: String,
    /// Whether the account is the Admin.
    pub is_admin: bool,
}

/// A Provider connected to the Space.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProviderView {
    /// The Provider identifier.
    pub id: Id,
    /// The protocol it speaks.
    pub protocol: String,
    /// Its base address.
    pub base_url: String,
}

/// A Model the Space offers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelView {
    /// The Model identifier.
    pub id: Id,
    /// The Provider that offers it.
    pub provider: Id,
    /// The name the Provider expects.
    pub name: String,
    /// The context window in tokens.
    pub context_limit: u32,
    /// The largest output in tokens.
    pub max_output: u32,
    /// Whether it reads images.
    pub reads_images: bool,
    /// Whether it is the Space's default.
    pub is_default: bool,
}

/// A Bot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BotView {
    /// The Bot identifier.
    pub id: Id,
    /// The User who owns it.
    pub owner: Id,
    /// Its name.
    pub name: String,
    /// Its Computer.
    pub computer: Id,
    /// Its Chat with the owner.
    pub chat: Id,
}

/// A Computer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputerView {
    /// The Computer identifier.
    pub id: Id,
    /// The User who owns it.
    pub owner: Id,
    /// `managed` or `real`.
    pub kind: String,
    /// Whether its channel is up.
    pub online: bool,
    /// The Computer's workspace directory.
    pub workspace: String,
}

/// A Chat.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChatView {
    /// The Chat identifier.
    pub id: Id,
    /// The User who owns the Chat.
    pub owner: Id,
    /// The Bot the Chat is with.
    pub bot: Id,
}

/// A Message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MessageView {
    /// The Message identifier.
    pub id: Id,
    /// The Chat it belongs to.
    pub chat: Id,
    /// Who wrote it.
    pub author: Author,
    /// The User a Bot acts for.
    pub acting: Option<Id>,
    /// Its text.
    pub text: String,
    /// The File it carries.
    pub file: Option<Id>,
    /// When the Space recorded it.
    pub at: Timestamp,
    /// Its cursor.
    pub seq: Seq,
    /// The run it belongs to, when it is part of one.
    pub run: Option<Id>,
}

/// One completed step of a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepView {
    /// The step number, starting at one.
    pub index: u32,
    /// The usage the Model reported.
    pub usage: Option<Usage>,
    /// How many tool calls the step requested.
    pub tool_calls: u32,
}

/// One action of a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionView {
    /// The action identifier.
    pub id: Id,
    /// The step that requested it.
    pub step: u32,
    /// The tool's name.
    pub tool: String,
    /// The tool's arguments.
    pub arguments: serde_json::Value,
    /// How the action ended; unknown until it finishes.
    pub status: ActionStatus,
    /// The text the Model reads.
    pub output: String,
    /// Whether the output dropped bytes.
    pub truncated: bool,
    /// How many bytes the output dropped.
    pub omitted_bytes: u64,
    /// The failure, when it failed.
    pub error: Option<String>,
}

/// A run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunView {
    /// The run identifier.
    pub id: Id,
    /// The Chat it works in.
    pub chat: Id,
    /// The Bot that works it.
    pub bot: Id,
    /// The User the Bot acts for.
    pub acting: Id,
    /// The command that started it.
    pub command: Id,
    /// The Message that started it.
    pub message: Id,
    /// Its status.
    pub status: RunStatus,
    /// The Bot's reply Message, when it succeeded.
    pub reply: Option<Id>,
    /// The usage the Model reported.
    pub usage: Option<Usage>,
    /// The failure, when it failed.
    pub error: Option<String>,
    /// The completed steps, in order.
    pub steps: Vec<StepView>,
    /// The actions, in order.
    pub actions: Vec<ActionView>,
}

/// The Space's record, projected into memory.
#[derive(Debug, Default)]
pub struct State {
    /// The Users, by identifier.
    pub users: HashMap<Id, UserView>,
    /// The Providers, by identifier.
    pub providers: HashMap<Id, ProviderView>,
    /// The Models, by identifier.
    pub models: HashMap<Id, ModelView>,
    /// The Bots, by identifier.
    pub bots: HashMap<Id, BotView>,
    /// The Computers, by identifier.
    pub computers: HashMap<Id, ComputerView>,
    /// The Chats, by identifier.
    pub chats: HashMap<Id, ChatView>,
    /// The Messages, in cursor order.
    pub messages: Vec<MessageView>,
    /// The Files, in cursor order.
    pub files: Vec<FileView>,
    /// The runs, by identifier.
    pub runs: HashMap<Id, RunView>,
}

impl State {
    /// Returns a Chat the User owns.
    #[must_use]
    pub fn owned_chat(&self, chat: Id, owner: Id) -> Option<&ChatView> {
        self.chats.get(&chat).filter(|chat| chat.owner == owner)
    }

    /// Returns a Bot the User owns.
    #[must_use]
    pub fn owned_bot(&self, bot: Id, owner: Id) -> Option<&BotView> {
        self.bots.get(&bot).filter(|bot| bot.owner == owner)
    }

    /// Returns a File in a Chat the User owns.
    #[must_use]
    pub fn owned_file(&self, file: Id, owner: Id) -> Option<&FileView> {
        self.files
            .iter()
            .find(|view| view.id == file && self.owned_chat(view.chat, owner).is_some())
    }

    /// Returns a File of a Chat.
    #[must_use]
    pub fn file_of(&self, chat: Id, file: Id) -> Option<&FileView> {
        self.files
            .iter()
            .find(|view| view.id == file && view.chat == chat)
    }

    /// Returns the Space's default Model.
    #[must_use]
    pub fn default_model(&self) -> Option<&ModelView> {
        self.models.values().find(|model| model.is_default)
    }

    /// Returns a Chat's Messages after a cursor.
    #[must_use]
    pub fn messages_of(&self, chat: Id, from: Option<Seq>) -> Vec<&MessageView> {
        let after = from.unwrap_or(Seq::ZERO);
        self.messages
            .iter()
            .filter(|message| message.chat == chat && message.seq > after)
            .collect()
    }

    /// Returns a Chat's Files after a cursor.
    #[must_use]
    pub fn files_of(&self, chat: Id, from: Option<Seq>) -> Vec<&FileView> {
        let after = from.unwrap_or(Seq::ZERO);
        self.files
            .iter()
            .filter(|file| file.chat == chat && file.seq > after)
            .collect()
    }

    /// Builds the Model history for a Chat, oldest first.
    ///
    /// A Message that carries a File ends with a line that names the File, so
    /// the Model can take it into its Workspace.
    #[must_use]
    pub fn history(&self, chat: Id) -> Vec<(Author, String)> {
        self.messages
            .iter()
            .filter(|message| message.chat == chat)
            .map(|message| (message.author, self.model_text(message)))
            .collect()
    }

    /// Returns what the Model reads for one Message.
    fn model_text(&self, message: &MessageView) -> String {
        let Some(file) = message
            .file
            .and_then(|file| self.files.iter().find(|view| view.id == file))
        else {
            return message.text.clone();
        };
        let line = format!(
            "[File id={} name={:?} type={} size={}]",
            file.id, file.name, file.media_type, file.size
        );
        if message.text.trim().is_empty() {
            line
        } else {
            format!("{}\n\n{line}", message.text)
        }
    }
}

impl Projection for State {
    fn apply(&mut self, event: &Event) -> Result<(), String> {
        let data = event
            .kind
            .decode(&event.data)
            .map_err(|error| error.to_string())?;
        match data {
            EventData::UserJoined(data) => {
                let Author::User(id) = event.author else {
                    return Err("a joined User must author the event".to_owned());
                };
                self.users.insert(
                    id,
                    UserView {
                        id,
                        display_name: data.display_name,
                        is_admin: data.is_admin,
                    },
                );
            }
            EventData::ProviderAdded(data) => {
                self.providers.insert(
                    data.provider,
                    ProviderView {
                        id: data.provider,
                        protocol: data.protocol,
                        base_url: data.base_url,
                    },
                );
                if data.default_model.is_some() {
                    for model in self.models.values_mut() {
                        model.is_default = false;
                    }
                }
                for model in data.models {
                    self.models.insert(
                        model.id,
                        ModelView {
                            id: model.id,
                            provider: data.provider,
                            name: model.name,
                            context_limit: model.context_limit,
                            max_output: model.max_output,
                            reads_images: model.reads_images,
                            is_default: data.default_model == Some(model.id),
                        },
                    );
                }
            }
            EventData::BotCreated(data) => {
                self.bots.insert(
                    data.bot,
                    BotView {
                        id: data.bot,
                        owner: data.owner,
                        name: data.name,
                        computer: data.computer,
                        chat: data.chat,
                    },
                );
            }
            EventData::ComputerCreated(data) => {
                self.computers.insert(
                    data.computer,
                    ComputerView {
                        id: data.computer,
                        owner: data.owner,
                        kind: data.kind,
                        online: true,
                        workspace: data.workspace,
                    },
                );
            }
            EventData::ChatCreated(data) => {
                self.chats.insert(
                    data.chat,
                    ChatView {
                        id: data.chat,
                        owner: data.owner,
                        bot: data.bot,
                    },
                );
            }
            EventData::FileAdded(data) => {
                if !self.chats.contains_key(&data.chat) {
                    return Err("a File must belong to an existing Chat".to_owned());
                }
                self.files.push(FileView {
                    id: data.file,
                    chat: data.chat,
                    name: data.name,
                    media_type: data.media_type,
                    size: data.size,
                    sha256: data.sha256,
                    author: event.author,
                    acting: event.acting,
                    at: event.at,
                    seq: event.seq,
                });
            }
            EventData::MessageAdded(data) => {
                if data
                    .file
                    .is_some_and(|file| self.file_of(data.chat, file).is_none())
                {
                    return Err("a Message's File must belong to its Chat".to_owned());
                }
                self.messages.push(MessageView {
                    id: data.message,
                    chat: data.chat,
                    author: event.author,
                    acting: event.acting,
                    text: data.text,
                    file: data.file,
                    at: event.at,
                    seq: event.seq,
                    run: data.run,
                });
            }
            EventData::RunStarted(data) => {
                let acting = event.acting.ok_or("a run names the User it acts for")?;
                self.runs.insert(
                    data.run,
                    RunView {
                        id: data.run,
                        chat: data.chat,
                        bot: data.bot,
                        acting,
                        command: data.command,
                        message: data.message,
                        status: RunStatus::Running,
                        reply: None,
                        usage: None,
                        error: None,
                        steps: Vec::new(),
                        actions: Vec::new(),
                    },
                );
            }
            EventData::RunFinished(data) => {
                let run = self
                    .runs
                    .get_mut(&data.run)
                    .ok_or("a finished run must have started")?;
                run.status = data.status;
                run.reply = data.reply;
                run.usage = data.usage;
                run.error = data.error;
            }
            EventData::RunStep(data) => {
                let run = self
                    .runs
                    .get_mut(&data.run)
                    .ok_or("a step must belong to a started run")?;
                run.steps.push(StepView {
                    index: data.step,
                    usage: data.usage,
                    tool_calls: data.tool_calls,
                });
            }
            EventData::ActionStarted(data) => {
                let run = self
                    .runs
                    .get_mut(&data.run)
                    .ok_or("an action must belong to a started run")?;
                run.actions.push(ActionView {
                    id: data.action,
                    step: data.step,
                    tool: data.tool,
                    arguments: data.arguments,
                    status: ActionStatus::Unknown,
                    output: String::new(),
                    truncated: false,
                    omitted_bytes: 0,
                    error: None,
                });
            }
            EventData::ActionFinished(data) => {
                let run = self
                    .runs
                    .get_mut(&data.run)
                    .ok_or("a finished action must belong to a started run")?;
                let action = run
                    .actions
                    .iter_mut()
                    .find(|action| action.id == data.action)
                    .ok_or("a finished action must have started")?;
                action.status = data.status;
                action.output = data.output;
                action.truncated = data.truncated;
                action.omitted_bytes = data.omitted_bytes;
                action.error = data.error;
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use neha::id::Id;
    use neha::record::{ActionStatus, Author, Event, EventKind, KIND_VERSION};
    use neha::seq::Seq;
    use neha::time::Timestamp;

    use super::State;
    use crate::store::Projection as _;

    fn event(kind: EventKind, author: Author, data: serde_json::Value, seq: i64) -> Event {
        let acting = match author {
            Author::Bot(_) => Some(Id::new()),
            Author::User(_) => None,
        };
        Event {
            seq: Seq::from_i64(seq).unwrap(),
            kind,
            version: KIND_VERSION,
            at: Timestamp::from_microseconds(0).unwrap(),
            author,
            acting,
            command: Some(Id::new()),
            data,
        }
    }

    fn chat_created(chat: Id, owner: Id) -> serde_json::Value {
        serde_json::json!({"chat": chat, "owner": owner, "bot": Id::new()})
    }

    fn file_added(file: Id, chat: Id) -> serde_json::Value {
        serde_json::json!({
            "file": file,
            "chat": chat,
            "name": "report.csv",
            "media_type": "text/csv",
            "size": 7,
            "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        })
    }

    fn message_added(chat: Id, text: &str, file: Option<Id>) -> serde_json::Value {
        serde_json::json!({"message": Id::new(), "chat": chat, "text": text, "file": file})
    }

    #[test]
    fn a_file_belongs_to_an_existing_chat() {
        let owner = Id::new();
        let user = Author::User(owner);
        let (chat, file) = (Id::new(), Id::new());
        let mut state = State::default();
        state
            .apply(&event(
                EventKind::ChatCreated,
                user,
                chat_created(chat, owner),
                1,
            ))
            .unwrap();
        state
            .apply(&event(
                EventKind::FileAdded,
                user,
                file_added(file, chat),
                2,
            ))
            .unwrap();

        let files = state.files_of(chat, None);
        assert_eq!(files.len(), 1);
        assert_eq!(
            (files[0].id, files[0].author, files[0].acting),
            (file, user, None)
        );
        assert_eq!(state.file_of(chat, file).unwrap().name, "report.csv");
        assert_eq!(state.owned_file(file, owner).unwrap().id, file);
        assert!(state.owned_file(file, Id::new()).is_none());

        let orphan = event(
            EventKind::FileAdded,
            user,
            file_added(Id::new(), Id::new()),
            3,
        );
        assert!(state.apply(&orphan).is_err());
        assert_eq!(state.files_of(chat, None).len(), 1);
    }

    #[test]
    fn a_message_file_belongs_to_the_message_chat() {
        let owner = Id::new();
        let user = Author::User(owner);
        let (first, second) = (Id::new(), Id::new());
        let file = Id::new();
        let mut state = State::default();
        for (seq, chat) in [(1, first), (2, second)] {
            state
                .apply(&event(
                    EventKind::ChatCreated,
                    user,
                    chat_created(chat, owner),
                    seq,
                ))
                .unwrap();
        }
        state
            .apply(&event(
                EventKind::FileAdded,
                user,
                file_added(file, first),
                3,
            ))
            .unwrap();

        let carried = |chat, file| {
            event(
                EventKind::MessageAdded,
                user,
                message_added(chat, "", Some(file)),
                4,
            )
        };
        assert!(state.apply(&carried(second, file)).is_err());
        assert!(state.apply(&carried(first, Id::new())).is_err());
        state.apply(&carried(first, file)).unwrap();

        assert_eq!(state.messages_of(first, None)[0].file, Some(file));
    }

    #[test]
    fn a_file_message_names_the_file_for_the_model() {
        let owner = Id::new();
        let user = Author::User(owner);
        let (chat, file, other) = (Id::new(), Id::new(), Id::new());
        let mut state = State::default();
        state
            .apply(&event(
                EventKind::ChatCreated,
                user,
                chat_created(chat, owner),
                1,
            ))
            .unwrap();
        for (seq, file) in [(2, file), (3, other)] {
            state
                .apply(&event(
                    EventKind::FileAdded,
                    user,
                    file_added(file, chat),
                    seq,
                ))
                .unwrap();
        }
        for (seq, (text, file)) in [
            (4, ("see attached", Some(file))),
            (5, ("", Some(other))),
            (6, ("plain", None)),
        ] {
            state
                .apply(&event(
                    EventKind::MessageAdded,
                    user,
                    message_added(chat, text, file),
                    seq,
                ))
                .unwrap();
        }

        let named = format!("[File id={file} name=\"report.csv\" type=text/csv size=7]");
        assert_eq!(
            state.history(chat),
            vec![
                (user, format!("see attached\n\n{named}")),
                (
                    user,
                    format!("[File id={other} name=\"report.csv\" type=text/csv size=7]")
                ),
                (user, "plain".to_owned()),
            ]
        );
    }

    #[test]
    fn a_started_action_is_unknown_until_it_finishes() {
        let bot = Author::Bot(Id::new());
        let run = Id::new();
        let action = Id::new();
        let mut state = State::default();
        state
            .apply(&event(
                EventKind::RunStarted,
                bot,
                serde_json::json!({
                    "run": run,
                    "chat": Id::new(),
                    "bot": Id::new(),
                    "command": Id::new(),
                    "message": Id::new(),
                }),
                1,
            ))
            .unwrap();
        state
            .apply(&event(
                EventKind::RunStep,
                bot,
                serde_json::json!({"run": run, "step": 1, "tool_calls": 1}),
                2,
            ))
            .unwrap();
        state
            .apply(&event(
                EventKind::ActionStarted,
                bot,
                serde_json::json!({
                    "run": run,
                    "step": 1,
                    "action": action,
                    "tool": "shell",
                    "arguments": {"command": "true"},
                }),
                3,
            ))
            .unwrap();

        let view = state.runs.get(&run).unwrap();
        assert_eq!(view.steps.len(), 1);
        assert_eq!(view.actions.len(), 1);
        assert_eq!(view.actions[0].status, ActionStatus::Unknown);

        state
            .apply(&event(
                EventKind::ActionFinished,
                bot,
                serde_json::json!({
                    "run": run,
                    "step": 1,
                    "action": action,
                    "tool": "shell",
                    "status": "succeeded",
                    "output": "ok",
                }),
                4,
            ))
            .unwrap();

        let view = state.runs.get(&run).unwrap();
        assert_eq!(view.actions[0].status, ActionStatus::Succeeded);
        assert_eq!(view.actions[0].output, "ok");
    }
}

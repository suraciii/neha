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

    /// Builds the Model history for a Chat, oldest first.
    #[must_use]
    pub fn history(&self, chat: Id) -> Vec<(Author, String)> {
        self.messages
            .iter()
            .filter(|message| message.chat == chat)
            .map(|message| (message.author, message.text.clone()))
            .collect()
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
            EventData::MessageAdded(data) => {
                self.messages.push(MessageView {
                    id: data.message,
                    chat: data.chat,
                    author: event.author,
                    acting: event.acting,
                    text: data.text,
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

    fn event(kind: EventKind, data: serde_json::Value, seq: i64) -> Event {
        Event {
            seq: Seq::from_i64(seq).unwrap(),
            kind,
            version: KIND_VERSION,
            at: Timestamp::from_microseconds(0).unwrap(),
            author: Author::Bot(Id::new()),
            acting: Some(Id::new()),
            command: Some(Id::new()),
            data,
        }
    }

    #[test]
    fn a_started_action_is_unknown_until_it_finishes() {
        let run = Id::new();
        let action = Id::new();
        let mut state = State::default();
        state
            .apply(&event(
                EventKind::RunStarted,
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
                serde_json::json!({"run": run, "step": 1, "tool_calls": 1}),
                2,
            ))
            .unwrap();
        state
            .apply(&event(
                EventKind::ActionStarted,
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
                serde_json::json!({
                    "run": run,
                    "step": 1,
                    "action": action,
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

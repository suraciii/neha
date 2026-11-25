//! The in-process projection of the record.
//!
//! A Space rebuilds this state by replaying its record. Commands append
//! events, then apply the same events to keep the projection current.

use std::collections::HashMap;

use neha::id::Id;
use neha::record::{Author, Event, EventKind};
use neha::seq::Seq;
use neha::time::Timestamp;
use serde::{Deserialize, Serialize};

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

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// The run is still working.
    Running,
    /// The run produced a reply.
    Succeeded,
    /// The run failed.
    Failed,
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
        match event.kind {
            EventKind::UserJoined => {
                let Author::User(id) = event.author else {
                    return Err("a joined User must author the event".to_owned());
                };
                let data: UserJoined = from_data(event)?;
                self.users.insert(
                    id,
                    UserView {
                        id,
                        display_name: data.display_name,
                        is_admin: data.is_admin,
                    },
                );
            }
            EventKind::ProviderAdded => {
                let data: ProviderAdded = from_data(event)?;
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
            EventKind::BotCreated => {
                let data: BotCreated = from_data(event)?;
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
            EventKind::ComputerCreated => {
                let data: ComputerCreated = from_data(event)?;
                self.computers.insert(
                    data.computer,
                    ComputerView {
                        id: data.computer,
                        owner: data.owner,
                        kind: data.kind,
                        online: true,
                    },
                );
            }
            EventKind::ChatCreated => {
                let data: ChatCreated = from_data(event)?;
                self.chats.insert(
                    data.chat,
                    ChatView {
                        id: data.chat,
                        owner: data.owner,
                        bot: data.bot,
                    },
                );
            }
            EventKind::MessageAdded => {
                let data: MessageAdded = from_data(event)?;
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
            EventKind::RunStarted => {
                let data: RunStarted = from_data(event)?;
                let acting = event.acting.ok_or("a run names the User it acts for")?;
                self.runs.insert(
                    data.run,
                    RunView {
                        id: data.run,
                        chat: data.chat,
                        bot: data.bot,
                        acting,
                        message: data.message,
                        status: RunStatus::Running,
                        reply: None,
                        usage: None,
                        error: None,
                    },
                );
            }
            EventKind::RunFinished => {
                let data: RunFinished = from_data(event)?;
                let run = self
                    .runs
                    .get_mut(&data.run)
                    .ok_or("a finished run must have started")?;
                run.status = data.status;
                run.reply = data.reply;
                run.usage = data.usage;
                run.error = data.error;
            }
        }

        Ok(())
    }
}

fn from_data<T: serde::de::DeserializeOwned>(event: &Event) -> Result<T, String> {
    serde_json::from_value(event.data.clone())
        .map_err(|_| format!("event {} does not match its kind", event.kind.as_str()))
}

#[derive(Deserialize)]
struct UserJoined {
    display_name: String,
    is_admin: bool,
}

#[derive(Deserialize)]
struct ProviderAdded {
    provider: Id,
    protocol: String,
    base_url: String,
    models: Vec<ModelData>,
    default_model: Option<Id>,
}

#[derive(Deserialize)]
struct ModelData {
    id: Id,
    name: String,
    context_limit: u32,
    max_output: u32,
    reads_images: bool,
}

#[derive(Deserialize)]
struct BotCreated {
    bot: Id,
    owner: Id,
    name: String,
    computer: Id,
    chat: Id,
}

#[derive(Deserialize)]
struct ComputerCreated {
    computer: Id,
    owner: Id,
    kind: String,
}

#[derive(Deserialize)]
struct ChatCreated {
    chat: Id,
    owner: Id,
    bot: Id,
}

#[derive(Deserialize)]
struct MessageAdded {
    message: Id,
    chat: Id,
    text: String,
    #[serde(default)]
    run: Option<Id>,
}

#[derive(Deserialize)]
struct RunStarted {
    run: Id,
    chat: Id,
    bot: Id,
    message: Id,
}

#[derive(Deserialize)]
struct RunFinished {
    run: Id,
    status: RunStatus,
    #[serde(default)]
    reply: Option<Id>,
    #[serde(default)]
    usage: Option<Usage>,
    #[serde(default)]
    error: Option<String>,
}

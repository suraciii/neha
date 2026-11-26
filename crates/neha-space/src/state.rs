//! The in-process projection of the record.
//!
//! A Space rebuilds this state by replaying its record. Commands append
//! events, then apply the same events to keep the projection current.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

use neha::id::Id;
use neha::record::{
    ActionStatus, ApprovalReason, Author, Event, EventData, RuleEffect, RunReply, Undoability,
};
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
    /// Whether it may write outside its Workspace fence.
    #[serde(default)]
    pub full_access: bool,
    /// Extra folders outside its Workspace it may write.
    #[serde(default)]
    pub grants: Vec<String>,
}

/// A Computer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputerView {
    /// The Computer identifier.
    pub id: Id,
    /// The User who owns it.
    pub owner: Id,
    /// `local` or `hosted`.
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
    /// The cursor of the event that queued it.
    pub seq: Seq,
    /// The cursor of the event that started it, when it started.
    #[serde(skip)]
    pub started: Option<Seq>,
    /// Its status.
    pub status: RunStatus,
    /// The Bot's reply Message, when it succeeded.
    pub reply: Option<Id>,
    /// The usage the Model reported.
    pub usage: Option<Usage>,
    /// The failure, when it failed.
    pub error: Option<String>,
    /// The approval the run waits on, while it is undecided.
    pub approval: Option<Id>,
    /// The directory the run works in, when it named one.
    pub cwd: Option<String>,
    /// A write folder allowed for the next action after a one-time grant.
    pub once_grant: Option<String>,
    /// The Computer the run started on, from the projection's replay.
    ///
    /// A move after the start does not change it: the run keeps the Computer
    /// it began on. It is not part of the wire view.
    #[serde(skip)]
    pub computer: Option<Id>,
    /// The completed steps, in order.
    pub steps: Vec<StepView>,
    /// The actions, in order.
    pub actions: Vec<ActionView>,
}

/// An action a run waits to run until its acting User answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApprovalView {
    /// The approval.
    pub approval: Id,
    /// The run that waits.
    pub run: Id,
    /// The Bot.
    pub bot: Id,
    /// The Computer the run works on.
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
    /// The arguments the action runs with.
    pub arguments: serde_json::Value,
    /// A short text a User reads.
    pub summary: String,
    /// Why the run asks.
    pub reason: ApprovalReason,
    /// Whether a command form defeated the rules and forced the ask.
    pub ambiguous: bool,
    /// The rule that asked, when a rule did.
    pub rule: Option<Id>,
    /// What an "Always allow" would allow, in words the User reads.
    pub lasting_scope: Option<String>,
    /// Whether the action can still be undone after it runs.
    pub undoable: Undoability,
    /// The answer, once the acting User gave one.
    pub decided: Option<bool>,
    /// The cursor of `approval.requested`.
    pub seq: Seq,
}

/// A lasting rule a User gave a Bot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuleView {
    /// The rule.
    pub rule: Id,
    /// The Bot it constrains.
    pub bot: Id,
    /// The User who made it.
    pub user: Id,
    /// Whether matching actions ask or run without asking.
    pub effect: RuleEffect,
    /// The tool name.
    pub tool: String,
    /// The shell command prefix a matching shell action starts with.
    pub command_prefix: Option<String>,
    /// The path prefix a matching file action's path starts with.
    pub path_prefix: Option<String>,
    /// The cursor of `rule.added`.
    pub seq: Seq,
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
    /// The Computers in the order the record created them.
    pub computer_order: Vec<Id>,
    /// The Chats, by identifier.
    pub chats: HashMap<Id, ChatView>,
    /// The Messages, in cursor order.
    pub messages: Vec<MessageView>,
    /// The Files, in cursor order.
    pub files: Vec<FileView>,
    /// The runs, by identifier.
    pub runs: HashMap<Id, RunView>,
    /// The approvals in the order the record asked them.
    pub approvals: Vec<ApprovalView>,
    /// The lasting rules in the order the record added them.
    pub rules: Vec<RuleView>,
    /// The (Bot, acting User, Computer) triples whose first Local use a User
    /// allowed. A Bot moved to another machine asks again there.
    pub first_use_allowed: HashSet<(Id, Id, Id)>,
    /// The Model's replies per run, in the order the record holds them.
    pub replies: HashMap<Id, Vec<RunReply>>,
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

    /// Returns whether a Bot's first Local use for a User on one Computer was
    /// allowed.
    #[must_use]
    pub fn first_use_allowed(&self, bot: Id, acting: Id, computer: Id) -> bool {
        self.first_use_allowed.contains(&(bot, acting, computer))
    }

    /// Returns an approval by identifier.
    #[must_use]
    pub fn approval(&self, approval: Id) -> Option<&ApprovalView> {
        self.approvals.iter().find(|view| view.approval == approval)
    }

    /// Returns the approvals the User may answer, oldest first.
    #[must_use]
    pub fn approvals_for(&self, acting: Id, chat: Option<Id>) -> Vec<&ApprovalView> {
        self.approvals
            .iter()
            .filter(|view| view.acting == acting && view.decided.is_none())
            .filter(|view| chat.is_none_or(|chat| view.chat == chat))
            .collect()
    }

    /// Returns the rules of one Bot.
    #[must_use]
    pub fn rules_of(&self, bot: Id) -> Vec<&RuleView> {
        self.rules.iter().filter(|rule| rule.bot == bot).collect()
    }

    /// Returns the replies the record holds for one run.
    #[must_use]
    pub fn run_replies(&self, run: Id) -> &[RunReply] {
        self.replies.get(&run).map_or(&[], Vec::as_slice)
    }

    /// Returns the latest approval of one run.
    #[must_use]
    pub fn latest_approval(&self, run: Id) -> Option<&ApprovalView> {
        self.approvals
            .iter()
            .filter(|view| view.run == run)
            .max_by_key(|view| view.seq)
    }

    /// Returns whether a run may resume: its approval was allowed and the
    /// approved action has not started.
    #[must_use]
    pub fn is_resumable(&self, run: &RunView) -> bool {
        run.status == RunStatus::Running
            && self.latest_approval(run.id).is_some_and(|approval| {
                approval.decided == Some(true)
                    && !run
                        .actions
                        .iter()
                        .any(|action| action.id == approval.action)
            })
    }

    /// Returns a run of one Bot whose approved action has not started yet.
    ///
    /// Such a run is ready to resume; see [`State::is_resumable`].
    #[must_use]
    pub fn resumable_run(&self, bot: Id) -> Option<&RunView> {
        self.runs
            .values()
            .filter(|run| run.bot == bot && self.is_resumable(run))
            .min_by_key(|run| run.seq)
    }

    /// Returns a run of one Bot that waits on a denied approval.
    #[must_use]
    pub fn denied_run(&self, bot: Id) -> Option<&RunView> {
        self.runs
            .values()
            .filter(|run| run.bot == bot && run.status == RunStatus::Waiting)
            .filter(|run| {
                self.latest_approval(run.id)
                    .is_some_and(|approval| approval.decided == Some(false))
            })
            .min_by_key(|run| run.seq)
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

    /// Returns the runs of a Bot that wait in its queue or work, in the
    /// record's order. A run waiting for an approval is not active: it does
    /// not hold the queue.
    #[must_use]
    pub fn active_runs(&self, bot: Id) -> Vec<&RunView> {
        let mut runs: Vec<&RunView> = self
            .runs
            .values()
            .filter(|run| {
                run.bot == bot && matches!(run.status, RunStatus::Queued | RunStatus::Running)
            })
            .collect();
        runs.sort_by_key(|run| run.seq);

        runs
    }

    /// Builds the Model history for a Chat up to a cursor.
    ///
    /// A resumed run rebuilds the history it saw when it started, not the
    /// Conversation that continued while it waited.
    #[must_use]
    pub fn history_until(&self, chat: Id, until: Seq) -> Vec<(Author, String)> {
        self.messages
            .iter()
            .filter(|message| message.chat == chat && message.seq <= until)
            .map(|message| (message.author, self.model_text(message)))
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
                        full_access: false,
                        grants: Vec::new(),
                    },
                );
            }
            EventData::BotMoved(data) => {
                let Some(bot) = self.bots.get_mut(&data.bot) else {
                    return Err("a moved Bot must exist".to_owned());
                };
                if bot.computer != data.from {
                    return Err("a move starts from the Bot's Computer".to_owned());
                }
                if !self.computers.contains_key(&data.to) {
                    return Err("a move goes to an existing Computer".to_owned());
                }
                bot.computer = data.to;
            }
            EventData::BotUpdated(data) => {
                let bot = self
                    .bots
                    .get_mut(&data.bot)
                    .ok_or("an updated Bot must exist")?;
                if let Some(full_access) = data.full_access {
                    bot.full_access = full_access;
                }
                if let Some(grants) = data.grants {
                    bot.grants = grants;
                }
            }
            EventData::ComputerCreated(data) => {
                self.computers.insert(
                    data.computer,
                    ComputerView {
                        id: data.computer,
                        owner: data.owner,
                        kind: data.kind.as_str().to_owned(),
                        online: true,
                        workspace: data.workspace,
                    },
                );
                self.computer_order.push(data.computer);
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
            EventData::RunQueued(data) => {
                let acting = event.acting.ok_or("a run names the User it acts for")?;
                if !self.bots.contains_key(&data.bot) {
                    return Err("a queued run must belong to an existing Bot".to_owned());
                }
                if !self.chats.contains_key(&data.chat) {
                    return Err("a queued run must belong to an existing Chat".to_owned());
                }
                self.runs.insert(
                    data.run,
                    RunView {
                        id: data.run,
                        chat: data.chat,
                        bot: data.bot,
                        acting,
                        command: data.command,
                        message: data.message,
                        seq: event.seq,
                        started: None,
                        status: RunStatus::Queued,
                        reply: None,
                        usage: None,
                        error: None,
                        approval: None,
                        cwd: None,
                        once_grant: None,
                        computer: None,
                        steps: Vec::new(),
                        actions: Vec::new(),
                    },
                );
            }
            EventData::RunStarted(data) => {
                let acting = event.acting.ok_or("a run names the User it acts for")?;
                let computer = self.bots.get(&data.bot).map(|bot| bot.computer);
                match self.runs.entry(data.run) {
                    Entry::Occupied(mut run) => {
                        run.get_mut().status = RunStatus::Running;
                        run.get_mut().started = Some(event.seq);
                        run.get_mut().computer = computer;
                    }
                    // A record this version wrote before it queued runs holds only
                    // the start event; it still creates the run it started.
                    Entry::Vacant(run) => {
                        run.insert(RunView {
                            id: data.run,
                            chat: data.chat,
                            bot: data.bot,
                            acting,
                            command: data.command,
                            message: data.message,
                            seq: event.seq,
                            started: Some(event.seq),
                            status: RunStatus::Running,
                            reply: None,
                            usage: None,
                            error: None,
                            approval: None,
                            cwd: None,
                            once_grant: None,
                            computer,
                            steps: Vec::new(),
                            actions: Vec::new(),
                        });
                    }
                }
            }
            EventData::RunContext(data) => {
                let run = self
                    .runs
                    .get_mut(&data.run)
                    .ok_or("a run context must belong to a queued run")?;
                if run.status != RunStatus::Queued {
                    return Err("a run context is recorded before the run starts".to_owned());
                }
                run.cwd = Some(data.working_dir);
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
                run.approval = None;
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
                run.once_grant = None;
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
            EventData::RunReply(data) => {
                if !self.runs.contains_key(&data.run) {
                    return Err("a reply must belong to a started run".to_owned());
                }
                self.replies.entry(data.run).or_default().push(data);
            }
            EventData::ApprovalRequested(data) => {
                let run = self
                    .runs
                    .get_mut(&data.run)
                    .ok_or("an approval must belong to a started run")?;
                if !matches!(run.status, RunStatus::Queued | RunStatus::Running) {
                    return Err("an approval must belong to a working run".to_owned());
                }
                if data.bot != run.bot
                    || data.chat != run.chat
                    || data.acting != run.acting
                    || run.computer != Some(data.computer)
                {
                    return Err(
                        "an approval must name its run's Bot, Chat, Computer, and User".to_owned(),
                    );
                }
                if data.ambiguous && data.reason != ApprovalReason::Rule {
                    return Err("only a rule ask can be ambiguous".to_owned());
                }
                if data.ambiguous && data.rule.is_some() {
                    return Err("an ambiguous approval names no rule".to_owned());
                }
                if self
                    .approvals
                    .iter()
                    .any(|view| view.approval == data.approval)
                {
                    return Err("an approval is recorded once".to_owned());
                }
                run.status = RunStatus::Waiting;
                run.approval = Some(data.approval);
                self.approvals.push(ApprovalView {
                    approval: data.approval,
                    run: data.run,
                    bot: data.bot,
                    computer: data.computer,
                    chat: data.chat,
                    acting: data.acting,
                    action: data.action,
                    index: data.index,
                    step: data.step,
                    call: data.call,
                    tool: data.tool,
                    arguments: data.arguments,
                    summary: data.summary,
                    reason: data.reason,
                    ambiguous: data.ambiguous,
                    rule: data.rule,
                    lasting_scope: data.lasting_scope,
                    undoable: data.undoable,
                    decided: None,
                    seq: event.seq,
                });
            }
            EventData::ApprovalDecided(data) => {
                let position = self
                    .approvals
                    .iter()
                    .position(|view| view.approval == data.approval)
                    .ok_or("a decided approval must have been requested")?;
                if self.approvals[position].decided.is_some() {
                    return Err("an approval is decided once".to_owned());
                }
                if self.approvals[position].run != data.run {
                    return Err("a decided approval must name its run".to_owned());
                }
                if self.approvals[position].acting != data.user
                    || event.acting != Some(data.user)
                    || !matches!(event.author, Author::User(author) if author == data.user)
                {
                    return Err("an approval is answered by its acting User".to_owned());
                }
                let (allow, reason, bot, acting, computer, scope) = {
                    let approval = &mut self.approvals[position];
                    approval.decided = Some(data.allow);

                    (
                        data.allow,
                        approval.reason,
                        approval.bot,
                        approval.acting,
                        approval.computer,
                        approval.lasting_scope.clone(),
                    )
                };
                let run = self
                    .runs
                    .get_mut(&data.run)
                    .ok_or("a decided approval must belong to a started run")?;
                if run.approval == Some(data.approval) {
                    run.approval = None;
                }
                if allow {
                    run.status = RunStatus::Running;
                    if reason == ApprovalReason::FirstUseLocal {
                        self.first_use_allowed.insert((bot, acting, computer));
                    }
                    if reason == ApprovalReason::Grant && !data.lasting {
                        run.once_grant = scope;
                    }
                }
            }
            EventData::RuleAdded(data) => {
                if !self.bots.contains_key(&data.bot) {
                    return Err("a rule must belong to an existing Bot".to_owned());
                }
                if event.acting != Some(data.user)
                    || !matches!(event.author, Author::User(author) if author == data.user)
                {
                    return Err("a rule is made by its User".to_owned());
                }
                if self.rules.iter().any(|rule| rule.rule == data.rule) {
                    return Err("a rule is recorded once".to_owned());
                }
                self.rules.push(RuleView {
                    rule: data.rule,
                    bot: data.bot,
                    user: data.user,
                    effect: data.effect,
                    tool: data.tool,
                    command_prefix: data.command_prefix,
                    path_prefix: data.path_prefix,
                    seq: event.seq,
                });
            }
            EventData::RuleRemoved(data) => {
                if event.acting != Some(data.user)
                    || !matches!(event.author, Author::User(author) if author == data.user)
                {
                    return Err("a rule is removed by the User who removed it".to_owned());
                }
                let position = self
                    .rules
                    .iter()
                    .position(|rule| rule.rule == data.rule)
                    .ok_or("a removed rule must exist")?;
                // `user` names the remover, not the maker: a Bot's owner may
                // remove a rule another User made.
                if self.rules[position].bot != data.bot {
                    return Err("a removed rule must name its Bot".to_owned());
                }
                self.rules.remove(position);
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

    use super::{RunStatus, State};
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

    fn acted_event(
        kind: EventKind,
        author: Author,
        acting: Id,
        data: serde_json::Value,
        seq: i64,
    ) -> Event {
        Event {
            seq: Seq::from_i64(seq).unwrap(),
            kind,
            version: KIND_VERSION,
            at: Timestamp::from_microseconds(0).unwrap(),
            author,
            acting: Some(acting),
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
    fn an_approval_must_name_its_runs_bot_and_chat() {
        let owner = Id::new();
        let computer = Id::new();
        let bot = Id::new();
        let chat = Id::new();
        let run = Id::new();
        let mut state = State::default();
        let created = |kind, data, seq| event(kind, Author::User(owner), data, seq);
        state
            .apply(&created(
                EventKind::ComputerCreated,
                serde_json::json!({
                    "computer": computer,
                    "owner": owner,
                    "kind": "hosted",
                    "workspace": "/tmp/workspace",
                }),
                1,
            ))
            .unwrap();
        state
            .apply(&created(
                EventKind::BotCreated,
                serde_json::json!({
                    "bot": bot,
                    "owner": owner,
                    "name": "Scout",
                    "computer": computer,
                    "chat": chat,
                }),
                2,
            ))
            .unwrap();
        state
            .apply(&created(
                EventKind::ChatCreated,
                serde_json::json!({"chat": chat, "owner": owner, "bot": bot}),
                3,
            ))
            .unwrap();
        let bot_author = Author::Bot(bot);
        let run_data = serde_json::json!({
            "run": run,
            "chat": chat,
            "bot": bot,
            "command": Id::new(),
            "message": Id::new(),
        });
        state
            .apply(&acted_event(
                EventKind::RunQueued,
                bot_author,
                owner,
                run_data.clone(),
                4,
            ))
            .unwrap();
        state
            .apply(&acted_event(
                EventKind::RunStarted,
                bot_author,
                owner,
                run_data,
                5,
            ))
            .unwrap();
        let approval = Id::new();
        let action = Id::new();
        let requested = |named_bot: Id, named_chat: Id| {
            serde_json::json!({
                "approval": approval,
                "run": run,
                "bot": named_bot,
                "computer": computer,
                "chat": named_chat,
                "acting": owner,
                "action": action,
                "index": 0,
                "step": 1,
                "call": "call-1",
                "tool": "shell",
                "arguments": {"command": "true"},
                "summary": "shell: true",
                "reason": "rule",
                "rule": null,
                "lasting_scope": null,
                "undoable": "no",
            })
        };
        assert!(
            state
                .apply(&event(
                    EventKind::ApprovalRequested,
                    bot_author,
                    requested(Id::new(), chat),
                    6,
                ))
                .is_err(),
            "an approval for another Bot is refused"
        );
        assert!(
            state
                .apply(&event(
                    EventKind::ApprovalRequested,
                    bot_author,
                    requested(bot, Id::new()),
                    6,
                ))
                .is_err(),
            "an approval for another Chat is refused"
        );
        state
            .apply(&event(
                EventKind::ApprovalRequested,
                bot_author,
                requested(bot, chat),
                6,
            ))
            .unwrap();

        // The acting User answers; nobody else.
        let decided = serde_json::json!({
            "approval": approval,
            "run": run,
            "allow": true,
            "lasting": false,
            "user": Id::new(),
        });
        let other = Id::new();
        assert!(
            state
                .apply(&acted_event(
                    EventKind::ApprovalDecided,
                    Author::User(other),
                    other,
                    decided,
                    7,
                ))
                .is_err(),
            "only the acting User decides"
        );
        assert!(
            state
                .apply(&acted_event(
                    EventKind::RuleAdded,
                    Author::User(owner),
                    owner,
                    serde_json::json!({
                        "rule": Id::new(),
                        "bot": Id::new(),
                        "user": owner,
                        "effect": "ask",
                        "tool": "shell",
                    }),
                    8,
                ))
                .is_err(),
            "a rule needs an existing Bot"
        );

        // An ambiguous approval is a guard, not a rule hit.
        let mut confused = requested(bot, chat);
        confused["approval"] = serde_json::json!(Id::new());
        confused["ambiguous"] = serde_json::json!(true);
        confused["rule"] = serde_json::json!(Id::new());
        assert!(
            state
                .apply(&event(
                    EventKind::ApprovalRequested,
                    bot_author,
                    confused,
                    9,
                ))
                .is_err(),
            "an ambiguous approval names no rule"
        );

        // First use never carries the ambiguity flag.
        let mut first_use = requested(bot, chat);
        first_use["approval"] = serde_json::json!(Id::new());
        first_use["reason"] = serde_json::json!("first_use_local");
        first_use["ambiguous"] = serde_json::json!(true);
        assert!(
            state
                .apply(&event(
                    EventKind::ApprovalRequested,
                    bot_author,
                    first_use,
                    10,
                ))
                .is_err(),
            "a first use is not an ambiguous rule ask"
        );
    }

    #[test]
    fn a_rule_can_be_removed_by_its_bots_owner() {
        let owner = Id::new();
        let maker = Id::new();
        let computer = Id::new();
        let bot = Id::new();
        let chat = Id::new();
        let rule = Id::new();
        let mut state = State::default();
        state
            .apply(&event(
                EventKind::ComputerCreated,
                Author::User(owner),
                serde_json::json!({
                    "computer": computer,
                    "owner": owner,
                    "kind": "hosted",
                    "workspace": "/tmp/workspace",
                }),
                1,
            ))
            .unwrap();
        state
            .apply(&event(
                EventKind::BotCreated,
                Author::User(owner),
                serde_json::json!({
                    "bot": bot,
                    "owner": owner,
                    "name": "Scout",
                    "computer": computer,
                    "chat": chat,
                }),
                2,
            ))
            .unwrap();
        state
            .apply(&event(
                EventKind::ChatCreated,
                Author::User(owner),
                serde_json::json!({"chat": chat, "owner": owner, "bot": bot}),
                3,
            ))
            .unwrap();
        // Another User made the rule; the Bot's owner removes it. The removal
        // names the remover, so it need not equal the maker.
        state
            .apply(&acted_event(
                EventKind::RuleAdded,
                Author::User(maker),
                maker,
                serde_json::json!({
                    "rule": rule,
                    "bot": bot,
                    "user": maker,
                    "effect": "ask",
                    "tool": "shell",
                }),
                4,
            ))
            .unwrap();
        state
            .apply(&acted_event(
                EventKind::RuleRemoved,
                Author::User(owner),
                owner,
                serde_json::json!({"rule": rule, "bot": bot, "user": owner}),
                5,
            ))
            .unwrap();
        assert!(state.rules.iter().all(|view| view.rule != rule));
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

    /// Builds a Bot and its Chat so a run has a place to be queued.
    fn bot_chat(state: &mut State, owner: Id, bot: Id, chat: Id, computer: Id) {
        let user = Author::User(owner);
        state
            .apply(&event(
                EventKind::ComputerCreated,
                user,
                serde_json::json!({
                    "computer": computer,
                    "owner": owner,
                    "kind": "managed",
                    "workspace": "/tmp/workspace",
                }),
                1,
            ))
            .unwrap();
        state
            .apply(&event(
                EventKind::BotCreated,
                user,
                serde_json::json!({
                    "bot": bot,
                    "owner": owner,
                    "name": "Scout",
                    "computer": computer,
                    "chat": chat,
                }),
                2,
            ))
            .unwrap();
        state
            .apply(&event(
                EventKind::ChatCreated,
                user,
                serde_json::json!({"chat": chat, "owner": owner, "bot": bot}),
                3,
            ))
            .unwrap();
    }

    #[test]
    fn a_queued_run_waits_its_turn() {
        let owner = Id::new();
        let bot_id = Id::new();
        let (computer, chat) = (Id::new(), Id::new());
        let (first, second) = (Id::new(), Id::new());
        let command = Id::new();
        let mut state = State::default();
        state
            .apply(&event(
                EventKind::UserJoined,
                Author::User(owner),
                serde_json::json!({"display_name": "Ada", "is_admin": true}),
                0,
            ))
            .unwrap();
        bot_chat(&mut state, owner, bot_id, chat, computer);
        let bot = Author::Bot(bot_id);
        for (seq, run) in [(4, first), (5, second)] {
            state
                .apply(&event(
                    EventKind::RunQueued,
                    bot,
                    serde_json::json!({
                        "run": run,
                        "chat": chat,
                        "bot": bot_id,
                        "command": command,
                        "message": Id::new(),
                    }),
                    seq,
                ))
                .unwrap();
        }
        state
            .apply(&event(
                EventKind::RunStarted,
                bot,
                serde_json::json!({
                    "run": first,
                    "chat": chat,
                    "bot": bot_id,
                    "command": command,
                    "message": Id::new(),
                }),
                6,
            ))
            .unwrap();

        let first = &state.runs[&first];
        assert_eq!(first.status, RunStatus::Running);
        assert_eq!(first.seq, Seq::from_i64(4).unwrap());
        let active = state.active_runs(bot_id);
        assert_eq!(
            active.iter().map(|run| run.seq).collect::<Vec<_>>(),
            [Seq::from_i64(4).unwrap(), Seq::from_i64(5).unwrap()]
        );
        assert_eq!(active[1].status, RunStatus::Queued);

        // A run queued for a Chat the record does not hold cannot be placed.
        let orphan = event(
            EventKind::RunQueued,
            bot,
            serde_json::json!({
                "run": Id::new(),
                "chat": Id::new(),
                "bot": bot_id,
                "command": command,
                "message": Id::new(),
            }),
            7,
        );
        assert!(state.apply(&orphan).is_err());
    }

    #[test]
    fn an_old_record_without_a_queued_event_still_replays() {
        // A record written before runs were queued holds only the start event;
        // the run it starts must still appear after a replay.
        let owner = Id::new();
        let bot_id = Id::new();
        let (computer, chat) = (Id::new(), Id::new());
        let (run, command, message) = (Id::new(), Id::new(), Id::new());
        let mut state = State::default();
        bot_chat(&mut state, owner, bot_id, chat, computer);
        state
            .apply(&event(
                EventKind::MessageAdded,
                Author::User(owner),
                message_added(chat, "hello", None),
                4,
            ))
            .unwrap();
        state
            .apply(&event(
                EventKind::RunStarted,
                Author::Bot(bot_id),
                serde_json::json!({
                    "run": run,
                    "chat": chat,
                    "bot": bot_id,
                    "command": command,
                    "message": message,
                }),
                5,
            ))
            .unwrap();

        let view = state.runs.get(&run).unwrap();
        assert_eq!(view.status, RunStatus::Running);
        assert_eq!(view.seq, Seq::from_i64(5).unwrap());
        assert_eq!(state.active_runs(bot_id), vec![view]);
    }

    #[test]
    fn a_move_changes_the_bots_computer() {
        let owner = Id::new();
        let (from, to, bot, chat) = (Id::new(), Id::new(), Id::new(), Id::new());
        let mut state = State::default();
        bot_chat(&mut state, owner, bot, chat, from);
        state
            .apply(&event(
                EventKind::ComputerCreated,
                Author::User(owner),
                serde_json::json!({
                    "computer": to,
                    "owner": owner,
                    "kind": "real",
                    "workspace": "/home/ada/neha",
                }),
                4,
            ))
            .unwrap();
        assert_eq!(state.bots[&bot].computer, from);

        state
            .apply(&event(
                EventKind::BotMoved,
                Author::User(owner),
                serde_json::json!({"bot": bot, "from": from, "to": to}),
                5,
            ))
            .unwrap();

        assert_eq!(state.bots[&bot].computer, to);
        assert_eq!(state.bots[&bot].id, bot);
    }

    #[test]
    fn a_moved_bot_must_exist() {
        let owner = Id::new();
        let mut state = State::default();
        let error = state
            .apply(&event(
                EventKind::BotMoved,
                Author::User(owner),
                serde_json::json!({"bot": Id::new(), "from": Id::new(), "to": Id::new()}),
                1,
            ))
            .unwrap_err();

        assert_eq!(error, "a moved Bot must exist");
    }

    #[test]
    fn a_move_starts_from_the_bots_computer() {
        let owner = Id::new();
        let (computer, bot, chat) = (Id::new(), Id::new(), Id::new());
        let mut state = State::default();
        bot_chat(&mut state, owner, bot, chat, computer);
        let other = Id::new();
        state
            .apply(&event(
                EventKind::ComputerCreated,
                Author::User(owner),
                serde_json::json!({
                    "computer": other,
                    "owner": owner,
                    "kind": "real",
                    "workspace": "/home/ada/neha",
                }),
                4,
            ))
            .unwrap();

        let error = state
            .apply(&event(
                EventKind::BotMoved,
                Author::User(owner),
                serde_json::json!({"bot": bot, "from": other, "to": other}),
                5,
            ))
            .unwrap_err();

        assert_eq!(error, "a move starts from the Bot's Computer");
        assert_eq!(state.bots[&bot].computer, computer);
    }

    #[test]
    fn a_move_goes_to_an_existing_computer() {
        let owner = Id::new();
        let (computer, bot, chat) = (Id::new(), Id::new(), Id::new());
        let mut state = State::default();
        bot_chat(&mut state, owner, bot, chat, computer);
        let missing = Id::new();

        let error = state
            .apply(&event(
                EventKind::BotMoved,
                Author::User(owner),
                serde_json::json!({"bot": bot, "from": computer, "to": missing}),
                4,
            ))
            .unwrap_err();

        assert_eq!(error, "a move goes to an existing Computer");
        assert_eq!(state.bots[&bot].computer, computer);
    }
    #[test]
    fn a_bot_update_changes_only_what_it_names() {
        let owner = Id::new();
        let (computer, bot, chat) = (Id::new(), Id::new(), Id::new());
        let mut state = State::default();
        bot_chat(&mut state, owner, bot, chat, computer);

        state
            .apply(&event(
                EventKind::BotUpdated,
                Author::User(owner),
                serde_json::json!({
                    "bot": bot,
                    "full_access": true,
                    "grants": ["/data/shared"],
                }),
                4,
            ))
            .unwrap();
        assert!(state.bots[&bot].full_access);
        assert_eq!(state.bots[&bot].grants, vec!["/data/shared".to_owned()]);

        state
            .apply(&event(
                EventKind::BotUpdated,
                Author::User(owner),
                serde_json::json!({"bot": bot, "grants": []}),
                5,
            ))
            .unwrap();
        assert!(
            state.bots[&bot].full_access,
            "an update leaves other fields"
        );
        assert!(state.bots[&bot].grants.is_empty());
    }

    #[test]
    fn an_old_computer_kind_replays_as_the_current_name() {
        let owner = Id::new();
        let mut state = State::default();
        for (seq, recorded, expected) in [(1, "real", "local"), (2, "managed", "hosted")] {
            let computer = Id::new();
            state
                .apply(&event(
                    EventKind::ComputerCreated,
                    Author::User(owner),
                    serde_json::json!({
                        "computer": computer,
                        "owner": owner,
                        "kind": recorded,
                        "workspace": "/tmp/workspace",
                    }),
                    seq,
                ))
                .unwrap();
            assert_eq!(state.computers[&computer].kind, expected, "{recorded}");
        }
    }
}

//! The in-process projection of the record.
//!
//! A Space rebuilds this state by replaying its record. Commands append
//! events, then apply the same events to keep the projection current.

use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::time::Instant;
use ts_rs::TS;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngExt as _;

use neha::id::Id;
use neha::record::{
    ActionStatus, ApprovalReason, Author, Event, EventData, FULL_ACCESS_CONSENT_VERSION,
    RuleEffect, RunReply, Undoability,
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
pub struct UserView {
    /// The account identifier.
    pub id: Id,
    /// The display name.
    pub display_name: String,
    /// Whether the account is the Admin.
    pub is_admin: bool,
}

/// A Provider connected to the Space.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
pub struct ProviderView {
    /// The Provider identifier.
    pub id: Id,
    /// The protocol it speaks.
    pub protocol: String,
    /// Its base address.
    pub base_url: String,
    /// The catalog vendor it came from, when it had a preset.
    #[serde(default)]
    pub vendor: Option<String>,
}

/// A Model the Space offers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
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
    /// Whether it can think before it answers.
    pub thinks: bool,
    /// Whether it can call tools.
    #[serde(default = "true_capability")]
    pub tools: bool,
    /// The thinking levels it supports; empty means every level.
    #[serde(default)]
    pub thinking_levels: Vec<neha::surface::ThinkingLevel>,
    /// Whether it is the Space's default.
    pub is_default: bool,
}

/// Models recorded before tools were a capability could call them.
const fn true_capability() -> bool {
    true
}

impl ModelView {
    /// The thinking levels this Model accepts.
    #[must_use]
    pub fn accepted_thinking(&self) -> Vec<neha::surface::ThinkingLevel> {
        if !self.thinks {
            return Vec::new();
        }
        if self.thinking_levels.is_empty() {
            return neha::surface::ThinkingLevel::ALL.to_vec();
        }
        self.thinking_levels.clone()
    }
}

/// A Bot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
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
    /// Its thinking level; absent uses the Space's default.
    #[serde(default)]
    pub thinking: Option<neha::surface::ThinkingLevel>,
    /// Its Model; absent uses the Space's default.
    #[serde(default)]
    pub model: Option<Id>,
}

/// A Computer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
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
    /// Whether its owner may grant it folders and name a run's directory.
    ///
    /// A real Computer declares this in its channel greeting; the projection
    /// keeps the broad default and [`crate::commands`] overlays what the
    /// machine said.
    #[serde(default = "true_capability")]
    pub supports_grants: bool,
    /// Whether its owner may turn a Bot's full access on.
    #[serde(default = "true_capability")]
    pub supports_full_access: bool,
    /// Whether its machine sandboxes runs; absent until it greets the Space.
    ///
    /// `Some(false)` marks a runtime with no OS fence (an opted-in Windows
    /// Local Computer); `None` means this Space process has not seen the
    /// machine's handshake yet, so a client must not assume either.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandboxed: Option<bool>,
}

/// The live state of a Chat, projected from its runs.
///
/// The state is not recorded. A replay of the same events projects the same
/// state again, so a snapshot and a client that followed the events agree.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
#[serde(rename_all = "snake_case")]
pub enum ChatStatus {
    /// The Chat has no run to follow.
    #[default]
    Idle,
    /// A run is queued or running.
    Working,
    /// A run waits for its acting User to allow or deny an action.
    Waiting,
    /// The Chat's latest run failed.
    Failed,
}

/// A Chat.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
pub struct ChatView {
    /// The Chat identifier.
    pub id: Id,
    /// The User who owns the Chat.
    pub owner: Id,
    /// The Bot the Chat is with.
    pub bot: Id,
    /// The Chat's live state, projected from its runs.
    ///
    /// A record written before the state existed reads as [`ChatStatus::Idle`]
    /// until a run event projects it.
    #[serde(default)]
    pub status: ChatStatus,
    /// The run the state follows: the oldest waiting run while the Chat waits,
    /// or the oldest queued or running run while it works.
    ///
    /// An idle or failed Chat follows no run.
    #[serde(default)]
    pub active_run: Option<Id>,
}

/// The runs one Chat follows, for the state the projection shows.
///
/// The index holds what the state needs and nothing per run that ended: the
/// oldest run waiting for an approval, the oldest queued or working run, and
/// whether the last run event that ended a run ended it in failure. A Space
/// with a long record therefore keeps one index entry per Chat, not one per
/// run, and projecting an event reads no earlier run.
#[derive(Debug, Default)]
struct ChatRuns {
    /// The runs that wait for an approval, oldest first.
    waiting: BTreeSet<(Seq, Id)>,
    /// The runs that are queued or working, oldest first.
    working: BTreeSet<(Seq, Id)>,
    /// Whether the latest event that ended a run in this Chat failed it.
    ///
    /// The flag follows the record's order, not a run's place in the queue:
    /// a later success does not hide an earlier failure and a newer failure
    /// outlives an older success until the next run begins.
    failed: bool,
}

impl ChatRuns {
    /// Follows a run at its current state.
    fn follow(&mut self, run: Id, seq: Seq, status: RunStatus) {
        let entry = (seq, run);
        match status {
            RunStatus::Waiting => {
                self.working.remove(&entry);
                self.waiting.insert(entry);
            }
            RunStatus::Queued | RunStatus::Running => {
                self.waiting.remove(&entry);
                self.working.insert(entry);
            }
            RunStatus::Succeeded | RunStatus::Failed => {
                self.waiting.remove(&entry);
                self.working.remove(&entry);
                self.failed = status == RunStatus::Failed;
            }
        }
    }
}

/// A Message.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
pub struct StepView {
    /// The step number, starting at one.
    pub index: u32,
    /// The usage the Model reported.
    pub usage: Option<Usage>,
    /// How many tool calls the step requested.
    pub tool_calls: u32,
}

/// One action of a run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
pub struct ActionView {
    /// The action identifier.
    pub id: Id,
    /// The step that requested it.
    pub step: u32,
    /// The tool's name.
    pub tool: String,
    /// The tool's arguments. A refused call keeps them malformed, so the value
    /// is not always an object.
    #[ts(type = "unknown")]
    pub arguments: serde_json::Value,
    /// Whether an `ActionFinished` event recorded this action.
    #[serde(skip)]
    pub finished: bool,
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

/// The access a run's actions carry, fixed when it starts.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AccessView {
    /// Whether full access was on when the run started.
    pub full_access: bool,
    /// The folders the owner had granted when the run started.
    pub grants: Vec<String>,
    /// Whether the run's directory was recorded as a write root for it.
    ///
    /// A record written before the marker existed has it false: the directory
    /// is not widened into a write root on restore, and the run is refused
    /// with a message that asks for the message to be sent again.
    #[serde(default)]
    pub cwd_writable: bool,
}

/// A run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
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
    /// Whether the User named that directory for this run, which makes it a
    /// write root for this run alone.
    ///
    /// A record written before the marker existed cannot tell, so it is false
    /// and a restore refuses the run instead of widening its directory into a
    /// write root. It is not part of the wire view.
    #[serde(skip)]
    pub cwd_writable: bool,
    /// A write folder allowed for the next action: the action an answered
    /// grant approval resumed, whether or not the answer also lasts.
    pub once_grant: Option<String>,
    /// The Computer the run started on, from the projection's replay.
    ///
    /// A move after the start does not change it: the run keeps the Computer
    /// it began on. It is not part of the wire view.
    #[serde(skip)]
    pub computer: Option<Id>,
    /// The access the run's actions carry, fixed when it started.
    ///
    /// A change to the Bot's access after the start applies to the next run,
    /// not this one. A record written before the event existed has none, and
    /// the Bot's current access answers for it. It is not part of the wire
    /// view.
    #[serde(skip)]
    pub access: Option<AccessView>,
    /// The completed steps, in order.
    pub steps: Vec<StepView>,
    /// The actions, in order.
    pub actions: Vec<ActionView>,
}

/// An action a run waits to run until its acting User answers.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
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
    #[ts(type = "Record<string, unknown>")]
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
    /// The folder a grant answer covers, when the reason is a grant.
    pub grant_folder: Option<String>,
    /// The directory the run works in, when it named one.
    pub cwd: Option<String>,
    /// Whether the action can still be undone after it runs.
    pub undoable: Undoability,
    /// The answer, once the acting User gave one.
    pub decided: Option<bool>,
    /// The cursor of `approval.requested`.
    pub seq: Seq,
}

/// A lasting rule a User gave a Bot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[ts(export_to = "state.ts")]
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

/// What a Bot's shell may do with its short-lived token.
#[derive(Clone, Debug)]
pub struct BotToken {
    /// The Bot the run works for.
    pub bot: Id,
    /// The Chat the run works in.
    pub chat: Id,
    /// The run.
    pub run: Id,
    /// The User the Bot acts for.
    pub acting: Id,
    /// When the token stops being accepted.
    pub expires: Instant,
}

impl State {
    /// Issues a token bound to one run and its Chat.
    pub fn issue_bot_token(&mut self, binding: BotToken) -> String {
        let mut bytes = [0_u8; 32];
        rand::rng().fill(&mut bytes);
        let token = URL_SAFE_NO_PAD.encode(bytes);
        self.bot_tokens
            .insert(token.clone(), BotToken { ..binding });

        token
    }

    /// Returns the binding of a token that is still valid.
    pub fn bot_token(&mut self, token: &str) -> Option<BotToken> {
        let entry = self.bot_tokens.get(token)?.clone();
        if entry.expires <= Instant::now() {
            self.bot_tokens.remove(token);
            return None;
        }

        Some(entry)
    }
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
    /// The Space's default thinking level.
    pub default_thinking: Option<neha::surface::ThinkingLevel>,
    /// The Bots, by identifier.
    pub bots: HashMap<Id, BotView>,
    /// The Computers, by identifier.
    pub computers: HashMap<Id, ComputerView>,
    /// The Computers in the order the record created them.
    pub computer_order: Vec<Id>,
    /// The Chats, by identifier.
    pub chats: HashMap<Id, ChatView>,
    /// The runs each Chat follows, so its state is projected from an index
    /// instead of a scan of the record's runs.
    chat_runs: HashMap<Id, ChatRuns>,
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
    /// Runs a Space start found interrupted and may recover. It is not part of
    /// the record: a later start rebuilds it from the same events.
    pub recovered_runs: HashSet<Id>,
    /// Recovered runs whose final answer is already in the record, so they
    /// need no Computer to finish.
    pub final_recovered_runs: HashSet<Id>,
    /// The address this Space answers at, once it is bound.
    pub url: Option<String>,
    /// Short-lived credentials a Bot's shell uses to reach this Space. They
    /// live in memory only: a restart drops them and a later run issues new
    /// ones.
    pub bot_tokens: HashMap<String, BotToken>,
    /// Hosted Computers whose Workspace is being reset. The pumps skip them
    /// until the clear finishes, so no run starts on a half-emptied folder.
    pub resetting_computers: HashSet<Id>,
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

    /// Marks a run the current Space start recovered from an interruption.
    pub fn mark_recovered(&mut self, run: Id) {
        self.recovered_runs.insert(run);
    }

    /// Marks a recovered run that ends from its recorded final answer.
    pub fn mark_final_recovered(&mut self, run: Id) {
        self.recovered_runs.insert(run);
        self.final_recovered_runs.insert(run);
    }

    /// Returns whether a recovered run needs no Computer to finish.
    #[must_use]
    pub fn is_final_recovered(&self, run: Id) -> bool {
        self.final_recovered_runs.contains(&run)
    }

    /// Returns whether a Hosted Computer's Workspace is being reset.
    #[must_use]
    pub fn is_resetting(&self, computer: Id) -> bool {
        self.resetting_computers.contains(&computer)
    }

    /// Marks a Hosted Computer's Workspace as being reset.
    pub fn begin_reset(&mut self, computer: Id) {
        self.resetting_computers.insert(computer);
    }

    /// Clears the reset mark once the Workspace is ready again.
    pub fn end_reset(&mut self, computer: Id) {
        self.resetting_computers.remove(&computer);
    }

    /// Returns whether this Space start recovered the run.
    #[must_use]
    pub fn is_recovering(&self, run: &RunView) -> bool {
        run.status == RunStatus::Running && self.recovered_runs.contains(&run.id)
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

    /// Shows the state a Chat's runs project.
    ///
    /// The oldest waiting run wins, then the oldest queued or working one.
    /// With neither, the latest event that ended a run decides: a failed run
    /// leaves the Chat failed until the next run begins, and any other end
    /// leaves it idle.
    fn project_chat_view(&mut self, chat: Id) {
        let (status, active_run) = match self.chat_runs.get(&chat) {
            Some(runs) => {
                if let Some((_, run)) = runs.waiting.first() {
                    (ChatStatus::Waiting, Some(*run))
                } else if let Some((_, run)) = runs.working.first() {
                    (ChatStatus::Working, Some(*run))
                } else if runs.failed {
                    (ChatStatus::Failed, None)
                } else {
                    (ChatStatus::Idle, None)
                }
            }
            None => (ChatStatus::Idle, None),
        };
        let Some(view) = self.chats.get_mut(&chat) else {
            return;
        };
        view.status = status;
        view.active_run = active_run;
    }

    /// Follows a run that just began; a new run clears the Chat's failure.
    fn begin_run(&mut self, run: Id) {
        self.project_run_status(run, true);
    }

    /// Follows a run whose status changed.
    fn follow_run(&mut self, run: Id) {
        self.project_run_status(run, false);
    }

    /// Follows a run's current state in its Chat's projection.
    fn project_run_status(&mut self, run: Id, clears_failure: bool) {
        let Some(view) = self.runs.get(&run) else {
            return;
        };
        let (chat, seq, status) = (view.chat, view.seq, view.status);
        if !self.chats.contains_key(&chat) {
            return;
        }
        let runs = self.chat_runs.entry(chat).or_default();
        if clears_failure {
            runs.failed = false;
        }
        runs.follow(run, seq, status);
        self.project_chat_view(chat);
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
                        vendor: data.vendor,
                    },
                );
                if data.default_model.is_some() {
                    for model in self.models.values_mut() {
                        model.is_default = false;
                    }
                    self.default_thinking = data.thinking;
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
                            thinks: model.thinks,
                            tools: model.tools,
                            thinking_levels: model.thinking_levels,
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
                        thinking: data.thinking,
                        model: data.model,
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
                    // Full access widened the record without a versioned
                    // answer before this rule: such a record replays as
                    // granted, and only the owner's new explicit answer turns
                    // it on again.
                    let consented = data
                        .full_access_version
                        .is_some_and(|version| version == FULL_ACCESS_CONSENT_VERSION);
                    bot.full_access = full_access && consented;
                }
                if let Some(grants) = data.grants {
                    bot.grants = grants;
                }
                if data.reset_model {
                    bot.model = None;
                }
                if let Some(model) = data.model {
                    bot.model = Some(model);
                }
                if data.reset_thinking {
                    bot.thinking = None;
                }
                if let Some(thinking) = data.thinking {
                    bot.thinking = Some(thinking);
                }
            }
            EventData::BotRenamed(data) => {
                let bot = self
                    .bots
                    .get_mut(&data.bot)
                    .ok_or("a renamed Bot must exist")?;
                bot.name = data.name;
            }
            EventData::ProviderModelsUpdated(data) => {
                if !self.providers.contains_key(&data.provider) {
                    return Err("an updated Provider must exist".to_owned());
                }
                if let Some(default) = data.default_model
                    && !data.models.iter().any(|model| model.id == default)
                {
                    return Err("an updated Provider's default Model must exist".to_owned());
                }
                let old: Vec<Id> = self
                    .models
                    .values()
                    .filter(|model| model.provider == data.provider)
                    .map(|model| model.id)
                    .collect();
                for id in &old {
                    self.models.remove(id);
                }
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
                            thinks: model.thinks,
                            tools: model.tools,
                            thinking_levels: model.thinking_levels,
                            is_default: data.default_model == Some(model.id),
                        },
                    );
                }
                // A Bot whose Model disappeared falls back to the default;
                // the next run's preflight then checks the new capabilities.
                for bot in self.bots.values_mut() {
                    if bot
                        .model
                        .is_some_and(|model| !self.models.contains_key(&model))
                    {
                        bot.model = None;
                    }
                }
            }
            EventData::ComputerCreated(data) => {
                let kind = data.kind.as_str().to_owned();
                // A Hosted Computer always runs in the Space's own sandbox;
                // only a real machine's handshake can tell what it serves.
                let sandboxed = (kind == "hosted").then_some(true);
                self.computers.insert(
                    data.computer,
                    ComputerView {
                        id: data.computer,
                        owner: data.owner,
                        kind,
                        online: true,
                        workspace: data.workspace,
                        supports_grants: true,
                        supports_full_access: true,
                        sandboxed,
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
                        status: ChatStatus::Idle,
                        active_run: None,
                    },
                );
                self.chat_runs.insert(data.chat, ChatRuns::default());
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
                        cwd_writable: false,
                        once_grant: None,
                        computer: None,
                        access: None,
                        steps: Vec::new(),
                        actions: Vec::new(),
                    },
                );
                self.begin_run(data.run);
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
                            cwd_writable: false,
                            once_grant: None,
                            computer,
                            access: None,
                            steps: Vec::new(),
                            actions: Vec::new(),
                        });
                    }
                }
                self.begin_run(data.run);
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
                run.cwd_writable = data.writable;
            }
            EventData::RunAccess(data) => {
                let run = self
                    .runs
                    .get_mut(&data.run)
                    .ok_or("a run's access must belong to a started run")?;
                if run.started.is_none() {
                    return Err("a run's access is recorded after it starts".to_owned());
                }
                if run.access.is_some() {
                    return Err("a run's access is recorded once".to_owned());
                }
                run.access = Some(AccessView {
                    full_access: data.full_access,
                    grants: data.grants,
                    cwd_writable: data.cwd_writable,
                });
            }
            EventData::RunFinished(data) => {
                self.recovered_runs.remove(&data.run);
                self.final_recovered_runs.remove(&data.run);
                // A token lives only while its run does.
                self.bot_tokens.retain(|_, token| token.run != data.run);
                let run = self
                    .runs
                    .get_mut(&data.run)
                    .ok_or("a finished run must have started")?;
                run.status = data.status;
                run.reply = data.reply;
                run.usage = data.usage;
                run.error = data.error;
                run.approval = None;
                self.follow_run(data.run);
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
                    finished: false,
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
                action.finished = true;
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
                // The card shows the directory the run works in, so the User
                // answers knowing where the action lands. A record written
                // before runs named a directory has none to project.
                let cwd = run.cwd.clone();
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
                    grant_folder: data.grant_folder,
                    cwd,
                    undoable: data.undoable,
                    decided: None,
                    seq: event.seq,
                });
                self.follow_run(data.run);
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
                        approval.grant_folder.clone(),
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
                    if reason == ApprovalReason::Grant {
                        // The owner answered a grant card, which names the
                        // Bot, the folder, and the machine: that also answers
                        // the first use of this machine.
                        self.first_use_allowed.insert((bot, acting, computer));
                        // The folder is allowed for the action this answer
                        // resumed, whether or not the answer also lasts for
                        // later runs. A lasting answer updates the Bot, which
                        // the run's pinned access deliberately ignores.
                        run.once_grant = scope;
                    }
                }
                self.follow_run(data.run);
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

    use super::{AccessView, ChatStatus, ChatView, FULL_ACCESS_CONSENT_VERSION, RunStatus, State};
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
    fn an_approval_carries_the_directory_its_run_works_in() {
        let owner = Id::new();
        let computer = Id::new();
        let bot = Id::new();
        let chat = Id::new();
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
        // Queues one run, records the directory it works in when the run named
        // one, starts it, and asks for an approval inside it.
        let ask = |state: &mut State, run: Id, working_dir: Option<&str>, seq: i64| {
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
                    seq,
                ))
                .unwrap();
            if let Some(working_dir) = working_dir {
                state
                    .apply(&acted_event(
                        EventKind::RunContext,
                        bot_author,
                        owner,
                        serde_json::json!({
                            "run": run,
                            "working_dir": working_dir,
                            "writable": true,
                        }),
                        seq + 1,
                    ))
                    .unwrap();
            }
            state
                .apply(&acted_event(
                    EventKind::RunStarted,
                    bot_author,
                    owner,
                    run_data,
                    seq + 2,
                ))
                .unwrap();
            state
                .apply(&acted_event(
                    EventKind::ApprovalRequested,
                    bot_author,
                    owner,
                    serde_json::json!({
                        "approval": Id::new(),
                        "run": run,
                        "bot": bot,
                        "computer": computer,
                        "chat": chat,
                        "acting": owner,
                        "action": Id::new(),
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
                    }),
                    seq + 3,
                ))
                .unwrap();
        };

        // A record from before runs named a directory has none to project.
        ask(&mut state, Id::new(), None, 4);
        assert_eq!(state.approvals[0].cwd, None);

        // The directory the run named is the one its card shows.
        ask(&mut state, Id::new(), Some("/work/logs"), 8);
        assert_eq!(
            state.approvals[1].cwd.as_deref(),
            Some("/work/logs"),
            "the card names where the run works"
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

    /// The live state the projection shows for one Chat.
    fn chat_projection(state: &State, chat: Id) -> (ChatStatus, Option<Id>) {
        let view = state.chats.get(&chat).expect("a known Chat");

        (view.status, view.active_run)
    }

    /// Applies a run event one Bot records.
    fn apply_run(
        state: &mut State,
        kind: EventKind,
        bot: Id,
        acting: Id,
        data: serde_json::Value,
        seq: i64,
    ) {
        state
            .apply(&acted_event(kind, Author::Bot(bot), acting, data, seq))
            .unwrap();
    }

    /// Applies the answer its acting User gives an approval.
    fn apply_decision(state: &mut State, owner: Id, data: serde_json::Value, seq: i64) {
        state
            .apply(&acted_event(
                EventKind::ApprovalDecided,
                Author::User(owner),
                owner,
                data,
                seq,
            ))
            .unwrap();
    }

    /// The data a run names itself with when it is queued or started.
    fn run_data(chat: Id, bot: Id, run: Id) -> serde_json::Value {
        serde_json::json!({
            "run": run,
            "chat": chat,
            "bot": bot,
            "command": Id::new(),
            "message": Id::new(),
        })
    }

    /// The data of an approval one run asks for.
    fn approval_requested(
        chat: Id,
        bot: Id,
        computer: Id,
        acting: Id,
        run: Id,
        approval: Id,
    ) -> serde_json::Value {
        serde_json::json!({
            "approval": approval,
            "run": run,
            "bot": bot,
            "computer": computer,
            "chat": chat,
            "acting": acting,
            "action": Id::new(),
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
    }

    /// The data of an answer to an approval.
    fn approval_decided(run: Id, approval: Id, allow: bool, user: Id) -> serde_json::Value {
        serde_json::json!({
            "approval": approval,
            "run": run,
            "allow": allow,
            "lasting": false,
            "user": user,
        })
    }

    /// The data of a run's end.
    fn run_finished(run: Id, status: &str) -> serde_json::Value {
        serde_json::json!({"run": run, "status": status})
    }

    /// Applies one event and keeps it, so a test can replay the record later.
    fn apply_recorded(state: &mut State, record: &mut Vec<Event>, event: Event) {
        state.apply(&event).unwrap();
        record.push(event);
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
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Working, Some(run)),
            "an old record's started run makes its Chat work"
        );

        state
            .apply(&event(
                EventKind::RunFinished,
                Author::Bot(bot_id),
                serde_json::json!({"run": run, "status": "failed"}),
                6,
            ))
            .unwrap();
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Failed, None),
            "an old record's failed run leaves its Chat failed"
        );
    }

    #[test]
    fn a_chat_projects_the_state_of_its_runs() {
        let owner = Id::new();
        let bot_id = Id::new();
        let (computer, chat) = (Id::new(), Id::new());
        let (first, second, third) = (Id::new(), Id::new(), Id::new());
        let first_ask = Id::new();
        let mut state = State::default();
        bot_chat(&mut state, owner, bot_id, chat, computer);
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Idle, None),
            "a new Chat follows no run"
        );

        // Queueing makes the Chat work, and the oldest queued run is the one
        // it follows.
        apply_run(
            &mut state,
            EventKind::RunQueued,
            bot_id,
            owner,
            run_data(chat, bot_id, first),
            4,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Working, Some(first))
        );
        apply_run(
            &mut state,
            EventKind::RunQueued,
            bot_id,
            owner,
            run_data(chat, bot_id, second),
            5,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Working, Some(first)),
            "a second queued run waits behind the first"
        );
        apply_run(
            &mut state,
            EventKind::RunStarted,
            bot_id,
            owner,
            run_data(chat, bot_id, first),
            6,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Working, Some(first))
        );

        // An approval makes the Chat wait; allowing it resumes the work.
        apply_run(
            &mut state,
            EventKind::ApprovalRequested,
            bot_id,
            owner,
            approval_requested(chat, bot_id, computer, owner, first, first_ask),
            7,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Waiting, Some(first))
        );
        apply_decision(
            &mut state,
            owner,
            approval_decided(first, first_ask, true, owner),
            8,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Working, Some(first))
        );

        // The next queued run is followed once the first ends.
        apply_run(
            &mut state,
            EventKind::RunFinished,
            bot_id,
            owner,
            run_finished(first, "succeeded"),
            9,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Working, Some(second))
        );
        apply_run(
            &mut state,
            EventKind::RunStarted,
            bot_id,
            owner,
            run_data(chat, bot_id, second),
            10,
        );
        apply_run(
            &mut state,
            EventKind::RunFinished,
            bot_id,
            owner,
            run_finished(second, "failed"),
            11,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Failed, None),
            "a failed latest run leaves the Chat failed"
        );

        // A new run clears the failure as soon as it queues.
        apply_run(
            &mut state,
            EventKind::RunQueued,
            bot_id,
            owner,
            run_data(chat, bot_id, third),
            12,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Working, Some(third))
        );
        apply_run(
            &mut state,
            EventKind::RunStarted,
            bot_id,
            owner,
            run_data(chat, bot_id, third),
            13,
        );
        apply_run(
            &mut state,
            EventKind::RunFinished,
            bot_id,
            owner,
            run_finished(third, "succeeded"),
            14,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Idle, None),
            "a succeeded latest run leaves the Chat idle"
        );
    }

    #[test]
    fn a_waiting_chat_follows_its_oldest_approval() {
        let owner = Id::new();
        let bot_id = Id::new();
        let (computer, chat) = (Id::new(), Id::new());
        let (first, second) = (Id::new(), Id::new());
        let (first_ask, second_ask) = (Id::new(), Id::new());
        let mut state = State::default();
        bot_chat(&mut state, owner, bot_id, chat, computer);
        for (seq, run) in [(4, first), (5, second)] {
            apply_run(
                &mut state,
                EventKind::RunQueued,
                bot_id,
                owner,
                run_data(chat, bot_id, run),
                seq,
            );
        }

        // A waiting run does not hold the Bot's queue, so a later run starts
        // and asks for its own approval too.
        apply_run(
            &mut state,
            EventKind::RunStarted,
            bot_id,
            owner,
            run_data(chat, bot_id, first),
            6,
        );
        apply_run(
            &mut state,
            EventKind::ApprovalRequested,
            bot_id,
            owner,
            approval_requested(chat, bot_id, computer, owner, first, first_ask),
            7,
        );
        apply_run(
            &mut state,
            EventKind::RunStarted,
            bot_id,
            owner,
            run_data(chat, bot_id, second),
            8,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Waiting, Some(first)),
            "a waiting run wins over a working one"
        );
        apply_run(
            &mut state,
            EventKind::ApprovalRequested,
            bot_id,
            owner,
            approval_requested(chat, bot_id, computer, owner, second, second_ask),
            9,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Waiting, Some(first)),
            "the oldest approval is the one the Chat waits on"
        );

        // Answering the first leaves the Chat waiting on the second.
        apply_decision(
            &mut state,
            owner,
            approval_decided(first, first_ask, true, owner),
            10,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Waiting, Some(second))
        );

        // Denying the second ends it, and the allowed first run works on.
        apply_decision(
            &mut state,
            owner,
            approval_decided(second, second_ask, false, owner),
            11,
        );
        apply_run(
            &mut state,
            EventKind::RunFinished,
            bot_id,
            owner,
            run_finished(second, "failed"),
            12,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Working, Some(first))
        );
    }

    #[test]
    fn a_denied_run_leaves_a_failed_chat() {
        let owner = Id::new();
        let bot_id = Id::new();
        let (computer, chat) = (Id::new(), Id::new());
        let (run, approval) = (Id::new(), Id::new());
        let mut state = State::default();
        bot_chat(&mut state, owner, bot_id, chat, computer);
        apply_run(
            &mut state,
            EventKind::RunQueued,
            bot_id,
            owner,
            run_data(chat, bot_id, run),
            4,
        );
        apply_run(
            &mut state,
            EventKind::RunStarted,
            bot_id,
            owner,
            run_data(chat, bot_id, run),
            5,
        );
        apply_run(
            &mut state,
            EventKind::ApprovalRequested,
            bot_id,
            owner,
            approval_requested(chat, bot_id, computer, owner, run, approval),
            6,
        );
        assert_eq!(
            chat_projection(&state, chat),
            (ChatStatus::Waiting, Some(run))
        );

        // The answer and the run's failure are recorded together, so a reader
        // never sees a wait the User already answered.
        apply_decision(
            &mut state,
            owner,
            approval_decided(run, approval, false, owner),
            7,
        );
        apply_run(
            &mut state,
            EventKind::RunFinished,
            bot_id,
            owner,
            run_finished(run, "failed"),
            8,
        );
        assert_eq!(chat_projection(&state, chat), (ChatStatus::Failed, None));
    }

    #[test]
    fn a_chat_view_crosses_the_wire_with_its_state() {
        for (status, slug) in [
            (ChatStatus::Idle, "idle"),
            (ChatStatus::Working, "working"),
            (ChatStatus::Waiting, "waiting"),
            (ChatStatus::Failed, "failed"),
        ] {
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::json!(slug)
            );
        }

        // A client written before the state existed reads a Chat as idle, and
        // an unknown state is an error rather than an idle Chat.
        let id = Id::new();
        let old = serde_json::json!({"id": id, "owner": Id::new(), "bot": Id::new()});
        let view: ChatView = serde_json::from_value(old).unwrap();
        assert_eq!(view.status, ChatStatus::Idle);
        assert_eq!(view.active_run, None);
        assert!(serde_json::from_str::<ChatStatus>("\"done\"").is_err());

        // The state a client receives is explicit, next to the old fields.
        let run = Id::new();
        let view = ChatView {
            id,
            owner: Id::new(),
            bot: Id::new(),
            status: ChatStatus::Working,
            active_run: Some(run),
        };
        let value = serde_json::to_value(&view).unwrap();
        assert_eq!(value["id"], serde_json::json!(id));
        assert_eq!(value["status"], "working");
        assert_eq!(value["active_run"], serde_json::json!(run));
    }

    #[test]
    fn a_later_failure_outlives_an_earlier_success() {
        // A waiting run does not hold the Bot's queue, so this order is
        // reachable: the first run waits for its approval, a later run starts
        // and succeeds, and only then is the first denied.
        let owner = Id::new();
        let bot_id = Id::new();
        let (computer, chat) = (Id::new(), Id::new());
        let (first, second, third) = (Id::new(), Id::new(), Id::new());
        let first_ask = Id::new();
        let mut live = State::default();
        bot_chat(&mut live, owner, bot_id, chat, computer);
        let mut record = Vec::new();

        apply_recorded(
            &mut live,
            &mut record,
            acted_event(
                EventKind::RunQueued,
                Author::Bot(bot_id),
                owner,
                run_data(chat, bot_id, first),
                4,
            ),
        );
        apply_recorded(
            &mut live,
            &mut record,
            acted_event(
                EventKind::RunStarted,
                Author::Bot(bot_id),
                owner,
                run_data(chat, bot_id, first),
                5,
            ),
        );
        apply_recorded(
            &mut live,
            &mut record,
            acted_event(
                EventKind::ApprovalRequested,
                Author::Bot(bot_id),
                owner,
                approval_requested(chat, bot_id, computer, owner, first, first_ask),
                6,
            ),
        );
        assert_eq!(
            chat_projection(&live, chat),
            (ChatStatus::Waiting, Some(first))
        );

        // The newer run starts and succeeds while the first run waits.
        apply_recorded(
            &mut live,
            &mut record,
            acted_event(
                EventKind::RunQueued,
                Author::Bot(bot_id),
                owner,
                run_data(chat, bot_id, second),
                7,
            ),
        );
        apply_recorded(
            &mut live,
            &mut record,
            acted_event(
                EventKind::RunStarted,
                Author::Bot(bot_id),
                owner,
                run_data(chat, bot_id, second),
                8,
            ),
        );
        apply_recorded(
            &mut live,
            &mut record,
            acted_event(
                EventKind::RunFinished,
                Author::Bot(bot_id),
                owner,
                run_finished(second, "succeeded"),
                9,
            ),
        );
        assert_eq!(
            chat_projection(&live, chat),
            (ChatStatus::Waiting, Some(first)),
            "the wait outranks the newer run"
        );

        // The first run is denied and fails. Its end is the latest event that
        // ended a run, so the Chat stays failed even though the run with the
        // larger queue cursor succeeded.
        apply_recorded(
            &mut live,
            &mut record,
            acted_event(
                EventKind::ApprovalDecided,
                Author::User(owner),
                owner,
                approval_decided(first, first_ask, false, owner),
                10,
            ),
        );
        apply_recorded(
            &mut live,
            &mut record,
            acted_event(
                EventKind::RunFinished,
                Author::Bot(bot_id),
                owner,
                run_finished(first, "failed"),
                11,
            ),
        );
        assert_eq!(
            chat_projection(&live, chat),
            (ChatStatus::Failed, None),
            "the later failure keeps the Chat failed"
        );

        // A replay of the same record shows the same state.
        let mut replay = State::default();
        bot_chat(&mut replay, owner, bot_id, chat, computer);
        for event in &record {
            replay.apply(event).unwrap();
        }
        assert_eq!(chat_projection(&replay, chat), (ChatStatus::Failed, None));
        assert_eq!(replay.chats[&chat], live.chats[&chat]);

        // The next run clears the failure when it queues.
        apply_recorded(
            &mut live,
            &mut record,
            acted_event(
                EventKind::RunQueued,
                Author::Bot(bot_id),
                owner,
                run_data(chat, bot_id, third),
                12,
            ),
        );
        assert_eq!(
            chat_projection(&live, chat),
            (ChatStatus::Working, Some(third))
        );
    }

    #[test]
    fn the_chat_projection_reads_its_index_not_the_record_history() {
        // A record long enough that a projection walking the runs would visit
        // tens of thousands of them: 128 Chats, each with 20 finished runs.
        const CHATS: usize = 128;
        const RUNS: usize = 20;

        let owner = Id::new();
        let user = Author::User(owner);
        let mut state = State::default();
        state
            .apply(&event(
                EventKind::UserJoined,
                user,
                serde_json::json!({"display_name": "Ada", "is_admin": true}),
                0,
            ))
            .unwrap();
        let mut seq = 1_i64;
        let mut chats: Vec<(Id, (ChatStatus, Option<Id>))> = Vec::with_capacity(CHATS);
        for index in 0..CHATS {
            let computer = Id::new();
            let bot = Id::new();
            let chat = Id::new();
            state
                .apply(&event(
                    EventKind::ComputerCreated,
                    user,
                    serde_json::json!({
                        "computer": computer,
                        "owner": owner,
                        "kind": "hosted",
                        "workspace": "/tmp/workspace",
                    }),
                    seq,
                ))
                .unwrap();
            seq += 1;
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
                    seq,
                ))
                .unwrap();
            seq += 1;
            state
                .apply(&event(
                    EventKind::ChatCreated,
                    user,
                    serde_json::json!({"chat": chat, "owner": owner, "bot": bot}),
                    seq,
                ))
                .unwrap();
            seq += 1;

            // Every Chat finishes 20 runs; an even Chat fails its last one.
            let mut last_ended = "succeeded";
            for run_index in 0..RUNS {
                let run = Id::new();
                apply_run(
                    &mut state,
                    EventKind::RunQueued,
                    bot,
                    owner,
                    run_data(chat, bot, run),
                    seq,
                );
                seq += 1;
                apply_run(
                    &mut state,
                    EventKind::RunStarted,
                    bot,
                    owner,
                    run_data(chat, bot, run),
                    seq,
                );
                seq += 1;
                let ended = if index % 2 == 0 && run_index + 1 == RUNS {
                    "failed"
                } else {
                    "succeeded"
                };
                apply_run(
                    &mut state,
                    EventKind::RunFinished,
                    bot,
                    owner,
                    run_finished(run, ended),
                    seq,
                );
                last_ended = ended;
                seq += 1;
            }

            // Two Chats end with a live run, so the index keeps one waiting
            // and one working run.
            let mut active = None;
            if index == 0 {
                let run = Id::new();
                apply_run(
                    &mut state,
                    EventKind::RunQueued,
                    bot,
                    owner,
                    run_data(chat, bot, run),
                    seq,
                );
                seq += 1;
                apply_run(
                    &mut state,
                    EventKind::RunStarted,
                    bot,
                    owner,
                    run_data(chat, bot, run),
                    seq,
                );
                seq += 1;
                apply_run(
                    &mut state,
                    EventKind::ApprovalRequested,
                    bot,
                    owner,
                    approval_requested(chat, bot, computer, owner, run, Id::new()),
                    seq,
                );
                seq += 1;
                active = Some((ChatStatus::Waiting, Some(run)));
            } else if index == 1 {
                let run = Id::new();
                apply_run(
                    &mut state,
                    EventKind::RunQueued,
                    bot,
                    owner,
                    run_data(chat, bot, run),
                    seq,
                );
                seq += 1;
                active = Some((ChatStatus::Working, Some(run)));
            }
            let expected = active.unwrap_or_else(|| {
                if last_ended == "failed" {
                    (ChatStatus::Failed, None)
                } else {
                    (ChatStatus::Idle, None)
                }
            });
            chats.push((chat, expected));
        }

        // The projection keeps one index entry per Chat and only the live
        // runs, not one entry per run the record ever held.
        assert_eq!(state.chat_runs.len(), CHATS);
        let mut live = 0;
        for (index, (chat, expected)) in chats.iter().enumerate() {
            let view = &state.chats[chat];
            assert_eq!(
                (view.status, view.active_run),
                *expected,
                "Chat {index} projects its state"
            );
            let runs = state.chat_runs.get(chat).unwrap();
            live += runs.waiting.len() + runs.working.len();
            assert_eq!(
                runs.failed,
                matches!(expected.0, ChatStatus::Failed),
                "the failure flag follows the latest ended run of Chat {index}"
            );
        }
        assert_eq!(live, 2, "the index keeps only the live runs");

        // The state comes from the index. With the run history gone, the same
        // Chats project the same state; a projection that walked the runs
        // would fall back to idle here.
        let before: Vec<(ChatStatus, Option<Id>)> = chats
            .iter()
            .map(|(chat, _)| (state.chats[chat].status, state.chats[chat].active_run))
            .collect();
        state.runs.clear();
        for (chat, _) in &chats {
            state.project_chat_view(*chat);
        }
        let after: Vec<(ChatStatus, Option<Id>)> = chats
            .iter()
            .map(|(chat, _)| (state.chats[chat].status, state.chats[chat].active_run))
            .collect();
        assert_eq!(before, after);
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
    fn a_bot_rename_changes_only_its_name() {
        let owner = Id::new();
        let (computer, bot, chat) = (Id::new(), Id::new(), Id::new());
        let mut state = State::default();
        bot_chat(&mut state, owner, bot, chat, computer);
        assert_eq!(
            state.bots[&bot].name, "Scout",
            "the Bot keeps the name it was created with"
        );

        state
            .apply(&event(
                EventKind::BotRenamed,
                Author::User(owner),
                serde_json::json!({"bot": bot, "name": "Scout the Second"}),
                4,
            ))
            .unwrap();

        assert_eq!(state.bots[&bot].name, "Scout the Second");
        assert_eq!(state.bots[&bot].owner, owner);
        assert_eq!(state.bots[&bot].chat, chat);
    }

    #[test]
    fn a_renamed_bot_must_exist() {
        let mut state = State::default();
        let error = state
            .apply(&event(
                EventKind::BotRenamed,
                Author::User(Id::new()),
                serde_json::json!({"bot": Id::new(), "name": "Scout"}),
                1,
            ))
            .unwrap_err();

        assert_eq!(error, "a renamed Bot must exist");
    }

    #[test]
    fn a_bot_update_resets_the_choices_it_names() {
        let owner = Id::new();
        let (computer, bot, chat) = (Id::new(), Id::new(), Id::new());
        let model = Id::new();
        let mut state = State::default();
        bot_chat(&mut state, owner, bot, chat, computer);
        let user = Author::User(owner);
        state
            .apply(&event(
                EventKind::BotUpdated,
                user,
                serde_json::json!({"bot": bot, "model": model, "thinking": "low"}),
                4,
            ))
            .unwrap();
        assert_eq!(state.bots[&bot].model, Some(model));
        assert_eq!(
            state.bots[&bot].thinking,
            Some(neha::surface::ThinkingLevel::Low)
        );

        // An old update without the reset fields never clears a choice.
        state
            .apply(&event(
                EventKind::BotUpdated,
                user,
                serde_json::json!({"bot": bot, "grants": []}),
                5,
            ))
            .unwrap();
        assert_eq!(state.bots[&bot].model, Some(model));
        assert!(state.bots[&bot].thinking.is_some());

        // A reset names the one field it clears.
        state
            .apply(&event(
                EventKind::BotUpdated,
                user,
                serde_json::json!({"bot": bot, "reset_model": true}),
                6,
            ))
            .unwrap();
        assert_eq!(state.bots[&bot].model, None);
        assert!(
            state.bots[&bot].thinking.is_some(),
            "a Model reset leaves the level alone"
        );

        state
            .apply(&event(
                EventKind::BotUpdated,
                user,
                serde_json::json!({"bot": bot, "reset_thinking": true}),
                7,
            ))
            .unwrap();
        assert_eq!(state.bots[&bot].thinking, None);
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
                    "full_access_version": FULL_ACCESS_CONSENT_VERSION,
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
    fn a_run_access_is_pinned_when_it_starts() {
        let owner = Id::new();
        let (computer, bot_id, chat) = (Id::new(), Id::new(), Id::new());
        let (run, command, message) = (Id::new(), Id::new(), Id::new());
        let mut state = State::default();
        bot_chat(&mut state, owner, bot_id, chat, computer);
        let bot = Author::Bot(bot_id);
        let run_data = serde_json::json!({
            "run": run,
            "chat": chat,
            "bot": bot_id,
            "command": command,
            "message": message,
        });
        state
            .apply(&event(EventKind::RunQueued, bot, run_data.clone(), 4))
            .unwrap();
        state
            .apply(&event(EventKind::RunStarted, bot, run_data, 5))
            .unwrap();
        state
            .apply(&event(
                EventKind::RunAccess,
                bot,
                serde_json::json!({
                    "run": run,
                    "full_access": false,
                    "grants": ["/data/shared"],
                }),
                6,
            ))
            .unwrap();

        // A change to the Bot after the start applies to the next run.
        state
            .apply(&event(
                EventKind::BotUpdated,
                Author::User(owner),
                serde_json::json!({
                    "bot": bot_id,
                    "full_access": true,
                    "full_access_version": FULL_ACCESS_CONSENT_VERSION,
                    "grants": ["/etc"],
                }),
                7,
            ))
            .unwrap();
        assert!(state.bots[&bot_id].full_access);
        assert_eq!(
            state.runs[&run].access,
            Some(AccessView {
                full_access: false,
                grants: vec!["/data/shared".to_owned()],
                cwd_writable: false,
            }),
            "the run keeps the access it started with"
        );

        // The access is recorded once, and only after the run starts.
        assert!(
            state
                .apply(&event(
                    EventKind::RunAccess,
                    bot,
                    serde_json::json!({"run": run, "full_access": true, "grants": []}),
                    8,
                ))
                .is_err(),
            "a run's access is recorded once"
        );
        assert!(
            state
                .apply(&event(
                    EventKind::RunAccess,
                    bot,
                    serde_json::json!({"run": Id::new(), "full_access": true, "grants": []}),
                    9,
                ))
                .is_err(),
            "a run's access needs a run that started"
        );
    }

    #[test]
    fn a_full_access_from_an_old_record_replays_as_granted() {
        let owner = Id::new();
        let (computer, bot, chat) = (Id::new(), Id::new(), Id::new());
        let mut state = State::default();
        bot_chat(&mut state, owner, bot, chat, computer);
        let user = Author::User(owner);

        // A record written before the owner's answer was versioned never
        // widens access on replay: the Bot restores as granted.
        state
            .apply(&event(
                EventKind::BotUpdated,
                user,
                serde_json::json!({"bot": bot, "full_access": true}),
                4,
            ))
            .unwrap();
        assert!(
            !state.bots[&bot].full_access,
            "an unversioned full access restores as granted"
        );

        // Only a versioned answer turns it on.
        state
            .apply(&event(
                EventKind::BotUpdated,
                user,
                serde_json::json!({
                    "bot": bot,
                    "full_access": true,
                    "full_access_version": FULL_ACCESS_CONSENT_VERSION + 1,
                }),
                5,
            ))
            .unwrap();
        assert!(
            !state.bots[&bot].full_access,
            "a consent this build does not know restores as granted"
        );
        state
            .apply(&event(
                EventKind::BotUpdated,
                user,
                serde_json::json!({
                    "bot": bot,
                    "full_access": true,
                    "full_access_version": FULL_ACCESS_CONSENT_VERSION,
                }),
                6,
            ))
            .unwrap();
        assert!(
            state.bots[&bot].full_access,
            "the versioned answer allows it"
        );
        state
            .apply(&event(
                EventKind::BotUpdated,
                user,
                serde_json::json!({"bot": bot, "full_access": false}),
                7,
            ))
            .unwrap();
        assert!(
            !state.bots[&bot].full_access,
            "turning it off needs no consent"
        );
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

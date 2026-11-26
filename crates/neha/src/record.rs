//! Record events.
//!
//! Every change to a Space enters the record as an event. An event carries a
//! global sequence number, the actor that wrote it, and the data of the kind
//! it names.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::event::EventClass;
use crate::file::{MAX_FILE_BYTES, is_sha256};
use crate::id::Id;
use crate::seq::Seq;
use crate::time::Timestamp;
use crate::usage::Usage;

/// Who wrote an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Author {
    /// A User.
    User(Id),
    /// A Bot.
    Bot(Id),
}

/// The kinds of event the record carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EventKind {
    /// A User joined the Space.
    #[serde(rename = "neha.user.joined")]
    UserJoined,
    /// The Admin connected a Provider.
    #[serde(rename = "neha.provider.added")]
    ProviderAdded,
    /// A Bot was created.
    #[serde(rename = "neha.bot.created")]
    BotCreated,
    /// A Bot moved to another Computer.
    #[serde(rename = "neha.bot.moved")]
    BotMoved,
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
}

impl EventKind {
    /// Returns the wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserJoined => "neha.user.joined",
            Self::ProviderAdded => "neha.provider.added",
            Self::BotCreated => "neha.bot.created",
            Self::BotMoved => "neha.bot.moved",
            Self::ComputerCreated => "neha.computer.created",
            Self::ChatCreated => "neha.chat.created",
            Self::MessageAdded => "neha.message.added",
            Self::FileAdded => "neha.file.added",
            Self::RunQueued => "neha.run.queued",
            Self::RunStarted => "neha.run.started",
            Self::RunFinished => "neha.run.finished",
            Self::RunStep => "neha.run.step",
            Self::ActionStarted => "neha.action.started",
            Self::ActionFinished => "neha.action.finished",
            Self::RunReply => "neha.run.reply",
            Self::ApprovalRequested => "neha.approval.requested",
            Self::ApprovalDecided => "neha.approval.decided",
            Self::RuleAdded => "neha.rule.added",
            Self::RuleRemoved => "neha.rule.removed",
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
            // Steps and actions decide which work may run again and what a
            // budget counted: a reader that does not know them must stop.
            Self::RunStep
            | Self::ActionStarted
            | Self::ActionFinished
            | Self::RunReply
            | Self::ApprovalRequested
            | Self::ApprovalDecided
            | Self::RuleAdded
            | Self::RuleRemoved => EventClass::Authority,
            Self::UserJoined
            | Self::ProviderAdded
            | Self::BotCreated
            | Self::BotMoved
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
            "neha.bot.created" => Ok(Self::BotCreated),
            "neha.bot.moved" => Ok(Self::BotMoved),
            "neha.computer.created" => Ok(Self::ComputerCreated),
            "neha.chat.created" => Ok(Self::ChatCreated),
            "neha.message.added" => Ok(Self::MessageAdded),
            "neha.file.added" => Ok(Self::FileAdded),
            "neha.run.queued" => Ok(Self::RunQueued),
            "neha.run.started" => Ok(Self::RunStarted),
            "neha.run.finished" => Ok(Self::RunFinished),
            "neha.run.step" => Ok(Self::RunStep),
            "neha.action.started" => Ok(Self::ActionStarted),
            "neha.action.finished" => Ok(Self::ActionFinished),
            "neha.run.reply" => Ok(Self::RunReply),
            "neha.approval.requested" => Ok(Self::ApprovalRequested),
            "neha.approval.decided" => Ok(Self::ApprovalDecided),
            "neha.rule.added" => Ok(Self::RuleAdded),
            "neha.rule.removed" => Ok(Self::RuleRemoved),
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acting: Option<Id>,
    /// The command that wrote it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Id>,
    /// The data of its kind.
    pub data: serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::{
        Author, BotMoved, ComputerKind, EventData, EventDataError, EventDraft, EventKind,
        FileAdded, FileError, RunQueued,
    };
    use crate::event::EventClass;
    use crate::file::MAX_FILE_BYTES;
    use crate::id::Id;

    #[test]
    fn wire_names_round_trip() {
        let kinds = [
            EventKind::UserJoined,
            EventKind::ProviderAdded,
            EventKind::BotCreated,
            EventKind::BotMoved,
            EventKind::ComputerCreated,
            EventKind::ChatCreated,
            EventKind::MessageAdded,
            EventKind::FileAdded,
            EventKind::RunQueued,
            EventKind::RunStarted,
            EventKind::RunFinished,
            EventKind::RunStep,
            EventKind::ActionStarted,
            EventKind::ActionFinished,
            EventKind::RunReply,
            EventKind::ApprovalRequested,
            EventKind::ApprovalDecided,
            EventKind::RuleAdded,
            EventKind::RuleRemoved,
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
        for kind in [EventKind::RunQueued, EventKind::RunStarted] {
            assert!(kind.validate_author(bot, Some(Id::new())).is_ok());
            assert!(kind.validate_author(user, None).is_err());
            assert!(kind.validate_author(user, Some(Id::new())).is_err());
            assert!(kind.validate_author(bot, None).is_err());
        }
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

        let plain = serde_json::json!({"message": Id::new(), "chat": chat, "text": "hi"});
        let EventData::MessageAdded(data) = EventKind::MessageAdded.decode(&plain).unwrap() else {
            panic!("expected a Message");
        };
        assert_eq!(data.file, None);
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
}

/// Where a run is: waiting, working, or how it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// A Bot was created.
    BotCreated(BotCreated),
    /// A Bot moved to another Computer.
    BotMoved(BotMoved),
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
}

/// The first version of every event kind.
pub const KIND_VERSION: u16 = 1;

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
            Self::BotCreated => EventData::BotCreated(decode(data)?),
            Self::BotMoved => EventData::BotMoved(decode(data)?),
            Self::ComputerCreated => EventData::ComputerCreated(decode(data)?),
            Self::ChatCreated => EventData::ChatCreated(decode(data)?),
            Self::MessageAdded => EventData::MessageAdded(decode(data)?),
            Self::FileAdded => {
                let data: FileAdded = decode(data)?;
                data.validate()?;

                EventData::FileAdded(data)
            }
            Self::RunQueued => EventData::RunQueued(decode(data)?),
            Self::RunStarted => EventData::RunStarted(decode(data)?),
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
        };

        Ok(decoded)
    }

    /// Checks that the author and acting User fit the kind.
    pub fn validate_author(self, author: Author, acting: Option<Id>) -> Result<(), EventDataError> {
        match self {
            Self::UserJoined
            | Self::ProviderAdded
            | Self::BotCreated
            | Self::BotMoved
            | Self::ComputerCreated
            | Self::ChatCreated => {
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserJoined {
    /// The display name.
    pub display_name: String,
    /// Whether the account is the Admin.
    pub is_admin: bool,
}

/// The data of `neha.provider.added`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderAdded {
    /// The Provider identifier.
    pub provider: Id,
    /// The protocol it speaks.
    pub protocol: String,
    /// Its base address.
    pub base_url: String,
    /// The Models it offers.
    pub models: Vec<ModelData>,
    /// The default Model.
    #[serde(default)]
    pub default_model: Option<Id>,
}

/// One Model a Provider offers.
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
}

/// The data of `neha.bot.created`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
}

/// The data of `neha.bot.moved`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotMoved {
    /// The Bot that moved.
    pub bot: Id,
    /// The Computer it worked on.
    pub from: Id,
    /// The Computer it works on now.
    pub to: Id,
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
}

/// The largest name a File carries.
const MAX_FILE_NAME_BYTES: usize = 255;
/// The largest media type a File carries.
const MAX_MEDIA_TYPE_BYTES: usize = 127;

/// The data of `neha.file.added`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
    /// The tool's arguments.
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// The data of `neha.action.finished`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionFinished {
    /// The run.
    pub run: Id,
    /// The step.
    pub step: u32,
    /// The action.
    pub action: Id,
    /// The tool's name.
    pub tool: String,
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

/// The data of `neha.run.finished`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedCall {
    /// The tool-call identifier the provider expects in the result.
    pub call: String,
    /// The tool's name.
    pub tool: String,
    /// The arguments the tool runs with.
    pub arguments: serde_json::Value,
    /// Why the loop refused the call before anything ran; `None` when it can run.
    #[serde(default)]
    pub refused: Option<String>,
}

/// The data of `neha.run.reply`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalReason {
    /// The first use of a Local Computer by this Bot for this User.
    FirstUseLocal,
    /// A lasting rule asks first.
    Rule,
}

/// Whether the User can undo an action after allowing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Undoability {
    /// The action cannot be undone.
    No,
    /// The action may or may not be undoable; this build cannot tell.
    Unknown,
}

/// What a lasting rule does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleEffect {
    /// Ask before a matching action.
    Ask,
    /// Allow matching actions without asking.
    Allow,
}

/// The data of `neha.approval.requested`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
    /// Whether the action can still be undone after it runs.
    pub undoable: Undoability,
}

/// The data of `neha.approval.decided`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Deserialize)]
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
}

/// The data of `neha.rule.removed`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
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

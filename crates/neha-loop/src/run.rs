//! The built-in Bot loop.
//!
//! A run repeats one step at a time: call the Model, record the step, run
//! every tool call the step requested, record each one before and after it
//! runs, and feed the results back. A tool call runs on the Computer or, for
//! the two File tools, moves a File between the Space and the Computer. The
//! loop stops with a final Message, a failure, or a step limit.

use std::future::Future;
use std::path::Path;

use neha::id::Id;
use neha::usage::Usage;
use neha_computer::{ActionResult, ActionStatus, ByteError, ComputerRuntime, ToolOutput};
use serde_json::Value;
use thiserror::Error;

use crate::files::{FileHost, workspace_name};
use crate::model::{Message, Model, ModelFailure, ModelRequest, ToolCall};
use crate::tools;

/// The largest raw tool-call arguments string a step event keeps whole.
const MAX_RECORDED_ARGUMENTS: usize = 128 * 1024;

/// What one run needs to start.
pub struct RunRequest {
    /// The system instruction.
    pub system: String,
    /// The Messages before the run.
    pub history: Vec<Message>,
    /// The largest Model reply in tokens.
    pub max_output: u32,
    /// How many steps the run may take.
    pub max_steps: usize,
}

impl RunRequest {
    /// Builds a request with the default step limit.
    #[must_use]
    pub fn new(system: impl Into<String>, history: Vec<Message>, max_output: u32) -> Self {
        Self {
            system: system.into(),
            history,
            max_output,
            max_steps: 8,
        }
    }

    /// Changes the step limit.
    #[must_use]
    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    /// The final Message text.
    pub text: String,
    /// The usage of every step.
    pub usage: Usage,
    /// Whether the run reached a final answer.
    pub succeeded: bool,
}

/// The record seam the loop writes through.
pub trait Recorder {
    /// Records one completed step.
    fn step(
        &mut self,
        step: u32,
        usage: Usage,
        tool_calls: u32,
    ) -> impl Future<Output = Result<(), LoopError>> + Send;

    /// Records an action before it runs.
    fn action_started(
        &mut self,
        step: u32,
        action: Id,
        tool: String,
        arguments: Value,
    ) -> impl Future<Output = Result<(), LoopError>> + Send;

    /// Records an action's result.
    fn action_finished(
        &mut self,
        step: u32,
        action: Id,
        tool: String,
        status: ActionStatus,
        output: ToolOutput,
        error: Option<String>,
    ) -> impl Future<Output = Result<(), LoopError>> + Send;
}

/// Runs one Bot run to its end.
pub async fn run<R, M, Rec, F>(
    runtime: &R,
    model: &M,
    recorder: &mut Rec,
    files: &F,
    request: RunRequest,
) -> Result<RunOutcome, LoopError>
where
    R: ComputerRuntime + Sync,
    M: Model + Sync,
    Rec: Recorder + Send,
    F: FileHost + Sync,
{
    let mut messages = Vec::with_capacity(request.history.len() + 8);
    messages.push(Message::system(request.system));
    messages.extend(request.history);
    let mut usage = Usage::default();
    let max_steps = request.max_steps.min(u32::MAX as usize) as u32;

    for step in 1..=max_steps {
        let reply = match model
            .complete(ModelRequest {
                messages: messages.clone(),
                tools: tools::specs(),
                max_output: request.max_output,
            })
            .await
        {
            Ok(reply) => reply,
            Err(failure) => {
                return Ok(failure_outcome(&failure, usage));
            }
        };
        usage += reply.usage;
        let calls = reply.tool_calls;
        if calls.is_empty() {
            // A step with no actions completes when the Model answers.
            recorder.step(step, reply.usage, 0).await?;
            let text = reply.content.unwrap_or_default();
            if text.trim().is_empty() {
                return Ok(RunOutcome {
                    text: "The Model returned an empty reply.".to_owned(),
                    usage,
                    succeeded: false,
                });
            }
            return Ok(RunOutcome {
                text,
                usage,
                succeeded: true,
            });
        }

        messages.push(Message::assistant_with_calls(reply.content, calls.clone()));
        for (index, call) in calls.iter().enumerate() {
            let action = Id::new();
            // A call that will run keeps its real arguments in the record.
            // Only a call that is refused may be summarized.
            let parsed = tools::parse(&call.name, &call.arguments);
            let arguments = match (&parsed, call.arguments.len() > MAX_RECORDED_ARGUMENTS) {
                (Ok(_), _) => serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| Value::String(call.arguments.clone())),
                (Err(_), true) => {
                    serde_json::json!({"truncated": true, "bytes": call.arguments.len()})
                }
                (Err(_), false) => serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| Value::String(call.arguments.clone())),
            };
            recorder
                .action_started(step, action, call.name.clone(), arguments)
                .await?;
            let result = match parsed {
                Ok(tools::Call::Computer(computer_action)) => {
                    runtime.execute(computer_action).await
                }
                Ok(tools::Call::TakeFile { file }) => take_file(runtime, files, file).await,
                Ok(tools::Call::PutFile { path, name }) => {
                    put_file(runtime, files, &path, name).await
                }
                Err(error) => ActionResult::failed(error),
            };
            recorder
                .action_finished(
                    step,
                    action,
                    call.name.clone(),
                    result.status,
                    result.output.clone(),
                    result.error.clone(),
                )
                .await?;
            messages.push(Message::tool_result(
                call_id(call, step, index),
                tool_result_text(&result),
            ));
            if result.status == ActionStatus::Unknown {
                // The action's result never returned. The record says unknown,
                // and the run ends here instead of resuming it blindly.
                return Ok(RunOutcome {
                    text: "I lost my Computer before the action returned.".to_owned(),
                    usage,
                    succeeded: false,
                });
            }
        }
        // A step completes only after all of its actions were recorded.
        recorder.step(step, reply.usage, calls.len() as u32).await?;
    }

    Ok(RunOutcome {
        text: format!("I stopped after {max_steps} steps without a final answer."),
        usage,
        succeeded: false,
    })
}

/// Turns a File transfer's error into the action's result.
///
/// A lost answer means whether the bytes moved is unknown, so the run ends the
/// way any lost action does instead of asking the Model and finishing as if
/// nothing happened. A refusal is an ordinary failed tool result the Model can
/// read and work around.
fn byte_failure(error: ByteError) -> ActionResult {
    match error {
        ByteError::Failed(message) => ActionResult::failed(message),
        ByteError::TimedOut(message) => ActionResult {
            status: ActionStatus::TimedOut,
            output: ToolOutput::full(message.clone()),
            error: Some(message),
        },
        ByteError::Lost(message) => ActionResult {
            status: ActionStatus::Unknown,
            output: ToolOutput::full(message.clone()),
            error: Some(message),
        },
    }
}

/// Puts one File into the Workspace under its name.
async fn take_file<R, F>(runtime: &R, files: &F, file: Id) -> ActionResult
where
    R: ComputerRuntime,
    F: FileHost,
{
    let taken = match files.take(file).await {
        Ok(taken) => taken,
        Err(error) => return ActionResult::failed(error),
    };
    let path = workspace_name(&taken.name, &file);
    let bytes = taken.bytes.len();
    if let Err(error) = runtime.write_bytes(&path, taken.bytes).await {
        return byte_failure(error);
    }
    ActionResult::succeeded(ToolOutput::full(format!(
        "took {} ({bytes} bytes) as {path}",
        taken.name
    )))
}

/// Delivers one Workspace file as a File of the Chat.
async fn put_file<R, F>(runtime: &R, files: &F, path: &str, name: Option<String>) -> ActionResult
where
    R: ComputerRuntime,
    F: FileHost,
{
    let bytes = match runtime.read_bytes(path).await {
        Ok(bytes) => bytes,
        Err(error) => return byte_failure(error),
    };
    let name = match name {
        Some(name) => name,
        None => {
            let Some(file_name) = Path::new(path).file_name() else {
                return ActionResult::failed("the path has no file name");
            };
            file_name.to_string_lossy().into_owned()
        }
    };
    match files.put(name, bytes).await {
        Ok(delivered) => ActionResult::succeeded(ToolOutput::full(format!(
            "delivered {} as file {}",
            delivered.name, delivered.id
        ))),
        Err(error) => ActionResult::failed(error),
    }
}

fn failure_outcome(failure: &ModelFailure, usage: Usage) -> RunOutcome {
    RunOutcome {
        text: format!("I could not answer: {}", failure.detail),
        usage: usage + failure.usage,
        succeeded: false,
    }
}

fn call_id(call: &ToolCall, step: u32, index: usize) -> String {
    if call.id.is_empty() {
        format!("call-{step}-{index}")
    } else {
        call.id.clone()
    }
}

fn tool_result_text(result: &ActionResult) -> String {
    match result.status {
        ActionStatus::Succeeded => result.output.text.clone(),
        ActionStatus::Failed | ActionStatus::TimedOut | ActionStatus::Unknown => {
            let reason = result
                .error
                .clone()
                .unwrap_or_else(|| "the action failed".to_owned());
            // A failed action already puts its reason in its output; repeating
            // it wastes the Model's context on every refused call.
            if result.output.text.trim() == reason.trim() {
                reason
            } else {
                format!("{reason}\n{}", result.output.text)
            }
        }
    }
}

/// The loop cannot continue because the record failed.
#[derive(Debug, Error)]
pub enum LoopError {
    /// The record could not hold an event.
    #[error("the record failed: {0}")]
    Record(String),
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::Mutex;

    use neha::id::Id;
    use neha::usage::Usage;
    use neha_computer::{
        Action, ActionResult, ActionStatus, ByteError, ComputerRuntime, ToolOutput,
    };
    use serde_json::{Value, json};

    use super::{LoopError, Recorder, RunRequest, run};
    use crate::files::{DeliveredFile, FileHost, TakenFile};
    use crate::model::{Message, Model, ModelFailure, ModelReply, ModelRequest, Role, ToolCall};
    use crate::tools::{PUT_FILE, TAKE_FILE};

    #[derive(Default)]
    struct FakeModel {
        replies: Mutex<VecDeque<Result<ModelReply, ModelFailure>>>,
        requests: Mutex<Vec<ModelRequest>>,
    }

    impl FakeModel {
        fn with(replies: Vec<Result<ModelReply, ModelFailure>>) -> Self {
            Self {
                replies: Mutex::new(replies.into()),
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl Model for FakeModel {
        fn complete(
            &self,
            request: ModelRequest,
        ) -> impl Future<Output = Result<ModelReply, ModelFailure>> + Send {
            self.requests.lock().unwrap().push(request);
            let reply = self.replies.lock().unwrap().pop_front().unwrap();
            async move { reply }
        }
    }

    /// A Computer whose workspace is a list of files.
    #[derive(Default)]
    struct FakeRuntime {
        actions: Mutex<Vec<Action>>,
        files: Mutex<Vec<(String, Vec<u8>)>>,
        fail_writes: bool,
        lost: bool,
        slow_writes: bool,
    }

    impl FakeRuntime {
        fn with_file(path: &str, bytes: &[u8]) -> Self {
            Self {
                files: Mutex::new(vec![(path.to_owned(), bytes.to_vec())]),
                ..Self::default()
            }
        }

        /// A Computer that goes away before it answers.
        fn lost() -> Self {
            Self {
                lost: true,
                ..Self::default()
            }
        }

        /// A Computer that is too slow to take a File.
        fn slow() -> Self {
            Self {
                slow_writes: true,
                ..Self::default()
            }
        }
    }

    impl ComputerRuntime for FakeRuntime {
        #[allow(clippy::manual_async_fn)]
        fn execute(&self, action: Action) -> impl Future<Output = ActionResult> + Send {
            self.actions.lock().unwrap().push(action);
            let lost = self.lost;
            async move {
                if lost {
                    return ActionResult {
                        status: ActionStatus::Unknown,
                        output: ToolOutput::full("the Computer went offline"),
                        error: Some("the Computer went offline".to_owned()),
                    };
                }
                ActionResult::succeeded(ToolOutput::full("42"))
            }
        }

        #[allow(clippy::manual_async_fn)]
        fn read_bytes(
            &self,
            path: &str,
        ) -> impl Future<Output = Result<Vec<u8>, ByteError>> + Send {
            let found = self
                .files
                .lock()
                .unwrap()
                .iter()
                .find(|(stored, _)| stored == path)
                .map(|(_, bytes)| bytes.clone());
            let lost = self.lost;
            async move {
                if lost {
                    return Err(ByteError::Lost("the Computer went offline".to_owned()));
                }
                found.ok_or_else(|| ByteError::Failed(format!("cannot read {path}")))
            }
        }

        #[allow(clippy::manual_async_fn)]
        fn write_bytes(
            &self,
            path: &str,
            bytes: Vec<u8>,
        ) -> impl Future<Output = Result<(), ByteError>> + Send {
            let written = if self.lost {
                Err(ByteError::Lost("the Computer went offline".to_owned()))
            } else if self.slow_writes {
                Err(ByteError::TimedOut(
                    "the Computer did not answer in time".to_owned(),
                ))
            } else if self.fail_writes {
                Err(ByteError::Failed("the workspace is full".to_owned()))
            } else {
                self.files.lock().unwrap().push((path.to_owned(), bytes));
                Ok(())
            };
            async move { written }
        }
    }

    /// The Space's Files of one Chat.
    #[derive(Default)]
    struct FakeFiles {
        files: Mutex<Vec<(Id, TakenFile)>>,
        puts: Mutex<Vec<(String, Vec<u8>)>>,
        delivered: Id,
        refuse_put: bool,
    }

    impl FakeFiles {
        fn with(files: Vec<(Id, TakenFile)>) -> Self {
            Self {
                files: Mutex::new(files),
                ..Self::default()
            }
        }
    }

    impl FileHost for FakeFiles {
        #[allow(clippy::manual_async_fn)]
        fn take(&self, file: Id) -> impl Future<Output = Result<TakenFile, String>> + Send {
            let taken = self
                .files
                .lock()
                .unwrap()
                .iter()
                .find(|(id, _)| *id == file)
                .map(|(_, taken)| taken.clone());
            async move { taken.ok_or_else(|| format!("there is no File {file} in this Chat")) }
        }

        #[allow(clippy::manual_async_fn)]
        fn put(
            &self,
            name: String,
            bytes: Vec<u8>,
        ) -> impl Future<Output = Result<DeliveredFile, String>> + Send {
            let delivered = if self.refuse_put {
                Err("the Chat cannot take this File".to_owned())
            } else {
                self.puts.lock().unwrap().push((name.clone(), bytes));
                Ok(DeliveredFile {
                    id: self.delivered,
                    name,
                })
            };
            async move { delivered }
        }
    }

    #[derive(Default)]
    struct TestRecorder {
        steps: Vec<(u32, Usage, u32)>,
        started: Vec<(u32, String)>,
        finished: Vec<(u32, ActionStatus)>,
        fail: bool,
        order: Vec<&'static str>,
    }

    impl Recorder for TestRecorder {
        async fn step(&mut self, step: u32, usage: Usage, calls: u32) -> Result<(), LoopError> {
            if self.fail {
                return Err(LoopError::Record("no".to_owned()));
            }
            self.order.push("step");
            self.steps.push((step, usage, calls));
            Ok(())
        }

        async fn action_started(
            &mut self,
            step: u32,
            _action: Id,
            tool: String,
            _arguments: Value,
        ) -> Result<(), LoopError> {
            self.order.push("action_started");
            self.started.push((step, tool));
            Ok(())
        }

        async fn action_finished(
            &mut self,
            step: u32,
            _action: Id,
            _tool: String,
            status: ActionStatus,
            _output: ToolOutput,
            _error: Option<String>,
        ) -> Result<(), LoopError> {
            self.order.push("action_finished");
            self.finished.push((step, status));
            Ok(())
        }
    }

    fn reply(text: &str) -> Result<ModelReply, ModelFailure> {
        Ok(ModelReply {
            content: Some(text.to_owned()),
            tool_calls: Vec::new(),
            usage: Usage {
                output: 1,
                ..Usage::default()
            },
        })
    }

    fn call(id: &str, name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            arguments: arguments.to_string(),
        }
    }

    fn calls_reply(tool_calls: Vec<ToolCall>) -> Result<ModelReply, ModelFailure> {
        Ok(ModelReply {
            content: None,
            tool_calls,
            usage: Usage {
                output: 2,
                ..Usage::default()
            },
        })
    }

    fn call_reply(name: &str, arguments: Value) -> Result<ModelReply, ModelFailure> {
        calls_reply(vec![call("call-1", name, arguments)])
    }

    fn taken_file(name: &str, bytes: &[u8]) -> TakenFile {
        TakenFile {
            name: name.to_owned(),
            media_type: "text/plain".to_owned(),
            bytes: bytes.to_vec(),
        }
    }

    /// The text each tool call of the first step returned to the Model.
    fn tool_texts(model: &FakeModel) -> Vec<String> {
        let requests = model.requests.lock().unwrap();
        requests[1]
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .map(|message| message.content.clone().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn a_final_answer_ends_the_run() {
        let model = FakeModel::with(vec![reply("hello")]);
        let runtime = FakeRuntime::default();
        let files = FakeFiles::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert_eq!(outcome.text, "hello");
        assert!(outcome.succeeded);
        assert_eq!(outcome.usage.output, 1);
        assert_eq!(recorder.steps.len(), 1);
        assert!(recorder.started.is_empty());
    }

    #[tokio::test]
    async fn a_tool_call_runs_through_the_computer_and_returns_to_the_model() {
        let model = FakeModel::with(vec![
            call_reply(
                "write_file",
                json!({"path": "hello.txt", "content": "hello neha"}),
            ),
            reply("done"),
        ]);
        let runtime = FakeRuntime::default();
        let files = FakeFiles::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert_eq!(outcome.text, "done");
        assert!(outcome.succeeded);
        assert_eq!(recorder.started.len(), 1);
        assert_eq!(recorder.started[0].1, "write_file");
        assert_eq!(recorder.finished[0].1, ActionStatus::Succeeded);
        assert_eq!(
            recorder.order,
            ["action_started", "action_finished", "step", "step"]
        );
        let actions = runtime.actions.lock().unwrap();
        assert_eq!(
            actions[0],
            Action::WriteFile {
                path: "hello.txt".to_owned(),
                content: "hello neha".to_owned(),
            }
        );
        let requests = model.requests.lock().unwrap();
        let second = requests[1].messages.last().unwrap();
        assert_eq!(second.role, crate::model::Role::Tool);
        assert_eq!(second.content.as_deref(), Some("42"));
    }

    #[tokio::test]
    async fn an_action_whose_result_never_returned_ends_the_run() {
        let model = FakeModel::with(vec![
            call_reply(
                "write_file",
                json!({"path": "hello.txt", "content": "hello neha"}),
            ),
            reply("the Model must not be asked again"),
        ]);
        let runtime = FakeRuntime::lost();
        let files = FakeFiles::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert!(!outcome.succeeded);
        assert_eq!(
            outcome.text,
            "I lost my Computer before the action returned."
        );
        assert_eq!(recorder.finished[0].1, ActionStatus::Unknown);
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn take_file_puts_a_file_into_the_workspace() {
        let id = Id::new();
        let files = FakeFiles::with(vec![(id, taken_file("report.txt", b"hello"))]);
        let model = FakeModel::with(vec![
            call_reply(TAKE_FILE, json!({"file": id})),
            reply("done"),
        ]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert!(outcome.succeeded);
        assert_eq!(
            *runtime.files.lock().unwrap(),
            vec![("report.txt".to_owned(), b"hello".to_vec())]
        );
        assert!(runtime.actions.lock().unwrap().is_empty());
        assert_eq!(recorder.started[0].1, TAKE_FILE);
        assert_eq!(recorder.finished[0].1, ActionStatus::Succeeded);
        assert_eq!(
            tool_texts(&model),
            ["took report.txt (5 bytes) as report.txt"]
        );
    }

    #[tokio::test]
    async fn take_file_of_an_unknown_file_is_returned_to_the_model() {
        let files = FakeFiles::default();
        let model = FakeModel::with(vec![
            call_reply(TAKE_FILE, json!({"file": Id::new()})),
            reply("done"),
        ]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert_eq!(outcome.text, "done");
        assert_eq!(recorder.finished[0].1, ActionStatus::Failed);
        assert!(tool_texts(&model)[0].contains("no File"));
        assert!(runtime.files.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_file_transfer_that_times_out_is_returned_to_the_model() {
        let id = Id::new();
        let files = FakeFiles::with(vec![(id, taken_file("report.txt", b"hello"))]);
        let model = FakeModel::with(vec![
            call_reply(TAKE_FILE, json!({"file": id})),
            reply("done"),
        ]);
        let runtime = FakeRuntime::slow();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert!(
            outcome.succeeded,
            "a timeout is a tool answer, not a lost run"
        );
        assert_eq!(recorder.finished[0].1, ActionStatus::TimedOut);
        assert_eq!(model.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn put_file_delivers_a_workspace_file() {
        for (arguments, name) in [
            (json!({"path": "out/report.txt"}), "report.txt"),
            (
                json!({"path": "out/report.txt", "name": "answer.txt"}),
                "answer.txt",
            ),
        ] {
            let files = FakeFiles::default();
            let model = FakeModel::with(vec![call_reply(PUT_FILE, arguments), reply("done")]);
            let runtime = FakeRuntime::with_file("out/report.txt", b"hello");
            let mut recorder = TestRecorder::default();

            let outcome = run(
                &runtime,
                &model,
                &mut recorder,
                &files,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await
            .unwrap();

            assert!(outcome.succeeded);
            assert_eq!(
                *files.puts.lock().unwrap(),
                vec![(name.to_owned(), b"hello".to_vec())]
            );
            assert!(runtime.actions.lock().unwrap().is_empty());
            assert_eq!(recorder.finished[0].1, ActionStatus::Succeeded);
            assert_eq!(
                tool_texts(&model),
                [format!("delivered {name} as file {}", files.delivered)]
            );
        }
    }

    #[tokio::test]
    async fn file_tools_and_computer_tools_run_in_record_order() {
        let id = Id::new();
        let files = FakeFiles::with(vec![(id, taken_file("in.txt", b"in"))]);
        let model = FakeModel::with(vec![
            calls_reply(vec![
                call("call-1", TAKE_FILE, json!({"file": id})),
                call(
                    "call-2",
                    "write_file",
                    json!({"path": "note.txt", "content": "note"}),
                ),
                call("call-3", PUT_FILE, json!({"path": "in.txt"})),
            ]),
            reply("done"),
        ]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert_eq!(outcome.text, "done");
        assert_eq!(
            recorder.order,
            [
                "action_started",
                "action_finished",
                "action_started",
                "action_finished",
                "action_started",
                "action_finished",
                "step",
                "step",
            ]
        );
        assert_eq!(
            recorder
                .started
                .iter()
                .map(|(_, tool)| tool.as_str())
                .collect::<Vec<_>>(),
            [TAKE_FILE, "write_file", PUT_FILE]
        );
        assert_eq!(
            recorder
                .finished
                .iter()
                .map(|(_, status)| *status)
                .collect::<Vec<_>>(),
            [ActionStatus::Succeeded; 3]
        );
        // Only write_file runs through the Computer.
        assert_eq!(
            *runtime.actions.lock().unwrap(),
            [Action::WriteFile {
                path: "note.txt".to_owned(),
                content: "note".to_owned(),
            }]
        );
        // take_file wrote in.txt, and put_file delivered what it found there.
        assert_eq!(
            *files.puts.lock().unwrap(),
            [("in.txt".to_owned(), b"in".to_vec())]
        );
        assert_eq!(
            tool_texts(&model),
            [
                "took in.txt (2 bytes) as in.txt".to_owned(),
                "42".to_owned(),
                format!("delivered in.txt as file {}", files.delivered),
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_file_move_is_returned_to_the_model_and_the_run_goes_on() {
        let id = Id::new();
        let files = FakeFiles {
            refuse_put: true,
            ..FakeFiles::with(vec![(id, taken_file("in.txt", b"in"))])
        };
        let model = FakeModel::with(vec![
            calls_reply(vec![
                call("call-1", TAKE_FILE, json!({"file": id})),
                call("call-2", PUT_FILE, json!({"path": "missing.txt"})),
                call("call-3", PUT_FILE, json!({"path": "present.txt"})),
            ]),
            reply("done"),
        ]);
        let runtime = FakeRuntime {
            fail_writes: true,
            ..FakeRuntime::with_file("present.txt", b"present")
        };
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert_eq!(outcome.text, "done");
        assert_eq!(
            recorder
                .finished
                .iter()
                .map(|(_, status)| *status)
                .collect::<Vec<_>>(),
            [ActionStatus::Failed; 3]
        );
        let texts = tool_texts(&model);
        assert!(texts[0].contains("the workspace is full"), "{}", texts[0]);
        assert!(texts[1].contains("cannot read missing.txt"), "{}", texts[1]);
        assert!(texts[2].contains("cannot take this File"), "{}", texts[2]);
        assert!(files.puts.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn take_file_that_loses_its_computer_ends_the_run_unknown() {
        let id = Id::new();
        let files = FakeFiles::with(vec![(id, taken_file("report.txt", b"hello"))]);
        let model = FakeModel::with(vec![
            call_reply(TAKE_FILE, json!({"file": id})),
            reply("the Model must not be asked again"),
        ]);
        let runtime = FakeRuntime::lost();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert!(!outcome.succeeded);
        assert_eq!(
            outcome.text,
            "I lost my Computer before the action returned."
        );
        assert_eq!(recorder.finished[0].1, ActionStatus::Unknown);
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        assert!(runtime.files.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn put_file_that_loses_its_computer_ends_the_run_unknown() {
        let files = FakeFiles::default();
        let model = FakeModel::with(vec![
            call_reply(PUT_FILE, json!({"path": "out/report.txt"})),
            reply("the Model must not be asked again"),
        ]);
        let mut runtime = FakeRuntime::with_file("out/report.txt", b"hello");
        runtime.lost = true;
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert!(!outcome.succeeded);
        assert_eq!(
            outcome.text,
            "I lost my Computer before the action returned."
        );
        assert_eq!(recorder.finished[0].1, ActionStatus::Unknown);
        assert_eq!(model.requests.lock().unwrap().len(), 1);
        assert!(
            files.puts.lock().unwrap().is_empty(),
            "a lost transfer delivers nothing"
        );
    }

    #[tokio::test]
    async fn every_tool_call_in_a_step_runs_and_returns() {
        let calls = Ok(ModelReply {
            content: None,
            tool_calls: vec![
                ToolCall {
                    id: "call-1".to_owned(),
                    name: "write_file".to_owned(),
                    arguments: json!({"path": "a.txt", "content": "a"}).to_string(),
                },
                ToolCall {
                    id: "call-2".to_owned(),
                    name: "read_file".to_owned(),
                    arguments: json!({"path": "a.txt"}).to_string(),
                },
            ],
            usage: Usage {
                output: 2,
                ..Usage::default()
            },
        });
        let model = FakeModel::with(vec![calls, reply("done")]);
        let runtime = FakeRuntime::default();
        let files = FakeFiles::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert!(outcome.succeeded);
        assert_eq!(recorder.started.len(), 2);
        assert_eq!(recorder.finished.len(), 2);
        let requests = model.requests.lock().unwrap();
        let tool_messages = requests[1]
            .messages
            .iter()
            .filter(|message| message.role == crate::model::Role::Tool)
            .count();
        assert_eq!(tool_messages, 2);
        assert_eq!(
            requests[1].messages.last().unwrap().tool_call_id.as_deref(),
            Some("call-2")
        );
    }

    #[tokio::test]
    async fn an_unknown_tool_is_recorded_and_returned_to_the_model() {
        let model = FakeModel::with(vec![
            call_reply("no_such_tool", json!({"x": 1})),
            reply("the tool did not exist"),
        ]);
        let runtime = FakeRuntime::default();
        let files = FakeFiles::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert!(outcome.succeeded);
        assert_eq!(recorder.started[0].1, "no_such_tool");
        assert_eq!(recorder.finished[0].1, ActionStatus::Failed);
        let requests = model.requests.lock().unwrap();
        let tool_message = requests[1]
            .messages
            .iter()
            .find(|message| message.role == crate::model::Role::Tool)
            .unwrap();
        assert!(
            tool_message
                .content
                .as_deref()
                .unwrap()
                .contains("no tool named"),
            "{tool_message:?}"
        );
    }

    #[tokio::test]
    async fn bad_arguments_are_returned_to_the_model() {
        let mut reply_with_bad_arguments = call_reply("write_file", json!({}));
        if let Ok(reply) = &mut reply_with_bad_arguments {
            reply.tool_calls[0].arguments = "not json".to_owned();
        }
        let model = FakeModel::with(vec![reply_with_bad_arguments, reply("bad call")]);
        let runtime = FakeRuntime::default();
        let files = FakeFiles::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert!(outcome.succeeded);
        assert_eq!(recorder.finished[0].1, ActionStatus::Failed);
        let requests = model.requests.lock().unwrap();
        let tool_message = requests[1]
            .messages
            .iter()
            .find(|message| message.role == crate::model::Role::Tool)
            .unwrap();
        assert!(
            tool_message
                .content
                .as_deref()
                .unwrap()
                .contains("expected"),
            "{tool_message:?}"
        );
    }

    #[tokio::test]
    async fn a_model_failure_becomes_a_failure_message() {
        let model = FakeModel::with(vec![Err(ModelFailure {
            code: neha::error::ErrorCode::Unavailable,
            detail: "down".to_owned(),
            usage: Usage {
                uncached_input: 40,
                output: 12,
                ..Usage::default()
            },
        })]);
        let runtime = FakeRuntime::default();
        let files = FakeFiles::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert!(!outcome.succeeded);
        assert!(outcome.text.contains("down"));
        assert_eq!(outcome.usage.budgeted(), 52);
        assert!(recorder.steps.is_empty());
    }

    #[tokio::test]
    async fn the_step_limit_ends_a_run_that_keeps_calling_tools() {
        let model = FakeModel::with(vec![
            call_reply("list_files", json!({"path": "."})),
            call_reply("list_files", json!({"path": "."})),
        ]);
        let runtime = FakeRuntime::default();
        let files = FakeFiles::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100).with_max_steps(2),
        )
        .await
        .unwrap();

        assert!(!outcome.succeeded);
        assert!(outcome.text.contains("2 steps"));
        assert_eq!(recorder.steps.len(), 2);
        assert_eq!(recorder.started.len(), 2);
    }

    #[tokio::test]
    async fn a_recorder_failure_stops_the_loop() {
        let model = FakeModel::with(vec![reply("hello")]);
        let runtime = FakeRuntime::default();
        let files = FakeFiles::default();
        let mut recorder = TestRecorder {
            fail: true,
            ..TestRecorder::default()
        };

        let error = run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await;

        assert!(matches!(error, Err(LoopError::Record(_))));
    }

    #[test]
    fn a_history_message_keeps_its_role() {
        let message = Message::user("hello");

        assert_eq!(message.role, crate::model::Role::User);
        assert_eq!(message.content.as_deref(), Some("hello"));
    }

    #[test]
    fn a_run_can_be_spawned() {
        fn is_send<T: Send>(_: &T) {}
        let runtime = FakeRuntime::default();
        let files = FakeFiles::default();
        let model = FakeModel::with(Vec::new());
        let mut recorder = TestRecorder::default();

        is_send(&run(
            &runtime,
            &model,
            &mut recorder,
            &files,
            RunRequest::new("system", Vec::new(), 100),
        ));
    }
}

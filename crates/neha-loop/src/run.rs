//! The built-in Bot loop.
//!
//! A run repeats one step at a time: call the Model, record the step, run
//! every action the call requested through the Computer, record each action
//! before and after it runs, and feed the results back. The loop stops with
//! a final Message, a failure, or a step limit.

use std::future::Future;

use neha::id::Id;
use neha::usage::Usage;
use neha_computer::{ActionStatus, ComputerRuntime, ToolOutput};
use serde_json::Value;
use thiserror::Error;

use crate::model::{Message, Model, ModelFailure, ModelRequest, ToolCall};
use crate::tools;

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
pub async fn run<R, M, Rec>(
    runtime: &R,
    model: &M,
    recorder: &mut Rec,
    request: RunRequest,
) -> Result<RunOutcome, LoopError>
where
    R: ComputerRuntime + Sync,
    M: Model + Sync,
    Rec: Recorder + Send,
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
            let arguments = serde_json::from_str(&call.arguments)
                .unwrap_or_else(|_| Value::String(call.arguments.clone()));
            recorder
                .action_started(step, action, call.name.clone(), arguments)
                .await?;
            let result = match tools::action(&call.name, &call.arguments) {
                Ok(computer_action) => runtime.execute(computer_action).await,
                Err(error) => neha_computer::ActionResult::failed(error),
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

fn failure_outcome(failure: &ModelFailure, usage: Usage) -> RunOutcome {
    RunOutcome {
        text: format!("I could not answer: {}", failure.detail),
        usage,
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

fn tool_result_text(result: &neha_computer::ActionResult) -> String {
    match result.status {
        ActionStatus::Succeeded => result.output.text.clone(),
        ActionStatus::Failed | ActionStatus::TimedOut | ActionStatus::Unknown => {
            let reason = result
                .error
                .clone()
                .unwrap_or_else(|| "the action failed".to_owned());
            format!("{reason}\n{}", result.output.text)
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
    use neha_computer::{Action, ActionResult, ActionStatus, ComputerRuntime, ToolOutput};
    use serde_json::{Value, json};

    use super::{LoopError, Recorder, RunRequest, run};
    use crate::model::{Message, Model, ModelFailure, ModelReply, ModelRequest, ToolCall};

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

    #[derive(Default)]
    struct FakeRuntime {
        actions: Mutex<Vec<Action>>,
    }

    impl ComputerRuntime for FakeRuntime {
        #[allow(clippy::manual_async_fn)]
        fn execute(&self, action: Action) -> impl Future<Output = ActionResult> + Send {
            self.actions.lock().unwrap().push(action);
            async move { ActionResult::succeeded(ToolOutput::full("42")) }
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

    fn call_reply(name: &str, arguments: Value) -> Result<ModelReply, ModelFailure> {
        Ok(ModelReply {
            content: None,
            tool_calls: vec![ToolCall {
                id: "call-1".to_owned(),
                name: name.to_owned(),
                arguments: arguments.to_string(),
            }],
            usage: Usage {
                output: 2,
                ..Usage::default()
            },
        })
    }

    #[tokio::test]
    async fn a_final_answer_ends_the_run() {
        let model = FakeModel::with(vec![reply("hello")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
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
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
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
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
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
    async fn a_model_failure_becomes_a_failure_message() {
        let model = FakeModel::with(vec![Err(ModelFailure {
            code: neha::error::ErrorCode::Unavailable,
            detail: "down".to_owned(),
        })]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
            RunRequest::new("system", Vec::new(), 100),
        )
        .await
        .unwrap();

        assert!(!outcome.succeeded);
        assert!(outcome.text.contains("down"));
        assert!(recorder.steps.is_empty());
    }

    #[tokio::test]
    async fn the_step_limit_ends_a_run_that_keeps_calling_tools() {
        let model = FakeModel::with(vec![
            call_reply("list_files", json!({"path": "."})),
            call_reply("list_files", json!({"path": "."})),
        ]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let outcome = run(
            &runtime,
            &model,
            &mut recorder,
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
        let mut recorder = TestRecorder {
            fail: true,
            ..TestRecorder::default()
        };

        let error = run(
            &runtime,
            &model,
            &mut recorder,
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
}

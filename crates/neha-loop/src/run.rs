//! The built-in Bot loop.
//!
//! A run repeats one step at a time: call the Model, record the reply, run
//! every tool call the step requested, record each one before and after it
//! runs, and feed the results back. A tool call runs on the Computer. The
//! Space approves each action before it starts; a run whose action must wait
//! ends with [`RunOutcome::Waiting`] and resumes from the record.
//! The loop stops with a final Message, a failure, or a step limit.

use std::collections::HashSet;
use std::future::Future;

use neha::id::Id;
use neha::usage::Usage;
use neha_computer::{ActionResult, ActionStatus, ComputerRuntime, ToolOutput};
use serde_json::Value;
use thiserror::Error;

use crate::model::{Continuation, Message, Model, ModelFailure, ModelRequest, ToolCall, ToolSpec};
use crate::tools::{self, SpaceCall};

/// The largest raw tool-call arguments string a step event keeps whole.
const MAX_RECORDED_ARGUMENTS: usize = 128 * 1024;

/// What one run needs to start.
pub struct RunRequest {
    /// The system instruction.
    pub system: String,
    /// The history ends with the results of the [`Resume::done`] calls.
    pub history: Vec<Message>,
    /// The largest Model reply in tokens.
    pub max_output: u32,
    /// How much thought the Model should give each reply.
    pub thinking: Option<neha::surface::ThinkingLevel>,
    /// Provider-native state from the step before this request, when resuming.
    pub continuation: Option<Continuation>,
    /// How many steps the run may take.
    pub max_steps: usize,
    /// The step to finish first, when the run resumes after an approval or a
    /// recovery.
    pub resume: Option<Resume>,
    /// The step a run without a recorded reply starts at, when the Space
    /// recovers it after an interruption.
    pub start_step: u32,
    /// The tools the Model may call; the built-in tools unless the Space
    /// names others.
    pub tools: Vec<ToolSpec>,
}

impl RunRequest {
    /// Builds a request with the default step limit and the built-in tools.
    #[must_use]
    pub fn new(system: impl Into<String>, history: Vec<Message>, max_output: u32) -> Self {
        Self {
            system: system.into(),
            history,
            max_output,
            thinking: None,
            continuation: None,
            max_steps: 8,
            resume: None,
            start_step: 1,
            tools: tools::specs(),
        }
    }

    /// Names the tools the Model may call, in place of the built-in tools.
    #[must_use]
    pub fn with_tools(mut self, tools: Vec<ToolSpec>) -> Self {
        self.tools = tools;
        self
    }

    /// Changes the step limit.
    #[must_use]
    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps;
        self
    }

    /// Resumes the run from a step that waited for an approval.
    #[must_use]
    pub fn with_resume(mut self, resume: Resume) -> Self {
        self.resume = Some(resume);
        self
    }

    /// Starts a recovered run at a step when the record has no reply to
    /// replay; the step itself is safe to call again.
    #[must_use]
    pub fn with_start_step(mut self, step: u32) -> Self {
        self.start_step = step;
        self
    }

    /// Sets the thinking level every Model call in the run uses.
    #[must_use]
    pub fn with_thinking(mut self, thinking: Option<neha::surface::ThinkingLevel>) -> Self {
        self.thinking = thinking;
        self
    }

    /// Sets the provider-native state a recovered run continues from.
    #[must_use]
    pub fn with_continuation(mut self, continuation: Option<Continuation>) -> Self {
        self.continuation = continuation;
        self
    }
}

/// A step that waited for an approval and now continues.
#[derive(Clone, Debug, PartialEq)]
pub struct Resume {
    /// The step to continue.
    pub step: u32,
    /// The text the Model wrote with the step's calls.
    pub content: Option<String>,
    /// The step's calls as the record keeps them.
    pub calls: Vec<RecordedCall>,
    /// How many calls already have a recorded result, refused ones included.
    /// Their results are the last `done` Messages of the history; the call at
    /// `done` is the approved action.
    pub done: usize,
    /// The approved action's identifier, which the call at `done` keeps when it runs.
    pub approved_action: Option<Id>,
    /// The usage of the reply that made the step, which the run's usage starts from.
    pub usage: Usage,
    /// Whether every call of this step already has a recorded result. The
    /// loop then records the step and moves on without running an action.
    pub recovered: bool,
    /// Provider-native state the reply produced.
    pub continuation: Option<Continuation>,
}

/// A tool call as the record keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedCall {
    /// The identifier the call's tool result answers under; see [`call_id`].
    pub call: String,
    /// The tool's name.
    pub tool: String,
    /// The Model's arguments as JSON; a refused call too large to keep is
    /// summarized instead.
    pub arguments: Value,
    /// Why the loop refused the call before anything ran; `None` when the call can run.
    pub refused: Option<String>,
}

/// Whether an action may start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The action runs.
    Allow,
    /// The action waits for the User; the run ends with [`RunOutcome::Waiting`].
    Wait,
}

/// How a run ended.
#[derive(Clone, Debug, PartialEq)]
pub enum RunOutcome {
    /// The run reached a final Message or a failure.
    Reply {
        /// The final Message text.
        text: String,
        /// The usage of every step.
        usage: Usage,
        /// Whether the run reached a final answer.
        succeeded: bool,
    },
    /// An action waits for the User's approval.
    Waiting {
        /// The action the run waits for.
        approval: WaitingApproval,
    },
}

/// The action a run waits for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitingApproval {
    /// The step that holds the action.
    pub step: u32,
    /// The call's position among the step's calls, from zero.
    pub index: u32,
    /// The call's identifier, which its tool result answers under.
    pub call: String,
    /// The tool the Model called.
    pub tool: String,
    /// The call's arguments as the record keeps them.
    pub arguments: Value,
    /// The action's identifier, which the action keeps when it runs.
    pub action: Id,
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

    /// Records a Model reply before any of its calls runs.
    fn reply(
        &mut self,
        step: u32,
        content: Option<String>,
        calls: Vec<RecordedCall>,
        usage: Usage,
        continuation: Option<Continuation>,
    ) -> impl Future<Output = Result<(), LoopError>> + Send;

    /// Asks whether one action may start under `action`. A call the loop
    /// refused runs nothing and is not asked; every call that can run asks, a
    /// File move included. An action the User already answered is allowed
    /// under its recorded identifier without a second card.
    fn approve(
        &mut self,
        step: u32,
        call: &RecordedCall,
        action: Id,
    ) -> impl Future<Output = Result<Authorization, LoopError>> + Send;

    /// Returns the script identity of an action the User already answered.
    ///
    /// The action runs under its recorded identifier, so the Space resolves
    /// the identity from the record instead of asking again.
    fn authorized_action(
        &mut self,
        action: Id,
        call: &RecordedCall,
    ) -> impl Future<Output = Result<Dispatched, LoopError>> + Send;

    /// Records an action before it runs, with the identities the Space
    /// resolved for it.
    fn action_started(
        &mut self,
        step: u32,
        action: Id,
        tool: String,
        arguments: Value,
        dispatched: &Dispatched,
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

/// The skill and script one action runs, resolved before the action starts.
///
/// A record names a script's skill by its identifier and the script's bytes by
/// their digest, so a replay credits the exact code that ran and never a
/// replacement under the same names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptIdentity {
    /// The installed skill.
    pub skill: Id,
    /// The script's file name within the skill.
    pub name: String,
    /// The digest of the script's bytes.
    pub sha256: String,
}

/// The identity a server-given tool call runs under.
///
/// A record names such a call by its server's id and the tool's original name,
/// never by the name the Model sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpIdentity {
    /// The server.
    pub server: Id,
    /// The tool's original name, as the server spells it.
    pub tool: String,
}

/// What the Space resolved for one action before it starts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Dispatched {
    /// The skill and script the action runs, when it runs one.
    pub script: Option<ScriptIdentity>,
    /// The server and tool the action calls, when it calls one.
    pub mcp: Option<McpIdentity>,
}

/// What the Space decided about one action before it starts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Authorization {
    /// The action may start, with the identities the Space resolved for it.
    Allow(Dispatched),
    /// The User must answer first.
    Wait(Dispatched),
    /// The action cannot run at all, and its failed result says why.
    Refuse(String),
}

/// What a run works with: its Computer, and the tools the Space serves itself.
pub trait Runtime: ComputerRuntime {
    /// Serves one tool call the Space runs itself: a skill's body, or a
    /// script it delivers to the Computer. The action is the identity the
    /// record gave it, so the Space can check what the answer covered. A
    /// refusal is a failed action, never a lost one.
    fn serve(
        &self,
        action: Id,
        call: SpaceCall,
    ) -> impl Future<Output = Result<ActionResult, LoopError>> + Send;
}

/// Runs one Bot run to its end, or until an action waits for an approval.
pub async fn run<R, M, Rec>(
    runtime: &R,
    model: &M,
    recorder: &mut Rec,
    request: RunRequest,
) -> Result<RunOutcome, LoopError>
where
    R: Runtime + Sync,
    M: Model + Sync,
    Rec: Recorder + Send,
{
    let RunRequest {
        system,
        history,
        max_output,
        thinking,
        continuation,
        max_steps,
        resume,
        start_step,
        tools,
    } = request;
    let mut messages = Vec::with_capacity(history.len() + 8);
    messages.push(Message::system(system));
    messages.extend(history);
    let max_steps = max_steps.min(u32::MAX as usize) as u32;
    // A resumed run finishes the step that waited before it calls the Model,
    // and its usage starts from that step's reply.
    let mut resume = resume;
    let mut continuation = resume
        .as_ref()
        .and_then(|resume| resume.continuation.clone())
        .or(continuation);
    let first_step = resume.as_ref().map_or(start_step, |resume| resume.step);
    let mut usage = resume
        .as_ref()
        .map_or(Usage::default(), |resume| resume.usage);

    for step in first_step..=max_steps {
        let (calls, reply_usage, from, approved, all_done) = match resume.take() {
            // The reply was recorded before the run waited, so it is not
            // recorded again; its done calls already have their results.
            Some(resume) => {
                let Resume {
                    content,
                    calls,
                    done,
                    approved_action,
                    usage,
                    recovered,
                    continuation: step_continuation,
                    ..
                } = resume;
                continuation = step_continuation.or(continuation);
                let calls = resumed_calls(calls, &tools)?;
                // The results of the done calls end the history, and the
                // reply's calls come before them.
                let at = messages.len() - done.min(messages.len() - 1);
                messages.insert(
                    at,
                    Message::assistant_with_calls(content, model_calls(&calls)),
                );
                let all_done = recovered && done >= calls.len();
                (calls, usage, done, approved_action, all_done)
            }
            None => {
                let reply = match model
                    .complete(ModelRequest {
                        messages: messages.clone(),
                        tools: tools.clone(),
                        max_output,
                        thinking,
                        continuation: continuation.clone(),
                    })
                    .await
                {
                    Ok(reply) => reply,
                    Err(failure) => {
                        return Ok(failure_outcome(&failure, usage));
                    }
                };
                usage += reply.usage;
                if reply.tool_calls.is_empty() {
                    // A step with no actions completes when the Model answers,
                    // but its reply still belongs in the record.
                    recorder
                        .reply(
                            step,
                            reply.content.clone(),
                            Vec::new(),
                            reply.usage,
                            reply.continuation.clone(),
                        )
                        .await?;
                    recorder.step(step, reply.usage, 0).await?;
                    return Ok(final_reply(reply.content, usage));
                }
                let calls = step_calls(step, &reply.tool_calls, &tools)?;
                recorder
                    .reply(
                        step,
                        reply.content.clone(),
                        calls.iter().map(|call| call.recorded.clone()).collect(),
                        reply.usage,
                        reply.continuation.clone(),
                    )
                    .await?;
                continuation = reply.continuation;
                messages.push(Message::assistant_with_calls(
                    reply.content,
                    reply.tool_calls,
                ));
                (calls, reply.usage, 0, None, false)
            }
        };
        let tool_calls = calls.len() as u32;
        // A recovered step whose calls all have their recorded results has no
        // action left to run; record the step and continue with the next one.
        if !(all_done && from >= calls.len()) {
            for (index, call) in calls.into_iter().enumerate().skip(from) {
                // The action the User approved runs under its recorded identifier
                // and is not asked for again, even when a tool it needed is no
                // longer offered: the card, the action's start, and its result
                // must name one action. A call the record itself refused was
                // never approved, so it takes no identifier.
                let action = if index == from && call.recorded.refused.is_none() {
                    approved
                } else {
                    None
                };
                let approved_here = action.is_some();
                let action = action.unwrap_or_else(Id::new);
                // A refused call runs nothing and asks nobody; every call that
                // can run asks, a File move included. An action the User
                // already answered runs under its identifier without a second
                // card, and the Space resolves the identity it recorded, but
                // only while the run still offers its tool.
                let mut dispatched = Dispatched::default();
                let mut refused = None;
                if approved_here {
                    // The answer is a decision about this call; a tool the run
                    // no longer offers cannot run under it, and recording a
                    // refused call credits no first run.
                    if tools.iter().any(|tool| tool.name == call.recorded.tool) {
                        dispatched = recorder.authorized_action(action, &call.recorded).await?;
                    } else {
                        refused = Some(format!(
                            "the tool {:?} is not offered to this run",
                            call.recorded.tool
                        ));
                    }
                } else if call.parsed.is_ok() {
                    match recorder.approve(step, &call.recorded, action).await? {
                        Authorization::Wait(_) => {
                            let recorded = call.recorded;
                            return Ok(RunOutcome::Waiting {
                                approval: WaitingApproval {
                                    step,
                                    index: index as u32,
                                    call: recorded.call,
                                    tool: recorded.tool,
                                    arguments: recorded.arguments,
                                    action,
                                },
                            });
                        }
                        Authorization::Refuse(reason) => refused = Some(reason),
                        Authorization::Allow(identity) => dispatched = identity,
                    }
                }
                recorder
                    .action_started(
                        step,
                        action,
                        call.recorded.tool.clone(),
                        call.recorded.arguments.clone(),
                        &dispatched,
                    )
                    .await?;
                // An answer names one kind of action. If the record's call is
                // not that kind, nothing runs: a built-in tool must never run
                // under a server's or a script's answer.
                let mismatch = match (dispatched.script.is_some(), dispatched.mcp.is_some()) {
                    (true, _) => !matches!(
                        call.parsed,
                        Ok(tools::Call::Space(tools::SpaceCall::Script { .. }))
                    ),
                    (_, true) => !matches!(
                        call.parsed,
                        Ok(tools::Call::Space(tools::SpaceCall::Mcp { .. }))
                    ),
                    (false, false) => false,
                };
                let result = match (refused, mismatch, call.parsed) {
                    (Some(reason), _, _) => ActionResult::failed(reason),
                    (None, true, _) => ActionResult::failed(
                        "the recorded action is not the one the answer names, so it did not run",
                    ),
                    (None, false, Ok(tools::Call::Computer(computer_action))) => {
                        runtime.execute(computer_action).await
                    }
                    (None, false, Ok(tools::Call::Space(space_call))) => {
                        runtime.serve(action, space_call).await?
                    }
                    (None, false, Err(error)) => ActionResult::failed(error),
                };
                recorder
                    .action_finished(
                        step,
                        action,
                        call.recorded.tool,
                        result.status,
                        result.output.clone(),
                        result.error.clone(),
                    )
                    .await?;
                messages.push(Message::tool_result(
                    call.recorded.call,
                    tool_result_text(&result),
                ));
                if result.status == ActionStatus::Unknown {
                    // The action's result never returned. The record says unknown,
                    // and the run ends here instead of resuming it blindly.
                    return Ok(RunOutcome::Reply {
                        text: "I lost my Computer before the action returned.".to_owned(),
                        usage,
                        succeeded: false,
                    });
                }
            }
        }
        // A step completes only after all of its actions were recorded.
        recorder.step(step, reply_usage, tool_calls).await?;
    }

    Ok(RunOutcome::Reply {
        text: format!("I stopped after {max_steps} steps without a final answer."),
        usage,
        succeeded: false,
    })
}

/// One call of a step: whether it can run, and what the record keeps of it.
struct StepCall {
    /// `Err` carries the refusal the Model is told; a resumed step keeps the
    /// one the record has.
    parsed: Result<tools::Call, String>,
    recorded: RecordedCall,
}

/// The calls of a Model reply as the step runs them.
fn step_calls(
    step: u32,
    calls: &[ToolCall],
    offered: &[ToolSpec],
) -> Result<Vec<StepCall>, LoopError> {
    let calls: Vec<StepCall> = calls
        .iter()
        .enumerate()
        .map(|(index, call)| {
            // A Model cannot call a tool the run did not offer it, whatever
            // its reply names: the offer is the Space's decision. An unknown
            // name keeps the parser's own refusal.
            let parsed = match tools::parse(&call.name, &call.arguments) {
                Ok(_) if !offered.iter().any(|tool| tool.name == call.name) => Err(format!(
                    "the tool {:?} is not offered to this run",
                    call.name
                )),
                parsed => parsed,
            };
            StepCall {
                recorded: RecordedCall {
                    call: call_id(&call.id, step, index),
                    tool: call.name.clone(),
                    arguments: recorded_arguments(&parsed, &call.arguments),
                    refused: parsed.as_ref().err().cloned(),
                },
                parsed,
            }
        })
        .collect();
    distinct_ids(&calls)?;
    Ok(calls)
}

/// The recorded calls of a resumed step as the step runs them.
fn resumed_calls(
    calls: Vec<RecordedCall>,
    offered: &[ToolSpec],
) -> Result<Vec<StepCall>, LoopError> {
    let calls: Vec<StepCall> = calls
        .into_iter()
        .map(|recorded| {
            // A refused call stays refused. The record keeps its arguments as
            // JSON but not what made the Model's text fail to parse, so
            // parsing them again could let the call run.
            let parsed = match &recorded.refused {
                Some(refusal) => Err(refusal.clone()),
                // A tool the run offers no longer cannot run, whatever the
                // record of the reply says: the Space's decision now wins.
                None if !offered.iter().any(|tool| tool.name == recorded.tool) => Err(format!(
                    "the tool {:?} is not offered to this run",
                    recorded.tool
                )),
                None => tools::parse(&recorded.tool, &recorded.arguments.to_string()),
            };
            StepCall { parsed, recorded }
        })
        .collect();
    distinct_ids(&calls)?;
    Ok(calls)
}

/// Two calls of one step must not answer under one identifier, or nobody
/// could tell which result is whose once the run resumes.
fn distinct_ids(calls: &[StepCall]) -> Result<(), LoopError> {
    let mut seen = HashSet::new();
    for call in calls {
        if !seen.insert(call.recorded.call.as_str()) {
            return Err(LoopError::RepeatedCall(call.recorded.call.clone()));
        }
    }
    Ok(())
}

/// The calls of a resumed step as the Model wrote them.
fn model_calls(calls: &[StepCall]) -> Vec<ToolCall> {
    calls
        .iter()
        .map(|call| ToolCall {
            id: call.recorded.call.clone(),
            name: call.recorded.tool.clone(),
            arguments: call.recorded.arguments.to_string(),
        })
        .collect()
}

/// Returns the identifier a tool result answers under.
///
/// The Model's own identifier when it gave one, and `call-<step>-<index>`
/// otherwise. The rule depends on nothing else, so the Space rebuilds a
/// run's Messages from the record with the same identifiers.
#[must_use]
pub fn call_id(id: &str, step: u32, index: usize) -> String {
    if id.is_empty() {
        format!("call-{step}-{index}")
    } else {
        id.to_owned()
    }
}

/// What the record keeps of a call's arguments.
fn recorded_arguments(parsed: &Result<tools::Call, String>, raw: &str) -> Value {
    // A call that will run keeps its real arguments in the record.
    // Only a call that is refused may be summarized.
    match (parsed, raw.len() > MAX_RECORDED_ARGUMENTS) {
        (Ok(_), _) => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned())),
        (Err(_), true) => serde_json::json!({"truncated": true, "bytes": raw.len()}),
        (Err(_), false) => {
            serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
        }
    }
}

/// The outcome of a final reply.
fn final_reply(content: Option<String>, usage: Usage) -> RunOutcome {
    let text = content.unwrap_or_default();
    if text.trim().is_empty() {
        RunOutcome::Reply {
            text: "The Model returned an empty reply.".to_owned(),
            usage,
            succeeded: false,
        }
    } else {
        RunOutcome::Reply {
            text,
            usage,
            succeeded: true,
        }
    }
}

fn failure_outcome(failure: &ModelFailure, usage: Usage) -> RunOutcome {
    RunOutcome::Reply {
        text: format!("I could not answer: {}", failure.detail),
        usage: usage + failure.usage,
        succeeded: false,
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

/// Why a run cannot continue.
#[derive(Debug, Error)]
pub enum LoopError {
    /// The record could not hold an event.
    #[error("the record failed: {0}")]
    Record(String),
    /// Two calls of a step answer under one tool-call identifier.
    #[error("two calls of the step share the tool-call identifier {0}")]
    RepeatedCall(String),
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

    use super::{
        Authorization, Dispatched, LoopError, RecordedCall, Recorder, Resume, RunOutcome,
        RunRequest, Runtime, WaitingApproval, call_id, run,
    };
    use crate::model::{
        Continuation, Message, Model, ModelFailure, ModelReply, ModelRequest, Role, ToolCall,
        ToolSpec,
    };
    use crate::tools::SpaceCall;

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
        served: Mutex<Vec<SpaceCall>>,
        fail_writes: bool,
        lost: bool,
        slow_writes: bool,
    }

    impl FakeRuntime {
        /// A Computer that goes away before it answers.
        fn lost() -> Self {
            Self {
                lost: true,
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

    impl Runtime for FakeRuntime {
        #[allow(clippy::manual_async_fn)]
        fn serve(
            &self,
            _action: Id,
            call: SpaceCall,
        ) -> impl Future<Output = Result<ActionResult, LoopError>> + Send {
            self.served.lock().unwrap().push(call);
            async move { Ok(ActionResult::succeeded(ToolOutput::full("the skill body"))) }
        }
    }

    /// The record, in the order the loop wrote it.
    #[derive(Default)]
    struct TestRecorder {
        steps: Vec<(u32, Usage, u32)>,
        replies: Vec<(u32, Option<String>, Vec<RecordedCall>, Usage)>,
        approvals: Vec<(u32, String, Id)>,
        /// The answers `approve` returns in order; Allow once it runs out.
        decisions: VecDeque<Authorization>,
        started: Vec<(u32, String, Id)>,
        finished: Vec<(u32, ActionStatus)>,
        /// The method that fails, named by its entry in `order`.
        fail_at: Option<&'static str>,
        order: Vec<&'static str>,
    }

    impl TestRecorder {
        /// A record that refuses the first action it is asked about.
        fn that_waits() -> Self {
            Self {
                decisions: VecDeque::from([Authorization::Wait(Dispatched::default())]),
                ..Self::default()
            }
        }

        fn record(&mut self, entry: &'static str) -> Result<(), LoopError> {
            self.order.push(entry);
            if self.fail_at == Some(entry) {
                return Err(LoopError::Record("no".to_owned()));
            }
            Ok(())
        }
    }

    impl Recorder for TestRecorder {
        async fn step(&mut self, step: u32, usage: Usage, calls: u32) -> Result<(), LoopError> {
            self.record("step")?;
            self.steps.push((step, usage, calls));
            Ok(())
        }

        async fn reply(
            &mut self,
            step: u32,
            content: Option<String>,
            calls: Vec<RecordedCall>,
            usage: Usage,
            _continuation: Option<Continuation>,
        ) -> Result<(), LoopError> {
            self.record("reply")?;
            self.replies.push((step, content, calls, usage));
            Ok(())
        }

        async fn approve(
            &mut self,
            step: u32,
            call: &RecordedCall,
            action: Id,
        ) -> Result<Authorization, LoopError> {
            self.record("approve")?;
            self.approvals.push((step, call.tool.clone(), action));
            Ok(self
                .decisions
                .pop_front()
                .unwrap_or(Authorization::Allow(Dispatched::default())))
        }

        async fn authorized_action(
            &mut self,
            _action: Id,
            _call: &RecordedCall,
        ) -> Result<Dispatched, LoopError> {
            Ok(Dispatched::default())
        }

        async fn action_started(
            &mut self,
            step: u32,
            action: Id,
            tool: String,
            _arguments: Value,
            _dispatched: &Dispatched,
        ) -> Result<(), LoopError> {
            self.record("action_started")?;
            self.started.push((step, tool, action));
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
            self.record("action_finished")?;
            self.finished.push((step, status));
            Ok(())
        }
    }

    /// The reply of a run that did not wait.
    fn reply_outcome(outcome: Result<RunOutcome, LoopError>) -> (String, Usage, bool) {
        match outcome.unwrap() {
            RunOutcome::Reply {
                text,
                usage,
                succeeded,
            } => (text, usage, succeeded),
            RunOutcome::Waiting { approval } => panic!("the run waited for {approval:?}"),
        }
    }

    /// The action a run waited for.
    fn waiting(outcome: Result<RunOutcome, LoopError>) -> WaitingApproval {
        match outcome.unwrap() {
            RunOutcome::Waiting { approval } => approval,
            RunOutcome::Reply { text, .. } => panic!("the run replied {text:?}"),
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
            continuation: None,
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
            continuation: None,
        })
    }

    fn call_reply(name: &str, arguments: Value) -> Result<ModelReply, ModelFailure> {
        calls_reply(vec![call("call-1", name, arguments)])
    }

    fn recorded(id: &str, tool: &str, arguments: Value) -> RecordedCall {
        RecordedCall {
            call: id.to_owned(),
            tool: tool.to_owned(),
            arguments,
            refused: None,
        }
    }

    /// Resumes the second step, which waited on the call after `done`.
    fn resume(calls: Vec<RecordedCall>, done: usize, approved_action: Id) -> Resume {
        Resume {
            step: 2,
            content: None,
            calls,
            done,
            approved_action: Some(approved_action),
            usage: Usage {
                uncached_input: 3,
                output: 7,
                ..Usage::default()
            },
            recovered: false,
            continuation: None,
        }
    }

    /// The `(tool_call_id, content)` of every tool Message in a request.
    fn tool_messages(request: &ModelRequest) -> Vec<(String, String)> {
        request
            .messages
            .iter()
            .filter(|message| message.role == Role::Tool)
            .map(|message| {
                (
                    message.tool_call_id.clone().unwrap(),
                    message.content.clone().unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn a_final_answer_ends_the_run() {
        let model = FakeModel::with(vec![reply("hello")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let (text, usage, succeeded) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert_eq!(text, "hello");
        assert!(succeeded);
        assert_eq!(usage.output, 1);
        assert_eq!(recorder.steps.len(), 1);
        assert!(recorder.started.is_empty());
        assert_eq!(
            recorder.replies,
            [(
                1,
                Some("hello".to_owned()),
                Vec::new(),
                Usage {
                    output: 1,
                    ..Usage::default()
                }
            )]
        );
        assert!(recorder.approvals.is_empty());
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

        let (text, _, succeeded) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert_eq!(text, "done");
        assert!(succeeded);
        assert_eq!(recorder.started.len(), 1);
        assert_eq!(recorder.started[0].1, "write_file");
        assert_eq!(recorder.finished[0].1, ActionStatus::Succeeded);
        assert_eq!(
            recorder.order,
            [
                "reply",
                "approve",
                "action_started",
                "action_finished",
                "step",
                "reply",
                "step",
            ]
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
    async fn a_space_call_is_served_by_the_space_and_returns_to_the_model() {
        let model = FakeModel::with(vec![
            call_reply("skill", json!({"name": "weekly-report"})),
            call_reply(
                "run_skill_script",
                json!({"skill": "weekly-report", "script": "collect.sh", "arguments": ["q4"]}),
            ),
            reply("done"),
        ]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let (text, _, succeeded) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert_eq!(text, "done");
        assert!(succeeded);
        assert_eq!(
            recorder
                .started
                .iter()
                .map(|(_, tool, _)| tool.as_str())
                .collect::<Vec<_>>(),
            ["skill", "run_skill_script"]
        );
        assert!(
            recorder
                .finished
                .iter()
                .all(|(_, status)| *status == ActionStatus::Succeeded)
        );
        // The Computer ran nothing: both calls went to the Space.
        assert!(runtime.actions.lock().unwrap().is_empty());
        let served = runtime.served.lock().unwrap();
        assert_eq!(
            served.as_slice(),
            [
                SpaceCall::Skill {
                    name: "weekly-report".to_owned(),
                },
                SpaceCall::Script {
                    skill: "weekly-report".to_owned(),
                    script: "collect.sh".to_owned(),
                    arguments: vec!["q4".to_owned()],
                },
            ]
        );
        let requests = model.requests.lock().unwrap();
        for request in &requests[..2] {
            let names: Vec<&str> = request
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect();
            assert!(names.contains(&"skill") && names.contains(&"run_skill_script"));
        }
        assert_eq!(
            requests[2].messages.last().unwrap().content.as_deref(),
            Some("the skill body")
        );
    }

    #[tokio::test]
    async fn the_space_names_the_tools_the_model_sees() {
        let model = FakeModel::with(vec![reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();
        let extra = ToolSpec {
            name: "mcp__docs__search".to_owned(),
            description: "Search the docs.".to_owned(),
            parameters: json!({"type": "object"}),
        };

        let _ = run(
            &runtime,
            &model,
            &mut recorder,
            RunRequest::new("system", Vec::new(), 100).with_tools(vec![extra]),
        )
        .await;

        let requests = model.requests.lock().unwrap();
        let names: Vec<&str> = requests[0]
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(names, ["mcp__docs__search"]);
    }

    #[tokio::test]
    async fn an_action_that_waits_is_not_started() {
        let model = FakeModel::with(vec![call_reply(
            "shell",
            json!({"command": "rm -rf build"}),
        )]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::that_waits();

        let approval = waiting(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert_eq!(
            approval,
            WaitingApproval {
                step: 1,
                index: 0,
                call: "call-1".to_owned(),
                tool: "shell".to_owned(),
                arguments: json!({"command": "rm -rf build"}),
                action: recorder.approvals[0].2,
            }
        );
        assert_eq!(recorder.order, ["reply", "approve"]);
        assert!(recorder.started.is_empty());
        assert!(recorder.steps.is_empty(), "the step did not complete");
        assert!(runtime.actions.lock().unwrap().is_empty());
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_reply_continuation_reaches_the_next_step() {
        let continuation = Continuation {
            protocol: "anthropic_messages".to_owned(),
            provider: "provider".to_owned(),
            model: "model".to_owned(),
            state: json!({"blocks": [{"type": "thinking", "signature": "sig"}]}),
        };
        let model = FakeModel::with(vec![
            Ok(ModelReply {
                content: Some("let me look".to_owned()),
                tool_calls: vec![call("call-7", "read_file", json!({"path": "a.txt"}))],
                usage: Usage::default(),
                continuation: Some(continuation.clone()),
            }),
            reply("done"),
        ]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let (text, _, _) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert_eq!(text, "done");
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].continuation, None);
        assert_eq!(requests[1].continuation, Some(continuation));
    }

    #[tokio::test]
    async fn a_reply_is_recorded_before_its_actions_are_approved() {
        let model = FakeModel::with(vec![
            Ok(ModelReply {
                content: Some("let me look".to_owned()),
                tool_calls: vec![call("call-7", "read_file", json!({"path": "a.txt"}))],
                usage: Usage {
                    output: 3,
                    ..Usage::default()
                },
                continuation: None,
            }),
            reply("done"),
        ]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let (text, _, _) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert_eq!(text, "done");
        assert_eq!(
            recorder.replies,
            [
                (
                    1,
                    Some("let me look".to_owned()),
                    vec![recorded("call-7", "read_file", json!({"path": "a.txt"}))],
                    Usage {
                        output: 3,
                        ..Usage::default()
                    },
                ),
                (
                    2,
                    Some("done".to_owned()),
                    Vec::new(),
                    Usage {
                        output: 1,
                        ..Usage::default()
                    },
                )
            ]
        );
        // The action the Space approves is the one that starts.
        assert_eq!(recorder.approvals.len(), 1);
        assert_eq!(
            recorder.approvals[0],
            (1, "read_file".to_owned(), recorder.started[0].2)
        );
    }

    #[tokio::test]
    async fn a_record_that_cannot_hold_an_approval_stops_the_loop() {
        let model = FakeModel::with(vec![call_reply("shell", json!({"command": "ls"}))]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder {
            fail_at: Some("approve"),
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
        assert_eq!(recorder.order, ["reply", "approve"]);
        assert!(recorder.started.is_empty());
        assert!(runtime.actions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_resumed_step_runs_the_approved_action_under_its_id() {
        let approved_action = Id::new();
        let calls = vec![
            recorded(
                "call-1",
                "write_file",
                json!({"path": "a.txt", "content": "a"}),
            ),
            recorded("call-2", "read_file", json!({"path": "a.txt"})),
            recorded("call-3", "list_files", json!({"path": "."})),
        ];
        // The Space rebuilt the first call's result at the end of the history.
        let history = vec![Message::tool_result("call-1", "wrote a.txt")];
        let model = FakeModel::with(vec![reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let (text, usage, succeeded) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", history, 100).with_resume(resume(
                    calls,
                    1,
                    approved_action,
                )),
            )
            .await,
        );

        assert_eq!(text, "done");
        assert!(succeeded);
        assert_eq!(
            recorder.replies,
            [(
                3,
                Some("done".to_owned()),
                Vec::new(),
                Usage {
                    output: 1,
                    ..Usage::default()
                }
            )],
            "the resumed step's final reply is recorded"
        );
        // The approved action keeps its id and is not asked for again.
        assert_eq!(
            recorder.started[0],
            (2, "read_file".to_owned(), approved_action)
        );
        assert_eq!(recorder.finished[0].1, ActionStatus::Succeeded);
        // A later action asks, and runs under the id it was asked with.
        assert_eq!(recorder.approvals.len(), 1);
        assert_eq!(recorder.approvals[0].1, "list_files");
        assert_eq!(recorder.started[1].2, recorder.approvals[0].2);
        assert_eq!(
            recorder.order,
            [
                "action_started",
                "action_finished",
                "approve",
                "action_started",
                "action_finished",
                "step",
                "reply",
                "step",
            ]
        );
        // Step numbering continues from the resumed step.
        assert_eq!(
            recorder.steps,
            [
                (2, resume(Vec::new(), 0, approved_action).usage, 3),
                (3, reply("done").unwrap().usage, 0),
            ]
        );
        assert_eq!(
            usage,
            resume(Vec::new(), 0, approved_action).usage + reply("done").unwrap().usage
        );
        assert_eq!(
            *runtime.actions.lock().unwrap(),
            [
                Action::ReadFile {
                    path: "a.txt".to_owned(),
                },
                Action::ListFiles {
                    path: ".".to_owned(),
                },
            ]
        );
        // The Model sees the reply's calls, then every result in order.
        let requests = model.requests.lock().unwrap();
        let messages = &requests[0].messages;
        assert_eq!(
            messages[1].tool_calls,
            [
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
                ToolCall {
                    id: "call-3".to_owned(),
                    name: "list_files".to_owned(),
                    arguments: json!({"path": "."}).to_string(),
                },
            ]
        );
        assert_eq!(
            tool_messages(&requests[0]),
            [
                ("call-1".to_owned(), "wrote a.txt".to_owned()),
                ("call-2".to_owned(), "42".to_owned()),
                ("call-3".to_owned(), "42".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn a_later_call_of_a_resumed_step_asks_again() {
        let approved_action = Id::new();
        let calls = vec![
            recorded(
                "call-1",
                "write_file",
                json!({"path": "a.txt", "content": "a"}),
            ),
            recorded("call-2", "shell", json!({"command": "rm -rf build"})),
        ];
        let model = FakeModel::with(Vec::new());
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::that_waits();

        let approval = waiting(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100).with_resume(resume(
                    calls,
                    0,
                    approved_action,
                )),
            )
            .await,
        );

        assert_eq!(
            approval,
            WaitingApproval {
                step: 2,
                index: 1,
                call: "call-2".to_owned(),
                tool: "shell".to_owned(),
                arguments: json!({"command": "rm -rf build"}),
                action: recorder.approvals[0].2,
            }
        );
        assert_ne!(approval.action, approved_action);
        assert_eq!(
            recorder.started,
            [(2, "write_file".to_owned(), approved_action)]
        );
        assert_eq!(recorder.finished.len(), 1);
        assert_eq!(
            recorder.order,
            ["action_started", "action_finished", "approve"]
        );
        assert!(recorder.steps.is_empty(), "the step did not complete");
        assert!(model.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_refused_call_stays_refused_after_a_resume() {
        // The Model repeated a field. The strict parse refuses the call, but
        // the JSON the record keeps has lost the duplicate and would parse.
        let raw = r#"{"path":"b.txt","content":"a","content":"b"}"#;
        let refusal = crate::tools::parse("write_file", raw).unwrap_err();
        let model = FakeModel::with(vec![calls_reply(vec![
            call("call-1", "read_file", json!({"path": "a.txt"})),
            ToolCall {
                id: "call-2".to_owned(),
                name: "write_file".to_owned(),
                arguments: raw.to_owned(),
            },
        ])]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::that_waits();

        let approval = waiting(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert_eq!(approval.index, 0);
        let calls = recorder.replies[0].2.clone();
        assert_eq!(calls[1].refused, Some(refusal.clone()));
        assert_eq!(calls[1].arguments, json!({"path": "b.txt", "content": "b"}));

        // The User allowed the first call; the run resumes with both to do.
        let model = FakeModel::with(vec![reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let (text, _, _) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100).with_resume(resume(
                    calls,
                    0,
                    approval.action,
                )),
            )
            .await,
        );

        assert_eq!(text, "done");
        assert_eq!(
            recorder.started[0],
            (2, "read_file".to_owned(), approval.action)
        );
        // The refused call is neither asked nor run, and fails as it did.
        assert_eq!(recorder.started[1].1, "write_file");
        assert_ne!(recorder.started[1].2, approval.action);
        assert!(recorder.approvals.is_empty());
        assert_eq!(recorder.finished[1].1, ActionStatus::Failed);
        assert_eq!(
            *runtime.actions.lock().unwrap(),
            [Action::ReadFile {
                path: "a.txt".to_owned(),
            }]
        );
        assert_eq!(
            tool_messages(&model.requests.lock().unwrap()[0]),
            [
                ("call-1".to_owned(), "42".to_owned()),
                ("call-2".to_owned(), refusal),
            ]
        );
    }

    #[tokio::test]
    async fn a_refused_call_never_takes_the_approved_action() {
        // A refused call cannot have been approved; a resume that says so
        // still runs nothing and leaves the approved identifier unused.
        let approved_action = Id::new();
        let refusal = "the content is larger than 65536 bytes";
        let calls = vec![RecordedCall {
            refused: Some(refusal.to_owned()),
            ..recorded(
                "call-1",
                "write_file",
                json!({"truncated": true, "bytes": 200_000}),
            )
        }];
        let model = FakeModel::with(vec![reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let (text, _, _) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100).with_resume(resume(
                    calls,
                    0,
                    approved_action,
                )),
            )
            .await,
        );

        assert_eq!(text, "done");
        assert_eq!(recorder.started.len(), 1);
        assert_eq!(recorder.started[0].1, "write_file");
        assert_ne!(recorder.started[0].2, approved_action);
        assert!(recorder.approvals.is_empty());
        assert_eq!(recorder.finished[0].1, ActionStatus::Failed);
        assert!(runtime.actions.lock().unwrap().is_empty());
        assert_eq!(
            tool_messages(&model.requests.lock().unwrap()[0]),
            [("call-1".to_owned(), refusal.to_owned())]
        );
    }

    #[tokio::test]
    async fn a_reply_that_repeats_a_call_identifier_fails_the_run() {
        // Two calls under one identifier, and a Model identifier that equals
        // the one the loop gives a call the Model left without one.
        for (ids, repeated) in [
            (["call-1", "call-1"], "call-1"),
            (["call-1-1", ""], "call-1-1"),
        ] {
            let model = FakeModel::with(vec![calls_reply(vec![
                call(ids[0], "read_file", json!({"path": "a.txt"})),
                call(ids[1], "read_file", json!({"path": "b.txt"})),
            ])]);
            let runtime = FakeRuntime::default();
            let mut recorder = TestRecorder::default();

            let error = run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await;

            assert!(
                matches!(&error, Err(LoopError::RepeatedCall(id)) if id == repeated),
                "{ids:?}: {error:?}"
            );
            assert!(recorder.order.is_empty(), "nothing is recorded");
            assert!(runtime.actions.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn a_recovered_step_with_every_result_continues_without_running_again() {
        let calls = vec![
            recorded(
                "call-1",
                "write_file",
                json!({"path": "a.txt", "content": "a"}),
            ),
            recorded("call-2", "read_file", json!({"path": "a.txt"})),
        ];
        let usage_for_resume = Usage {
            output: 3,
            ..Usage::default()
        };
        let history = vec![
            Message::tool_result("call-1", "wrote a.txt"),
            Message::tool_result("call-2", "a"),
        ];
        let model = FakeModel::with(vec![reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let (text, _usage, succeeded) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", history, 100).with_resume(Resume {
                    step: 2,
                    content: Some("working".to_owned()),
                    calls,
                    done: 2,
                    approved_action: None,
                    usage: usage_for_resume,
                    recovered: true,
                    continuation: None,
                }),
            )
            .await,
        );

        assert_eq!(text, "done");
        assert!(succeeded);
        assert!(recorder.started.is_empty(), "no recorded action runs again");
        assert!(recorder.approvals.is_empty(), "no recorded call asks again");
        assert_eq!(
            recorder.steps,
            [
                (2, usage_for_resume, 2),
                (3, reply("done").unwrap().usage, 0),
            ]
        );
    }

    #[tokio::test]
    async fn a_resumed_step_with_repeated_identifiers_fails_the_run() {
        let calls = vec![
            recorded("call-1", "read_file", json!({"path": "a.txt"})),
            recorded("call-1", "read_file", json!({"path": "b.txt"})),
        ];
        let model = FakeModel::with(Vec::new());
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let error = run(
            &runtime,
            &model,
            &mut recorder,
            RunRequest::new("system", Vec::new(), 100).with_resume(resume(calls, 0, Id::new())),
        )
        .await;

        assert!(matches!(&error, Err(LoopError::RepeatedCall(id)) if id == "call-1"));
        assert!(recorder.order.is_empty());
        assert!(runtime.actions.lock().unwrap().is_empty());
    }

    #[test]
    fn a_call_the_model_left_without_an_id_answers_under_its_position() {
        assert_eq!(call_id("", 3, 0), "call-3-0");
        assert_eq!(call_id("call-9", 1, 2), "call-9");
        assert_eq!(call_id("call-3-0", 3, 0), "call-3-0");
    }

    #[tokio::test]
    async fn the_space_can_rebuild_the_id_of_an_id_less_call() {
        let model = FakeModel::with(vec![
            calls_reply(vec![call("", "list_files", json!({"path": "."}))]),
            reply("done"),
        ]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        let recorded = &recorder.replies[0].2[0];
        assert_eq!(recorded.call, "call-1-0");
        assert_eq!(
            tool_messages(&model.requests.lock().unwrap()[1]),
            [("call-1-0".to_owned(), "42".to_owned())]
        );
        assert_eq!(call_id(&recorded.call, 1, 0), "call-1-0");
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
        let mut recorder = TestRecorder::default();

        let (text, _, succeeded) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert!(!succeeded);
        assert_eq!(text, "I lost my Computer before the action returned.");
        assert_eq!(recorder.finished[0].1, ActionStatus::Unknown);
        assert_eq!(model.requests.lock().unwrap().len(), 1);
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
            continuation: None,
        });
        let model = FakeModel::with(vec![calls, reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let (_, _, succeeded) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert!(succeeded);
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
        let mut recorder = TestRecorder::default();

        let (_, _, succeeded) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert!(succeeded);
        assert_eq!(recorder.started[0].1, "no_such_tool");
        assert_eq!(recorder.finished[0].1, ActionStatus::Failed);
        assert!(
            recorder.approvals.is_empty(),
            "a call that runs nothing asks nobody"
        );
        assert_eq!(
            recorder.replies[0].2[0].refused.as_deref(),
            Some("there is no tool named no_such_tool"),
            "the record says why the call was refused"
        );
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
        let mut recorder = TestRecorder::default();

        let (_, _, succeeded) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert!(succeeded);
        assert_eq!(recorder.finished[0].1, ActionStatus::Failed);
        assert!(
            recorder.approvals.is_empty(),
            "a call that runs nothing asks nobody"
        );
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
        let mut recorder = TestRecorder::default();

        let (text, usage, succeeded) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100),
            )
            .await,
        );

        assert!(!succeeded);
        assert!(text.contains("down"));
        assert_eq!(usage.budgeted(), 52);
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

        let (text, _, succeeded) = reply_outcome(
            run(
                &runtime,
                &model,
                &mut recorder,
                RunRequest::new("system", Vec::new(), 100).with_max_steps(2),
            )
            .await,
        );

        assert!(!succeeded);
        assert!(text.contains("2 steps"));
        assert_eq!(recorder.steps.len(), 2);
        assert_eq!(recorder.started.len(), 2);
    }

    #[tokio::test]
    async fn a_recorder_failure_stops_the_loop() {
        let model = FakeModel::with(vec![reply("hello")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder {
            fail_at: Some("step"),
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

    #[test]
    fn a_run_can_be_spawned() {
        fn is_send<T: Send>(_: &T) {}
        let runtime = FakeRuntime::default();
        let model = FakeModel::with(Vec::new());
        let mut recorder = TestRecorder::default();

        is_send(&run(
            &runtime,
            &model,
            &mut recorder,
            RunRequest::new("system", Vec::new(), 100),
        ));
    }
}

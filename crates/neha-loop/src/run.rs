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

use crate::model::{
    Continuation, Message, Model, ModelFailure, ModelRequest, Role, ToolCall, ToolSpec,
};
use crate::tools::{self, SpaceCall};

/// The largest raw tool-call arguments string a step event keeps whole.
const MAX_RECORDED_ARGUMENTS: usize = 128 * 1024;

/// What one run needs to start.
pub struct RunRequest {
    /// The system instruction.
    pub system: String,
    /// The history ends with the results of the calls [`Resume::from`] covers.
    pub history: Vec<Message>,
    /// The largest Model reply in tokens.
    pub max_output: u32,
    /// The Model's context window in tokens; zero means unknown, so the run
    /// neither prunes the projection nor compacts the conversation.
    pub context_window: u32,
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
            context_window: 0,
            thinking: None,
            continuation: None,
            max_steps: 8,
            resume: None,
            start_step: 1,
            tools: tools::specs(),
        }
    }

    /// Sets the Model's context window in tokens; zero means unknown.
    #[must_use]
    pub fn with_context_window(mut self, context_window: u32) -> Self {
        self.context_window = context_window;
        self
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
    /// The position of the first call the run did not finish. The calls
    /// before it replay from the record: a Computer action's result is the one
    /// the record gives back, and a memory call's answer is the one it
    /// recorded. The calls from it run again.
    pub from: usize,
    /// The approved action's identifier, which the call at `from` keeps when
    /// it runs.
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecordedCall {
    /// The identifier the call's tool result answers under; see [`call_id`].
    pub call: String,
    /// The tool's name.
    pub tool: String,
    /// The MCP server the call names, when the record kept one.
    ///
    /// A replay pins the server: a replacement registered under the same name
    /// must never run a call the answer was made for another server.
    pub server: Option<Id>,
    /// The tool's original name in that server.
    pub tool_name: Option<String>,
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

    /// Returns what an action the User already answered may do.
    ///
    /// The action runs under its recorded identifier, so the Space resolves
    /// the identity from the record instead of asking again; the answer is
    /// refused when the offer no longer names that identity.
    fn authorized_action(
        &mut self,
        action: Id,
        call: &RecordedCall,
    ) -> impl Future<Output = Result<Authorization, LoopError>> + Send;

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

    /// Runs one memory tool call and returns the answer the Model reads.
    ///
    /// A memory write is a record operation, not an action on the Computer:
    /// it asks no approval, records no started or finished action, and never
    /// reaches the Computer. The answer is recorded, so the same call in a
    /// replay returns it instead of running again.
    fn memory(
        &mut self,
        step: u32,
        call: &RecordedCall,
        request: &tools::MemoryCall,
    ) -> impl Future<Output = Result<ToolOutput, LoopError>> + Send;

    /// Records, or replays, the answer to a memory call the loop refused
    /// before running it.
    ///
    /// A refusal is a memory tool result too: the record keeps the exact text
    /// the Model reads, and a replay returns it instead of refusing again.
    fn memory_refused(
        &mut self,
        step: u32,
        call: &RecordedCall,
        answer: String,
    ) -> impl Future<Output = Result<ToolOutput, LoopError>> + Send;

    /// Compacts the conversation into a projection the next request sends.
    ///
    /// The record keeps every Message; the returned projection is a summary of
    /// the earlier span as a User Message, followed by the original text after
    /// the cut. It carries no system Message unless the Space rebuilt one (a
    /// silent note turn can change the prompt). `None` means the conversation
    /// has no span a summary can replace, so the loop sends the request it
    /// would have sent.
    fn compact(
        &mut self,
        step: u32,
        messages: &[Message],
    ) -> impl Future<Output = Result<Option<Compaction>, LoopError>> + Send;
}

/// What one compaction replaces in the loop's projection.
pub struct Compaction {
    /// The system Message the next request reads, when the Space rebuilt it.
    pub system: Option<String>,
    /// The Messages after the system Message.
    pub messages: Vec<Message>,
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
        context_window,
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
    // The usage the Provider last reported for a plain step, with the message
    // prefix its request covered: the estimate then counts only what a later
    // request added. A compaction replaces the projection and drops it.
    let mut anchor: Option<Anchor> = None;
    // A run compacts at most once, whichever path asks for it.
    let mut compacted = false;

    for step in first_step..=max_steps {
        let (calls, reply_usage, from, approved, all_done) = match resume.take() {
            // The reply was recorded before the run waited, so it is not
            // recorded again; its finished calls already have their results.
            Some(resume) => {
                let Resume {
                    content,
                    calls,
                    from,
                    approved_action,
                    usage,
                    recovered,
                    continuation: step_continuation,
                    ..
                } = resume;
                continuation = step_continuation.or(continuation);
                let calls = resumed_calls(calls, &tools)?;
                let from = from.min(calls.len());
                // The history ends with one result per call before `from`: a
                // Computer action's recorded result, or a memory call's
                // recorded answer. Neither runs again in a replay.
                if messages.len() < from + 1 {
                    return Err(LoopError::Resume(
                        "the history is shorter than the results it gave back".to_owned(),
                    ));
                }
                let mut rebuilt = messages.split_off(messages.len() - from).into_iter();
                messages.push(Message::assistant_with_calls(content, model_calls(&calls)));
                for _ in calls.iter().take(from) {
                    let result = rebuilt.next().ok_or_else(|| {
                        LoopError::Resume("the record lost a replayed call's result".to_owned())
                    })?;
                    messages.push(result);
                }
                let all_done = recovered && from >= calls.len();
                (calls, usage, from, approved_action, all_done)
            }
            None => {
                // The request is a projection: old tool results become
                // placeholders, and a conversation that still does not fit is
                // compacted once. The loop's own Messages keep the original
                // text; only the copy sent to the Model is trimmed.
                let mut projection = project(&messages, &tools, anchor, context_window, max_output);
                if !compacted
                    && context_threshold(context_window, max_output)
                        .is_some_and(|threshold| projection.estimate >= threshold)
                    && let Some(compaction) = recorder.compact(step, &messages).await?
                {
                    compacted = true;
                    messages = with_system(&messages, compaction);
                    projection = project(&messages, &tools, None, context_window, max_output);
                }
                let reply = loop {
                    match model
                        .complete(ModelRequest {
                            messages: projection.messages.clone(),
                            tools: tools.clone(),
                            max_output,
                            thinking,
                            continuation: continuation.clone(),
                        })
                        .await
                    {
                        Ok(reply) => break reply,
                        Err(failure) => {
                            // A Provider that reports the input as too long
                            // gets one compaction and one retry, and only a
                            // projection strictly shorter than the refused one
                            // is retried: the same text would be refused again.
                            if failure.context_too_large
                                && !compacted
                                && may_compact(context_window, max_output)
                                && let Some(compaction) = recorder.compact(step, &messages).await?
                            {
                                compacted = true;
                                messages = with_system(&messages, compaction);
                                let candidate =
                                    project(&messages, &tools, None, context_window, max_output);
                                if chars_of(&candidate.messages) < chars_of(&projection.messages) {
                                    projection = candidate;
                                    continue;
                                }
                            }
                            return Ok(failure_outcome(&failure, usage));
                        }
                    }
                };
                // The anchor names the exact Messages the request sent: a
                // projection that replaced old results with placeholders is
                // fingerprinted as sent, so a later request with the original
                // text is not mistaken for the same prefix.
                anchor = Some(Anchor {
                    tokens: prompt_tokens(reply.usage),
                    prefix: projection.messages.iter().map(fingerprinted).collect(),
                });
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
                // The assistant turn the Model sees must answer under the same
                // tool-call identifiers the record keeps. A provider may send
                // a blank identifier; the stable fallback replaces it, and a
                // provider-native continuation that still holds the raw id
                // cannot pair with the normalized turn, so it is dropped.
                let normalized = calls
                    .iter()
                    .zip(&reply.tool_calls)
                    .any(|(call, original)| call.recorded.call != original.id);
                let reply_continuation = if normalized { None } else { reply.continuation };
                recorder
                    .reply(
                        step,
                        reply.content.clone(),
                        calls.iter().map(|call| call.recorded.clone()).collect(),
                        reply.usage,
                        reply_continuation.clone(),
                    )
                    .await?;
                continuation = reply_continuation;
                messages.push(Message::assistant_with_calls(
                    reply.content,
                    model_calls(&calls),
                ));
                (calls, reply.usage, 0, None, false)
            }
        };
        let tool_calls = calls.len() as u32;
        // A recovered step whose calls all have their recorded results has no
        // action left to run; record the step and continue with the next one.
        if !(all_done && from >= calls.len()) {
            for (index, call) in calls.into_iter().enumerate().skip(from) {
                // A memory call never asks, starts no action, and never
                // reaches the Computer: its entry is what the record keeps.
                if tools::is_memory(&call.recorded.tool) {
                    let output = memory_result(recorder, step, &call).await?;
                    messages.push(Message::tool_result(call.recorded.call, output.text));
                    continue;
                }
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
                        match recorder.authorized_action(action, &call.recorded).await? {
                            Authorization::Allow(answered) => dispatched = answered,
                            Authorization::Refuse(reason) => refused = Some(reason),
                            // An answered call cannot ask again: a record that
                            // says otherwise is not one this loop can replay.
                            Authorization::Wait(_) => {
                                return Err(LoopError::Record(
                                    "an answered call asked again".to_owned(),
                                ));
                            }
                        }
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
                    (None, false, Ok(tools::Call::Memory(_))) => {
                        ActionResult::failed("a memory tool is not a Computer action")
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

/// The tokens one request may spend on input before it must be trimmed.
///
/// A window that cannot hold its own maximum output leaves no room for input,
/// so no conversation can be trimmed into it.
fn input_budget(context_window: u32, max_output: u32) -> Option<u32> {
    if context_window == 0 || context_window <= max_output {
        return None;
    }

    Some(context_window - max_output)
}

/// Returns the tokens a Provider counts as input: everything but the reply.
fn prompt_tokens(usage: Usage) -> u32 {
    let tokens = usage
        .uncached_input
        .saturating_add(usage.cache_read)
        .saturating_add(usage.cache_write);

    u32::try_from(tokens).unwrap_or(u32::MAX)
}

/// The characters of one text as the Model reads it.
///
/// A character is a Unicode scalar value, not a UTF-8 byte: a CJK character
/// costs one character like a Latin one.
fn text_chars(text: &str) -> u64 {
    u64::try_from(text.chars().count()).unwrap_or(u64::MAX)
}

/// The characters of one Message as the Model reads it.
pub fn message_chars(message: &Message) -> u64 {
    let mut chars = message.content.as_deref().map_or(0, text_chars);
    chars = chars.saturating_add(message.tool_call_id.as_deref().map_or(0, text_chars));
    for call in &message.tool_calls {
        chars = chars
            .saturating_add(text_chars(&call.id))
            .saturating_add(text_chars(&call.name))
            .saturating_add(text_chars(&call.arguments));
    }

    chars
}

/// The characters of some Messages as the Model reads them.
pub fn chars_of(messages: &[Message]) -> u64 {
    messages.iter().map(message_chars).sum()
}

/// The characters of the tool schemas a request carries.
fn tool_chars(tools: &[ToolSpec]) -> u64 {
    tools
        .iter()
        .map(|tool| {
            text_chars(&tool.name)
                .saturating_add(text_chars(&tool.description))
                .saturating_add(text_chars(&tool.parameters.to_string()))
        })
        .sum()
}

/// The tokens a count of characters costs, at four characters per token.
fn tokens_of(chars: u64) -> u32 {
    u32::try_from(chars / 4).unwrap_or(u32::MAX)
}

/// Estimates the tokens a request costs, at four characters per token.
#[must_use]
pub fn estimate_tokens(messages: &[Message], tools: &[ToolSpec]) -> u32 {
    tokens_of(chars_of(messages).saturating_add(tool_chars(tools)))
}

/// The token budget above which a conversation compacts.
///
/// A window with no headroom over its own maximum output compacts at its
/// input budget; otherwise the budget is the smaller of 80% of the window and
/// the window minus the maximum output minus the 65536-token reserve. `None`
/// means the window cannot hold its own maximum output, so there is no input
/// space to compact into and the request is not compacted.
#[must_use]
pub fn context_threshold(context_window: u32, max_output: u32) -> Option<u32> {
    let budget = input_budget(context_window, max_output)?;
    let headroom = max_output.saturating_add(65_536);
    if context_window <= headroom {
        return Some(budget);
    }
    let eighty_percent = u32::try_from(u64::from(context_window) * 4 / 5).unwrap_or(u32::MAX);

    Some(eighty_percent.min(context_window - headroom))
}

/// Whether a run may compact at all in this window.
///
/// An unknown window may still be too small for the request the Provider
/// refused, so the run makes one cut and compares the result. A known window
/// that cannot hold its own maximum output has no input space to compact
/// into: a Provider error then ends the run unchanged.
fn may_compact(context_window: u32, max_output: u32) -> bool {
    context_window == 0 || context_threshold(context_window, max_output).is_some()
}

/// The Provider's own count of the last request, and the exact prefix it sent.
///
/// The prefix is fingerprinted, not counted: the loop's raw Messages keep the
/// original tool results while a request replaces the old ones with
/// placeholders, so the same length can cover different text.
#[derive(Clone)]
struct Anchor {
    /// The input tokens the Provider reported for that request.
    tokens: u32,
    /// One fingerprint per Message that request sent, in order.
    prefix: Vec<Fingerprint>,
}

/// One Message's identity in a request prefix: its role, size, and content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fingerprint {
    role: u8,
    chars: u64,
    hash: u64,
}

/// Returns the identity of one Message as a request prefix entry.
///
/// A tool result is identified by its call identifier as well as its text, so
/// an original result and the placeholder that replaces it never look alike.
fn fingerprinted(message: &Message) -> Fingerprint {
    use std::hash::{Hash as _, Hasher as _};

    let role = match message.role {
        Role::System => 0,
        Role::User => 1,
        Role::Assistant => 2,
        Role::Tool => 3,
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    message.content.hash(&mut hasher);
    message.tool_call_id.hash(&mut hasher);
    for call in &message.tool_calls {
        call.id.hash(&mut hasher);
        call.name.hash(&mut hasher);
        call.arguments.hash(&mut hasher);
    }

    Fingerprint {
        role,
        chars: message_chars(message),
        hash: hasher.finish(),
    }
}

/// Whether one request prefix matches the fingerprinted prefix of the last one.
fn prefix_matches(prefix: &[Fingerprint], messages: &[Message]) -> bool {
    prefix.len() <= messages.len()
        && prefix
            .iter()
            .zip(messages)
            .all(|(fingerprint, message)| *fingerprint == fingerprinted(message))
}

/// Estimates one candidate projection against the Provider's last count.
///
/// The anchor covers the prefix of the request the Provider counted. While the
/// candidate's own prefix still matches that fingerprint, its count plus the
/// characters after the prefix is reused; once a replacement lands inside the
/// prefix, the count starts from zero over the candidate. The tool schemas are
/// already inside the anchored count, so the anchored branch adds only the
/// text the Provider never saw.
fn estimate_projection(projection: &[Message], tools: &[ToolSpec], anchor: Option<&Anchor>) -> u32 {
    match anchor.filter(|anchor| prefix_matches(&anchor.prefix, projection)) {
        Some(anchor) => anchor
            .tokens
            .saturating_add(tokens_of(chars_of(&projection[anchor.prefix.len()..]))),
        None => estimate_tokens(projection, tools),
    }
}

/// The Messages one request sends, and the estimate that decided them.
struct Projection {
    /// The Messages after the system Message.
    messages: Vec<Message>,
    /// The tokens the projection is estimated to cost.
    estimate: u32,
}

/// The largest number of tool results a projection keeps raw.
const KEPT_TOOL_RESULTS: usize = 3;

/// The text a pruned tool result carries; it names the call it answered.
const PRUNED_TOOL_RESULT: &str = "[tool result removed from context; call ";

/// Projects one request: old tool results become placeholders until the input
/// fits, and the estimate says what the projection costs.
///
/// The anchor covers the prefix of the previous request. While the candidate
/// projection still starts with that exact prefix the Provider's count is
/// reused; once a replacement lands inside it, the estimate starts from zero
/// over the projection instead.
fn project(
    messages: &[Message],
    tools: &[ToolSpec],
    anchor: Option<Anchor>,
    context_window: u32,
    max_output: u32,
) -> Projection {
    let mut projection = messages.to_vec();
    // The Provider's count covers the request it saw, placeholders included.
    // It is reused only while the candidate projection's prefix is
    // byte-for-byte what that request sent; otherwise the estimate starts
    // from the current text. The check runs on the candidate, not the raw
    // Messages: a trim can restore the exact placeholder prefix the Provider
    // counted, and then the count still covers it.
    let mut estimate = estimate_projection(&projection, tools, anchor.as_ref());
    let Some(budget) = input_budget(context_window, max_output) else {
        return Projection {
            messages: projection,
            estimate,
        };
    };
    if estimate < budget {
        return Projection {
            messages: projection,
            estimate,
        };
    }
    // The three most recent results always stay; the older ones become
    // placeholders, oldest first, until the input fits its budget. A replaced
    // prefix invalidates the anchor, so the estimate restarts over the
    // projection.
    let results: Vec<usize> = projection
        .iter()
        .enumerate()
        .filter(|(_, message)| message.role == Role::Tool)
        .map(|(index, _)| index)
        .collect();
    let Some(removable) = results.len().checked_sub(KEPT_TOOL_RESULTS) else {
        return Projection {
            messages: projection,
            estimate,
        };
    };
    for &index in &results[..removable] {
        let call = projection[index].tool_call_id.clone().unwrap_or_default();
        projection[index].content = Some(format!("{PRUNED_TOOL_RESULT}{call}]"));
        // A replacement after the anchored prefix keeps the Provider's count;
        // one inside it invalidates the anchor, and the candidate is counted
        // from zero again.
        estimate = estimate_projection(&projection, tools, anchor.as_ref());
        if estimate < budget {
            break;
        }
    }

    Projection {
        messages: projection,
        estimate,
    }
}

/// Puts a compaction projection after the loop's system Message.
fn with_system(messages: &[Message], compaction: Compaction) -> Vec<Message> {
    let system = compaction.system.or_else(|| {
        messages
            .first()
            .filter(|message| message.role == Role::System)
            .and_then(|message| message.content.clone())
    });
    let mut kept = Vec::with_capacity(compaction.messages.len() + 1);
    if let Some(system) = system {
        kept.push(Message::system(system));
    }
    kept.extend(compaction.messages);

    kept
}

/// Runs one memory call and returns what the Model reads.
///
/// A call the loop refused never runs, but its refusal is still a memory tool
/// result: the recorder keeps it, and a replay returns the recorded answer.
async fn memory_result<Rec>(
    recorder: &mut Rec,
    step: u32,
    call: &StepCall,
) -> Result<ToolOutput, LoopError>
where
    Rec: Recorder + Send,
{
    match &call.parsed {
        Ok(tools::Call::Memory(request)) => recorder.memory(step, &call.recorded, request).await,
        Ok(tools::Call::Computer(_)) => Err(LoopError::Resume(
            "a Computer action was given to the memory recorder".to_owned(),
        )),
        Ok(tools::Call::Space(_)) => Err(LoopError::Resume(
            "a Space tool was given to the memory recorder".to_owned(),
        )),
        // A refusal is a memory tool result: the record keeps it, and a replay
        // returns it instead of refusing again.
        Err(refusal) => {
            recorder
                .memory_refused(step, &call.recorded, refusal.clone())
                .await
        }
    }
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
                    server: None,
                    tool_name: None,
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
/// The Model's own identifier when it gave a usable one, and
/// `call-<step>-<index>` otherwise. A provider may send a blank identifier;
/// tracing treats blank as absent, so the record and every tool result use
/// the same stable fallback. The rule depends on nothing else, so the Space
/// rebuilds a run's Messages from the record with the same identifiers.
#[must_use]
pub fn call_id(id: &str, step: u32, index: usize) -> String {
    if id.trim().is_empty() {
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
    /// The record does not line up with the step it resumes.
    #[error("the resumed step does not line up with the record: {0}")]
    Resume(String),
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
        Anchor, Authorization, Compaction, Dispatched, LoopError, RecordedCall, Recorder, Resume,
        RunOutcome, RunRequest, Runtime, WaitingApproval, call_id, chars_of, context_threshold,
        estimate_tokens, fingerprinted, message_chars, project, run,
    };
    use crate::model::{
        Continuation, Message, Model, ModelFailure, ModelReply, ModelRequest, Role, ToolCall,
        ToolSpec,
    };
    use crate::tools::{self, MemoryCall, SpaceCall};
    use neha::memory::{MemoryKind, MemoryScope};

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
        memories: Vec<(u32, tools::MemoryCall)>,
        refusals: Vec<(u32, String, String)>,
        /// The compactions `compact` returns, in order; `None` once it runs out.
        compactions: VecDeque<Option<Compaction>>,
        /// The steps and Messages `compact` was asked about.
        compacted: Vec<(u32, Vec<Message>)>,
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
        ) -> Result<Authorization, LoopError> {
            Ok(Authorization::Allow(Dispatched::default()))
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

        async fn memory(
            &mut self,
            step: u32,
            _call: &RecordedCall,
            request: &tools::MemoryCall,
        ) -> Result<ToolOutput, LoopError> {
            self.record("memory")?;
            self.memories.push((step, request.clone()));

            Ok(ToolOutput::full("{\"memory\":\"remembered\"}"))
        }

        async fn memory_refused(
            &mut self,
            step: u32,
            call: &RecordedCall,
            answer: String,
        ) -> Result<ToolOutput, LoopError> {
            self.record("memory_refused")?;
            self.refusals
                .push((step, call.call.clone(), answer.clone()));

            Ok(ToolOutput::full(answer))
        }

        async fn compact(
            &mut self,
            step: u32,
            messages: &[Message],
        ) -> Result<Option<Compaction>, LoopError> {
            self.record("compact")?;
            self.compacted.push((step, messages.to_vec()));

            Ok(self.compactions.pop_front().unwrap_or(None))
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
            ..RecordedCall::default()
        }
    }

    /// Resumes the second step, which waited on the call after `from`.
    fn resume(calls: Vec<RecordedCall>, from: usize, approved_action: Id) -> Resume {
        Resume {
            step: 2,
            content: None,
            calls,
            from,
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
    async fn a_normalized_call_id_drops_the_native_continuation() {
        let continuation = Continuation {
            protocol: "openai_responses".to_owned(),
            provider: "provider".to_owned(),
            model: "model".to_owned(),
            state: json!({"items": [{"type": "reasoning", "id": "rs_1"}]}),
        };
        let model = FakeModel::with(vec![
            Ok(ModelReply {
                content: Some("let me look".to_owned()),
                tool_calls: vec![call("   ", "read_file", json!({"path": "a.txt"}))],
                usage: Usage::default(),
                continuation: Some(continuation),
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
        // The record keeps the normalized id.
        assert_eq!(recorder.replies[0].2[0].call, "call-1-0");
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[1].continuation, None,
            "a normalized call id drops the native continuation that pairs the raw id"
        );
        // The live assistant turn and the tool result answer under one id.
        let assistant = requests[1]
            .messages
            .iter()
            .find(|message| message.role == Role::Assistant && !message.tool_calls.is_empty())
            .expect("the assistant turn is in the next request");
        assert_eq!(assistant.tool_calls[0].id, "call-1-0");
        assert_eq!(
            tool_messages(&requests[1]),
            [("call-1-0".to_owned(), "42".to_owned())]
        );
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
                    from: 2,
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

    #[test]
    fn a_blank_call_id_gets_the_stable_fallback() {
        assert_eq!(call_id("", 1, 0), "call-1-0");
        assert_eq!(call_id("   ", 2, 3), "call-2-3");
        assert_eq!(call_id("call-abc", 1, 0), "call-abc");
    }

    #[tokio::test]
    async fn the_space_can_rebuild_the_id_of_an_id_less_call() {
        let model = FakeModel::with(vec![
            calls_reply(vec![call("   ", "list_files", json!({"path": "."}))]),
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
            context_too_large: false,
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

    #[tokio::test]
    async fn a_memory_call_never_asks_and_returns_its_result_to_the_model() {
        let model = FakeModel::with(vec![
            call_reply(
                "remember",
                json!({"kind": "rule", "scope": "owner", "text": "keep notes"}),
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
            recorder.order,
            ["reply", "memory", "step", "reply", "step"],
            "a memory call asks for no approval and starts no action"
        );
        assert!(recorder.approvals.is_empty());
        assert!(recorder.started.is_empty());
        assert!(recorder.finished.is_empty());
        assert!(runtime.actions.lock().unwrap().is_empty());
        assert_eq!(
            recorder.memories,
            [(
                1,
                MemoryCall::Remember {
                    kind: MemoryKind::Rule,
                    scope: MemoryScope::Owner,
                    text: "keep notes".to_owned(),
                    replaces: Vec::new(),
                }
            )]
        );
        // The Model reads the result under the call's own identifier.
        let requests = model.requests.lock().unwrap();
        assert_eq!(
            tool_messages(&requests[1]),
            [(
                "call-1".to_owned(),
                "{\"memory\":\"remembered\"}".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn a_refused_memory_call_asks_nothing_and_returns_the_refusal() {
        let model = FakeModel::with(vec![
            call_reply(
                "remember",
                json!({"kind": "rule", "scope": "group", "text": "keep notes"}),
            ),
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
        assert!(recorder.approvals.is_empty());
        assert!(recorder.started.is_empty());
        assert!(recorder.memories.is_empty(), "the call never runs");
        let requests = model.requests.lock().unwrap();
        assert_eq!(
            tool_messages(&requests[1]),
            [(
                "call-1".to_owned(),
                "group memory is not available yet".to_owned()
            )]
        );
    }

    #[tokio::test]
    async fn a_resumed_step_replays_a_memory_answer_before_the_approved_action() {
        // The step asked to remember something, then to run a shell command,
        // and waited on the shell command. The memory call's answer is in the
        // record, so the resumed step replays it instead of running the call
        // again.
        let approved_action = Id::new();
        let calls = vec![
            recorded(
                "call-1",
                "remember",
                json!({"kind": "note", "scope": "owner", "text": "the build uses nix"}),
            ),
            recorded("call-2", "shell", json!({"command": "nix build"})),
        ];
        // The history ends with the memory answer the record keeps.
        let history = vec![
            Message::user("go"),
            Message::tool_result("call-1".to_owned(), "{\"memory\":\"recorded\"}".to_owned()),
        ];
        let model = FakeModel::with(vec![reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();

        let (text, _, succeeded) = reply_outcome(
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
        assert!(
            recorder.memories.is_empty(),
            "a replay reads the recorded answer and never runs the memory call again"
        );
        assert_eq!(
            recorder.started,
            [(2, "shell".to_owned(), approved_action)],
            "the approved action keeps its id and is not asked again"
        );
        assert!(recorder.approvals.is_empty());
        // The Model reads the recorded memory answer, then the action's result.
        let requests = model.requests.lock().unwrap();
        assert_eq!(
            tool_messages(&requests[0]),
            [
                ("call-1".to_owned(), "{\"memory\":\"recorded\"}".to_owned()),
                ("call-2".to_owned(), "42".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn a_refused_memory_call_records_its_answer() {
        // Bad arguments make the loop refuse a memory call before it runs; the
        // refusal is still the memory tool's answer, so the record keeps it.
        let model = FakeModel::with(vec![
            calls_reply(vec![call("call-1", "remember", json!({"kind": "note"}))]),
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
        assert_eq!(recorder.refusals.len(), 1, "the refusal is recorded");
        assert_eq!(recorder.refusals[0].0, 1);
        assert_eq!(recorder.refusals[0].1, "call-1");
        assert!(recorder.memories.is_empty());
        // The Model reads the recorded answer, and the loop asked nothing and
        // started no action for the refused call.
        let refusal = recorder.refusals[0].2.clone();
        assert!(recorder.approvals.is_empty());
        assert!(recorder.started.is_empty());
        let requests = model.requests.lock().unwrap();
        assert_eq!(
            tool_messages(&requests[1]),
            [("call-1".to_owned(), refusal)]
        );
    }

    fn too_large() -> Result<ModelReply, ModelFailure> {
        Err(ModelFailure {
            code: neha::error::ErrorCode::InvalidArgument,
            detail: "the request is larger than the Model's context".to_owned(),
            usage: Usage::default(),
            context_too_large: true,
        })
    }

    fn compaction(messages: Vec<Message>) -> Compaction {
        Compaction {
            system: None,
            messages,
        }
    }

    #[test]
    fn the_context_threshold_keeps_the_output_and_reserve_it_needs() {
        // A large window compacts at 80% or at the window minus its output and
        // the 65536-token reserve, whichever is smaller.
        assert_eq!(context_threshold(200_000, 8_192), Some(126_272));
        assert_eq!(context_threshold(1_000_000, 8_192), Some(800_000));
        // A window that cannot hold its own maximum output has no input room,
        // so there is nothing to compact into.
        assert_eq!(context_threshold(100, 200), None);
        // A window with no headroom over the reserve compacts at its input
        // budget.
        assert_eq!(context_threshold(1_000, 100), Some(900));
    }

    #[test]
    fn an_estimate_counts_unicode_characters_and_tool_schemas() {
        // A CJK character is one character, not its three UTF-8 bytes.
        assert_eq!(message_chars(&Message::user("你好")), 2);
        assert_eq!(chars_of(&[Message::user("hello")]), 5);
        let messages = vec![Message::system("ab"), Message::user("cdef")];
        let tools = vec![ToolSpec {
            name: "shell".to_owned(),
            description: "run".to_owned(),
            parameters: json!({}),
        }];

        assert_eq!(estimate_tokens(&messages, &tools), 4);
        assert_eq!(estimate_tokens(&messages, &[]), 1);
    }

    #[tokio::test]
    async fn the_projection_keeps_the_three_newest_tool_results_raw() {
        let history = vec![
            Message::user("go"),
            Message::tool_result("old", "a".repeat(4_000)),
            Message::tool_result("older", "b".repeat(4_000)),
            Message::tool_result("newest", "c".repeat(4_000)),
            Message::tool_result("newer", "d".repeat(4_000)),
        ];
        let model = FakeModel::with(vec![Err(ModelFailure {
            code: neha::error::ErrorCode::InvalidArgument,
            detail: "unused".to_owned(),
            usage: Usage::default(),
            context_too_large: false,
        })]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();
        let request = RunRequest::new("system", history, 100)
            .with_context_window(4_000)
            .with_max_steps(1);

        let _ = run(&runtime, &model, &mut recorder, request).await;

        let requests = model.requests.lock().unwrap();
        let results = tool_messages(&requests[0]);
        assert_eq!(results.len(), 4, "every result still answers its call");
        assert!(
            results[0].1.contains("call old"),
            "the oldest result becomes a placeholder naming its call: {}",
            results[0].1
        );
        for expected in ["b".repeat(4_000), "c".repeat(4_000), "d".repeat(4_000)] {
            assert!(
                results.iter().any(|(_, content)| *content == expected),
                "the three newest results stay raw"
            );
        }
        assert_eq!(
            recorder.compacted.len(),
            0,
            "a projection that fits after pruning takes no summary"
        );
        assert!(recorder.compactions.is_empty(), "the fake never compacted");
    }

    #[tokio::test]
    async fn the_projection_prunes_at_the_input_budget_not_the_threshold() {
        // The window leaves 70,000 tokens for input but compacts at 4,464, so
        // the projection fits its input budget and is sent whole.
        let history = vec![
            Message::user("go"),
            Message::tool_result("a", "a".repeat(4_000)),
            Message::tool_result("b", "b".repeat(4_000)),
            Message::tool_result("c", "c".repeat(4_000)),
            Message::tool_result("d", "d".repeat(4_000)),
        ];
        let model = FakeModel::with(vec![reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();
        let request = RunRequest::new("system", history, 0).with_context_window(70_000);

        let (_, _, succeeded) = reply_outcome(run(&runtime, &model, &mut recorder, request).await);

        assert!(succeeded);
        let requests = model.requests.lock().unwrap();
        for (_, content) in tool_messages(&requests[0]) {
            assert!(
                !content.starts_with("[tool result removed"),
                "the input budget had room, so nothing was pruned"
            );
        }
        assert_eq!(
            recorder.compacted.len(),
            1,
            "the compaction threshold is smaller than the input budget"
        );
    }

    /// A tool schema small enough that the counts in the anchor probe stay
    /// readable.
    fn probe_tools() -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "shell".to_owned(),
            description: "run".to_owned(),
            parameters: json!({}),
        }]
    }

    #[test]
    fn an_anchor_covers_only_the_prefix_a_request_actually_sent() {
        // The first request trims a 12,000-character tool result to a
        // placeholder. The next request holds the same raw result again, but
        // trimming it restores the exact placeholder prefix the Provider
        // counted: the anchored count stays valid, and the result is trimmed
        // again so the request really is that prefix.
        let tools = probe_tools();
        let mut messages = vec![
            Message::system("system"),
            Message::user("u".repeat(3_000)),
            Message::tool_result("call-a", "A".repeat(12_000)),
            Message::tool_result("call-b", "B".repeat(10)),
            Message::tool_result("call-c", "C".repeat(10)),
            Message::tool_result("call-d", "D".repeat(10)),
        ];
        // A window of 6,900 tokens with a 5,000-token output leaves 1,900 for
        // input.
        let (window, max_output) = (6_900, 5_000);
        let first = project(&messages, &tools, None, window, max_output);
        assert!(
            first.messages[2]
                .content
                .as_deref()
                .unwrap()
                .contains("call call-a"),
            "the first request trims the old result: {:?}",
            first.messages[2].content
        );
        assert!(
            first.messages[3..].iter().all(|message| {
                message.content.as_deref().is_some_and(|content| {
                    !content.starts_with("[tool result removed from context")
                })
            }),
            "the three newest results stay raw"
        );
        assert_eq!(first.estimate, estimate_tokens(&first.messages, &tools));
        let anchor = Anchor {
            tokens: 36,
            prefix: first.messages.iter().map(fingerprinted).collect(),
        };

        // Four characters are appended; the raw prefix keeps the original
        // result, which is not the placeholder the anchored request sent.
        messages.push(Message::assistant("done"));
        let second = project(&messages, &tools, Some(anchor), window, max_output);
        assert!(
            second.messages[2]
                .content
                .as_deref()
                .unwrap()
                .contains("call call-a"),
            "the second request trims the same result again: {:?}",
            second.messages[2].content
        );
        // Trimming restores exactly the placeholder prefix the Provider
        // counted, so its 36 tokens still cover the request: the appended
        // "done" adds one token.
        assert_eq!(
            second.estimate, 37,
            "the anchor covers the restored placeholder prefix"
        );
        assert_ne!(
            second.estimate,
            estimate_tokens(&second.messages, &tools),
            "counting the whole text from zero would report the raw result"
        );
    }

    #[test]
    fn an_anchor_counts_only_what_a_matching_prefix_added() {
        let tools = probe_tools();
        let messages = vec![Message::system("sys"), Message::user("hi")];
        let first = project(&messages, &tools, None, 0, 100);
        let anchor = Anchor {
            tokens: 100,
            prefix: messages.iter().map(fingerprinted).collect(),
        };
        let mut next = messages;
        next.push(Message::user("more"));

        let second = project(&next, &tools, Some(anchor), 0, 100);

        assert_eq!(second.estimate, 101, "the anchor covers the prefix");
        assert_ne!(second.estimate, estimate_tokens(&second.messages, &tools));
        assert!(first.estimate < second.estimate - 1);
    }

    #[test]
    fn the_estimate_keeps_the_anchor_when_a_new_result_is_pruned() {
        // The previous request cost the Provider 1,800 input tokens. This step
        // appends four tool results; trimming the oldest leaves 672 characters
        // after the anchored prefix, so the estimate is 1,800 + 168 = 1,968 and
        // the step is over its 1,900-token compaction threshold. Counting the
        // whole text from zero again would hide the overage.
        let tools = probe_tools();
        let mut messages = vec![Message::system("system"), Message::user("go")];
        let anchor = Anchor {
            tokens: 1_800,
            prefix: messages.iter().map(fingerprinted).collect(),
        };
        messages.push(Message::tool_result("call-a", "a".repeat(200)));
        messages.push(Message::tool_result("call-b", "b".repeat(200)));
        messages.push(Message::tool_result("call-c", "c".repeat(205)));
        messages.push(Message::tool_result("call-d", "d".repeat(196)));
        // A window of 2,100 tokens with a 100-token output leaves 2,000 for
        // input: the 2,006-token projection trims once and then fits.
        let projection = project(&messages, &tools, Some(anchor), 2_100, 100);

        assert!(
            projection.messages[2]
                .content
                .as_deref()
                .unwrap()
                .starts_with("[tool result removed from context"),
            "the oldest new result is trimmed: {:?}",
            projection.messages[2].content
        );
        assert_eq!(
            projection.estimate, 1_968,
            "the anchor still covers the prefix before the trimmed result"
        );
        assert!(
            projection.estimate > 1_900,
            "the 1,900-token compaction threshold sees the step as over"
        );
        assert_ne!(
            projection.estimate,
            estimate_tokens(&projection.messages, &tools),
            "counting the whole text again would hide the overage"
        );
    }

    #[test]
    fn a_pruned_message_inside_the_anchor_prefix_counts_from_zero() {
        // The Provider counted 5,000 tokens for exactly these Messages. A trim
        // that replaces the result inside that prefix invalidates the count:
        // the estimate starts over the candidate projection instead.
        let tools = probe_tools();
        let messages = vec![
            Message::system("system"),
            Message::user("u".repeat(3_000)),
            Message::tool_result("call-a", "A".repeat(12_000)),
            Message::tool_result("call-b", "B".repeat(10)),
            Message::tool_result("call-c", "C".repeat(10)),
            Message::tool_result("call-d", "D".repeat(10)),
        ];
        let anchor = Anchor {
            tokens: 5_000,
            prefix: messages.iter().map(fingerprinted).collect(),
        };
        let projection = project(&messages, &tools, Some(anchor), 6_900, 5_000);

        assert!(
            projection.messages[2]
                .content
                .as_deref()
                .unwrap()
                .contains("call call-a"),
            "the old result inside the anchored prefix is trimmed: {:?}",
            projection.messages[2].content
        );
        assert_eq!(
            projection.estimate,
            estimate_tokens(&projection.messages, &tools),
            "a replacement inside the prefix invalidates the anchored count"
        );
        assert!(projection.estimate < 5_000);
    }

    #[test]
    fn a_second_step_can_reuse_the_anchor_after_trimming_the_same_result() {
        // The previous step was sent with its oldest result trimmed to the
        // placeholder this step restores at the same position. The Provider
        // counted 4,350 input tokens for that request; the step appends one
        // call and result (108 characters, 27 tokens), so the candidate is
        // 4,377 tokens: over the 4,364-token threshold. Counting the whole
        // text from zero would report 1,069 and skip the compaction.
        let tools = vec![ToolSpec {
            name: "shell".to_owned(),
            description: "d".repeat(4_000),
            parameters: json!({}),
        }];
        let mut raw = vec![Message::system("system"), Message::user("go")];
        for (id, content) in [
            ("call-a", "A".repeat(300_000)),
            ("call-b", "B".repeat(10)),
            ("call-c", "C".repeat(10)),
            ("call-d", "D".repeat(10)),
        ] {
            raw.push(Message::assistant_with_calls(
                None,
                vec![call(id, "shell", json!({}))],
            ));
            raw.push(Message::tool_result(id, content));
        }
        // The first step's raw text is over the 69,900-token input budget, so
        // its oldest result becomes a placeholder.
        let first = project(&raw, &tools, None, 70_000, 100);
        assert!(
            first.messages[3]
                .content
                .as_deref()
                .unwrap()
                .contains("call call-a"),
            "{:?}",
            first.messages[3].content
        );
        let anchor = Anchor {
            tokens: 4_350,
            prefix: first.messages.iter().map(fingerprinted).collect(),
        };

        // The step appends one more call and result after the sent prefix.
        let mut next = raw;
        next.push(Message::assistant_with_calls(
            None,
            vec![call("call-e", "shell", json!({}))],
        ));
        next.push(Message::tool_result("call-e", "e".repeat(89)));
        let second = project(&next, &tools, Some(anchor.clone()), 70_000, 100);

        assert!(
            second.messages[3]
                .content
                .as_deref()
                .unwrap()
                .contains("call call-a"),
            "the same result is trimmed to the same placeholder: {:?}",
            second.messages[3].content
        );
        assert_eq!(
            fingerprinted(&second.messages[3]),
            anchor.prefix[3],
            "the restored placeholder is the prefix the Provider counted"
        );
        assert_eq!(context_threshold(70_000, 100), Some(4_364));
        assert_eq!(
            second.estimate, 4_377,
            "the Provider's count plus the 27 appended tokens"
        );
        assert!(
            second.estimate > 4_364,
            "the step is over the compaction threshold"
        );
        assert_eq!(
            estimate_tokens(&second.messages, &tools),
            1_069,
            "counting the whole text from zero would hide the overage"
        );
    }

    #[tokio::test]
    async fn a_too_large_failure_in_a_window_with_no_input_space_does_not_compact() {
        // A window smaller than the Model's own output leaves no input space:
        // a Provider error ends the run instead of trying to compact into it.
        let model = FakeModel::with(vec![too_large()]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder {
            compactions: VecDeque::from([Some(compaction(vec![Message::user("s")]))]),
            ..TestRecorder::default()
        };
        let request = RunRequest::new("system", vec![Message::user("x".repeat(400))], 200)
            .with_context_window(100);

        let (text, _, succeeded) =
            reply_outcome(run(&runtime, &model, &mut recorder, request).await);

        assert!(!succeeded);
        assert!(text.contains("larger than the Model's context"), "{text}");
        assert!(
            recorder.compacted.is_empty(),
            "a window with no input space is not compacted"
        );
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_over_budget_projection_compacts_once_with_a_new_prompt() {
        let model = FakeModel::with(vec![reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder {
            compactions: VecDeque::from([Some(Compaction {
                system: Some("system with new notes".to_owned()),
                messages: vec![Message::user("summary")],
            })]),
            ..TestRecorder::default()
        };
        let request =
            RunRequest::new("system", vec![Message::user("go")], 10).with_context_window(100);

        let (text, _, succeeded) =
            reply_outcome(run(&runtime, &model, &mut recorder, request).await);

        assert_eq!(text, "done");
        assert!(succeeded);
        assert_eq!(recorder.compacted.len(), 1, "the step compacted once");
        assert_eq!(recorder.compacted[0].0, 1);
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "the compacted step asked the Model once");
        assert_eq!(
            requests[0].messages[0],
            Message::system("system with new notes"),
            "the new notes reach the next request's system prompt"
        );
        assert_eq!(requests[0].messages[1], Message::user("summary"));
    }

    #[tokio::test]
    async fn a_context_too_large_failure_retries_a_strictly_shorter_projection() {
        let model = FakeModel::with(vec![too_large(), reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder {
            compactions: VecDeque::from([Some(compaction(vec![Message::user("s")]))]),
            ..TestRecorder::default()
        };
        let request = RunRequest::new("system", vec![Message::user("x".repeat(400))], 100)
            .with_context_window(1_000_000);

        let (text, _, succeeded) =
            reply_outcome(run(&runtime, &model, &mut recorder, request).await);

        assert_eq!(text, "done");
        assert!(succeeded);
        assert_eq!(recorder.compacted.len(), 1, "the failure compacted once");
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "the request was retried once");
        assert_eq!(requests[1].messages[1], Message::user("s"));
    }

    #[tokio::test]
    async fn an_unknown_window_still_retries_a_shorter_projection() {
        // Without a known window the loop cannot compact on its estimate, but a
        // Provider that reports the input as too long still gets one attempt.
        let model = FakeModel::with(vec![too_large(), reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder {
            compactions: VecDeque::from([Some(compaction(vec![Message::user("s")]))]),
            ..TestRecorder::default()
        };
        let request = RunRequest::new("system", vec![Message::user("x".repeat(400))], 100);

        let (text, _, succeeded) =
            reply_outcome(run(&runtime, &model, &mut recorder, request).await);

        assert_eq!(text, "done");
        assert!(succeeded);
        assert_eq!(recorder.compacted.len(), 1);
        assert_eq!(model.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_context_too_large_failure_with_a_longer_projection_is_not_retried() {
        let model = FakeModel::with(vec![too_large()]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder {
            compactions: VecDeque::from([Some(compaction(vec![Message::user("summary")]))]),
            ..TestRecorder::default()
        };
        let request = RunRequest::new("system", vec![Message::user("go")], 100)
            .with_context_window(1_000_000);

        let (text, _, succeeded) =
            reply_outcome(run(&runtime, &model, &mut recorder, request).await);

        assert!(!succeeded);
        assert!(text.contains("larger than the Model's context"), "{text}");
        assert_eq!(recorder.compacted.len(), 1, "the loop asked once");
        assert_eq!(
            model.requests.lock().unwrap().len(),
            1,
            "a projection that is not shorter is not retried"
        );
    }

    #[tokio::test]
    async fn a_context_too_large_failure_without_a_cut_ends_the_run() {
        let model = FakeModel::with(vec![too_large()]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();
        let request = RunRequest::new("system", vec![Message::user("go")], 100)
            .with_context_window(1_000_000);

        let (text, _, succeeded) =
            reply_outcome(run(&runtime, &model, &mut recorder, request).await);

        assert!(!succeeded);
        assert!(text.contains("larger than the Model's context"), "{text}");
        assert_eq!(recorder.compacted.len(), 1, "the loop asked once");
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_second_context_too_large_failure_ends_the_run() {
        let model = FakeModel::with(vec![too_large(), too_large()]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder {
            compactions: VecDeque::from([Some(compaction(vec![Message::user("s")]))]),
            ..TestRecorder::default()
        };
        let request = RunRequest::new("system", vec![Message::user("x".repeat(400))], 100)
            .with_context_window(1_000_000);

        let (_, _, succeeded) = reply_outcome(run(&runtime, &model, &mut recorder, request).await);

        assert!(!succeeded);
        assert_eq!(recorder.compacted.len(), 1, "only one compaction and retry");
        assert_eq!(model.requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_provider_error_that_is_not_context_length_does_not_compact() {
        let model = FakeModel::with(vec![Err(ModelFailure {
            code: neha::error::ErrorCode::InvalidArgument,
            detail: "the Provider rejected the request: unsupported context parameter".to_owned(),
            usage: Usage::default(),
            context_too_large: false,
        })]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder::default();
        let request = RunRequest::new("system", vec![Message::user("x".repeat(400))], 100)
            .with_context_window(1_000_000);

        let (text, _, succeeded) =
            reply_outcome(run(&runtime, &model, &mut recorder, request).await);

        assert!(!succeeded);
        assert!(text.contains("unsupported context parameter"), "{text}");
        assert!(
            recorder.compacted.is_empty(),
            "an unrelated failure compacts nothing"
        );
        assert_eq!(model.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_unknown_window_neither_prunes_nor_compacts() {
        let mut long = "x".repeat(40_000);
        let history = vec![Message::user(std::mem::take(&mut long))];
        let model = FakeModel::with(vec![reply("done")]);
        let runtime = FakeRuntime::default();
        let mut recorder = TestRecorder {
            compactions: VecDeque::from([Some(compaction(vec![Message::user("summary")]))]),
            ..TestRecorder::default()
        };
        let request = RunRequest::new("system", history, 100);

        let (text, _, succeeded) =
            reply_outcome(run(&runtime, &model, &mut recorder, request).await);

        assert_eq!(text, "done");
        assert!(succeeded);
        assert!(recorder.compacted.is_empty());
        let requests = model.requests.lock().unwrap();
        assert_eq!(
            requests[0].messages[1].content.as_deref(),
            Some("x".repeat(40_000).as_str())
        );
    }
}

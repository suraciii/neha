export interface Account {
  id: string;
  display_name: string;
  is_admin: boolean;
}

export interface Provider {
  id: string;
  protocol: string;
  base_url: string;
}

export interface Model {
  id: string;
  provider: string;
  name: string;
  context_limit: number;
  max_output: number;
  reads_images: boolean;
  is_default: boolean;
}

export interface Bot {
  id: string;
  owner: string;
  name: string;
  computer: string;
  chat: string;
}

export interface Computer {
  id: string;
  owner: string;
  kind: string;
  online: boolean;
  workspace: string;
}

// What a Chat's active Run is doing, as the Space projects it.
export type ChatStatus = "idle" | "working" | "waiting" | "failed";

export interface Chat {
  id: string;
  owner: string;
  bot: string;
  // The Space projects the Chat's state. A Space that says nothing leaves the
  // Chat idle.
  status?: ChatStatus;
  // The Run the Chat works on, when it has one.
  active_run?: string | null;
}

// One approval the User has not answered yet, as the attention list names it.
// Each pending approval is its own entry: a Chat with two of them lists two.
export interface AttentionEntry {
  approval: Approval;
  chat: Chat;
  botName: string;
  // What the approval asks for, in the record's words.
  detail: string;
}

export interface Author {
  kind: "user" | "bot";
  id: string;
}

export interface Message {
  id: string;
  chat: string;
  author: Author;
  acting: string | null;
  text: string;
  at: string;
  seq: string;
  run: string | null;
  file: string | null;
}

// One File as the client reads it.
export interface FileSummary {
  id: string;
  chat: string;
  name: string;
  media_type: string;
  size: number;
  sha256: string;
  author: Author;
  acting?: string;
  at: string;
  seq: string;
}

export interface ProviderResult {
  provider: Provider;
  models: Model[];
}

export interface BotResult {
  bot: Bot;
  computer: Computer;
  chat: Chat;
}

// How a run is doing, from queued to finished. A running run that asks for
// an approval waits until the User answers.
export type RunStatus = "queued" | "running" | "waiting" | "succeeded" | "failed";

// The command result that starts a run: the Space accepts it into the queue.
export interface SendResult {
  run: string;
  status: "queued";
  position: number;
}

export interface ApiErrorBody {
  code: string;
  message: string;
}

export interface RunView {
  id: string;
  chat: string;
  bot: string;
  acting: string;
  command: string;
  message: string;
  status: RunStatus;
  seq: string;
  // The run's place in its Bot's queue; only a queued run has one.
  position?: number;
  // The approval a waiting run stops for.
  approval?: string | null;
  reply: string | null;
  error: string | null;
  usage: unknown;
  // The steps the run recorded, in order, so a reopened Chat shows what the
  // run already did.
  steps?: RunStepView[];
  // The actions the run recorded, in order.
  actions?: RunActionView[];
}

// One step of a run, as the Space answers run_show.
export interface RunStepView {
  // The step number, starting at one.
  index: number;
  // How many tool calls the step asked for.
  tool_calls: number;
}

// One action of a run, as the Space answers run_show.
export interface RunActionView {
  id: string;
  step: number;
  tool: string;
  // How the action ended.
  status: "succeeded" | "failed" | "timed_out" | "unknown";
  // The failure, when it failed.
  error?: string | null;
}

// Why a Bot asked before an action: the first use of the User's own Computer,
// the first run of a skill's script, or a rule the User set to be asked.
export type ApprovalReason = "first_use_local" | "first_script_run" | "rule" | "grant";

// Whether an action can be undone, as far as the Space knows.
export type Undoable = "no" | "unknown";

// One action a Bot waits to run until the User answers.
export interface Approval {
  approval: string;
  run: string;
  bot: string;
  chat: string;
  action: string;
  step: number;
  tool: string;
  // What the Bot asked for, under the tool's own argument names.
  arguments?: Record<string, unknown> | null;
  summary: string;
  reason: ApprovalReason;
  // No rule could judge this action's form, so the Space asked instead.
  ambiguous?: boolean | null;
  rule?: string | null;
  // Whether the action can be undone, as far as the Space knows.
  undoable?: Undoable | null;
  // What an Always allow would cover, in the Space's words: null when the Space
  // says no lasting rule can come from this action, absent when it says nothing.
  lasting_scope?: string | null;
  // The directory the run works in, when it named one: the action lands there.
  cwd?: string | null;
  // The machine the action would run on, and the User who answers.
  computer?: string | null;
  acting?: string | null;
  // The call's position among its step's calls, and the call's identifier.
  index?: number | null;
  call?: string | null;
  // The User's answer, once given. The Space lists only approvals still waiting.
  decided?: boolean | null;
}

// The answer the Space records for one approval.
export interface Decision {
  approval: string;
  run: string;
  status: "allowed" | "denied";
}

// What the Chat shows while its run has not finished.
export interface RunProgress {
  status: "queued" | "running";
  position?: number;
}

// How one tool action of a Run is doing.
export type ToolStatus = "running" | "succeeded" | "failed" | "unknown";

// One tool action the record reported for the Run the page follows.
export interface ToolAction {
  // The action's identifier.
  action: string;
  // The step that asked for it.
  step: number;
  // The tool's name.
  tool: string;
  // How it ended so far.
  status: ToolStatus;
  // Why it failed, when it did.
  error: string | null;
}

// The Run the page follows: its steps, its tool actions, and the Files it
// produced, while it has any.
export interface RunActivity {
  // The Run this belongs to.
  run: string;
  // The newest step the record reported.
  step: number;
  // The actions the Run took, oldest first.
  actions: ToolAction[];
  // The Files the Run produced, oldest first.
  files: string[];
  // Why the Run failed, when it did.
  error: string | null;
}

export type Preview = { text: string; file: boolean } | { error: true } | null;

// One recorded event as the subscription carries it.
export interface RecordEvent {
  seq: string;
  kind: string;
  version: number;
  at: string;
  author: Author;
  acting?: string;
  command?: string;
  data: Record<string, unknown>;
}

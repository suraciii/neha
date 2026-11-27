export interface Account {
  id: string;
  display_name: string;
  is_admin: boolean;
}

export interface Provider {
  id: string;
  protocol: string;
  base_url: string;
  // The catalog vendor the Provider came from, when it had a preset.
  vendor?: string | null;
}

// How much thought a Model gives a reply.
export type ThinkingLevel = "minimal" | "low" | "medium" | "high" | "max";

export interface Model {
  id: string;
  provider: string;
  name: string;
  context_limit: number;
  max_output: number;
  reads_images: boolean;
  // Whether it can think before it answers.
  thinks: boolean;
  // Whether it can call tools.
  tools: boolean;
  // The thinking levels it accepts; empty means every level.
  thinking_levels: ThinkingLevel[];
  is_default: boolean;
}

export interface Bot {
  id: string;
  owner: string;
  name: string;
  computer: string;
  chat: string;
  // Whether it may write outside its Workspace fence.
  full_access: boolean;
  // Extra folders outside its Workspace it may write.
  grants: string[];
  // Its thinking level; absent uses the Space's default.
  thinking?: ThinkingLevel | null;
  // Its Model; absent uses the Space's default.
  model?: string | null;
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

// The answer a Bot's move carries: where it works now, and where it left.
export interface BotMoveResult {
  bot: Bot;
  from: Computer;
  to: Computer;
}

// What a catalog refresh answers: the cache it read, and whether it changed.
export interface CatalogRefreshResult {
  status: CatalogStatus;
  changed: boolean;
}

// One lasting rule about what a Bot may do without asking.
export interface Rule {
  rule: string;
  bot: string;
  user: string;
  // Whether matching actions ask or run without asking.
  effect: "ask" | "allow";
  tool: string;
  // The shell command prefix a matching shell action starts with.
  command_prefix: string | null;
  // The path prefix a matching file action's path starts with.
  path_prefix: string | null;
}

// One vendor the catalog knows, as the Space names it.
export interface CatalogVendor {
  id: string;
  name: string;
  // The protocol the Space speaks to it, when it can speak one.
  protocol: string | null;
  // Whether the catalog names enough to connect it without overrides.
  selectable: boolean;
  // The base address the catalog names.
  api: string | null;
  // How many Models the vendor offers.
  model_count: number;
}

// The cache state of the Space's catalog.
export interface CatalogStatus {
  // Where the catalog in use came from: the built-in snapshot, or a URL.
  source: string;
  builtin: boolean;
  etag: string | null;
  // When the cached catalog was fetched, in unix seconds.
  fetched_at: number | null;
  // When the cache was last confirmed current, in unix seconds.
  checked_at: number | null;
  // How long ago the cache was last confirmed, in seconds.
  age_seconds: number | null;
  // Whether the cache is older than the refresh interval.
  stale: boolean;
  provider_count: number;
  model_count: number;
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
// or a rule the User set to be asked.
export type ApprovalReason = "first_use_local" | "rule" | "grant";

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

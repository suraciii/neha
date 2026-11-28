// The client's view of the wire.
//
// The shapes the Space sends are generated from the Rust definitions into
// `./generated` and re-exported here under the names the client reads, so a
// wire change starts in Rust. `just wire-types` regenerates the files and
// fails when the committed ones differ.
//
// The types below that are not re-exported have no Rust counterpart: command
// results the Space builds as JSON, the activity the client derives from
// events, and the `run_show` answer, which the Space bounds at read time.

import type { CatalogRefresh, CatalogStatus } from "./generated/catalog";
import type { ErrorCode } from "./generated/error";
import type { Account, Role } from "./generated/identity";
import type {
  ActionStatus,
  Author,
  CardData,
  ComputerSharing,
  MemoryKind,
  MemoryScope,
  MemorySourceKind,
  PlanEntry,
  PlanState,
  RunStatus,
  Undoability,
  ApprovalReason as WireApprovalReason,
  Event as WireEvent,
  EventKind as WireEventKind,
} from "./generated/record";
import type {
  ActionView,
  BotView,
  ChatStatus,
  ChatView,
  ComputerView,
  InvitationView,
  MemoryView,
  MessageView,
  ModelView,
  ProviderView,
  RuleView,
  StepView,
  UserView,
  CreatedInvitation as WireCreatedInvitation,
} from "./generated/state";
import type {
  ClientFrame,
  CommandCall,
  FileSummary,
  ServerFrame,
  ThinkingLevel,
} from "./generated/surface";
import type { Usage } from "./generated/usage";

export type {
  Account,
  ActionStatus,
  Author,
  CatalogRefresh as CatalogRefreshResult,
  CatalogStatus,
  ChatStatus,
  ChatView as Chat,
  ClientFrame,
  CommandCall,
  ComputerSharing,
  ComputerView as Computer,
  ErrorCode,
  FileSummary,
  InvitationView as Invitation,
  MemoryKind,
  MemoryScope,
  MemorySourceKind,
  MemoryView as Memory,
  ModelView as Model,
  PlanEntry,
  PlanState,
  ProviderView as Provider,
  Role,
  RunStatus,
  ServerFrame,
  StepView as RunStepView,
  ThinkingLevel,
  Undoability as Undoable,
  Usage,
  UserView as User,
};

// What the Space's record now says beyond the generated shapes above. The
// fields travel on the wire the same way the generated ones do; the generated
// files are rebuilt from the Rust definitions with `just wire-types`, and until
// then the client states the additions here, beside the shapes they extend.

// The answer of `invitation_create`: the generated invitation and the one-time
// secret its link carries, plus the account that made it. The Space builds the
// answer as JSON, so the addition is stated here.
export type CreatedInvitation = WireCreatedInvitation & { user: string };

// One lasting rule as `rule_list` answers it. The generated view carries the
// identifiers a rule stores; the read adds the names behind them — the skill's
// and the MCP server's — so a reader shows the name it can check against the
// library instead of an identifier.
export interface Rule extends RuleView {
  // The name of the skill a `run_skill_script` rule runs, when the Space can
  // still find it. The record holds the identifier only.
  skill_name?: string | null;
  // The name of the MCP server whose tool a rule matches, when the Space can
  // still find it. The record holds the identifier only.
  server_name?: string | null;
}

// One MCP server as the Integrations view lists it: how the Space reaches it,
// who connected it, and whether a call by the reader would find it.
export interface McpServer {
  server: string;
  name: string;
  // How the Space reaches the server: over Streamable HTTP, or by launching
  // the package's executable on its own machine.
  transport?: "streamable-http" | "stdio";
  // Where the server answers, when it is an HTTP server.
  url: string | null;
  // The executable a stdio server launches, and the arguments it gives it.
  command?: string | null;
  args?: string[];
  // A stdio server's environment. The values are package data, so the view
  // never shows them.
  env?: Record<string, string>;
  // A stdio server's working directory, relative to the root below.
  cwd?: string | null;
  // Whether the working directory hangs from the package's data directory
  // rather than its root. Absent means the package root.
  data_cwd?: boolean;
  // Where the server belongs: the whole Space, or one User.
  scope: "space" | "user";
  // The User whose server it is, when its scope is one User's.
  user: string | null;
  // The account that connected it.
  by: string;
  // That account's display name, when the Space names it.
  by_name?: string;
  // The package that brought it, when one did.
  plugin?: string | null;
  connected: boolean;
  // What a call by the reader would find: a personal credential wins over the
  // Space's, so this names both when both are stored.
  credential?: "none" | "space" | "user" | "space-and-user";
}

// One package an Admin installed, and what it brought.
export interface InstalledPackage {
  plugin: string;
  name: string;
  version: string | null;
  commit: string | null;
  digest: string;
  source: string;
  enabled: boolean;
  // What one server row of the package says: the transport, and the address
  // of an HTTP server or the command of a stdio one.
  servers: {
    server: string;
    name: string;
    transport?: "streamable-http" | "stdio";
    url?: string | null;
    command?: string | null;
  }[];
  skills: { skill: string; name: string }[];
}

// One skill a library holds: the Space's or one Bot's.
export interface Skill {
  id: string;
  name: string;
  description: string;
  body: string;
  scripts: { name: string; sha256: string }[];
  library: "space" | "bot";
  bot: string | null;
  plugin?: string | null;
}

// What became of one MCP browser sign-in, as the client polls it.
export interface McpSignInState {
  sign_in: string;
  state: "pending" | "finished" | "failed";
  server: string | null;
  connected: boolean;
  reason: string | null;
}

// A Bot, a Message, and the Card a tool's result carries: the generated shapes
// already hold the fields the library and Card work added.
export type Bot = BotView;
export type Message = MessageView;
export type Card = CardData;

// The record's event kinds, generated from the Rust definitions.
export type EventKind = WireEventKind;

// One recorded event, with the kinds above. Its `data` is the kind's own
// record, which the page reads by field rather than by one fixed shape.
export type RecordEvent = Omit<WireEvent, "kind"> & { kind: EventKind };

// Why a Bot asked before an action: the first use of the User's own Computer,
// the first run of a skill's script, the first call of an MCP server, or a rule
// the User set to be asked.
export type ApprovalReason = WireApprovalReason;

// The Approval card: `neha.approval.requested` carries the ask, and the
// approval listing adds the answer fields, so the answer fields stay optional
// and nullable: the event spells silence as an absent key, an empty answer as
// null.
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
  undoable?: Undoability | null;
  lasting_scope?: string | null;
  cwd?: string | null;
  computer?: string | null;
  acting?: string | null;
  index?: number | null;
  call?: string | null;
  decided?: boolean | null;
  // The folder a grant answer covers, when the reason is a grant.
  grant_folder?: string | null;
  // The cursor of `approval.requested`.
  seq?: string;
}

// One approval the User has not answered yet, as the attention list names it.
// Each pending approval is its own entry: a Chat with two of them lists two.
export interface AttentionEntry {
  approval: Approval;
  chat: ChatView;
  botName: string;
  // What the approval asks for, in the record's words.
  detail: string;
}

export interface BotResult {
  bot: BotView;
  computer: ComputerView;
  chat: ChatView;
}

// The answer of `bot_create`: the Bot and its Computer, and the Chat the event
// named before any projection of it existed, so the Chat carries identity only.
export interface BotCreateResult {
  bot: BotView;
  computer: ComputerView;
  chat: Pick<ChatView, "id" | "owner" | "bot">;
}

// The answer a Bot's move carries: where it works now, and where it left.
export interface BotMoveResult {
  bot: BotView;
  from: ComputerView;
  to: ComputerView;
}

// One entry of a Bot's memory as `memory_list` names it. The Space builds the
// answer as JSON, so the shape is stated here. An entry the User does not read
// carries no text: it names its metadata only.
export interface MemoryEntry {
  memory: string;
  kind: MemoryKind;
  scope: MemoryScope;
  // The Chat or Group it applies in; null for an owner entry.
  scope_id: string | null;
  author: string;
  // Whether the author is the Bot.
  author_bot: boolean;
  // The Message, run, or command that wrote it.
  source: string;
  source_kind: MemorySourceKind;
  // The entry's text in UTF-8 bytes.
  size: number;
  // Whether the User reads the entry's text.
  readable: boolean;
  // The entry's text, present only when the User reads it.
  text?: string;
}

// The answer of `memory_list`: one Bot's memory, and how many bytes each kind
// holds against its limit.
export interface MemoryList {
  entries: MemoryEntry[];
  used: Record<MemoryKind, number>;
  limits: Record<MemoryKind, number>;
}

// One vendor the catalog knows, as `catalog_list` names it. The command builds
// the answer as JSON, so the shape is stated here.
export interface CatalogVendor {
  id: string;
  name: string;
  npm: string;
  // The protocol the Space speaks to it, when it can speak one.
  protocol: string | null;
  // Whether the catalog names enough to connect it without overrides.
  selectable: boolean;
  // The base address the catalog names.
  api: string | null;
  // The environment variables that hold the vendor's API key.
  env: string[];
  doc: string | null;
  // How many Models the vendor offers.
  model_count: number;
}

// The command result that starts a run: the Space accepts it into the queue.
export interface SendResult {
  run: string;
  status: "queued";
  position: number;
  // The directory the run works in, when the request named one.
  cwd?: string;
}

export interface ApiErrorBody {
  // The Space's stable code, or the client's name for a failure that arrived
  // without one.
  code: ErrorCode | "http_error";
  message: string;
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

// The Run the page follows: its steps, its tool actions, its plan, and the
// Files it produced, while it has any.
export interface RunActivity {
  // The Run this belongs to.
  run: string;
  // The newest step the record reported.
  step: number;
  // The actions the Run took, oldest first.
  actions: ToolAction[];
  // The plan the Run holds now, oldest entry first; empty when it wrote none or
  // cleared it.
  plan: PlanEntry[];
  // The Files the Run produced, oldest first.
  files: string[];
  // Why the Run failed, when it did.
  error: string | null;
}

// How many entries of a Run's plan are done, out of all of them. A Chat list row
// shows it beside its Working mark.
export interface PlanProgress {
  done: number;
  total: number;
}

// One read of a Run's plan, and the plan word it was sent against. An answer
// whose Run gained a newer word, or a newer read, while it was on the way must
// not put an older list back.
export interface PlanRead {
  run: string;
  read: number;
  word: number;
}

export type Preview = { text: string; file: boolean; card?: boolean } | { error: true } | null;

// The `run_show` answer. The Space starts from the projection's RunView and
// adds a queued run's position; a very large run is bounded, so its actions
// keep a summary and `truncated` is set.
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
  // The directory the run works in, when it named one.
  cwd?: string | null;
  // The write folder allowed for the next action, when an answered grant set one.
  once_grant?: string | null;
  reply: string | null;
  error: string | null;
  usage: Usage | null;
  // The steps the run recorded, in order, so a reopened Chat shows what the
  // run already did.
  steps?: StepView[];
  // The actions the run recorded, in order.
  actions?: RunActionView[];
  // The plan the run holds now, oldest entry first; empty when it wrote none or
  // cleared it.
  plan: PlanEntry[];
  // Whether the answer was bounded; a bounded answer's actions may lack an
  // output or arguments.
  truncated?: boolean;
}

// One action of a run, as the Space answers run_show. A bounded answer keeps
// only the summary of an action, so its arguments and counters may be absent.
export type RunActionView = Omit<ActionView, "output" | "arguments" | "omitted_bytes"> & {
  output?: string | null;
  arguments?: unknown;
  omitted_bytes?: number;
};

// The HTTP bodies of the push endpoints. They are not the command surface: the
// Space declares them beside the routes in `server.rs`, as private structs the
// client never shares, so the client states them here.
export interface PushStatus {
  // The public VAPID key, and the Space's current mute.
  key: string;
  muted_until: number | null;
}

// One browser subscription's address and encryption keys.
export interface PushSubscriptionBody {
  endpoint: string;
  keys: { p256dh: string; auth: string };
}

export interface PushMute {
  muted_until: number | null;
}

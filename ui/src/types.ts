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

export interface Chat {
  id: string;
  owner: string;
  bot: string;
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
}

// Why a Bot asked before an action: the first use of the User's own Computer,
// or a rule the User set to be asked.
export type ApprovalReason = "first_use_local" | "rule";

// One action a Bot waits to run until the User answers.
export interface Approval {
  approval: string;
  run: string;
  bot: string;
  chat: string;
  action: string;
  step: number;
  tool: string;
  summary: string;
  reason: ApprovalReason;
  rule?: string | null;
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

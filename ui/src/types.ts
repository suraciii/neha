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

export interface SendResult {
  run: string;
  message: string;
  status: "succeeded" | "failed";
  reply: string;
  text: string;
  error?: string;
  usage?: unknown;
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
  status: "running" | "succeeded" | "failed";
  reply: string | null;
  error: string | null;
  usage: unknown;
}

export type Preview = { text: string } | { error: true } | null;

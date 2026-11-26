import type { RecordEvent } from "../types";
import { uuidv7 } from "./uuid";

export class ProtocolError extends Error {
  readonly code: string;

  constructor(code: string, message: string) {
    super(message);
    this.name = "ProtocolError";
    this.code = code;
  }
}

interface Pending {
  resolve: (value: unknown) => void;
  reject: (reason: unknown) => void;
  timer: number;
}

interface Frame {
  type: string;
  id?: string;
  result?: unknown;
  code?: string;
  message?: string;
  event?: RecordEvent;
  events?: RecordEvent[];
  cursor?: string;
}

/** What the caller wants to know about the record. */
export interface SubscriptionHandlers {
  /** A page of history. `resumed` is true when the caller asked to continue. */
  onSnapshot?: (events: RecordEvent[], cursor: string, resumed: boolean) => void;
  /** One live event. */
  onEvent?: (event: RecordEvent) => void;
  /** The cursor advanced. Persist it to resume after a reload. */
  onCursor?: (cursor: string) => void;
  /** The subscription could not continue. */
  onError?: (error: ProtocolError) => void;
}

/** Where a subscription starts. */
export interface SubscribeOptions {
  /** The cursor to resume from; omit to read the record from the start. */
  cursor?: string | null;
  /** The page size to ask the Space for. */
  limit?: number;
}

type CommandName =
  | "provider_add"
  | "provider_list"
  | "model_list"
  | "bot_create"
  | "bot_list"
  | "bot_show"
  | "chat_list"
  | "chat_read"
  | "file_list"
  | "message_send"
  | "run_show";

interface Following {
  handlers: SubscriptionHandlers;
  cursor: string | null;
  startedWithCursor: boolean;
  frameSent: boolean;
  limit: number;
}

export class Commands {
  private socket: WebSocket | null = null;
  private connecting: Promise<void> | null = null;
  private hello: Promise<void> | null = null;
  private pending = new Map<string, Pending>();
  private following: Following | null = null;
  private reconnectTimer: number | null = null;
  private closed = false;

  async close(): Promise<void> {
    this.closed = true;
    this.following = null;
    if (this.reconnectTimer !== null) {
      window.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    this.socket?.close();
    this.socket = null;
    this.rejectAll(new ProtocolError("closed", "The Space connection is closed."));
  }

  /**
   * Follows the record from a cursor. The Space sends a snapshot and then live
   * events; reconnecting resumes from the last cursor.
   */
  async subscribe(
    handlers: SubscriptionHandlers,
    options: SubscribeOptions = {},
  ): Promise<void> {
    this.closed = false;
    this.following = {
      handlers,
      cursor: options.cursor ?? null,
      startedWithCursor: Boolean(options.cursor),
      frameSent: false,
      limit: options.limit ?? 200,
    };
    await this.ensureConnected();
    await this.hello;
    this.sendSubscribe();
  }

  async command<T>(name: CommandName, args?: unknown, timeoutMs = 20_000): Promise<T> {
    await this.ensureConnected();
    const socket = this.socket;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      throw new ProtocolError("closed", "The Space connection is not open.");
    }
    await this.hello;
    const id = uuidv7();
    return new Promise<T>((resolve, reject) => {
      const timer = window.setTimeout(() => {
        this.pending.delete(id);
        reject(new ProtocolError("timeout", "The Space did not answer in time."));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (value) => resolve(value as T),
        reject,
        timer,
      });
      socket.send(JSON.stringify(commandFrame(id, name, args)));
    });
  }

  private sendSubscribe(): void {
    const following = this.following;
    const socket = this.socket;
    if (
      !following ||
      following.frameSent ||
      !socket ||
      socket.readyState !== WebSocket.OPEN
    ) {
      return;
    }
    following.frameSent = true;
    const frame: Record<string, unknown> = {
      type: "subscribe",
      id: uuidv7(),
      limit: following.limit,
    };
    if (following.cursor !== null) frame.cursor = following.cursor;
    socket.send(JSON.stringify(frame));
  }

  private async ensureConnected(): Promise<void> {
    if (this.socket?.readyState === WebSocket.OPEN) return;
    if (this.connecting) return this.connecting;
    this.connecting = new Promise<void>((resolve, reject) => {
      const scheme = location.protocol === "https:" ? "wss:" : "ws:";
      const socket = new WebSocket(`${scheme}//${location.host}/api/socket`);
      this.socket = socket;
      let settled = false;
      const timer = window.setTimeout(() => {
        fail(new ProtocolError("hello_timeout", "The Space did not open the session."));
      }, 10_000);
      const succeed = () => {
        if (settled) return;
        settled = true;
        window.clearTimeout(timer);
        resolve();
      };
      const fail = (error: Error) => {
        if (settled) return;
        settled = true;
        window.clearTimeout(timer);
        this.rejectAll(error);
        reject(error);
        if (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING) {
          socket.close();
        }
      };
      socket.addEventListener("open", () => {
        if (this.following) this.following.frameSent = false;
        socket.send(JSON.stringify({ type: "hello", version: 1 }));
        this.hello = new Promise<void>((helloResolve, helloReject) => {
          const onHello = (event: MessageEvent) => {
            const frame = parse(event);
            if (frame?.type === "hello") {
              socket.removeEventListener("message", onHello);
              helloResolve();
            } else if (frame?.type === "protocol_error") {
              socket.removeEventListener("message", onHello);
              helloReject(new ProtocolError("protocol", frame.message ?? "The Space rejected the hello frame."));
            }
          };
          socket.addEventListener("message", onHello);
        });
        this.hello.then(() => {
          this.sendSubscribe();
          succeed();
        }, fail);
      });
      socket.addEventListener("message", (event) => {
        const frame = parse(event);
        if (!frame) return;
        if (frame.type === "hello") return;
        if (frame.type === "accepted" || frame.type === "committed") return;
        if (frame.type === "heartbeat") return;
        if (frame.type === "snapshot") {
          const following = this.following;
          if (!following) return;
          const cursor = frame.cursor ?? following.cursor ?? "";
          if (cursor) following.cursor = cursor;
          const resumed = following.startedWithCursor;
          following.handlers.onSnapshot?.(frame.events ?? [], cursor, resumed);
          if (cursor) following.handlers.onCursor?.(cursor);
          following.startedWithCursor = true;
          return;
        }
        if (frame.type === "event") {
          const following = this.following;
          if (!following || !frame.event) return;
          following.cursor = frame.event.seq;
          following.startedWithCursor = true;
          following.handlers.onEvent?.(frame.event);
          following.handlers.onCursor?.(frame.event.seq);
          return;
        }
        if (frame.type === "confirmed" || frame.type === "error") {
          if (frame.type === "error" && !frame.id) {
            this.following?.handlers.onError?.(
              new ProtocolError(frame.code ?? "error", frame.message ?? "The subscription failed."),
            );
            socket.close();
            return;
          }
          const id = frame.id;
          if (!id) return;
          const pending = this.pending.get(id);
          if (!pending) return;
          this.pending.delete(id);
          window.clearTimeout(pending.timer);
          if (frame.type === "confirmed") pending.resolve(frame.result);
          else pending.reject(new ProtocolError(frame.code ?? "error", frame.message ?? "The command failed."));
          return;
        }
        if (frame.type === "protocol_error") {
          const error = new ProtocolError("protocol", frame.message ?? "The Space rejected a frame.");
          this.following?.handlers.onError?.(error);
          this.following = null;
          this.rejectAll(error);
          socket.close();
        }
      });
      socket.addEventListener("close", () => {
        this.socket = null;
        this.connecting = null;
        this.hello = null;
        if (this.following) this.following.frameSent = false;
        this.rejectAll(new ProtocolError("closed", "The Space connection closed."));
        if (!settled) {
          settled = true;
          window.clearTimeout(timer);
          reject(new ProtocolError("closed", "The Space connection closed before the session opened."));
        }
        this.scheduleReconnect();
      });
      socket.addEventListener("error", () => {
        fail(new ProtocolError("network", "The browser cannot reach the Space."));
      });
    }).finally(() => {
      this.connecting = null;
    });
    return this.connecting;
  }

  private scheduleReconnect(): void {
    if (this.closed || !this.following || this.reconnectTimer !== null) return;
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = null;
      if (this.closed || !this.following) return;
      void this.ensureConnected().catch(() => this.scheduleReconnect());
    }, 1_000);
  }

  private rejectAll(error: Error): void {
    for (const [id, pending] of this.pending) {
      this.pending.delete(id);
      window.clearTimeout(pending.timer);
      pending.reject(error);
    }
  }
}

export function commandFrame(id: string, name: CommandName, args?: unknown): unknown {
  return {
    type: "command",
    id,
    command: args === undefined ? { name } : { name, args },
  };
}

function parse(event: MessageEvent): Frame | null {
  try {
    return JSON.parse(String(event.data)) as Frame;
  } catch {
    return null;
  }
}

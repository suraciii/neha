import type { RecordEvent } from "../types";
import { uuidv7 } from "./uuid";

/**
 * The protocol version this client speaks. It states the version in its hello
 * frame, and the Space refuses a session that greets with another version.
 */
export const PROTOCOL_VERSION = 4;

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

// The Space's `CommandCall` is an adjacently tagged enum: `name` is the tag and
// `args` is the content. A unit variant has no content, so its frame must leave
// `args` off; every other variant has a content object, so its frame must state
// one. These two lists mirror every variant of `CommandCall`
// (crates/neha/src/surface.rs), and `commandPayload` is their only reader, so
// the client never guesses a shape from a caller.
const UNIT_COMMAND_NAMES = [
  "catalog_list",
  "catalog_status",
  "catalog_refresh",
  "provider_list",
  "model_list",
  "computer_list",
  "bot_list",
  "chat_list",
  "mcp_list",
  "plugin_list",
  "user_list",
  "invitation_create",
  "invitation_list",
] as const;

const ARGUMENT_COMMAND_NAMES = [
  "provider_add",
  "provider_connect",
  "provider_refresh",
  "computer_add",
  "computer_reset",
  "computer_revoke",
  "computer_create",
  "device_approve",
  "computer_update",
  "bot_create",
  "bot_update",
  "bot_move",
  "bot_show",
  "file_list",
  "chat_read",
  "message_send",
  "run_show",
  "approval_list",
  "approval_decide",
  "model_default",
  "rule_list",
  "rule_add",
  "rule_remove",
  "memory_list",
  "memory_remove",
  "skill_add",
  "skill_list",
  "skill_remove",
  "bot_skill_enable",
  "bot_skill_disable",
  "mcp_add",
  "mcp_remove",
  "mcp_connect",
  "mcp_sign_in_start",
  "mcp_sign_in_status",
  "mcp_sign_in_finish",
  "bot_mcp_enable",
  "bot_mcp_disable",
  "plugin_install",
  "plugin_remove",
  "plugin_enable",
  "plugin_disable",
  "invitation_revoke",
  "user_set_role",
  "user_transfer",
] as const;

/** A command whose frame carries no `args` object. */
export type UnitCommandName = (typeof UNIT_COMMAND_NAMES)[number];
/** A command whose frame carries an `args` object. */
export type ArgumentCommandName = (typeof ARGUMENT_COMMAND_NAMES)[number];
/** Every command the Space's decoder knows. */
export type CommandName = UnitCommandName | ArgumentCommandName;

/** The production commands a frame must leave `args` off. */
export const UNIT_COMMANDS: ReadonlySet<string> = new Set(UNIT_COMMAND_NAMES);

const ARGUMENT_COMMANDS: ReadonlySet<string> = new Set(ARGUMENT_COMMAND_NAMES);

/**
 * The command payload of a frame, exactly as the Space's decoder reads it: a
 * unit command carries no `args`, every other command carries its object. A
 * name the Space does not know is a client bug, not a shape to guess, so it
 * throws instead of picking one.
 */
export function commandPayload(name: string, args?: unknown): Record<string, unknown> {
  if (UNIT_COMMANDS.has(name)) {
    if (args !== undefined) {
      throw new Error(`the Space's ${name} carries no args`);
    }
    return { name };
  }
  if (ARGUMENT_COMMANDS.has(name)) {
    if (typeof args !== "object" || args === null || Array.isArray(args)) {
      throw new Error(`the Space's ${name} requires an args object`);
    }
    return { name, args };
  }
  throw new Error(`the Space knows no command named ${name}`);
}

interface Following {
  handlers: SubscriptionHandlers;
  cursor: string | null;
  /** True until the first live event; every page of the opening snapshot is initial. */
  initialSnapshot: boolean;
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
    this.connecting = null;
    this.hello = null;
    this.rejectAll(new ProtocolError("closed", "The Space connection is closed."));
  }

  /**
   * Follows the record from a cursor. The Space sends a snapshot and then live
   * events; reconnecting resumes from the last cursor.
   */
  async subscribe(handlers: SubscriptionHandlers, options: SubscribeOptions = {}): Promise<void> {
    this.closed = false;
    this.following = {
      handlers,
      cursor: options.cursor ?? null,
      initialSnapshot: !options.cursor,
      frameSent: false,
      limit: options.limit ?? 200,
    };
    await this.ensureConnected();
    await this.hello;
    this.sendSubscribe();
  }

  /**
   * Sends one command and resolves with its answer. A unit command names no
   * arguments; every other command names its object. `timeoutMs` is the wait
   * for that answer; a command whose Space-side work is slow names its own.
   */
  async command<T>(name: UnitCommandName): Promise<T>;
  async command<T>(name: ArgumentCommandName, args: object, timeoutMs?: number): Promise<T>;
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
    if (!following || following.frameSent || !socket || socket.readyState !== WebSocket.OPEN) {
      return;
    }
    following.frameSent = true;
    following.initialSnapshot = following.cursor === null;
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

    const connecting = new Promise<void>((resolve, reject) => {
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
        if (this.socket !== socket) {
          socket.close();
          reject(error);
          return;
        }
        this.rejectAll(error);
        reject(error);
        if (socket.readyState === WebSocket.OPEN || socket.readyState === WebSocket.CONNECTING) {
          socket.close();
        }
      };
      socket.addEventListener("open", () => {
        if (this.socket !== socket) {
          fail(new ProtocolError("closed", "A newer Space connection replaced this one."));
          socket.close();
          return;
        }
        if (this.following) this.following.frameSent = false;
        // The client states the Presentation kinds it reads. A Space sends the
        // memory, compaction, and plan events only to a client that names their
        // capabilities, so an older client that stays silent never receives
        // them.
        socket.send(
          JSON.stringify({
            type: "hello",
            version: PROTOCOL_VERSION,
            capabilities: ["memory", "compaction", "plan"],
          }),
        );
        this.hello = new Promise<void>((helloResolve, helloReject) => {
          const onHello = (event: MessageEvent) => {
            if (this.socket !== socket) return;
            const frame = parse(event);
            if (frame?.type === "hello") {
              socket.removeEventListener("message", onHello);
              helloResolve();
            } else if (frame?.type === "protocol_error") {
              socket.removeEventListener("message", onHello);
              helloReject(
                new ProtocolError(
                  "protocol",
                  frame.message ?? "The Space rejected the hello frame.",
                ),
              );
            }
          };
          socket.addEventListener("message", onHello);
        });
        this.hello.then(() => {
          if (this.socket !== socket) {
            fail(new ProtocolError("closed", "A newer Space connection replaced this one."));
            return;
          }
          this.sendSubscribe();
          succeed();
        }, fail);
      });
      socket.addEventListener("message", (event) => {
        if (this.socket !== socket) return;
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
          const resumed = !following.initialSnapshot;
          following.handlers.onSnapshot?.(frame.events ?? [], cursor, resumed);
          if (cursor) following.handlers.onCursor?.(cursor);
          return;
        }
        if (frame.type === "event") {
          const following = this.following;
          if (!following || !frame.event) return;
          following.cursor = frame.event.seq;
          following.initialSnapshot = false;
          following.handlers.onEvent?.(frame.event);
          following.handlers.onCursor?.(frame.event.seq);
          return;
        }
        if (frame.type === "confirmed" || frame.type === "error") {
          if (frame.type === "error" && !frame.id) {
            this.following?.handlers.onError?.(
              new ProtocolError(frame.code ?? "error", frame.message ?? "The subscription failed."),
            );
            // Keep the subscription so the close schedules a resume from the cursor.
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
          else
            pending.reject(
              new ProtocolError(frame.code ?? "error", frame.message ?? "The command failed."),
            );
          return;
        }
        if (frame.type === "protocol_error") {
          const error = new ProtocolError(
            "protocol",
            frame.message ?? "The Space rejected a frame.",
          );
          this.following?.handlers.onError?.(error);
          this.following = null;
          this.rejectAll(error);
          socket.close();
        }
      });
      socket.addEventListener("close", () => {
        if (this.socket !== socket) {
          if (!settled) {
            settled = true;
            window.clearTimeout(timer);
            reject(new ProtocolError("closed", "A newer Space connection replaced this one."));
          }
          return;
        }
        this.socket = null;
        this.connecting = null;
        this.hello = null;
        if (this.following) this.following.frameSent = false;
        this.rejectAll(new ProtocolError("closed", "The Space connection closed."));
        if (!settled) {
          settled = true;
          window.clearTimeout(timer);
          reject(
            new ProtocolError("closed", "The Space connection closed before the session opened."),
          );
        }
        this.scheduleReconnect();
      });
      socket.addEventListener("error", () => {
        if (this.socket !== socket) {
          socket.close();
          return;
        }
        fail(new ProtocolError("network", "The browser cannot reach the Space."));
      });
    });
    let guarded: Promise<void>;
    guarded = connecting.finally(() => {
      if (this.connecting === guarded) this.connecting = null;
    });
    this.connecting = guarded;
    return guarded;
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

// The frame one command travels in. The Space's decoder refuses a unit command
// that names an `args` object and an argument command that names none, so every
// frame is shaped by `commandPayload` from the decoder's own classification.
export function commandFrame(id: string, name: string, args?: unknown): unknown {
  return { type: "command", id, command: commandPayload(name, args) };
}

function parse(event: MessageEvent): Frame | null {
  try {
    return JSON.parse(String(event.data)) as Frame;
  } catch {
    return null;
  }
}

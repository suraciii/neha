import { afterEach, describe, expect, it, vi } from "vitest";

import { Commands, type SubscriptionHandlers } from "./ws";

type Listener = (event: { data?: string }) => void;

// The protocol version the Space speaks, as `PROTOCOL_VERSION` in
// `crates/neha/src/surface.rs` states it. The fake Space refuses a hello that
// greets with any other version, so a stale browser version fails these tests.
const SERVER_PROTOCOL_VERSION = 3;

class FakeSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSED = 3;
  static instances: FakeSocket[] = [];
  static pages: Array<{ cursor: string }> = [{ cursor: "5" }];
  // The version the fake Space speaks; a test sets another to model a Space
  // that is not the one this client expects.
  static accepts = SERVER_PROTOCOL_VERSION;

  readyState = FakeSocket.CONNECTING;
  readonly sent: Array<Record<string, unknown>> = [];
  private listeners = new Map<string, Set<Listener>>();

  constructor() {
    FakeSocket.instances.push(this);
    queueMicrotask(() => {
      this.readyState = FakeSocket.OPEN;
      this.emit("open");
    });
  }

  addEventListener(type: string, listener: Listener) {
    let listeners = this.listeners.get(type);
    if (!listeners) {
      listeners = new Set();
      this.listeners.set(type, listeners);
    }
    listeners.add(listener);
  }

  removeEventListener(type: string, listener: Listener) {
    this.listeners.get(type)?.delete(listener);
  }

  send(raw: string) {
    const frame = JSON.parse(raw) as Record<string, unknown>;
    this.sent.push(frame);
    if (frame.type === "hello") {
      queueMicrotask(() => {
        if (frame.version !== FakeSocket.accepts) {
          this.message({
            type: "protocol_error",
            message: `this Space speaks protocol version ${FakeSocket.accepts}`,
          });
          return;
        }
        this.message({ type: "hello", version: FakeSocket.accepts });
      });
    }
    if (frame.type === "subscribe") {
      for (const page of FakeSocket.pages) {
        queueMicrotask(() => this.message({ type: "snapshot", cursor: page.cursor, events: [] }));
      }
    }
  }

  closeLater() {
    this.readyState = 2;
  }

  close() {
    this.readyState = FakeSocket.CLOSED;
    this.emit("close");
  }

  message(value: unknown) {
    this.emit("message", { data: JSON.stringify(value) });
  }

  private emit(type: string, event: { data?: string } = {}) {
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
}

afterEach(() => {
  vi.unstubAllGlobals();
  FakeSocket.instances = [];
  FakeSocket.pages = [{ cursor: "5" }];
  FakeSocket.accepts = SERVER_PROTOCOL_VERSION;
});

describe("record subscription", () => {
  it("greets the Space with the protocol version it speaks", async () => {
    vi.stubGlobal("WebSocket", FakeSocket);
    const handlers: SubscriptionHandlers = { onSnapshot: vi.fn() };
    const commands = new Commands();

    await commands.subscribe(handlers);
    const socket = FakeSocket.instances[0]!;
    expect(socket.sent.find((frame) => frame.type === "hello")).toEqual({
      type: "hello",
      version: SERVER_PROTOCOL_VERSION,
      capabilities: ["memory", "compaction"],
    });
    await commands.close();
  });

  it("refuses a Space that answers another protocol version", async () => {
    FakeSocket.accepts = SERVER_PROTOCOL_VERSION - 1;
    vi.stubGlobal("WebSocket", FakeSocket);
    const handlers: SubscriptionHandlers = { onSnapshot: vi.fn(), onError: vi.fn() };
    const commands = new Commands();

    await expect(commands.subscribe(handlers)).rejects.toBeInstanceOf(Error);
    expect(handlers.onError).toHaveBeenCalledWith(
      expect.objectContaining({ name: "ProtocolError", code: "protocol" }),
    );
    expect(handlers.onSnapshot).not.toHaveBeenCalled();
    await commands.close();
  });

  it("resumes from a cursor and reports live events", async () => {
    vi.stubGlobal("WebSocket", FakeSocket);
    const handlers: SubscriptionHandlers = {
      onSnapshot: vi.fn(),
      onEvent: vi.fn(),
      onCursor: vi.fn(),
    };
    const commands = new Commands();

    await commands.subscribe(handlers, { cursor: "4" });
    await Promise.resolve();
    const socket = FakeSocket.instances[0]!;
    // The client declares the Presentation kinds it reads, so the Space sends
    // it the memory and compaction events; an older client that stays silent
    // receives none.
    expect(socket.sent.find((frame) => frame.type === "hello")).toMatchObject({
      version: SERVER_PROTOCOL_VERSION,
      capabilities: ["memory", "compaction"],
    });
    expect(socket.sent.find((frame) => frame.type === "subscribe")).toMatchObject({
      cursor: "4",
    });
    expect(handlers.onSnapshot).toHaveBeenCalledWith([], "5", true);
    expect(handlers.onCursor).toHaveBeenCalledWith("5");

    socket.message({
      type: "event",
      event: {
        seq: "6",
        kind: "neha.message.added",
        version: 1,
        at: "2026-01-01T00:00:00.000Z",
        author: { kind: "user", id: "u1" },
        data: { chat: "h1", text: "hello" },
      },
    });
    await Promise.resolve();
    expect(handlers.onEvent).toHaveBeenCalledWith(
      expect.objectContaining({ seq: "6", kind: "neha.message.added" }),
    );
    expect(handlers.onCursor).toHaveBeenLastCalledWith("6");
    await commands.close();
  });

  it("ignores a delayed close from a socket a newer one replaced", async () => {
    vi.stubGlobal("WebSocket", FakeSocket);
    const handlers: SubscriptionHandlers = { onSnapshot: vi.fn() };
    const commands = new Commands();

    await commands.subscribe(handlers, { cursor: "2" });
    await Promise.resolve();
    const first = FakeSocket.instances[0]!;
    first.closeLater();
    await commands.subscribe(handlers, { cursor: "5" });
    await Promise.resolve();
    const second = FakeSocket.instances[1];
    expect(second).toBeDefined();
    first.close();
    expect(second!.readyState).toBe(FakeSocket.OPEN);
    expect(second!.sent.some((frame) => frame.type === "subscribe")).toBe(true);
    await commands.close();
  });

  it("reconnects after an id-less subscription error", async () => {
    vi.useFakeTimers();
    try {
      vi.stubGlobal("WebSocket", FakeSocket);
      const handlers: SubscriptionHandlers = { onSnapshot: vi.fn(), onError: vi.fn() };
      const commands = new Commands();

      await commands.subscribe(handlers, { cursor: "2" });
      await Promise.resolve();
      const first = FakeSocket.instances[0]!;
      first.message({ type: "error", code: "internal", message: "read failed" });

      await vi.advanceTimersByTimeAsync(1_000);
      await vi.advanceTimersByTimeAsync(0);
      const second = FakeSocket.instances[1];
      expect(second).toBeDefined();
      expect(second!.sent.find((frame) => frame.type === "subscribe")).toMatchObject({
        cursor: "5",
      });
      await commands.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("resubscribes from the last cursor after the socket closes", async () => {
    vi.useFakeTimers();
    try {
      vi.stubGlobal("WebSocket", FakeSocket);
      const handlers: SubscriptionHandlers = { onSnapshot: vi.fn() };
      const commands = new Commands();

      await commands.subscribe(handlers, { cursor: "2" });
      await Promise.resolve();
      const first = FakeSocket.instances[0]!;
      first.close();
      await vi.advanceTimersByTimeAsync(1_000);
      await vi.advanceTimersByTimeAsync(0);

      const second = FakeSocket.instances[1];
      expect(second).toBeDefined();
      expect(second!.sent.find((frame) => frame.type === "subscribe")).toMatchObject({
        cursor: "5",
      });
      await commands.close();
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps every page of the opening snapshot initial", async () => {
    FakeSocket.pages = [{ cursor: "4" }, { cursor: "5" }];
    vi.stubGlobal("WebSocket", FakeSocket);
    const handlers: SubscriptionHandlers = { onSnapshot: vi.fn() };
    const commands = new Commands();

    await commands.subscribe(handlers);
    await Promise.resolve();
    await Promise.resolve();
    expect(handlers.onSnapshot).toHaveBeenNthCalledWith(1, [], "4", false);
    expect(handlers.onSnapshot).toHaveBeenNthCalledWith(2, [], "5", false);
    await commands.close();
  });

  it("starts at the beginning when no cursor exists", async () => {
    vi.stubGlobal("WebSocket", FakeSocket);
    const handlers: SubscriptionHandlers = {
      onSnapshot: vi.fn(),
    };
    const commands = new Commands();

    await commands.subscribe(handlers);
    await Promise.resolve();
    const socket = FakeSocket.instances[0]!;
    expect(socket.sent.find((frame) => frame.type === "subscribe")).not.toHaveProperty("cursor");
    expect(handlers.onSnapshot).toHaveBeenCalledWith([], "5", false);
    await commands.close();
  });
});

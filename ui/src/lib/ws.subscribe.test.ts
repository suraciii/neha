import { afterEach, describe, expect, it, vi } from "vitest";

import { Commands, type SubscriptionHandlers } from "./ws";

type Listener = (event: { data?: string }) => void;

class FakeSocket {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSED = 3;
  static instances: FakeSocket[] = [];

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
      queueMicrotask(() => this.message({ type: "hello", version: 1 }));
    }
    if (frame.type === "subscribe") {
      queueMicrotask(() =>
        this.message({ type: "snapshot", cursor: "5", events: [] }),
      );
    }
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
});

describe("record subscription", () => {
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

  it("starts at the beginning when no cursor exists", async () => {
    vi.stubGlobal("WebSocket", FakeSocket);
    const handlers: SubscriptionHandlers = {
      onSnapshot: vi.fn(),
    };
    const commands = new Commands();

    await commands.subscribe(handlers);
    await Promise.resolve();
    const socket = FakeSocket.instances[0]!;
    expect(socket.sent.find((frame) => frame.type === "subscribe")).not.toHaveProperty(
      "cursor",
    );
    expect(handlers.onSnapshot).toHaveBeenCalledWith([], "5", false);
    await commands.close();
  });
});

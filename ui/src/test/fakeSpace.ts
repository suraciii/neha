// A Space that answers every command from memory and records what it was asked.
// Tests import `space` to set up and read what happened, and hand
// `FakeCommands` to the client in place of its WebSocket.
import type { SubscriptionHandlers } from "../lib/ws";
import type { Approval, Chat, FileSummary, Message, RunView } from "../types";

const account = { id: "u1", display_name: "Ada", is_admin: true };
const bot = { id: "b1", owner: "u1", name: "Scout", computer: "p1", chat: "h1" };
const computer = { id: "p1", owner: "u1", kind: "local", online: true, workspace: "/w" };
const chat = { id: "h1", owner: "u1", bot: "b1" };
const otherChat = { id: "h2", owner: "u1", bot: "b1" };
// A send or a decision can be held open, so a test can act while the Space
// has not answered yet.
let sendGate: Promise<void> | null = null;
let openSend: (() => void) | null = null;
let decideGate: Promise<void> | null = null;
let openDecide: (() => void) | null = null;

export const space = {
  account,
  bot,
  computer,
  chat,
  otherChat,
  asked: [] as string[],
  sent: [] as unknown[],
  files: [] as FileSummary[],
  messages: [] as Message[],
  otherMessages: [] as Message[],
  chats: [chat] as Chat[],
  chatDelay: null as null | ((chatId: string) => Promise<void>),
  handlers: null as SubscriptionHandlers | null,
  // What the record says about each Run. A Run this map omits has succeeded.
  runs: {} as Record<string, { status: RunView["status"]; position?: number; approval?: string }>,
  runShows: [] as string[],
  // A run_show answer can be held open, so a test can act while it is in flight.
  runHolds: {} as Record<string, { gate: Promise<void>; release: () => void; fail: () => void } | undefined>,
  // Sends that settled in the fake Space.
  sendsDone: 0,
  // Runs the Space accepted, in order.
  sends: 0,
  // The approvals the Space lists for the Chat: the ones still waiting.
  approvals: [] as Approval[],
  // The decisions the client sent, in order.
  decisions: [] as { approval: string; allow: boolean; lasting: boolean }[],
  // Decisions that settled in the fake Space.
  decidesDone: 0,
  // The seq of the last event the client read.
  seq: 100,
  // Puts the Space back where a test expects to find it.
  reset() {
    space.releaseSends();
    space.releaseDecide();
    space.computer.kind = "local";
    space.asked = [];
    space.sent = [];
    space.files = [];
    space.messages = [];
    space.otherMessages = [];
    space.chats = [chat];
    space.chatDelay = null;
    space.handlers = null;
    space.runs = {};
    space.runShows = [];
    space.runHolds = {};
    space.sendsDone = 0;
    space.sends = 0;
    space.approvals = [];
    space.decisions = [];
    space.decidesDone = 0;
    space.seq = 100;
  },
  holdSends() {
    sendGate = new Promise<void>((resolve) => {
      openSend = resolve;
    });
  },
  releaseSends() {
    openSend?.();
    openSend = null;
    sendGate = null;
  },
  holdDecide() {
    decideGate = new Promise<void>((resolve) => {
      openDecide = resolve;
    });
  },
  releaseDecide() {
    openDecide?.();
    openDecide = null;
    decideGate = null;
  },
  holdRunShow(run: string) {
    let release!: () => void;
    let fail!: () => void;
    const gate = new Promise<void>((resolve, reject) => {
      release = resolve;
      fail = () => reject(new Error("the run_show answer was lost"));
    });
    space.runHolds[run] = { gate, release, fail };
  },
};

export class FakeCommands {
  async subscribe(handlers: SubscriptionHandlers): Promise<void> {
    space.handlers = handlers;
  }

  async close(): Promise<void> {
    space.handlers = null;
  }

  async command<T>(name: string, args?: unknown): Promise<T> {
    space.asked.push(name);
    switch (name) {
      case "provider_list":
        return [{ id: "v1", protocol: "chat_completions", base_url: "http://provider" }] as T;
      case "model_list":
        return [] as T;
      case "bot_list":
        return [space.bot] as T;
      case "chat_list":
        return [...space.chats] as T;
      case "chat_read": {
        const asked = args as { chat: string };
        await space.chatDelay?.(asked.chat);
        return (asked.chat === space.chat.id ? [...space.messages] : [...space.otherMessages]) as T;
      }
      case "file_list":
        return [...space.files] as T;
      case "approval_list":
        // Like the Space's JSON, each answer carries its own copies.
        return space.approvals.map((approval) => ({ ...approval })) as T;
      case "bot_show":
        return { bot: space.bot, computer: space.computer, chat: space.chat } as T;
      case "message_send": {
        if (sendGate) await sendGate;
        try {
          const sent = args as { chat: string; text: string; file?: string };
          space.sent.push(sent);
          space.sends += 1;
          const run = `r${space.sends}`;
          // The place sits behind every Run its Bot has not finished.
          const active = Object.values(space.runs).filter(
            (known) => known.status === "queued" || known.status === "running",
          ).length;
          const position = active + 1;
          space.messages.push({
            id: `m${space.messages.length + 1}`,
            chat: sent.chat,
            author: { kind: "user", id: "u1" },
            acting: null,
            text: sent.text,
            at: "2026-01-01T00:00:00.000Z",
            seq: "3",
            run,
            file: sent.file ?? null,
          });
          space.runs[run] = { status: "queued", position };
          return { run, status: "queued", position } as T;
        } finally {
          space.sendsDone += 1;
        }
      }
      case "approval_decide": {
        const decided = args as { approval: string; allow: boolean; lasting: boolean };
        space.decisions.push(decided);
        try {
          if (decideGate) await decideGate;
          const waiting = space.approvals.find((known) => known.approval === decided.approval);
          if (waiting) space.approvals = space.approvals.filter((known) => known !== waiting);
          return {
            approval: decided.approval,
            run: waiting?.run ?? "r0",
            status: decided.allow ? "allowed" : "denied",
          } as T;
        } finally {
          space.decidesDone += 1;
        }
      }
      case "run_show": {
        const requested = (args as { run: string }).run;
        space.runShows.push(requested);
        const hold = space.runHolds[requested];
        if (hold) await hold.gate;
        const known = space.runs[requested] ?? { status: "succeeded" };
        return {
          id: requested,
          chat: space.chat.id,
          bot: space.bot.id,
          acting: "u1",
          command: "c1",
          message: "m1",
          status: known.status,
          seq: "4",
          position: known.position,
          approval: known.status === "waiting" ? (known.approval ?? null) : null,
          reply: null,
          error: null,
          usage: null,
        } as T;
      }
      default:
        throw new Error(`the fake Space cannot answer ${name}`);
    }
  }
}

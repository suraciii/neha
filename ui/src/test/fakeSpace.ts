// A Space that answers every command from memory and records what it was asked.
// Tests import `space` to set up and read what happened, and hand
// `FakeCommands` to the client in place of its WebSocket.
import type { SubscriptionHandlers } from "../lib/ws";
import type { Approval, Chat, ChatStatus, FileSummary, Message, RecordEvent, RunView } from "../types";

const account = { id: "u1", display_name: "Ada", is_admin: true };
const bot = { id: "b1", owner: "u1", name: "Scout", computer: "p1", chat: "h1" };
const otherBot = { id: "b2", owner: "u1", name: "Quill", computer: "p1", chat: "h2" };
const thirdBot = { id: "b3", owner: "u1", name: "Sage", computer: "p1", chat: "h3" };
const computer = { id: "p1", owner: "u1", kind: "local", online: true, workspace: "/w" };
const chat = { id: "h1", owner: "u1", bot: "b1" };
const otherChat = { id: "h2", owner: "u1", bot: "b2" };
const thirdChat = { id: "h3", owner: "u1", bot: "b3" };

// The Bot that owns a Chat, like the Space answers bot_show.
function botFor(id: string) {
  if (id === otherBot.id) return otherBot;
  if (id === thirdBot.id) return thirdBot;
  return bot;
}
// One Chat as the Space projects it: its own state, and the Run it follows.
function project(known: Chat): Chat {
  const status = space.statuses[known.id];
  const active = space.activeRuns[known.id];
  return {
    ...known,
    ...(status === undefined ? {} : { status }),
    ...(active === undefined ? {} : { active_run: active }),
  };
}
// A send or a decision can be held open, so a test can act while the Space
// has not answered yet.
let sendGate: Promise<void> | null = null;
let openSend: (() => void) | null = null;
let decideGate: Promise<void> | null = null;
let openDecide: (() => void) | null = null;
let approvalGate: Promise<void> | null = null;
let openApproval: (() => void) | null = null;

export const space = {
  account,
  bot,
  otherBot,
  thirdBot,
  computer,
  chat,
  otherChat,
  thirdChat,
  asked: [] as string[],
  sent: [] as unknown[],
  files: [] as FileSummary[],
  messages: [] as Message[],
  otherMessages: [] as Message[],
  chats: [chat] as Chat[],
  // What the Space projects each Chat to be doing. A Chat it omits is idle.
  statuses: {} as Record<string, ChatStatus>,
  // The Run each Chat works on, as the Space projects it: the waiting Run while
  // it waits, else the oldest queued or running Run, else none.
  activeRuns: {} as Record<string, string | null>,
  chatDelay: null as null | ((chatId: string) => Promise<void>),
  handlers: null as SubscriptionHandlers | null,
  // What the record says about each Run. A Run this map omits has succeeded.
  // A Run can also carry the steps and actions the record holds for it, which
  // is how a reopened Chat shows what the Run already did.
  runs: {} as Record<
    string,
    {
      status: RunView["status"];
      position?: number;
      approval?: string;
      steps?: { index: number; tool_calls: number }[];
      actions?: { id: string; step: number; tool: string; status: string; error?: string | null }[];
      // Why the Run failed, when it did before any step or action.
      error?: string | null;
    }
  >,
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
    space.releaseApprovals();
    space.computer.kind = "local";
    space.asked = [];
    space.sent = [];
    space.files = [];
    space.messages = [];
    space.otherMessages = [];
    space.chats = [chat];
    space.statuses = {};
    space.activeRuns = {};
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
  // An approval read can be held open, so a test can answer while the Space's
  // word about the record is still in flight.
  holdApprovals() {
    approvalGate = new Promise<void>((resolve) => {
      openApproval = resolve;
    });
  },
  releaseApprovals() {
    openApproval?.();
    openApproval = null;
    approvalGate = null;
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
  // Lets every read held about one Run answer, after the page moved on.
  releaseRunShow(run: string) {
    space.runHolds[run]?.release();
    space.runHolds[run] = undefined;
  },
  // A page that reconnects reads a snapshot of the events it missed, and the
  // Space says the cursor it now holds. This is what a real reconnect delivers.
  resume(events: RecordEvent[]) {
    space.seq += 1;
    space.handlers?.onSnapshot?.(events, `s${space.seq}`, true);
  },
};

// One event the Space writes to the record, delivered live to the page that
// follows it. The commands that change the record write their event, so a page
// hears about its own answer the same way it hears about everyone else's.
function emit(kind: string, data: Record<string, unknown>) {
  space.seq += 1;
  space.handlers?.onEvent?.({
    seq: String(space.seq),
    kind,
    version: 1,
    at: "2026-01-01T00:00:00.000Z",
    author: { kind: "user", id: "u1" },
    acting: "u1",
    data,
  });
}

export class FakeCommands {
  async subscribe(handlers: SubscriptionHandlers): Promise<void> {
    space.handlers = handlers;
    // A real Space answers a subscribe with one snapshot frame, which a page
    // that asked for no cursor reads as its first page of the record.
    await Promise.resolve();
    handlers.onSnapshot?.([], "", false);
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
        // Each Chat's Bot answers, like the Space lists them.
        return space.chats.map((known) => botFor(known.bot)) as T;
      case "chat_list":
        // Like the Space's JSON, each answer carries its own copies, with what
        // the Space projects the Chat to be doing and the Run it follows.
        return space.chats.map((known) => project(known)) as T;
      case "chat_read": {
        const asked = args as { chat: string };
        await space.chatDelay?.(asked.chat);
        return (asked.chat === space.chat.id ? [...space.messages] : [...space.otherMessages]) as T;
      }
      case "file_list":
        return [...space.files] as T;
      case "approval_list": {
        // Like the Space's JSON, each answer carries its own copies, and names
        // only the approvals the User may still answer: the asked Chat's, or
        // every Chat's when the call names none.
        const asked = args as { chat?: string };
        if (approvalGate) await approvalGate;
        return space.approvals
          .filter((approval) => asked.chat === undefined || approval.chat === asked.chat)
          .map((approval) => ({ ...approval })) as T;
      }
      case "bot_show": {
        const asked = args as { bot: string };
        const known = botFor(asked.bot);
        const own = space.chats.find((entry) => entry.bot === known.id) ?? space.chat;
        return { bot: known, computer: space.computer, chat: project(own) } as T;
      }
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
          // The Space writes the decision to the record, so the page hears
          // about its own answer the same way it hears about any other.
          emit("neha.approval.decided", {
            approval: decided.approval,
            run: waiting?.run ?? "r0",
            allow: decided.allow,
            lasting: decided.lasting,
            user: "u1",
            rule: null,
          });
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
          error: known.error ?? null,
          usage: null,
          steps: known.steps ?? [],
          actions: known.actions ?? [],
        } as T;
      }
      default:
        throw new Error(`the fake Space cannot answer ${name}`);
    }
  }
}

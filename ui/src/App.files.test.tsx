import { cleanup, fireEvent, render, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import App from "./App";
import { api, ApiError } from "./lib/api";
import type { SubscriptionHandlers } from "./lib/ws";
import { MAX_FILE_BYTES } from "./lib/files";
import { setLocale } from "./i18n";
import type { Account, Chat, FileSummary, Message, RunView } from "./types";

// A Space that answers every command from memory and records what it was asked.
const { space, FakeCommands } = vi.hoisted(() => {
  const account = { id: "u1", display_name: "Ada", is_admin: true };
  const bot = { id: "b1", owner: "u1", name: "Scout", computer: "p1", chat: "h1" };
  const computer = { id: "p1", owner: "u1", kind: "managed", online: true, workspace: "/w" };
  const chat = { id: "h1", owner: "u1", bot: "b1" };
  const otherChat = { id: "h2", owner: "u1", bot: "b1" };
  // A send can be held open, so a test can act while the Space has not
  // accepted the Message yet.
  let sendGate: Promise<void> | null = null;
  let openSend: (() => void) | null = null;
  const space = {
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
    runs: {} as Record<string, { status: RunView["status"]; position?: number }>,
    runShows: [] as string[],
    // Sends that settled in the fake Space.
    sendsDone: 0,
    // The seq of the last event the client read.
    seq: 100,
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
  };
  class FakeCommands {
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
        case "bot_show":
          return { bot: space.bot, computer: space.computer, chat: space.chat } as T;
        case "message_send": {
          if (sendGate) await sendGate;
          try {
            const sent = args as { chat: string; text: string; file?: string };
            space.sent.push(sent);
            // The Space records the Message with the Run it starts and confirms
            // the place the Run holds at once.
            space.messages.push({
              id: `m${space.messages.length + 1}`,
              chat: sent.chat,
              author: { kind: "user", id: "u1" },
              acting: null,
              text: sent.text,
              at: "2026-01-01T00:00:00.000Z",
              seq: "3",
              run: "r1",
              file: sent.file ?? null,
            });
            space.runs["r1"] = { status: "queued", position: 2 };
            return { run: "r1", status: "queued", position: 2 } as T;
          } finally {
            space.sendsDone += 1;
          }
        }
        case "run_show": {
          const requested = (args as { run: string }).run;
          space.runShows.push(requested);
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
  return { space, FakeCommands };
});

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: FakeCommands,
}));

const notes: FileSummary = {
  id: "f1",
  chat: "h1",
  name: "notes.txt",
  media_type: "text/plain",
  size: 5,
  sha256: "ab".repeat(32),
  author: { kind: "user", id: "u1" },
  at: "2026-01-01T00:00:00.000Z",
  seq: "2",
};

function element<T extends Element>(view: ReturnType<typeof render>, selector: string): T {
  const found = view.container.querySelector<T>(selector);
  if (!found) throw new Error(`no ${selector} in the view`);
  return found;
}

function renderApp() {
  vi.spyOn(api, "me").mockResolvedValue(space.account as Account);
  return render(() => <App />);
}

async function openChat() {
  const view = renderApp();
  await waitFor(() => expect(view.container.querySelector("form.composer")).toBeTruthy());
  await waitFor(() => expect(space.asked).toContain("bot_show"));
  await waitFor(() => expect(space.handlers).not.toBeNull());
  flush();
  return view;
}

function attach(view: ReturnType<typeof render>, file: File) {
  fireEvent.change(element<HTMLInputElement>(view, 'input[type="file"]'), {
    target: { files: [file] },
  });
}

function alertText(view: ReturnType<typeof render>): string | undefined {
  return view.container.querySelector('[role="alert"]')?.textContent ?? undefined;
}

// What the Chat says about the Run it waits for.
function queueText(view: ReturnType<typeof render>): string | undefined {
  return view.container.querySelector(".composer__queue")?.textContent ?? undefined;
}

function type(view: ReturnType<typeof render>, text: string) {
  flush(() => fireEvent.input(element<HTMLTextAreaElement>(view, "textarea"), { target: { value: text } }));
}

function submit(view: ReturnType<typeof render>) {
  fireEvent.submit(element(view, "form.composer"));
}

// One event the page follows from the record.
function record(kind: string, data: Record<string, unknown>) {
  space.seq += 1;
  space.handlers?.onEvent?.({
    seq: String(space.seq),
    kind,
    version: 1,
    at: "2026-01-01T00:00:00.000Z",
    author: { kind: "bot", id: "b1" },
    acting: "u1",
    data,
  });
}

// Ends a Run in the record, with the reply Message when it has one.
function finishRun(run: string, reply: Message | null = null) {
  space.runs[run] = { status: "succeeded" };
  if (reply) {
    space.messages.push(reply);
    record("neha.message.added", { chat: reply.chat, message: reply.id, text: reply.text, file: null, run });
  }
  record("neha.run.finished", { run, status: "succeeded", reply: reply?.id ?? null, usage: null, error: null });
}

// A second Chat on the sidebar whose last Run waits at place 3.
function waitingChat() {
  space.chats = [space.chat, space.otherChat];
  space.otherMessages.push({
    id: "m9",
    chat: "h2",
    author: { kind: "user", id: "u1" },
    acting: null,
    text: "earlier",
    at: "2026-01-01T00:00:00.000Z",
    seq: "2",
    run: "r2",
    file: null,
  });
  space.runs["r2"] = { status: "queued", position: 3 };
}

async function openWaitingChat(view: ReturnType<typeof render>) {
  fireEvent.click(element<HTMLButtonElement>(view, ".conv:nth-of-type(2)"));
  await waitFor(() => expect(queueText(view)).toBe("Waiting behind 2 requests"));
}

beforeAll(() => {
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: false,
      media: query,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
    }),
  });
});

beforeEach(() => {
  space.releaseSends();
  space.asked = [];
  space.sent = [];
  space.files = [];
  space.messages = [];
  space.otherMessages = [];
  space.chats = [space.chat];
  space.chatDelay = null;
  space.handlers = null;
  space.runs = {};
  space.runShows = [];
  space.sendsDone = 0;
  space.seq = 100;
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  flush(() => setLocale("en-US"));
});

describe("App with Files", () => {
  it("shows a File the Bot delivered with its name, size, and download", async () => {
    space.files.push({
      ...notes,
      id: "f9",
      name: "report.pdf",
      media_type: "application/pdf",
      size: 2_048_000,
      author: { kind: "bot", id: "b1" },
      acting: "u1",
    });
    space.messages.push({
      id: "m1",
      chat: "h1",
      author: { kind: "bot", id: "b1" },
      acting: "u1",
      text: "Here it is.",
      at: "2026-01-01T00:00:00.000Z",
      seq: "3",
      run: null,
      file: "f9",
    });
    const view = await openChat();

    const card = element<HTMLAnchorElement>(view, ".file-card");
    expect(card.getAttribute("href")).toBe("/api/files/f9");
    expect(card.textContent).toContain("report.pdf");
    expect(card.textContent).toContain("2.0 MiB");
    expect(view.container.querySelector(".msg__text")?.textContent).toBe("Here it is.");
  });

  it("uploads the file before sending the Message, which then shows its card", async () => {
    const view = await openChat();
    const upload = vi.spyOn(api, "uploadFile").mockImplementation(async () => {
      space.asked.push("upload");
      space.files.push(notes);
      return notes;
    });
    const file = new File(["hello"], "notes.txt", { type: "text/plain" });
    attach(view, file);
    await waitFor(() => expect(view.container.querySelector(".attachment")?.textContent).toContain("5 B"));

    submit(view);
    await waitFor(() => expect(view.container.querySelector(".file-card")).toBeTruthy());
    // The Message is accepted at once, and its Run holds the Chat until it ends.
    await waitFor(() => expect(queueText(view)).toBe("Waiting behind 1 request"));
    finishRun("r1");
    await waitFor(() => expect(view.container.querySelector(".msg--working")).toBeNull());

    expect(upload).toHaveBeenCalledWith("h1", file, "notes.txt", "text/plain");
    expect(space.sent).toEqual([{ chat: "h1", text: "", file: "f1" }]);
    expect(space.asked.indexOf("upload")).toBeLessThan(space.asked.indexOf("message_send"));
    const card = element<HTMLAnchorElement>(view, ".file-card");
    expect(card.getAttribute("href")).toBe("/api/files/f1");
    expect(card.textContent).toContain("notes.txt");
    expect(card.textContent).toContain("5 B");
    // A File alone is a Message, without an empty text.
    expect(view.container.querySelectorAll(".msg__text")).toHaveLength(0);
    expect(view.container.querySelector(".attachment")).toBeNull();
  });

  it("keeps the Message and says why when the Space refuses the upload", async () => {
    const view = await openChat();
    vi.spyOn(api, "uploadFile").mockRejectedValue(
      new ApiError(413, "invalid_argument", "the File is too large"),
    );
    attach(view, new File(["hello"], "notes.txt", { type: "text/plain" }));
    await waitFor(() => expect(view.container.querySelector(".attachment")).toBeTruthy());
    type(view, "please summarize");
    submit(view);

    await waitFor(() => expect(alertText(view)).toBe("notes.txt is larger than 32 MiB."));
    // Nothing left the composer, so it still holds the Message.
    await waitFor(() => expect(element<HTMLTextAreaElement>(view, "textarea").value).toBe("please summarize"));
    expect(view.container.querySelector(".attachment")?.textContent).toContain("notes.txt");
    expect(space.sent).toEqual([]);
  });

  it("never uploads a file larger than the limit", async () => {
    const view = await openChat();
    const upload = vi.spyOn(api, "uploadFile");
    attach(view, new File([new Uint8Array(MAX_FILE_BYTES + 1)], "big.bin"));
    await waitFor(() => expect(view.container.querySelector(".attachment")).toBeTruthy());
    submit(view);

    await waitFor(() => expect(alertText(view)).toBe("big.bin is larger than 32 MiB."));
    expect(upload).not.toHaveBeenCalled();
    expect(space.sent).toEqual([]);
  });

  it("names any other upload failure, in the reader's language", async () => {
    const view = await openChat();
    vi.spyOn(api, "uploadFile").mockRejectedValue(new ApiError(500, "internal", "boom"));
    attach(view, new File(["hello"], "notes.txt"));
    await waitFor(() => expect(view.container.querySelector(".attachment")).toBeTruthy());
    submit(view);

    await waitFor(() => expect(alertText(view)).toBe("The file could not be uploaded. Try again."));
    expect(space.sent).toEqual([]);
    await waitFor(() => expect(view.container.querySelector(".attachment")?.textContent).toContain("notes.txt"));

    flush(() => fireEvent.click(element(view, ".locale-switch")));
    expect(alertText(view)).toBe("文件上传失败，请重试。");
  });

  it("keeps a live preview for an inactive Chat while previews load", async () => {
    space.chats = [space.chat, space.otherChat];
    space.otherMessages.push({
      id: "old",
      chat: "h2",
      author: { kind: "bot", id: "b1" },
      acting: "u1",
      text: "old preview",
      at: "2026-01-01T00:00:00.000Z",
      seq: "2",
      run: null,
      file: null,
    });
    let release: (() => void) | undefined;
    space.chatDelay = (chatId) =>
      chatId === "h2" ? new Promise<void>((resolve) => (release = resolve)) : Promise.resolve();

    const view = renderApp();
    await waitFor(() => expect(space.handlers).not.toBeNull());
    space.handlers?.onEvent?.({
      seq: "3",
      kind: "neha.message.added",
      version: 1,
      at: "2026-01-01T00:00:01.000Z",
      author: { kind: "bot", id: "b1" },
      data: { chat: "h2", text: "new live", file: null, run: null },
    });

    await waitFor(() => expect(release).toBeDefined());
    release?.();
    await waitFor(() => expect(view.container.querySelector("form.composer")).toBeTruthy());
    await waitFor(() => expect(view.container.textContent).toContain("new live"));
    expect(view.container.textContent).not.toContain("old preview");
  });

  it("keeps a live preview when a later load races an existing preview", async () => {
    space.chats = [space.chat, space.otherChat];
    space.otherMessages.push({
      id: "old",
      chat: "h2",
      author: { kind: "bot", id: "b1" },
      acting: "u1",
      text: "old preview",
      at: "2026-01-01T00:00:00.000Z",
      seq: "2",
      run: null,
      file: null,
    });
    const view = await openChat();
    await waitFor(() => expect(view.container.textContent).toContain("old preview"));

    let release: (() => void) | undefined;
    let h2Reads = 0;
    space.chatDelay = (chatId) => {
      if (chatId !== "h2") return Promise.resolve();
      h2Reads += 1;
      return h2Reads === 1 ? new Promise<void>((resolve) => (release = resolve)) : Promise.resolve();
    };
    space.handlers?.onEvent?.({
      seq: "3",
      kind: "neha.bot.created",
      version: 1,
      at: "2026-01-01T00:00:01.000Z",
      author: { kind: "user", id: "u1" },
      data: { bot: "b1", chat: "h1", owner: "u1", computer: "p1" },
    });
    await waitFor(() => expect(release).toBeDefined());
    space.handlers?.onEvent?.({
      seq: "4",
      kind: "neha.message.added",
      version: 1,
      at: "2026-01-01T00:00:02.000Z",
      author: { kind: "bot", id: "b1" },
      data: { chat: "h2", text: "new live", file: null, run: null },
    });
    release?.();

    await waitFor(() => expect(view.container.textContent).toContain("new live"));
    expect(view.container.textContent).not.toContain("old preview");
  });

  it("does not unlock the active Chat when another Run finishes", async () => {
    space.messages.push({
      id: "m1",
      chat: "h1",
      author: { kind: "user", id: "u1" },
      acting: null,
      text: "please work",
      at: "2026-01-01T00:00:00.000Z",
      seq: "3",
      run: null,
      file: null,
    });
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(element(view, ".msg--working")).toBeTruthy());

    record("neha.run.finished", { run: "another-run", status: "failed", error: "nope" });
    expect(element(view, ".msg--working")).toBeTruthy();

    record("neha.run.finished", { run: "r1", status: "failed", error: "nope" });
    await waitFor(() => expect(view.container.querySelector(".msg--working")).toBeNull());
  });

  it("refreshes the Chat from a live record event", async () => {
    const view = await openChat();
    expect(space.handlers).not.toBeNull();
    space.messages.push({
      id: "m-live",
      chat: "h1",
      author: { kind: "bot", id: "b1" },
      acting: "u1",
      text: "Live reply",
      at: "2026-01-01T00:00:01.000Z",
      seq: "8",
      run: null,
      file: null,
    });
    space.handlers?.onEvent?.({
      seq: "8",
      kind: "neha.message.added",
      version: 1,
      at: "2026-01-01T00:00:01.000Z",
      author: { kind: "bot", id: "b1" },
      data: { chat: "h1", message: "m-live", text: "Live reply", file: null, run: null },
    });

    await waitFor(() => expect(view.container.textContent).toContain("Live reply"));
  });

  it("shows how many requests are ahead while the Run waits", async () => {
    const view = await openChat();
    // A record read can no longer repair the view, so only record events move it.
    space.chatDelay = () => new Promise<void>(() => undefined);
    type(view, "hello");
    submit(view);

    // The command result names the place the Message holds in its Bot's queue.
    await waitFor(() => expect(queueText(view)).toBe("Waiting behind 1 request"));
    expect(element<HTMLButtonElement>(view, ".composer__attach").disabled).toBe(true);

    // The record holds a newer place than the command result did.
    space.runs["r1"] = { status: "queued", position: 4 };
    record("neha.run.queued", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(queueText(view)).toBe("Waiting behind 3 requests"));

    // A Run ahead of it ended, so the page reads the place again from the record.
    space.runs["r1"] = { status: "queued", position: 1 };
    record("neha.run.finished", { run: "r0", status: "succeeded" });
    await waitFor(() => expect(queueText(view)).toBe("Waiting in the queue"));
  });

  it("restores a waiting Run when its Chat opens", async () => {
    space.messages.push({
      id: "m1",
      chat: "h1",
      author: { kind: "user", id: "u1" },
      acting: null,
      text: "earlier",
      at: "2026-01-01T00:00:00.000Z",
      seq: "1",
      run: "r9",
      file: null,
    });
    space.runs["r9"] = { status: "queued", position: 3 };
    const view = await openChat();

    // A waiting Run is unfinished: its place shows and the composer stays shut.
    await waitFor(() => expect(queueText(view)).toBe("Waiting behind 2 requests"));
    expect(element(view, ".msg--working")).toBeTruthy();
    expect(element<HTMLButtonElement>(view, ".composer__attach").disabled).toBe(true);

    finishRun("r9");
    await waitFor(() => expect(queueText(view)).toBeUndefined());
    await waitFor(() => expect(view.container.querySelector(".msg--working")).toBeNull());
    expect(element<HTMLButtonElement>(view, ".composer__attach").disabled).toBe(false);
  });

  it("shows the Bot working once the waiting Run starts", async () => {
    const view = await openChat();
    const reply: Message = {
      id: "m2",
      chat: "h1",
      author: { kind: "bot", id: "b1" },
      acting: "u1",
      text: "Here is the answer.",
      at: "2026-01-01T00:00:01.000Z",
      seq: "5",
      run: "r1",
      file: null,
    };
    type(view, "hello");
    submit(view);
    await waitFor(() => expect(queueText(view)).toBe("Waiting behind 1 request"));

    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(queueText(view)).toBe("Working"));
    expect(element(view, ".msg--working")).toBeTruthy();

    finishRun("r1", reply);
    await waitFor(() => expect(queueText(view)).toBeUndefined());
    await waitFor(() => expect(view.container.querySelector(".msg--working")).toBeNull());
    expect(element<HTMLButtonElement>(view, ".composer__attach").disabled).toBe(false);
    await waitFor(() => expect(view.container.textContent).toContain("Here is the answer."));
  });

  it("keeps the new Chat's place while an old Message is still being accepted", async () => {
    waitingChat();
    const view = await openChat();

    space.holdSends();
    type(view, "to the first Chat");
    submit(view);
    await waitFor(() => expect(space.asked).toContain("message_send"));

    // Switch to the waiting Chat while the first Chat's Message is in flight.
    await openWaitingChat(view);
    const readsBefore = space.runShows.filter((run) => run === "r2").length;

    // A record read can no longer repair the view, so only the record events
    // that belong to this Chat may change it from here on.
    space.chatDelay = (chatId) => (chatId === "h2" ? new Promise<void>(() => undefined) : Promise.resolve());
    space.releaseSends();
    await waitFor(() => expect(space.sendsDone).toBe(1));

    // The old Chat's accepted Run must not replace the new Chat's place.
    expect(queueText(view)).toBe("Waiting behind 2 requests");

    // A Run of this Bot ended, so the new Chat's place improved: read it once.
    space.runs["r2"] = { status: "queued", position: 1 };
    record("neha.run.finished", { run: "r0", status: "succeeded" });
    await waitFor(() => expect(queueText(view)).toBe("Waiting in the queue"));
    expect(space.runShows.filter((run) => run === "r2").length).toBeGreaterThan(readsBefore);
    expect(alertText(view)).toBeUndefined();
  });

  it("previews a Message that carries only a File, in the reader's language", async () => {
    space.messages.push({
      id: "m1",
      chat: "h1",
      author: { kind: "user", id: "u1" },
      acting: null,
      text: "",
      at: "2026-01-01T00:00:00.000Z",
      seq: "1",
      run: null,
      file: "f1",
    });
    const view = await openChat();

    await waitFor(() =>
      expect(element<HTMLElement>(view, ".conv__preview").textContent).toBe("Sent a File"),
    );
    flush(() => fireEvent.click(element(view, ".locale-switch--foot")));
    expect(element<HTMLElement>(view, ".conv__preview").textContent).toBe("已发送文件");
  });
});

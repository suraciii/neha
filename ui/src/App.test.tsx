import { cleanup, fireEvent, render, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import App from "./App";
import { api, ApiError } from "./lib/api";
import { MAX_FILE_BYTES } from "./lib/files";
import { setLocale } from "./i18n";
import type { Account, Bot, Chat, RunView, FileSummary, Message } from "./types";

// A Space that answers every command from memory and records what it was asked.
const { space, FakeCommands } = vi.hoisted(() => {
  const account = { id: "u1", display_name: "Ada", is_admin: true };
  const bot = { id: "b1", owner: "u1", name: "Scout", computer: "p1", chat: "h1" };
  const computer = { id: "p1", owner: "u1", kind: "managed", online: true, workspace: "/w" };
  const chat = { id: "h1", owner: "u1", bot: "b1" };
  // A send can be held open, so a test can act while the Space has not
  // accepted the Message yet.
  let sendGate: Promise<void> | null = null;
  let openSend: (() => void) | null = null;
  const space = {
    account,
    bot,
    computer,
    chat,
    extraBots: [] as Bot[],
    extraChats: [] as Chat[],
    asked: [] as string[],
    sent: [] as unknown[],
    files: [] as FileSummary[],
    messages: [] as Message[],
    runs: [] as { status: RunView["status"]; position?: number; reply?: Message }[],
    // Runs that are always reported queued at this place.
    queued: {} as Record<string, number>,
    runShows: [] as string[],
    // The next send is refused instead of accepted.
    failSend: false,
    // Sends that settled in the fake Space.
    sendsDone: 0,
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
    async command<T>(name: string, args?: unknown): Promise<T> {
      space.asked.push(name);
      switch (name) {
        case "provider_list":
          return [{ id: "v1", protocol: "chat_completions", base_url: "http://provider" }] as T;
        case "model_list":
          return [] as T;
        case "bot_list":
          return [space.bot, ...space.extraBots] as T;
        case "chat_list":
          return [space.chat, ...space.extraChats] as T;
        case "chat_read": {
          const read = args as { chat: string };
          return space.messages.filter((message) => message.chat === read.chat) as T;
        }
        case "file_list": {
          const listed = args as { chat: string };
          return space.files.filter((file) => file.chat === listed.chat) as T;
        }
        case "bot_show": {
          const shown = args as { bot: string };
          const shownBot =
            [space.bot, ...space.extraBots].find((candidate) => candidate.id === shown.bot) ?? space.bot;
          const shownChat =
            [space.chat, ...space.extraChats].find((candidate) => candidate.bot === shownBot.id) ?? space.chat;
          return { bot: shownBot, computer: space.computer, chat: shownChat } as T;
        }
        case "message_send": {
          if (sendGate) await sendGate;
          try {
            if (space.failSend) throw new Error("the fake Space refused the Message");
            const sent = args as { chat: string; text: string; file?: string };
            space.sent.push(sent);
            space.messages.push({
              id: `m${space.messages.length + 1}`,
              chat: sent.chat,
              author: { kind: "user", id: "u1" },
              acting: null,
              text: sent.text,
              at: "2026-01-01T00:00:00.000Z",
              seq: "3",
              run: null,
              file: sent.file ?? null,
            });
            return { run: "r1", status: "queued", position: 2 } as T;
          } finally {
            space.sendsDone += 1;
          }
        }
        case "run_show": {
          const requested = (args as { run: string }).run;
          space.runShows.push(requested);
          const heldPosition = space.queued[requested];
          if (heldPosition !== undefined) {
            return {
              id: requested,
              chat: space.chat.id,
              bot: space.bot.id,
              acting: space.account.id,
              command: "c1",
              message: "m1",
              status: "queued",
              seq: "4",
              position: heldPosition,
              reply: null,
              error: null,
              usage: null,
            } as T;
          }
          const scripted = space.runs.shift();
          // A reply Message appears in the Chat when its run finishes.
          if (scripted?.reply) space.messages.push(scripted.reply);
          return {
            id: requested,
            chat: space.chat.id,
            bot: space.bot.id,
            acting: space.account.id,
            command: "c1",
            message: "m1",
            status: scripted?.status ?? "succeeded",
            seq: "4",
            position: scripted?.position,
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

async function openChat() {
  vi.spyOn(api, "me").mockResolvedValue(space.account as Account);
  const view = render(() => <App />);
  await waitFor(() => expect(view.container.querySelector("form.composer")).toBeTruthy());
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

/// A second Bot and Chat whose run waits at place 3.
function waitingChat() {
  space.extraBots.push({ id: "b2", owner: "u1", name: "Quill", computer: "p2", chat: "h2" });
  space.extraChats.push({ id: "h2", owner: "u1", bot: "b2" });
  space.messages.push({
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
  space.queued["r2"] = 3;
}

/// Switches to the waiting Chat and waits for its queue place.
async function openWaitingChat(view: ReturnType<typeof render>) {
  fireEvent.click(element<HTMLButtonElement>(view, ".conv:nth-of-type(2)"));
  await waitFor(
    () => expect(element<HTMLElement>(view, ".composer__queue").textContent).toBe("Waiting behind 2 requests"),
    { timeout: 3_000 },
  );
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
  space.runs = [];
  space.extraBots = [];
  space.extraChats = [];
  space.queued = {};
  space.runShows = [];
  space.failSend = false;
  space.sendsDone = 0;
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  flush(() => setLocale("en-US"));
});

describe("App", () => {
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

    fireEvent.submit(element(view, "form.composer"));
    await waitFor(() => expect(view.container.querySelector(".file-card")).toBeTruthy());
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
    flush(() => fireEvent.input(element<HTMLTextAreaElement>(view, "textarea"), { target: { value: "please summarize" } }));
    fireEvent.submit(element(view, "form.composer"));

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
    fireEvent.submit(element(view, "form.composer"));

    await waitFor(() => expect(alertText(view)).toBe("big.bin is larger than 32 MiB."));
    expect(upload).not.toHaveBeenCalled();
    expect(space.sent).toEqual([]);
  });

  it("names any other upload failure, in the reader's language", async () => {
    const view = await openChat();
    vi.spyOn(api, "uploadFile").mockRejectedValue(new ApiError(500, "internal", "boom"));
    attach(view, new File(["hello"], "notes.txt"));
    await waitFor(() => expect(view.container.querySelector(".attachment")).toBeTruthy());
    fireEvent.submit(element(view, "form.composer"));

    await waitFor(() => expect(alertText(view)).toBe("The file could not be uploaded. Try again."));
    expect(space.sent).toEqual([]);
    await waitFor(() => expect(view.container.querySelector(".attachment")?.textContent).toContain("notes.txt"));

    flush(() => fireEvent.click(element(view, ".locale-switch")));
    expect(alertText(view)).toBe("文件上传失败，请重试。");
  });

  it("shows a queued Message behind the running run until its reply lands", async () => {
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
    // The first run holds the Bot at place 1; this Message waits behind it.
    space.runs = [{ status: "queued", position: 2 }, { status: "succeeded", reply }];
    flush(() => fireEvent.input(element<HTMLTextAreaElement>(view, "textarea"), { target: { value: "hello" } }));
    fireEvent.submit(element(view, "form.composer"));

    await waitFor(
      () => expect(element<HTMLElement>(view, ".composer__queue").textContent).toBe("Waiting behind 1 request"),
      { timeout: 3_000 },
    );
    await waitFor(() => expect(view.container.querySelector(".composer__queue")).toBeNull(), { timeout: 3_000 });
    await waitFor(() => expect(view.container.textContent).toContain("Here is the answer."), { timeout: 3_000 });
  });

  it("restores a queued run when a Chat opens", async () => {
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
    space.runs = [{ status: "queued", position: 3 }, { status: "succeeded" }];
    const view = await openChat();

    await waitFor(
      () => expect(element<HTMLElement>(view, ".composer__queue").textContent).toBe("Waiting behind 2 requests"),
      { timeout: 3_000 },
    );
    await waitFor(() => expect(view.container.querySelector(".composer__queue")).toBeNull(), { timeout: 3_000 });
  });

  it("says the Bot is working once the queued run starts", async () => {
    const view = await openChat();
    space.runs = [{ status: "running" }, { status: "succeeded" }];
    flush(() => fireEvent.input(element<HTMLTextAreaElement>(view, "textarea"), { target: { value: "hello" } }));
    fireEvent.submit(element(view, "form.composer"));

    await waitFor(
      () => expect(element<HTMLElement>(view, ".composer__queue").textContent).toBe("Working"),
      { timeout: 3_000 },
    );
    await waitFor(() => expect(view.container.querySelector(".composer__queue")).toBeNull(), { timeout: 3_000 });
  });

  it("keeps the new Chat's queue while an old Message is still being accepted", async () => {
    waitingChat();
    const view = await openChat();

    space.holdSends();
    flush(() =>
      fireEvent.input(element<HTMLTextAreaElement>(view, "textarea"), { target: { value: "to the first Chat" } }),
    );
    fireEvent.submit(element(view, "form.composer"));
    await waitFor(() => expect(space.asked).toContain("message_send"));

    // Switch to the waiting Chat while the first Chat's send is in flight.
    await openWaitingChat(view);
    const polls = () => space.runShows.filter((run) => run === "r2").length;
    const before = polls();
    space.releaseSends();

    // The stale send must not replace the new Chat's place or stop its polling.
    await waitFor(() => expect(polls()).toBeGreaterThan(before), { timeout: 3_000 });
    expect(element<HTMLElement>(view, ".composer__queue").textContent).toBe("Waiting behind 2 requests");
  });

  it("keeps the new Chat's queue when an old Message is refused", async () => {
    waitingChat();
    const view = await openChat();

    space.failSend = true;
    space.holdSends();
    flush(() =>
      fireEvent.input(element<HTMLTextAreaElement>(view, "textarea"), { target: { value: "to the first Chat" } }),
    );
    fireEvent.submit(element(view, "form.composer"));
    await waitFor(() => expect(space.asked).toContain("message_send"));

    await openWaitingChat(view);
    space.releaseSends();
    // Let the refused send settle, and give the App's stale continuation a turn.
    await waitFor(() => expect(space.sendsDone).toBe(1));
    await new Promise((resolve) => setTimeout(resolve, 10));

    // The refusal belongs to the old Chat, so the new Chat keeps its state.
    expect(alertText(view)).toBeUndefined();
    expect(element<HTMLElement>(view, ".composer__queue").textContent).toBe("Waiting behind 2 requests");
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

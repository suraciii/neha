import { cleanup, fireEvent, render, waitFor } from "@solidjs/testing-library";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

import App from "./App";
import { api, ApiError } from "./lib/api";
import { MAX_FILE_BYTES } from "./lib/files";
import { setLocale } from "./i18n";
import type { Account, Bot, Chat, Computer, FileSummary, Message } from "./types";

// A Space that answers every command from memory and records what it was asked.
const { space, FakeCommands } = vi.hoisted(() => {
  const account = { id: "u1", display_name: "Ada", is_admin: true };
  const bot = { id: "b1", owner: "u1", name: "Scout", computer: "p1", chat: "h1" };
  const computer = { id: "p1", owner: "u1", kind: "managed", online: true, workspace: "/w" };
  const chat = { id: "h1", owner: "u1", bot: "b1" };
  const space = {
    account,
    bot,
    computer,
    chat,
    asked: [] as string[],
    sent: [] as unknown[],
    files: [] as FileSummary[],
    messages: [] as Message[],
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
          return [space.bot] as T;
        case "chat_list":
          return [space.chat] as T;
        case "chat_read":
          return [...space.messages] as T;
        case "file_list":
          return [...space.files] as T;
        case "bot_show":
          return { bot: space.bot, computer: space.computer, chat: space.chat } as T;
        case "message_send": {
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
          return { run: "r1", message: "m1", status: "succeeded", reply: "m2", text: "ok" } as T;
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
  space.asked = [];
  space.sent = [];
  space.files = [];
  space.messages = [];
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  setLocale("en-US");
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
    fireEvent.input(element<HTMLTextAreaElement>(view, "textarea"), { target: { value: "please summarize" } });
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

    fireEvent.click(element(view, ".locale-switch"));
    expect(alertText(view)).toBe("文件上传失败，请重试。");
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
    fireEvent.click(element(view, ".locale-switch--foot"));
    expect(element<HTMLElement>(view, ".conv__preview").textContent).toBe("已发送文件");
  });
});

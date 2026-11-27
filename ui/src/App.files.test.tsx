import { fireEvent, type render, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { describe, expect, it, vi } from "vitest";

import { ApiError, api } from "./lib/api";
import { MAX_FILE_BYTES } from "./lib/files";
import { space } from "./test/fakeSpace";
import { drain, element, openChat, record, renderApp, useView } from "./test/view";
import type { FileSummary, Message } from "./types";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
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
  flush(() =>
    fireEvent.input(element<HTMLTextAreaElement>(view, "textarea"), { target: { value: text } }),
  );
}

function submit(view: ReturnType<typeof render>) {
  fireEvent.submit(element(view, "form.composer"));
}

// The card starts folded, so a test that reads inside it opens it first.
function openCard(view: ReturnType<typeof render>) {
  fireEvent.click(element<HTMLButtonElement>(view, ".activity__head"));
}

// The record starts a queued Run.
function startRun(run: string) {
  space.runs[run] = { status: "running" };
  record("neha.run.started", {
    chat: space.chat.id,
    run,
    bot: space.bot.id,
    command: "c1",
    message: "m1",
  });
}

// The record ends a Run, with the reply Message when it has one.
function finishRun(run: string, reply: Message | null = null) {
  space.runs[run] = { status: "succeeded" };
  if (reply) {
    space.messages.push(reply);
    record("neha.message.added", {
      chat: reply.chat,
      message: reply.id,
      text: reply.text,
      file: null,
      run,
    });
  }
  record("neha.run.finished", {
    run,
    status: "succeeded",
    reply: reply?.id ?? null,
    usage: null,
    error: null,
  });
}

function request(id: string, chat: string, run: string, text = "earlier"): Message {
  return {
    id,
    chat,
    author: { kind: "user", id: "u1" },
    acting: null,
    text,
    at: "2026-01-01T00:00:00.000Z",
    seq: "1",
    run,
    file: null,
  };
}

function reply(id: string, chat: string, run: string, text: string): Message {
  return {
    id,
    chat,
    author: { kind: "bot", id: "b1" },
    acting: "u1",
    text,
    at: "2026-01-01T00:00:01.000Z",
    seq: "2",
    run,
    file: null,
  };
}

// A second Chat on the sidebar whose last Run waits at place 3.
function waitingChat() {
  space.chats = [space.chat, space.otherChat];
  space.otherMessages.push(request("m9", "h2", "r2"));
  space.runs["r2"] = { status: "queued", position: 3 };
}

async function openWaitingChat(view: ReturnType<typeof render>) {
  fireEvent.click(element<HTMLButtonElement>(view, ".conv:nth-of-type(2)"));
  await waitFor(() => expect(queueText(view)).toBe("Waiting behind 2 requests"));
}

useView();

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
    await waitFor(() =>
      expect(view.container.querySelector(".attachment")?.textContent).toContain("5 B"),
    );

    submit(view);
    await waitFor(() => expect(view.container.querySelector(".file-card")).toBeTruthy());
    // The Message is accepted at once: its Run waits, and the composer takes no
    // second Message until the Run is done.
    await waitFor(() => expect(queueText(view)).toBe("Waiting in the queue"));
    expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(true);
    expect(element<HTMLButtonElement>(view, ".composer__attach").disabled).toBe(true);
    finishRun("r1");
    await waitFor(() => expect(queueText(view)).toBeUndefined());
    await waitFor(() =>
      expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false),
    );
    expect(element<HTMLButtonElement>(view, ".composer__attach").disabled).toBe(false);

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
    await waitFor(() =>
      expect(element<HTMLTextAreaElement>(view, "textarea").value).toBe("please summarize"),
    );
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
    await waitFor(() =>
      expect(view.container.querySelector(".attachment")?.textContent).toContain("notes.txt"),
    );

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
      return h2Reads === 1
        ? new Promise<void>((resolve) => (release = resolve))
        : Promise.resolve();
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
    // The Chat shows the working state only beside recorded Messages.
    space.messages.push({ ...request("m1", "h1", "r1", "please work"), run: null });
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(element(view, ".activity")).toBeTruthy());

    record("neha.run.finished", { run: "another-run", status: "failed", error: "nope" });
    expect(element(view, ".activity")).toBeTruthy();

    // A failed Run leaves one line behind, and the card opens itself with the
    // reason, and the Chat header marks the failure.
    record("neha.run.finished", { run: "r1", status: "failed", error: "nope" });
    await waitFor(() => expect(element(view, ".activity").textContent).toContain("Failed"));
    expect(element(view, ".chat__state--failed").textContent).toBe("Failed");
    expect(element(view, ".activity__reason").textContent).toContain("nope");
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
    await waitFor(() => expect(queueText(view)).toBe("Waiting in the queue"));

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
    space.messages.push(request("m1", "h1", "r9"));
    space.runs["r9"] = { status: "queued", position: 3 };
    const view = await openChat();

    // A waiting Run is unfinished: its place shows, and the composer takes no
    // second Message while it waits.
    await waitFor(() => expect(queueText(view)).toBe("Waiting behind 2 requests"));
    expect(view.container.querySelector(".activity")).toBeNull();
    expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(true);
    expect(element<HTMLButtonElement>(view, ".composer__attach").disabled).toBe(true);
    // The queue place says why, so the composer adds nothing else.
    expect(view.container.querySelector(".composer__closed")).toBeNull();

    finishRun("r9");
    await waitFor(() => expect(queueText(view)).toBeUndefined());
    await waitFor(() =>
      expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false),
    );
    expect(element<HTMLButtonElement>(view, ".composer__attach").disabled).toBe(false);
  });

  it("restores the newest request, not a later Bot reply", async () => {
    space.messages.push(
      request("m1", "h1", "r1"),
      request("m2", "h1", "r2"),
      reply("m3", "h1", "r1", "first done"),
    );
    space.runs["r1"] = { status: "succeeded" };
    space.runs["r2"] = { status: "queued", position: 2 };
    const view = await openChat();

    // The reply names r1, but the Chat waits on the newest User request: r2.
    await waitFor(() => expect(queueText(view)).toBe("Waiting behind 1 request"));
    expect(space.runShows).toContain("r2");
    expect(space.runShows).not.toContain("r1");
  });

  it("shows the Bot working once the waiting Run starts", async () => {
    const view = await openChat();
    type(view, "hello");
    submit(view);
    await waitFor(() => expect(queueText(view)).toBe("Waiting in the queue"));
    // A Run waiting in the queue has done nothing yet, so no activity shows.
    expect(view.container.querySelector(".activity")).toBeNull();

    startRun("r1");
    await waitFor(() => expect(element(view, ".activity").textContent).toContain("Working"));
    expect(queueText(view)).toBeUndefined();
    // The Chat works, so the composer takes no second Message.
    expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(true);

    finishRun("r1", reply("m2", "h1", "r1", "Here is the answer."));
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeNull());
    await waitFor(() => expect(view.container.textContent).toContain("Here is the answer."));
    // The Run answered, so the composer opens again.
    await waitFor(() =>
      expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false),
    );
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
    space.chatDelay = (chatId) =>
      chatId === "h2" ? new Promise<void>(() => undefined) : Promise.resolve();
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

  it("keeps following its own Run when another Run works in the same Chat", async () => {
    const view = await openChat();
    type(view, "first");
    submit(view);
    await waitFor(() => expect(queueText(view)).toBe("Waiting in the queue"));
    startRun("r1");
    await waitFor(() => expect(element(view, ".activity")).toBeTruthy());

    // Another client put a Run behind ours in this Chat. The page keeps
    // following its own, and the composer stays closed either way.
    space.runs["r9"] = { status: "queued", position: 1 };
    record("neha.run.queued", { chat: "h1", run: "r9", bot: "b1", command: "c9", message: "m9" });
    await drain();
    expect(element(view, ".activity")).toBeTruthy();

    // Our Run ends; its reply names it, and the page reads the record about the
    // Run it followed.
    const showsBefore = space.runShows.length;
    space.runs["r1"] = { status: "succeeded" };
    const firstReply = reply("m3", "h1", "r1", "first done");
    space.messages.push(firstReply);
    record("neha.message.added", {
      chat: "h1",
      message: firstReply.id,
      text: firstReply.text,
      file: null,
      run: "r1",
    });
    await waitFor(() => expect(view.container.textContent).toContain("first done"));
    // The reply's read asks the record about the Run the page follows.
    await waitFor(() => expect(space.runShows.slice(showsBefore)).toContain("r1"));

    // The other Run ends too, and the page follows neither of them.
    record("neha.run.finished", {
      run: "r9",
      status: "succeeded",
      reply: null,
      usage: null,
      error: null,
    });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeNull());
    expect(queueText(view)).toBeUndefined();
    expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false);
  });

  it("leaves a File another Run produced out of the Run it follows", async () => {
    // Two Files sit in the Chat's records: one our Run made, one another Run of
    // the same Bot made.
    const theirs: FileSummary = {
      id: "f9",
      chat: "h1",
      name: "theirs.txt",
      media_type: "text/plain",
      size: 12,
      sha256: "cd".repeat(32),
      author: { kind: "bot", id: "b1" },
      at: "2026-01-01T00:00:00.000Z",
      seq: "20",
    };
    const mine: FileSummary = {
      id: "f1",
      chat: "h1",
      name: "mine.txt",
      media_type: "text/plain",
      size: 12,
      sha256: "ef".repeat(32),
      author: { kind: "bot", id: "b1" },
      at: "2026-01-01T00:00:00.000Z",
      seq: "21",
    };
    space.files.push(theirs, mine);

    const view = await openChat();
    type(view, "first");
    submit(view);
    await waitFor(() => expect(queueText(view)).toBe("Waiting in the queue"));
    startRun("r1");
    await waitFor(() => expect(element(view, ".activity")).toBeTruthy());

    // Another client queued a second Run behind ours in this Chat. The page
    // keeps following its own.
    space.runs["r9"] = { status: "queued", position: 1 };
    record("neha.run.queued", { chat: "h1", run: "r9", bot: "b1", command: "c9", message: "m9" });
    await drain();
    expect(element(view, ".activity")).toBeTruthy();

    // The other Run's reply carries its File and names that Run, so the page
    // keeps the File out of the work of the Run it follows.
    record("neha.message.added", {
      chat: "h1",
      message: "m9",
      text: "here is theirs",
      file: "f9",
      run: "r9",
    });
    await drain();
    openCard(view);
    expect(view.container.querySelector(".activity .file-card")).toBeNull();
    expect(view.container.querySelector(".activity")?.textContent ?? "").not.toContain(
      "theirs.txt",
    );

    // Our own Run's reply carries its File and names our Run, and the card
    // names that one.
    record("neha.message.added", {
      chat: "h1",
      message: "m3",
      text: "here is mine",
      file: "f1",
      run: "r1",
    });
    await waitFor(() => expect(view.container.querySelector(".activity .file-card")).toBeTruthy());
    const card = view.container.querySelector(".activity")?.textContent ?? "";
    expect(card).toContain("mine.txt");
    expect(card).not.toContain("theirs.txt");
  });

  it("takes no second Message while a Run is unfinished", async () => {
    const view = await openChat();
    type(view, "first");
    submit(view);
    await waitFor(() => expect(queueText(view)).toBe("Waiting in the queue"));
    startRun("r1");
    await waitFor(() => expect(element(view, ".activity")).toBeTruthy());

    // The Chat works, so the composer takes no second Message.
    expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(true);
    expect(element<HTMLButtonElement>(view, ".composer__attach").disabled).toBe(true);
    submit(view);
    await drain();
    expect(space.asked.filter((name) => name === "message_send")).toHaveLength(1);
    expect(space.sent).toHaveLength(1);

    finishRun("r1", reply("m2", "h1", "r1", "the answer"));
    await waitFor(() => expect(view.container.textContent).toContain("the answer"));
    // The Run answered, so the composer opens again.
    await waitFor(() =>
      expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false),
    );
    expect(element<HTMLButtonElement>(view, ".composer__attach").disabled).toBe(false);
  });

  it("keeps what the reader wrote while the composer is closed", async () => {
    // The Chat holds a Message, so its Run's activity shows beside it.
    space.messages.push({ ...request("m1", "h1", "r7", "please work"), run: null });
    const view = await openChat();
    type(view, "a draft");

    // Another client's Run works in this Chat, so the composer closes.
    record("neha.run.started", { chat: "h1", run: "r7", bot: "b1", command: "c7", message: "m7" });
    await waitFor(() => expect(element(view, ".activity")).toBeTruthy());
    expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(true);
    expect(element<HTMLTextAreaElement>(view, "textarea").value).toBe("a draft");

    record("neha.run.finished", {
      run: "r7",
      status: "succeeded",
      reply: null,
      usage: null,
      error: null,
    });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeNull());
    expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false);
    // The draft waited in the composer.
    expect(element<HTMLTextAreaElement>(view, "textarea").value).toBe("a draft");
  });

  it("does not move back to queued when the Run started before the answer", async () => {
    const view = await openChat();
    space.holdSends();
    type(view, "hello");
    submit(view);
    await waitFor(() => expect(space.asked).toContain("message_send"));

    // The record starts the Run before the command's answer reaches the page.
    startRun("r1");
    await waitFor(() => expect(element(view, ".activity")).toBeTruthy());

    space.releaseSends();
    await waitFor(() => expect(space.sendsDone).toBe(1));
    await drain();
    expect(element(view, ".activity")).toBeTruthy();
    expect(queueText(view)).toBeUndefined();
  });

  it("does not revive a Run that finished before its answer", async () => {
    const view = await openChat();
    space.holdSends();
    type(view, "hello");
    submit(view);
    await waitFor(() => expect(space.asked).toContain("message_send"));

    startRun("r1");
    finishRun("r1", reply("m2", "h1", "r1", "done already"));
    await drain();
    space.releaseSends();
    await waitFor(() => expect(space.sendsDone).toBe(1));
    await drain();

    expect(queueText(view)).toBeUndefined();
    expect(view.container.querySelector(".activity")).toBeNull();
    await waitFor(() => expect(view.container.textContent).toContain("done already"));
  });

  it("does not revive a Run that finished while its answer was in flight", async () => {
    const view = await openChat();
    space.holdRunShow("r1");
    type(view, "hello");
    submit(view);
    await waitFor(() => expect(space.runShows).toContain("r1"));
    startRun("r1");
    await waitFor(() => expect(element(view, ".activity")).toBeTruthy());

    // The Run ends while the record's answer about it is still in flight.
    space.runs["r1"] = { status: "succeeded" };
    record("neha.run.finished", {
      run: "r1",
      status: "succeeded",
      reply: null,
      usage: null,
      error: null,
    });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeNull());

    space.runHolds["r1"]?.release();
    await drain();
    // The late answer names a Run that already ended, so it revives nothing.
    expect(view.container.querySelector(".activity")).toBeNull();
    expect(queueText(view)).toBeUndefined();
    expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false);
  });

  it("does not let an old Chat's lost run_show answer clear the new Chat", async () => {
    waitingChat();
    space.messages.push(request("m1", "h1", "r9"));
    space.runs["r9"] = { status: "queued", position: 2 };
    const view = await openChat();
    await waitFor(() => expect(queueText(view)).toBe("Waiting behind 1 request"));

    // A refresh of the old Chat asks the record about its Run and the answer hangs.
    space.holdRunShow("r9");
    const showsBefore = space.runShows.filter((run) => run === "r9").length;
    record("neha.message.added", {
      chat: "h1",
      message: "m2",
      text: "hello",
      file: null,
      run: null,
    });
    await waitFor(() =>
      expect(space.runShows.filter((run) => run === "r9").length).toBeGreaterThan(showsBefore),
    );

    // The reader moves to the waiting Chat before the old answer is lost.
    await openWaitingChat(view);
    space.runHolds["r9"]?.fail();
    await drain();

    // The lost answer belongs to the old Chat; the new Chat keeps its place.
    expect(queueText(view)).toBe("Waiting behind 2 requests");
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

describe("App with Computers", () => {
  it("names a Local Computer in the Chat header, in the reader's language", async () => {
    const view = await openChat();

    expect(element(view, ".chat__sub").textContent).toContain("Local Computer");
    flush(() => fireEvent.click(element(view, ".locale-switch")));
    expect(element(view, ".chat__sub").textContent).toContain("Local Computer");
  });

  it("names a Hosted Computer in the Chat header", async () => {
    space.computer.kind = "hosted";
    const view = await openChat();

    const header = element(view, ".chat__sub").textContent ?? "";
    expect(header).toContain("Hosted Computer");
    expect(header).not.toContain("Local Computer");
  });
});

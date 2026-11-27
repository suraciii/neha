// What the Chat shows while a Run works: one quiet summary of its steps, the
// tools it used, how each action ended, and the Files it produced.
import { fireEvent, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { describe, expect, it, vi } from "vitest";

import App from "./App";
import { space } from "./test/fakeSpace";
import { element, openChat, record, useView, type View } from "./test/view";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

useView();

// A Chat that holds a Message, so its Run's activity shows beside it.
function workingChat() {
  space.messages.push({
    id: "m1",
    chat: "h1",
    author: { kind: "user", id: "u1" },
    acting: null,
    text: "please work",
    at: "2026-01-01T00:00:00.000Z",
    seq: "1",
    run: null,
    file: null,
  });
}

// The record ends a Run, with the Bot's failure Message and why it failed.
function failRun(run: string, reply: { id: string; text: string }, error: string) {
  space.runs[run] = { status: "failed" };
  space.messages.push({
    id: reply.id,
    chat: "h1",
    author: { kind: "bot", id: "b1" },
    acting: "u1",
    text: reply.text,
    at: "2026-01-01T00:00:01.000Z",
    seq: "3",
    run,
    file: null,
  });
  record("neha.message.added", { chat: "h1", message: reply.id, text: reply.text, file: null, run });
  record("neha.run.finished", { run, status: "failed", reply: reply.id, usage: null, error });
}

function cardText(view: View): string {
  return view.container.querySelector(".activity")?.textContent ?? "";
}

function stepRows(view: View): string[] {
  return Array.from(view.container.querySelectorAll(".activity__step")).map((row) => row.textContent ?? "");
}

describe("App run activity", () => {
  it("names the steps and tools of the Run the page follows", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());

    // The Run works on its first step and asks a tool to do something.
    record("neha.run.step", { run: "r1", step: 1, usage: null, tool_calls: [{ id: "call1", tool: "shell" }] });
    record("neha.action.started", {
      run: "r1",
      step: 1,
      action: "act1",
      tool: "shell",
      arguments: { command: "printf hi > a.txt" },
    });
    await waitFor(() => expect(cardText(view)).toContain("shell"));
    expect(cardText(view)).toContain("Working");
    expect(cardText(view)).toContain("Step 1");
    expect(stepRows(view)).toEqual(["Step 1shellRunning"]);

    // The action ends, and the card says how.
    record("neha.action.finished", {
      run: "r1",
      step: 1,
      action: "act1",
      tool: "shell",
      status: "succeeded",
      output: "hi",
      error: null,
    });
    await waitFor(() => expect(stepRows(view)).toEqual(["Step 1shellSucceeded"]));

    // A second step's action fails, which the card names without its arguments.
    record("neha.run.step", { run: "r1", step: 2, usage: null, tool_calls: [] });
    record("neha.action.started", { run: "r1", step: 2, action: "act2", tool: "write_file", arguments: { path: "notes/a.txt" } });
    record("neha.action.finished", {
      run: "r1",
      step: 2,
      action: "act2",
      tool: "write_file",
      status: "failed",
      output: null,
      error: "the folder is not granted",
    });
    await waitFor(() => expect(stepRows(view)).toHaveLength(2));
    expect(stepRows(view)[1]).toContain("Step 2");
    expect(stepRows(view)[1]).toContain("write_file");
    expect(stepRows(view)[1]).toContain("Failed");
    // A tool's arguments never reach the surface.
    expect(cardText(view)).not.toContain("printf hi > a.txt");
    expect(cardText(view)).not.toContain("notes/a.txt");
  });

  it("folds to one line and opens again on request", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());
    record("neha.action.started", { run: "r1", step: 1, action: "act1", tool: "shell", arguments: { command: "printf hi" } });
    await waitFor(() => expect(cardText(view)).toContain("shell"));
    const head = element<HTMLButtonElement>(view, ".activity__head");
    expect(head.getAttribute("aria-expanded")).toBe("true");
    expect(view.container.querySelector(".activity__steps")).toBeTruthy();

    // Folding keeps one line: what the Run is doing, and the tools it used.
    flush(() => fireEvent.click(head));
    expect(head.getAttribute("aria-expanded")).toBe("false");
    expect(view.container.querySelector(".activity__steps")).toBeNull();
    expect(cardText(view)).toContain("Working");
    expect(cardText(view)).toContain("shell");

    flush(() => fireEvent.click(head));
    expect(head.getAttribute("aria-expanded")).toBe("true");
    expect(view.container.querySelector(".activity__steps")).toBeTruthy();
  });

  it("names the Files the Run produced", async () => {
    workingChat();
    space.files.push({
      id: "f1",
      chat: "h1",
      name: "report.pdf",
      media_type: "application/pdf",
      size: 2048,
      sha256: "ab".repeat(32),
      author: { kind: "bot", id: "b1" },
      at: "2026-01-01T00:00:02.000Z",
      seq: "4",
    });
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());

    record("neha.file.added", { chat: "h1", file: "f1", name: "report.pdf", media_type: "application/pdf", size: 2048, sha256: "ab".repeat(32) });
    // The File names itself once the Chat reads the record again.
    await waitFor(() => expect(view.container.querySelector(".activity .file-card")).toBeTruthy());
    expect(element(view, ".activity .file-card").textContent).toContain("report.pdf");
    expect(element(view, ".activity .file-card").getAttribute("href")).toBe("/api/files/f1");
  });

  it("keeps one line with the reason when the Run fails", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());
    record("neha.action.started", { run: "r1", step: 1, action: "act1", tool: "shell", arguments: { command: "printf hi" } });
    record("neha.action.finished", {
      run: "r1",
      step: 1,
      action: "act1",
      tool: "shell",
      status: "failed",
      output: null,
      error: "the shell is not reachable",
    });
    await waitFor(() => expect(stepRows(view)).toEqual(["Step 1shellFailed"]));

    // The Run fails: the card folds to one line and keeps the reason behind it.
    failRun("r1", { id: "m2", text: "I could not finish this." }, "the shell is not reachable");
    await waitFor(() => expect(cardText(view)).toContain("Failed"));
    // The Chat reads the record again, and the Bot's failure Message is in the
    // stream, in its place.
    await waitFor(() => expect(view.container.querySelectorAll(".msg__text")).toHaveLength(2));
    expect(view.container.querySelector(".activity__steps")).toBeNull();
    expect(cardText(view)).not.toContain("the shell is not reachable");
    flush(() => fireEvent.click(element(view, ".activity__head")));
    expect(element(view, ".activity__reason").textContent).toContain("the shell is not reachable");
    const texts = Array.from(view.container.querySelectorAll(".msg__text")).map((node) => node.textContent ?? "");
    expect(texts).toEqual(["please work", "I could not finish this."]);
  });

  it("marks the Chat header failed and closes the composer", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());
    expect(element(view, ".chat__state").textContent).toBe("Working");

    failRun("r1", { id: "m2", text: "I could not finish this." }, "the shell is not reachable");
    await waitFor(() => expect(element(view, ".chat__state--failed").textContent).toBe("Failed"));
    expect(view.container.querySelector(".chat__state--working")).toBeNull();
    // The composer opens again: the Run is over.
    await waitFor(() => expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false));
  });

  it("names the Run's work in the reader's language", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());
    record("neha.action.started", { run: "r1", step: 1, action: "act1", tool: "shell", arguments: { command: "printf hi" } });
    await waitFor(() => expect(cardText(view)).toContain("shell"));

    flush(() => fireEvent.click(element(view, ".locale-switch--foot")));
    expect(cardText(view)).toContain("正在处理");
    expect(cardText(view)).toContain("第 1 步");
    expect(stepRows(view)).toEqual(["第 1 步shell进行中"]);

    record("neha.action.finished", {
      run: "r1",
      step: 1,
      action: "act1",
      tool: "shell",
      status: "succeeded",
      output: "hi",
      error: null,
    });
    await waitFor(() => expect(stepRows(view)).toEqual(["第 1 步shell成功"]));
  });
});

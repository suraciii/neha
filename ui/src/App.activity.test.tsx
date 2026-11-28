// What the Chat shows while a Run works: one quiet summary of its steps, the
// tools it used, how each action ended, and the Files the Run produced.
import { fireEvent, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { describe, expect, it, vi } from "vitest";
import { space } from "./test/fakeSpace";
import { drain, element, openChat, record, useView, type View } from "./test/view";
import type { PlanEntry } from "./types";

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
  record("neha.message.added", {
    chat: "h1",
    message: reply.id,
    text: reply.text,
    file: null,
    run,
  });
  record("neha.run.finished", { run, status: "failed", reply: reply.id, usage: null, error });
}

function cardText(view: View): string {
  return view.container.querySelector(".activity")?.textContent ?? "";
}

function stepRows(view: View): string[] {
  return Array.from(view.container.querySelectorAll(".activity__step")).map(
    (row) => row.textContent ?? "",
  );
}

// The card starts folded, so a test that reads its steps opens it first.
function openCard(view: View) {
  const head = element<HTMLButtonElement>(view, ".activity__head");
  if (head.getAttribute("aria-expanded") !== "true") flush(() => fireEvent.click(head));
}

// A plan of `total` lines where the first `done` lines are done, the next is
// being worked on, and the rest wait.
function plan(total: number, done: number): PlanEntry[] {
  return Array.from({ length: total }, (_, index) => ({
    title: `Line ${index + 1}`,
    state: index < done ? "done" : index === done ? "doing" : "pending",
  }));
}

// What each Chat in the list says about its state.
function badges(view: View): string[] {
  return Array.from(view.container.querySelectorAll(".conv__badge")).map(
    (badge) => badge.textContent ?? "",
  );
}

// What one expanded card says about its plan's lines.
function planRows(view: View): string[] {
  return Array.from(view.container.querySelectorAll(".activity__plan-entry")).map(
    (row) => row.textContent ?? "",
  );
}

describe("App run activity", () => {
  it("names the steps and tools of the Run the page follows", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());

    // The Run works on its first step and asks a tool to do something.
    record("neha.run.step", {
      run: "r1",
      step: 1,
      usage: null,
      tool_calls: [{ id: "call1", tool: "shell" }],
    });
    record("neha.action.started", {
      run: "r1",
      step: 1,
      action: "act1",
      tool: "shell",
      arguments: { command: "printf hi > a.txt" },
    });
    await waitFor(() => expect(cardText(view)).toContain("shell"));
    // The card stays folded, and its one line names the work.
    expect(element<HTMLButtonElement>(view, ".activity__head").getAttribute("aria-expanded")).toBe(
      "false",
    );
    expect(cardText(view)).toContain("Working");
    expect(cardText(view)).toContain("Step 1");
    expect(cardText(view)).toContain("shell");
    openCard(view);
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
    record("neha.action.started", {
      run: "r1",
      step: 2,
      action: "act2",
      tool: "write_file",
      arguments: { path: "notes/a.txt" },
    });
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

  it("starts folded and opens on request", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());
    record("neha.action.started", {
      run: "r1",
      step: 1,
      action: "act1",
      tool: "shell",
      arguments: { command: "printf hi" },
    });
    await waitFor(() => expect(cardText(view)).toContain("shell"));
    const head = element<HTMLButtonElement>(view, ".activity__head");

    // The stream stays calm: one line, with the state, the step, and the tools.
    // A Run with no plan keeps that line and shows no progress bar.
    expect(head.getAttribute("aria-expanded")).toBe("false");
    expect(view.container.querySelector(".activity__steps")).toBeNull();
    expect(cardText(view)).toContain("Working");
    expect(cardText(view)).toContain("Step 1");
    expect(cardText(view)).toContain("shell");
    expect(element(view, ".activity__title").textContent).toBe("Working · Step 1");
    expect(view.container.querySelector(".activity__progress")).toBeNull();
    expect(view.container.querySelector(".activity__count")).toBeNull();

    flush(() => fireEvent.click(head));
    expect(head.getAttribute("aria-expanded")).toBe("true");
    expect(view.container.querySelector(".activity__steps")).toBeTruthy();
    expect(stepRows(view)).toEqual(["Step 1shellRunning"]);

    flush(() => fireEvent.click(head));
    expect(head.getAttribute("aria-expanded")).toBe("false");
    expect(view.container.querySelector(".activity__steps")).toBeNull();
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

    // The Bot's reply carries the File, and names the Run that made it.
    record("neha.message.added", {
      chat: "h1",
      message: "m2",
      text: "here is the report",
      file: "f1",
      run: "r1",
    });
    // The File names itself once the Chat reads the record again.
    await waitFor(() => expect(view.container.querySelector(".activity__tools")).toBeTruthy());
    openCard(view);
    await waitFor(() => expect(view.container.querySelector(".activity .file-card")).toBeTruthy());
    expect(element(view, ".activity .file-card").textContent).toContain("report.pdf");
    expect(element(view, ".activity .file-card").getAttribute("href")).toBe("/api/files/f1");
  });

  it("takes a newer word about an action the record already reported", async () => {
    workingChat();
    // The Space compacted this Run: it knows the action, but not how it ended.
    space.runs["r1"] = {
      status: "running",
      steps: [{ index: 1, tool_calls: 1 }],
      actions: [{ id: "act1", step: 1, tool: "shell", status: "unknown" }],
    };
    space.statuses["h1"] = "working";
    space.activeRuns["h1"] = "r1";
    const view = await openChat();
    await waitFor(() => expect(space.runShows).toContain("r1"));
    openCard(view);
    expect(stepRows(view)).toEqual(["Step 1shellUnknown"]);

    // The record says the action ended, and the card takes that word.
    space.runs["r1"] = {
      status: "running",
      steps: [{ index: 1, tool_calls: 1 }],
      actions: [{ id: "act1", step: 1, tool: "shell", status: "succeeded" }],
    };
    space.resume([]);
    await waitFor(() => expect(stepRows(view)).toEqual(["Step 1shellSucceeded"]));
  });

  it("shows the reason when a Run failed before its first step", async () => {
    workingChat();
    // The Provider could not be reached, so the Run failed with no step and no
    // action behind it: its reason is all the record holds.
    space.statuses["h1"] = "working";
    space.activeRuns["h1"] = "r1";
    space.runs["r1"] = {
      status: "failed",
      steps: [],
      actions: [],
      error: "the Provider is not reachable",
    };
    const view = await openChat();
    await waitFor(() => expect(space.runShows).toContain("r1"));

    // The card is there, opened, with the reason the Run gave.
    await waitFor(() =>
      expect(element(view, ".activity__reason").textContent).toContain(
        "the Provider is not reachable",
      ),
    );
    expect(element<HTMLButtonElement>(view, ".activity__head").getAttribute("aria-expanded")).toBe(
      "true",
    );
    expect(element(view, ".chat__state--failed").textContent).toBe("Failed");
    // The Run is over, so the composer opens again.
    await waitFor(() =>
      expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false),
    );
  });

  it("opens itself with the reason when the Run fails", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());
    record("neha.action.started", {
      run: "r1",
      step: 1,
      action: "act1",
      tool: "shell",
      arguments: { command: "printf hi" },
    });
    record("neha.action.finished", {
      run: "r1",
      step: 1,
      action: "act1",
      tool: "shell",
      status: "failed",
      output: null,
      error: "the shell is not reachable",
    });
    await waitFor(() => expect(cardText(view)).toContain("shell"));
    openCard(view);
    expect(stepRows(view)).toEqual(["Step 1shellFailed"]);

    // The Run fails: the card opens itself, because the reason is why it failed.
    failRun("r1", { id: "m2", text: "I could not finish this." }, "the shell is not reachable");
    await waitFor(() => expect(cardText(view)).toContain("Failed"));
    expect(element<HTMLButtonElement>(view, ".activity__head").getAttribute("aria-expanded")).toBe(
      "true",
    );
    expect(element(view, ".activity__reason").textContent).toContain("the shell is not reachable");
    // The Chat reads the record again, and the Bot's failure Message is in the
    // stream, in its place.
    await waitFor(() => expect(view.container.querySelectorAll(".msg__text")).toHaveLength(2));
    const texts = Array.from(view.container.querySelectorAll(".msg__text")).map(
      (node) => node.textContent ?? "",
    );
    expect(texts).toEqual(["please work", "I could not finish this."]);

    // The User can fold it away again.
    flush(() => fireEvent.click(element(view, ".activity__head")));
    expect(element<HTMLButtonElement>(view, ".activity__head").getAttribute("aria-expanded")).toBe(
      "false",
    );
    expect(view.container.querySelector(".activity__reason")).toBeNull();
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
    await waitFor(() =>
      expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false),
    );
  });

  it("names the Run's work in the reader's language", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());
    record("neha.action.started", {
      run: "r1",
      step: 1,
      action: "act1",
      tool: "shell",
      arguments: { command: "printf hi" },
    });
    await waitFor(() => expect(cardText(view)).toContain("shell"));

    flush(() => fireEvent.click(element(view, ".locale-switch--foot")));
    expect(cardText(view)).toContain("正在处理");
    expect(cardText(view)).toContain("第 1 步");
    openCard(view);
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

  it("shows the steps a Run recorded once its Chat reopens", async () => {
    workingChat();
    space.runs["r1"] = {
      status: "running",
      steps: [
        { index: 1, tool_calls: 1 },
        { index: 2, tool_calls: 1 },
      ],
      actions: [
        { id: "act1", step: 1, tool: "shell", status: "succeeded" },
        { id: "act2", step: 2, tool: "read_file", status: "succeeded" },
      ],
    };
    space.messages.push({
      id: "m2",
      chat: "h1",
      author: { kind: "bot", id: "b1" },
      acting: "u1",
      text: "the report is ready",
      at: "2026-01-01T00:00:03.000Z",
      seq: "2",
      run: "r1",
      file: null,
    });
    // The Space says the Chat works on r1, so opening it follows that Run.
    space.statuses["h1"] = "working";
    space.activeRuns["h1"] = "r1";
    const view = await openChat();
    await waitFor(() => expect(space.runShows).toContain("r1"));

    openCard(view);
    expect(stepRows(view)).toEqual(["Step 1shellSucceeded", "Step 2read_fileSucceeded"]);
    expect(cardText(view)).toContain("Step 2");
    // The Run's reply is in the stream beside the card.
    const texts = Array.from(view.container.querySelectorAll(".msg__text")).map(
      (node) => node.textContent ?? "",
    );
    expect(texts).toContain("the report is ready");
  });

  it("leaves a File the User uploaded out of the Run's work", async () => {
    workingChat();
    space.files.push({
      id: "f9",
      chat: "h1",
      name: "notes.txt",
      media_type: "text/plain",
      size: 12,
      sha256: "cd".repeat(32),
      author: { kind: "user", id: "u1" },
      at: "2026-01-01T00:00:02.000Z",
      seq: "4",
    });
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());

    // Another client's User uploaded the File and sent it with their request:
    // the Message names the Run it started, but the File is theirs, not the
    // Run's work. Only a Bot reply carries a File into a Run's card.
    record(
      "neha.message.added",
      { chat: "h1", message: "m9", text: "have a look at this", file: "f9", run: "r1" },
      { kind: "user", id: "u2" },
    );
    openCard(view);
    expect(view.container.querySelector(".activity .file-card")).toBeNull();
    expect(cardText(view)).not.toContain("notes.txt");
  });

  it("shows a plan's progress in the title row instead of the step", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());
    record("neha.run.step", { run: "r1", step: 1, usage: null, tool_calls: [] });
    await waitFor(() => expect(cardText(view)).toContain("Step 1"));
    expect(view.container.querySelector(".activity__progress")).toBeNull();

    // A plan with nothing done still shows its bar and its count.
    record("neha.run.planned", { run: "r1", step: 1, call: "call1", entries: plan(7, 0) });
    await waitFor(() => expect(element(view, ".activity__count").textContent).toBe("0 / 7"));
    const head = element<HTMLButtonElement>(view, ".activity__head");
    expect(head.getAttribute("aria-expanded")).toBe("false");
    expect(element(view, ".activity__title").textContent).toBe("Working");
    // The plan replaces the step and the tools on the title row.
    expect(cardText(view)).not.toContain("Step 1");
    expect(view.container.querySelector(".activity__tools")).toBeNull();
    const bar = element(view, ".activity__progress");
    expect(bar.getAttribute("role")).toBe("progressbar");
    expect(bar.getAttribute("aria-label")).toBe("Plan");
    expect(bar.getAttribute("aria-valuemin")).toBe("0");
    expect(bar.getAttribute("aria-valuemax")).toBe("7");
    expect(bar.getAttribute("aria-valuenow")).toBe("0");
    expect(bar.getAttribute("aria-valuetext")).toBe("0 of 7 done");
    expect(element<HTMLElement>(view, ".activity__progress-fill").style.width).toBe("0%");
    // The card is still folded: the count is visible text without opening it.
    expect(view.container.querySelector(".activity__plan")).toBeNull();

    // A later write replaces the whole plan, and the bar follows it.
    record("neha.run.planned", { run: "r1", step: 1, call: "call1", entries: plan(7, 3) });
    await waitFor(() => expect(element(view, ".activity__count").textContent).toBe("3 / 7"));
    expect(element(view, ".activity__progress").getAttribute("aria-valuenow")).toBe("3");
    expect(element(view, ".activity__progress").getAttribute("aria-valuetext")).toBe("3 of 7 done");
    expect(element<HTMLElement>(view, ".activity__progress-fill").style.width).toBe("43%");

    // An empty list clears the plan, and the title row goes back to the step.
    record("neha.run.planned", { run: "r1", step: 1, call: "call1", entries: [] });
    await waitFor(() => expect(view.container.querySelector(".activity__progress")).toBeNull());
    expect(view.container.querySelector(".activity__count")).toBeNull();
    expect(cardText(view)).toContain("Step 1");
  });

  it("lists a plan's entries with their states before the actions", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());
    record("neha.run.planned", {
      run: "r1",
      step: 1,
      call: "call1",
      entries: [
        { title: "Read the request", state: "done" },
        { title: "Write the report", state: "doing" },
        { title: "Send the report", state: "pending" },
      ],
    });
    record("neha.action.started", {
      run: "r1",
      step: 1,
      action: "act1",
      tool: "shell",
      arguments: { command: "printf hi" },
    });
    await waitFor(() => expect(view.container.querySelector(".activity__progress")).toBeTruthy());

    openCard(view);
    expect(planRows(view)).toEqual([
      "Read the requestDone",
      "Write the reportIn progress",
      "Send the reportPending",
    ]);
    // The states differ by their words and their markers, not by color alone.
    expect(view.container.querySelectorAll(".activity__plan-entry--done .icon")).toHaveLength(1);
    expect(
      view.container.querySelectorAll(".activity__plan-entry--doing .activity__plan-dot"),
    ).toHaveLength(1);
    expect(
      view.container.querySelectorAll(".activity__plan-entry--pending .activity__plan-dot"),
    ).toHaveLength(1);
    // The plan comes first, then the actions under their own label.
    const parts = Array.from(
      element(view, ".activity").querySelectorAll(
        ".activity__plan, .activity__label, .activity__steps",
      ),
    ).map((node) => node.className);
    expect(parts).toEqual(["activity__plan", "activity__label", "activity__steps"]);
    expect(element(view, ".activity__label").textContent).toBe("Actions");
    expect(stepRows(view)).toEqual(["Step 1shellRunning"]);

    // The plan's copy is in the reader's language too.
    flush(() => fireEvent.click(element(view, ".locale-switch--foot")));
    expect(planRows(view)).toEqual([
      "Read the request完成",
      "Write the report进行中",
      "Send the report待开始",
    ]);
    expect(element(view, ".activity__label").textContent).toBe("操作");
  });

  it("opens a failed Run's card with its plan and its reason", async () => {
    workingChat();
    const view = await openChat();
    record("neha.run.started", { chat: "h1", run: "r1", bot: "b1", command: "c1", message: "m1" });
    await waitFor(() => expect(view.container.querySelector(".activity")).toBeTruthy());
    record("neha.run.planned", { run: "r1", step: 1, call: "call1", entries: plan(3, 1) });
    await waitFor(() => expect(element(view, ".activity__count").textContent).toBe("1 / 3"));

    failRun("r1", { id: "m2", text: "I could not finish this." }, "the shell is not reachable");
    await waitFor(() => expect(cardText(view)).toContain("Failed"));
    // The card opens itself, and the plan it held is still there.
    expect(element<HTMLButtonElement>(view, ".activity__head").getAttribute("aria-expanded")).toBe(
      "true",
    );
    expect(element(view, ".activity__title").textContent).toBe("Failed");
    expect(planRows(view)).toHaveLength(3);
    expect(element(view, ".activity__count").textContent).toBe("1 / 3");
    expect(element(view, ".activity__reason").textContent).toContain("the shell is not reachable");
  });

  it("restores a reopened Chat's plan from the Space's answer", async () => {
    workingChat();
    // The record holds the Run's plan only: its steps and actions are gone.
    space.runs["r1"] = {
      status: "running",
      plan: [
        { title: "Read the request", state: "done" },
        { title: "Write the report", state: "doing" },
        { title: "Send the report", state: "pending" },
      ],
    };
    space.statuses["h1"] = "working";
    space.activeRuns["h1"] = "r1";
    const view = await openChat();
    await waitFor(() => expect(space.runShows).toContain("r1"));

    // The answer seeds the card, so a Run with a plan and nothing else still
    // shows its progress and its lines.
    await waitFor(() => expect(element(view, ".activity__count").textContent).toBe("1 / 3"));
    expect(element(view, ".activity__progress").getAttribute("aria-valuemax")).toBe("3");
    openCard(view);
    expect(planRows(view)).toEqual([
      "Read the requestDone",
      "Write the reportIn progress",
      "Send the reportPending",
    ]);
  });

  it("names a plan's done count in the Chat list's Working mark", async () => {
    workingChat();
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();
    // Another Chat's Run starts, and the page knows no plan for it yet.
    record("neha.run.started", { chat: "h2", run: "r2", bot: "b2", command: "c2", message: "m2" });
    await waitFor(() => expect(badges(view)).toEqual(["Working"]));
    expect(element(view, ".conv__badge").classList.contains("conv__badge--working")).toBe(true);

    // The record names the Chat's plan, and the one Working mark carries it.
    record("neha.run.planned", { run: "r2", step: 1, call: "call2", entries: plan(7, 3) });
    await waitFor(() => expect(badges(view)).toEqual(["Working 3/7"]));
    expect(element(view, ".conv__badge").querySelectorAll(".dot")).toHaveLength(1);

    // The Run ends, so the mark goes and its plan with it.
    record("neha.run.finished", { run: "r2", status: "succeeded", reply: null, usage: null });
    await waitFor(() => expect(badges(view)).toEqual([]));
  });

  it("shows a fresh page the progress a background Chat already holds", async () => {
    workingChat();
    // The page opens on h1. Another Chat already works, and its plan was
    // written before the page existed, so no event will name it.
    space.chats = [space.chat, space.otherChat];
    space.statuses["h2"] = "working";
    space.activeRuns["h2"] = "r2";
    space.runs["r2"] = { status: "running", plan: plan(7, 3) };

    const view = await openChat();

    // The Space's own answer about the Chat carries the progress, so a page
    // that reads the record afresh is not left with plain "Working", and it
    // reads no other Chat's Run to get it.
    await waitFor(() => expect(badges(view)).toEqual(["Working 3/7"]));
    expect(space.runShows).not.toContain("r2");
  });

  it("keeps a background Chat's count when another device approves its Run", async () => {
    workingChat();
    space.chats = [space.chat, space.otherChat];
    space.statuses["h2"] = "waiting";
    space.activeRuns["h2"] = "r1";
    space.runs["r1"] = { status: "waiting", approval: "a1", plan: plan(3, 1) };
    space.approvals.push({
      approval: "a1",
      run: "r1",
      bot: "b2",
      chat: "h2",
      action: "act1",
      step: 1,
      tool: "shell",
      summary: "printf hi > a.txt",
      reason: "first_use_local",
    });

    const view = await openChat();
    await waitFor(() => expect(badges(view)).toEqual(["Waiting"]));

    // Another device approves the Run: the page hears the Run work again, but
    // its saved Chat list still says waiting. The saved count belongs to the
    // same Run, so the mark keeps it.
    record(
      "neha.approval.decided",
      { approval: "a1", run: "r1", allow: true },
      { kind: "user", id: "u1" },
    );
    await waitFor(() => expect(badges(view)).toEqual(["Working 1/3"]));
  });

  it("drops the count when the record clears the plan", async () => {
    workingChat();
    space.chats = [space.chat, space.otherChat];
    space.statuses["h2"] = "working";
    space.activeRuns["h2"] = "r2";
    space.runs["r2"] = { status: "running", plan: plan(7, 3) };

    const view = await openChat();
    await waitFor(() => expect(badges(view)).toEqual(["Working 3/7"]));

    // A cleared plan is a word the record wrote: the cold answer's count does
    // not outlive it.
    record("neha.run.planned", { run: "r2", step: 1, call: "call2", entries: [] });
    await waitFor(() => expect(badges(view)).toEqual(["Working"]));
  });

  it("keeps the record's newer plan over a read that answers late", async () => {
    workingChat();
    space.statuses["h1"] = "working";
    space.activeRuns["h1"] = "r1";
    space.runs["r1"] = { status: "running", plan: plan(7, 3) };
    space.holdRunShow("r1");

    const view = await openChat();
    await waitFor(() => expect(space.runShows).toContain("r1"));

    // The plan changes while the answer about the older one is still on its
    // way: the record's newer word wins, and the late read puts nothing back.
    record("neha.run.planned", { run: "r1", step: 1, call: "call1", entries: plan(7, 5) });
    await waitFor(() => expect(badges(view)).toEqual(["Working 5/7"]));
    space.releaseRunShow("r1");
    await drain();
    expect(badges(view)).toEqual(["Working 5/7"]);
    expect(element(view, ".activity__count").textContent).toBe("5 / 7");
  });
});

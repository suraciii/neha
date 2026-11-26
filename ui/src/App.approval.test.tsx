import { fireEvent, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { describe, expect, it, vi } from "vitest";

import { space } from "./test/fakeSpace";
import { element, openChat, record, useView, type View } from "./test/view";
import type { Approval } from "./types";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

// The first action of a Bot on the User's own Computer.
const firstUse: Approval = {
  approval: "a1",
  run: "r1",
  bot: "b1",
  chat: "h1",
  action: "act1",
  step: 1,
  tool: "shell",
  summary: "printf hi > a.txt",
  reason: "first_use_local",
  rule: null,
};

// An action a rule of the User's stops every time, as the Space describes it.
const byRule: Approval = {
  ...firstUse,
  approval: "a2",
  action: "act2",
  tool: "write_file",
  summary: "write_file: notes.txt",
  reason: "rule",
  rule: "rule1",
};

function card(view: View): string {
  return element(view, ".approval").textContent ?? "";
}

function header(view: View): string {
  return element(view, ".chat__sub").textContent ?? "";
}

function buttons(view: View) {
  return [...view.container.querySelectorAll<HTMLButtonElement>(".approval__actions button")];
}

// Presses the approval button with this label.
function press(view: View, label: string) {
  const button = buttons(view).find((candidate) => candidate.textContent === label);
  if (!button) throw new Error(`no ${label} button in the view`);
  fireEvent.click(button);
}

function asked(name: string): number {
  return space.asked.filter((asked) => asked === name).length;
}

function lines(view: View, selector: string) {
  return [...view.container.querySelectorAll(selector)].map((line) => line.textContent);
}

// A Chat whose newest request waits for an approval.
function waitingRun() {
  space.messages.push({
    id: "m1",
    chat: "h1",
    author: { kind: "user", id: "u1" },
    acting: null,
    text: "write it",
    at: "2026-01-01T00:00:00.000Z",
    seq: "1",
    run: "r1",
    file: null,
  });
  space.runs["r1"] = { status: "waiting", approval: "a1" };
  space.approvals.push(firstUse);
}

// The record event the Space writes when a Bot asks before an action.
function requested(approval: Approval) {
  record("neha.approval.requested", {
    computer: "p1",
    acting: "u1",
    index: 0,
    call: "c1",
    ...approval,
  });
}

useView();

describe("App with approvals", () => {
  it("shows a first-use approval with Allow and Deny, and nothing lasting", async () => {
    space.approvals.push(firstUse);
    const view = await openChat();

    expect(card(view)).toContain("Scout");
    expect(card(view)).toContain("shell");
    expect(card(view)).toContain("printf hi > a.txt");
    expect(card(view)).toContain("This is the first time Scout uses your Computer.");
    expect(element(view, ".approval__undo").textContent).toBe("This action cannot be undone.");
    expect(buttons(view).map((button) => button.textContent)).toEqual(["Allow", "Deny"]);
    // Nothing lasting is offered, so no scope is promised.
    expect(view.container.querySelector(".approval__scope")).toBeNull();
  });

  it("takes the Space's word on whether an action can be undone", async () => {
    space.approvals.push(
      { ...firstUse, approval: "a1", undoable: "unknown" },
      { ...firstUse, approval: "a2", tool: "read_file", summary: "notes.txt", undoable: "no" },
    );
    const view = await openChat();

    expect(lines(view, ".approval__undo")).toEqual([
      "It is not known whether this action can be undone.",
      "This action cannot be undone.",
    ]);
  });

  it("shows what an Always allow would cover, in the Space's words or this page's", async () => {
    space.approvals.push(
      { ...byRule, approval: "a1", lasting_scope: 'shell commands starting with "git"' },
      // A Space that says nothing about the scope leaves the page to name it.
      { ...byRule, approval: "a2", tool: "shell", summary: "shell: git status" },
      { ...byRule, approval: "a3", tool: "write_file", summary: "write_file: logs/out.txt" },
      // The arguments say where the action lands; the summary only names it.
      { ...byRule, approval: "a4", tool: "write_file", arguments: { path: "cache/tmp.txt" }, summary: "write_file: logs/out.txt" },
    );
    const view = await openChat();

    expect(lines(view, ".approval__scope")).toEqual([
      'Always allow covers: shell commands starting with "git"',
      'Always allow covers: shell commands starting with "git"',
      'Always allow covers: files under "logs"',
      'Always allow covers: files under "cache"',
    ]);

    // The scope is a promise about the rule, so it leaves with the buttons.
    press(view, "Always allow");
    await waitFor(() => expect(lines(view, ".approval__scope")).toHaveLength(3));
    flush(() => fireEvent.click(element(view, ".locale-switch")));
    expect(lines(view, ".approval__scope")[0]).toBe('始终允许的范围：以 "git" 开头的 shell 命令');
  });

  it("does not blame the User's rule for an action no rule could judge", async () => {
    space.approvals.push(
      { ...byRule, approval: "a1", ambiguous: true },
      { ...byRule, approval: "a2", ambiguous: false },
      // The first use of the User's machine comes first, whatever else is true.
      { ...firstUse, approval: "a3", ambiguous: true },
    );
    const view = await openChat();

    expect(lines(view, ".approval__reason")).toEqual([
      "This action has a form no rule can judge.",
      "A rule you set asks before this action.",
      "This is the first time Scout uses your Computer.",
    ]);

    flush(() => fireEvent.click(element(view, ".locale-switch")));
    expect(lines(view, ".approval__reason").slice(0, 2)).toEqual([
      "规则无法判断这个操作的形式。",
      "你设置的规则要求先确认这个动作。",
    ]);
  });

  it("says whether an action can be undone, in the reader's language", async () => {
    space.approvals.push(
      { ...firstUse, approval: "a1", action: "act-read", tool: "read_file", summary: "notes.txt" },
      { ...firstUse, approval: "a2", action: "act-list", tool: "list_files", summary: "." },
      { ...firstUse, approval: "a3", action: "act-write", tool: "write_file", summary: "out.txt" },
    );
    const view = await openChat();
    const undo = () =>
      [...view.container.querySelectorAll(".approval__undo")].map((line) => line.textContent);

    // Reading leaves nothing to undo, but the Space has not said so; writing does not come back.
    expect(undo()).toEqual([
      "It is not known whether this action can be undone.",
      "It is not known whether this action can be undone.",
      "This action cannot be undone.",
    ]);

    flush(() => fireEvent.click(element(view, ".locale-switch")));
    expect(undo()).toEqual([
      "这个操作能否撤销尚不清楚。",
      "这个操作能否撤销尚不清楚。",
      "这个操作无法撤销。",
    ]);
  });

  it("offers Always allow when a rule of the User's asks", async () => {
    space.approvals.push(byRule);
    const view = await openChat();

    expect(card(view)).toContain("A rule you set asks before this action.");
    expect(buttons(view).map((button) => button.textContent)).toEqual(["Allow", "Always allow", "Deny"]);
    // The Space said nothing about the scope, so the page names what it can see.
    expect(element(view, ".approval__scope").textContent).toBe(
      "Always allow covers: files in the Workspace root",
    );
  });

  it("offers no Always allow when nothing can be made lasting", async () => {
    // The Space gave no scope, and the action is too bare for the page to guess one.
    space.approvals.push({ ...byRule, tool: "shell", summary: "shell: ", lasting_scope: "" });
    const view = await openChat();

    expect(card(view)).toContain("A rule you set asks before this action.");
    expect(buttons(view).map((button) => button.textContent)).toEqual(["Allow", "Deny"]);
    expect(view.container.querySelector(".approval__scope")).toBeNull();
  });

  it("offers no Always allow when the Space, or the path, says none can last", async () => {
    space.approvals.push(
      // The Space said no; the page does not second-guess it with its own guess.
      { ...byRule, approval: "a1", tool: "shell", summary: "shell: git status", lasting_scope: null },
      // A path that leaves the Workspace, or is blank, makes no rule.
      { ...byRule, approval: "a2", tool: "write_file", arguments: { path: "../x" }, summary: "write_file: ../x" },
      { ...byRule, approval: "a3", tool: "read_file", arguments: { path: "   " }, summary: "read_file: notes.txt" },
    );
    const view = await openChat();

    expect(view.container.querySelectorAll(".approval__scope")).toHaveLength(0);
    for (const approval of view.container.querySelectorAll(".approval")) {
      expect([...approval.querySelectorAll("button")].map((button) => button.textContent)).toEqual([
        "Allow",
        "Deny",
      ]);
    }
  });

  it("sends Allow once, waits for the Space, and then shows the verdict", async () => {
    space.approvals.push(firstUse);
    const view = await openChat();
    space.holdDecide();

    press(view, "Allow");
    await flush();
    expect(space.decisions).toEqual([{ approval: "a1", allow: true, lasting: false }]);
    expect(buttons(view).every((button) => button.disabled)).toBe(true);
    // A second click while the answer is out changes nothing.
    press(view, "Allow");
    expect(space.decisions).toHaveLength(1);

    space.releaseDecide();
    await waitFor(() => expect(card(view)).toContain("Allowed"));
    expect(buttons(view)).toEqual([]);
    expect(space.approvals).toEqual([]);
  });

  it("answers Always allow with a rule that lasts", async () => {
    space.approvals.push(byRule);
    const view = await openChat();

    press(view, "Always allow");
    await waitFor(() => expect(card(view)).toContain("Allowed"));
    expect(space.decisions).toEqual([{ approval: "a2", allow: true, lasting: true }]);
  });

  it("answers Deny and shows it", async () => {
    space.approvals.push(firstUse);
    const view = await openChat();

    press(view, "Deny");
    await waitFor(() => expect(card(view)).toContain("Denied"));
    expect(space.decisions).toEqual([{ approval: "a1", allow: false, lasting: false }]);
  });

  it("shows a decision the record reports, even when another client made it", async () => {
    space.approvals.push(firstUse);
    const view = await openChat();
    space.approvals = [];

    record("neha.approval.decided", { approval: "a1", run: "r1", allow: false, lasting: false, user: "u1", rule: null });

    await waitFor(() => expect(card(view)).toContain("Denied"));
    expect(space.decisions).toEqual([]);
  });

  it("keeps the card this page answered once the Space stops listing it", async () => {
    space.approvals.push(firstUse);
    const view = await openChat();

    press(view, "Allow");
    await waitFor(() => expect(card(view)).toContain("Allowed"));
    // Any later read of the Chat finds no approval for it any more.
    const lists = asked("approval_list");
    record("neha.message.added", { chat: "h1", message: "m9", text: "done", file: null, run: "r1" });
    await waitFor(() => expect(asked("approval_list")).toBeGreaterThan(lists));
    await flush();

    expect(card(view)).toContain("Allowed");
  });

  it("shows a request that arrives live, with the Chat waiting, in the reader's language", async () => {
    const view = await openChat();
    expect(header(view)).not.toContain("Waiting");
    space.approvals.push(firstUse);
    record("neha.approval.requested", {
      approval: "a1",
      run: "r1",
      bot: "b1",
      chat: "h1",
      acting: "u1",
      action: "act1",
      step: 1,
      call: "c1",
      tool: "shell",
      arguments: { command: "printf hi > a.txt" },
      summary: "printf hi > a.txt",
      reason: "first_use_local",
      rule: null,
    });

    await waitFor(() => expect(view.container.querySelectorAll(".approval")).toHaveLength(1));
    expect(card(view)).toContain("printf hi > a.txt");
    expect(element(view, ".state-text--waiting").textContent).toBe("Waiting for approval");

    flush(() => fireEvent.click(element(view, ".locale-switch")));
    expect(header(view)).toContain("等待审批");
    expect(buttons(view).map((button) => button.textContent)).toEqual(["允许", "拒绝"]);

    // A granted approval lets the Run work again, and the header says so.
    press(view, "允许");
    await waitFor(() => expect(element(view, ".msg--working").textContent).toContain("正在处理"));
    expect(header(view)).not.toContain("等待审批");
    expect(card(view)).toContain("已允许");
  });

  it("follows a Run that already waits when the Chat opens", async () => {
    waitingRun();
    const view = await openChat();

    expect(header(view)).toContain("Waiting for approval");
    expect(card(view)).toContain("printf hi > a.txt");
    // A waiting Run holds no place, so the composer names none.
    expect(view.container.querySelector(".composer__queue")).toBeNull();

    // A denied approval ends the wait on this page; the record finishes the Run.
    press(view, "Deny");
    await waitFor(() => expect(card(view)).toContain("Denied"));
    expect(header(view)).not.toContain("Waiting for approval");
  });

  it("keeps waiting when a late answer about the record says the Run works", async () => {
    waitingRun();
    const view = await openChat();
    expect(header(view)).toContain("Waiting for approval");

    // An answer read before the approval still says the Run is running.
    space.runs["r1"] = { status: "running" };
    const shows = space.runShows.length;
    record("neha.message.added", { chat: "h1", message: "m8", text: "tick", file: null, run: "r1" });
    await waitFor(() => expect(space.runShows.length).toBeGreaterThan(shows));
    await flush();
    expect(header(view)).toContain("Waiting for approval");
    expect(view.container.querySelector(".msg--working")).toBeNull();

    // Only the decision lets the Run work again.
    press(view, "Allow");
    await waitFor(() => expect(element(view, ".msg--working").textContent).toContain("Working"));
    expect(header(view)).not.toContain("Waiting for approval");
  });

  it("keeps what a live request says about a lasting rule", async () => {
    const view = await openChat();
    // The page could guess "files under logs", but the Space said no rule can last.
    const noLasting: Approval = {
      ...byRule,
      approval: "a1",
      tool: "write_file",
      summary: "write_file: logs/x",
      lasting_scope: null,
    };
    space.approvals.push(noLasting);
    requested(noLasting);
    await waitFor(() => expect(view.container.querySelectorAll(".approval")).toHaveLength(1));
    expect(view.container.querySelector(".approval__scope")).toBeNull();
    expect(buttons(view).map((button) => button.textContent)).toEqual(["Allow", "Deny"]);

    // A request that carries the Space's words shows them, and the lasting button.
    const said: Approval = {
      ...byRule,
      approval: "a2",
      tool: "shell",
      summary: "shell: git status",
      lasting_scope: 'shell commands starting with "git"',
    };
    space.approvals.push(said);
    requested(said);
    await waitFor(() => expect(view.container.querySelectorAll(".approval")).toHaveLength(2));
    expect(lines(view, ".approval__scope")).toEqual(['Always allow covers: shell commands starting with "git"']);
    expect(buttons(view).map((button) => button.textContent)).toEqual([
      "Allow",
      "Deny",
      "Allow",
      "Always allow",
      "Deny",
    ]);
  });

  it("folds a long action and shows all of it on request", async () => {
    const script = Array.from({ length: 12 }, (_, index) => `echo line ${index + 1}`).join("\n");
    space.approvals.push({ ...firstUse, summary: script });
    const view = await openChat();
    const summary = element(view, ".approval__summary");

    expect(summary.classList.contains("approval__summary--folded")).toBe(true);
    // The whole action is in the card; only its height is folded.
    expect(summary.textContent).toBe(script);
    flush(() => fireEvent.click(element<HTMLButtonElement>(view, ".approval__expand")));
    expect(summary.classList.contains("approval__summary--folded")).toBe(false);
    expect(element(view, ".approval__expand").textContent).toBe("Show less");
  });

  it("shows control characters in an action as escapes", async () => {
    space.approvals.push({ ...firstUse, summary: "printf \u001b[31mred\u001b[0m\rnext" });
    const view = await openChat();

    expect(element(view, ".approval__summary").textContent).toBe("printf \\u{1b}[31mred\\u{1b}[0m\\u{d}next");
    expect(view.container.querySelector(".approval__expand")).toBeNull();
  });
});

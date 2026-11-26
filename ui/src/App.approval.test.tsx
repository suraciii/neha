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

// An action a rule of the User's stops every time.
const byRule: Approval = {
  ...firstUse,
  approval: "a2",
  action: "act2",
  tool: "write_file",
  summary: "notes.txt",
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

useView();

describe("App with approvals", () => {
  it("shows a first-use approval with Allow and Deny, and nothing lasting", async () => {
    space.approvals.push(firstUse);
    const view = await openChat();

    expect(card(view)).toContain("Scout");
    expect(card(view)).toContain("shell");
    expect(card(view)).toContain("printf hi > a.txt");
    expect(card(view)).toContain("This is the first time Scout uses your Computer.");
    expect(buttons(view).map((button) => button.textContent)).toEqual(["Allow", "Deny"]);
  });

  it("offers Always allow when a rule of the User's asks", async () => {
    space.approvals.push(byRule);
    const view = await openChat();

    expect(card(view)).toContain("A rule you set asks before this action.");
    expect(buttons(view).map((button) => button.textContent)).toEqual(["Allow", "Always allow", "Deny"]);
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
});

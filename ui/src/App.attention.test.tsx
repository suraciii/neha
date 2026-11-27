// What the Chats list and the attention section say while Runs work, wait, and
// answer in Chats the reader is not looking at.
import { fireEvent, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { describe, expect, it, vi } from "vitest";

import { space } from "./test/fakeSpace";
import { drain, element, openChat, record, useView, type View } from "./test/view";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

useView();

// A Chat whose Run waits for an approval, as the Space projects it.
function waitingIn(chatId: string, run: string, tool: string, summary: string) {
  space.statuses[chatId] = "waiting";
  space.approvals.push({
    approval: `a-${run}`,
    run,
    bot: chatId === "h1" ? "b1" : chatId === "h2" ? "b2" : "b3",
    chat: chatId,
    action: `act-${run}`,
    step: 1,
    tool,
    summary,
    reason: "first_use_local",
  });
}

// An approval the record asks for, in any Chat.
function asksApproval(chatId: string, run: string, tool: string, summary: string) {
  record("neha.approval.requested", {
    approval: `a-${run}`,
    run,
    bot: chatId === "h1" ? "b1" : chatId === "h2" ? "b2" : "b3",
    chat: chatId,
    acting: "u1",
    action: `act-${run}`,
    step: 1,
    call: "c1",
    tool,
    arguments: { command: "printf hi > a.txt" },
    summary,
    reason: "first_use_local",
    rule: null,
  });
}

// What each row of the attention section says.
function attentionRows(view: View): string[] {
  return Array.from(view.container.querySelectorAll(".attention__row")).map((row) => row.textContent ?? "");
}

// What each Chat in the list says about its state.
function badges(view: View): string[] {
  return Array.from(view.container.querySelectorAll(".conv__badge")).map((badge) => badge.textContent ?? "");
}

describe("App attention", () => {
  it("names the Chats that wait for the User and opens the one picked", async () => {
    waitingIn("h2", "r2", "shell", "printf hi > a.txt");
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();

    // The section names the waiting Chat and how many wait.
    expect(element(view, ".attention__title").textContent).toBe("Needs attention");
    expect(element(view, ".attention__count").textContent).toBe("1");
    expect(element(view, ".attention__count").getAttribute("title")).toBe("1 Chat waiting");
    // The Chat on screen answers its own approvals, so it is not listed. The
    // page names the Chat and what it waits for; the record names the tool when
    // it asks, not on a read.
    expect(view.container.querySelectorAll(".attention__row")).toHaveLength(1);
    const row = attentionRows(view)[0] ?? "";
    expect(row).toContain("Quill");
    expect(row).toContain("Waiting for your approval");

    // Picking a row opens that Chat, where its approval card waits.
    fireEvent.click(element(view, ".attention__row"));
    await waitFor(() => expect(element(view, ".chat__titles h1").textContent).toBe("Quill"));
    await waitFor(() => expect(view.container.querySelector(".approval")).toBeTruthy());
    expect(view.container.querySelector(".attention")).toBeNull();
  });

  it("hides the section when nothing waits", async () => {
    const view = await openChat();
    expect(view.container.querySelector(".attention")).toBeNull();
    expect(badges(view)).toEqual([]);
  });

  it("counts the waiting Chats", async () => {
    waitingIn("h2", "r2", "shell", "printf hi > a.txt");
    waitingIn("h3", "r3", "write_file", "notes/log.txt");
    space.chats = [space.chat, space.otherChat, space.thirdChat];
    const view = await openChat();

    expect(element(view, ".attention__count").textContent).toBe("2");
    expect(element(view, ".attention__count").getAttribute("title")).toBe("2 Chats waiting");
    expect(view.container.querySelectorAll(".attention__row")).toHaveLength(2);
    expect(badges(view)).toEqual(["Waiting", "Waiting"]);
  });

  it("marks a Chat waiting when the record asks for an approval there", async () => {
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();
    expect(view.container.querySelector(".attention")).toBeNull();

    asksApproval("h2", "r2", "shell", "printf hi > a.txt");
    await waitFor(() => expect(element(view, ".attention__row")).toBeTruthy());
    const row = attentionRows(view)[0] ?? "";
    expect(row).toContain("Quill");
    expect(row).toContain("shell · printf hi > a.txt");
    expect(badges(view)).toContain("Waiting");
  });

  it("shows the three states on the Chat list", async () => {
    space.chats = [space.chat, space.otherChat];
    space.statuses["h2"] = "working";
    const view = await openChat();
    // The other Chat works; the one on screen is idle.
    expect(badges(view)).toEqual(["Working"]);

    // Its Run ends and answers, which the reader has not opened yet.
    space.runs["r9"] = { status: "succeeded" };
    record("neha.message.added", { chat: "h2", message: "m9", text: "an answer", file: null, run: "r9" });
    record("neha.run.finished", { run: "r9", status: "succeeded", reply: "m9", usage: null, error: null });
    await waitFor(() => expect(badges(view)).toEqual(["New activity"]));
    await drain();

    // The same Chat then waits for an approval, which outranks the activity.
    asksApproval("h2", "r9", "shell", "printf hi > a.txt");
    await waitFor(() => expect(badges(view)).toEqual(["Waiting"]));
    expect(element(view, ".attention__row")).toBeTruthy();
  });

  it("clears the new activity when the User opens that Chat", async () => {
    space.chats = [space.chat, space.otherChat];
    space.otherMessages.push({
      id: "m9",
      chat: "h2",
      author: { kind: "bot", id: "b2" },
      acting: "u1",
      text: "an answer",
      at: "2026-01-01T00:00:00.000Z",
      seq: "3",
      run: null,
      file: null,
    });
    const view = await openChat();
    record("neha.message.added", { chat: "h2", message: "m9", text: "an answer", file: null, run: null });
    await waitFor(() => expect(badges(view)).toContain("New activity"));

    // Opening the Chat answers its new activity, wherever the reader came from.
    fireEvent.click(element(view, ".conv:nth-of-type(2)"));
    await waitFor(() => expect(element(view, ".chat__titles h1").textContent).toBe("Quill"));
    await waitFor(() => expect(badges(view)).not.toContain("New activity"));
    expect(view.container.querySelector(".conv__badge--new")).toBeNull();
  });

  it("names a waiting Chat in the reader's language", async () => {
    waitingIn("h2", "r2", "shell", "printf hi > a.txt");
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();
    flush(() => fireEvent.click(element(view, ".locale-switch--foot")));
    expect(element(view, ".attention__title").textContent).toBe("在等你");
    expect(attentionRows(view)[0]).toContain("Quill");
    expect(attentionRows(view)[0]).toContain("等待你的审批");
    expect(badges(view)).toContain("等待中");
  });
});

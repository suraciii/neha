// What the page shows when a Chat opens on a Space that already knows how it
// stands: the Run it works on, the state its header marks, and what never
// belongs to the attention list.
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

// A request the User sent, with the Run it started.
function request(id: string, run: string, text: string, chatId = "h1") {
  return {
    id,
    chat: chatId,
    author: { kind: "user" as const, id: "u1" },
    acting: null,
    text,
    at: "2026-01-01T00:00:00.000Z",
    seq: "1",
    run,
    file: null,
  };
}

// What each Chat in the list says about its state.
function badges(view: View): string[] {
  return Array.from(view.container.querySelectorAll(".conv__badge")).map((badge) => badge.textContent ?? "");
}

describe("App with a Chat the Space already knows", () => {
  it("marks a failed Chat in its header before any event lands", async () => {
    space.messages.push(request("m1", "r1", "please work"));
    space.messages.push({
      id: "m2",
      chat: "h1",
      author: { kind: "bot", id: "b1" },
      acting: "u1",
      text: "I could not finish this.",
      at: "2026-01-01T00:00:01.000Z",
      seq: "2",
      run: "r1",
      file: null,
    });
    // The Space says the Chat's latest Run failed and follows no Run now.
    space.statuses["h1"] = "failed";
    space.runs["r1"] = { status: "failed" };
    const view = await openChat();

    expect(element(view, ".chat__state--failed").textContent).toBe("Failed");
    expect(view.container.querySelector(".chat__state--working")).toBeNull();
    expect(view.container.querySelector(".chat__attention")).toBeNull();
    // The composer takes a new Message: the Run is over.
    await waitFor(() => expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false));
    expect(badges(view)).toEqual([]);

    // Opening it again keeps the mark: the Space still says so.
    fireEvent.click(element(view, ".conv:nth-of-type(1)"));
    await waitFor(() => expect(element(view, ".chat__state--failed").textContent).toBe("Failed"));
  });

  it("follows the Run the Chat works on, not the newest one it holds", async () => {
    space.chats = [space.chat, space.otherChat];
    space.otherMessages.push(request("m1", "r1", "first request", "h2"));
    space.otherMessages.push(request("m2", "r2", "second request", "h2"));
    // r1 waits for an approval while r2 already works: the Space says the Chat
    // follows r1, the waiting one.
    space.statuses["h2"] = "waiting";
    space.activeRuns["h2"] = "r1";
    space.runs["r1"] = { status: "waiting", approval: "a1" };
    space.runs["r2"] = { status: "running" };
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

    // The page opens h2 and asks about r1, never about the running r2.
    fireEvent.click(element(view, ".conv:nth-of-type(2)"));
    await waitFor(() => expect(element(view, ".chat__titles h1").textContent).toBe("Quill"));
    await waitFor(() => expect(space.runShows).toContain("r1"));
    expect(space.runShows).not.toContain("r2");
    // The header marks the wait, not the later Run's work.
    await waitFor(() => expect(element(view, ".chat__attention").textContent).toContain("Waiting"));
    expect(view.container.querySelector(".chat__state--working")).toBeNull();
    expect(badges(view)).toContain("Waiting");
  });

  it("keeps one Chat's wait while another Chat's Run works", async () => {
    space.chats = [space.chat, space.otherChat];
    space.statuses["h1"] = "waiting";
    space.activeRuns["h1"] = "r1";
    space.runs["r1"] = { status: "waiting", approval: "a1" };
    space.statuses["h2"] = "working";
    space.activeRuns["h2"] = "r2";
    space.runs["r2"] = { status: "running" };
    space.approvals.push({
      approval: "a1",
      run: "r1",
      bot: "b1",
      chat: "h1",
      action: "act1",
      step: 1,
      tool: "shell",
      summary: "printf hi > a.txt",
      reason: "first_use_local",
    });
    const view = await openChat();

    // Both Chats are marked, each with what it does.
    expect(badges(view)).toEqual(["Waiting", "Working"]);
    expect(element(view, ".attention__count").textContent).toBe("1");
    expect(element(view, ".chat__attention").textContent).toContain("Waiting");
    // The page follows the waiting Run of the Chat on screen.
    await waitFor(() => expect(space.runShows).toContain("r1"));
    expect(space.runShows).not.toContain("r2");
  });

  it("leaves a Computer that is offline out of the attention list", async () => {
    space.chats = [space.chat, space.otherChat];
    space.computer.online = false;
    space.statuses["h2"] = "working";
    space.activeRuns["h2"] = "r2";
    space.runs["r2"] = { status: "running" };
    const view = await openChat();

    // A Run that works needs no answer, whatever its Computer is doing.
    expect(view.container.querySelector(".attention")).toBeNull();
    expect(badges(view)).toEqual(["Working"]);
    expect(element(view, ".chat__sub").textContent).toContain("Offline");
  });

  it("leaves a failed Run out of the attention list", async () => {
    space.chats = [space.chat, space.otherChat];
    space.statuses["h2"] = "failed";
    space.runs["r2"] = { status: "failed" };
    const view = await openChat();

    // A Run that failed asks for nothing, so nothing waits for the User.
    expect(view.container.querySelector(".attention")).toBeNull();
    expect(view.container.querySelectorAll(".attention__row")).toHaveLength(0);
  });

  it("marks a waiting Chat in its header on a cold open", async () => {
    space.statuses["h1"] = "waiting";
    space.activeRuns["h1"] = "r1";
    space.runs["r1"] = { status: "waiting", approval: "a1" };
    space.messages.push(request("m1", "r1", "please work"));
    space.approvals.push({
      approval: "a1",
      run: "r1",
      bot: "b1",
      chat: "h1",
      action: "act1",
      step: 1,
      tool: "shell",
      summary: "printf hi > a.txt",
      reason: "first_use_local",
    });
    const view = await openChat();

    // The header names the wait, and the approval card is in the stream.
    expect(element(view, ".chat__attention").textContent).toContain("Waiting");
    await waitFor(() => expect(view.container.querySelector('[data-approval="a1"]')).toBeTruthy());
    expect(element(view, ".attention__count").textContent).toBe("1");
    await drain();
    expect(badges(view)).toEqual(["Waiting"]);
  });

  it("keeps the waiting Run's wait when a later Run of the same Chat ends", async () => {
    // Two requests in the Chat: r1 waits for an approval, and r2 queues behind
    // it. The Space names r1 as the Run the Chat follows.
    space.messages.push(request("m1", "r1", "first request"));
    space.messages.push(request("m2", "r2", "second request"));
    space.statuses["h1"] = "waiting";
    space.activeRuns["h1"] = "r1";
    space.runs["r1"] = { status: "waiting", approval: "a1" };
    space.runs["r2"] = { status: "queued", position: 2 };
    space.approvals.push({
      approval: "a1",
      run: "r1",
      bot: "b1",
      chat: "h1",
      action: "act1",
      step: 1,
      tool: "shell",
      summary: "printf hi > a.txt",
      reason: "first_use_local",
    });
    const view = await openChat();

    // The page starts from what the Space said, without any event landing.
    expect(element(view, ".chat__attention").textContent).toContain("Waiting");
    expect(badges(view)).toEqual(["Waiting"]);
    expect(element(view, ".attention__count").textContent).toBe("1");
    expect(view.container.querySelector('[data-approval="a1"]')).toBeTruthy();

    // The later Run leaves the queue, and then finishes, while r1 still waits.
    record("neha.run.started", { chat: "h1", run: "r2", bot: "b1", command: "c2", message: "m2" });
    await waitFor(() => expect(element(view, ".chat__attention").textContent).toContain("Waiting"));
    record("neha.run.finished", { run: "r2", status: "succeeded", reply: null, usage: null, error: null });
    await drain();

    // r1 is still the Run the Chat waits on, and its approval still waits.
    expect(element(view, ".chat__attention").textContent).toContain("Waiting");
    expect(view.container.querySelector(".chat__state--working")).toBeNull();
    expect(view.container.querySelector('[data-approval="a1"]')).toBeTruthy();
    expect(element(view, ".attention__count").textContent).toBe("1");
  });

  it("follows the Run the Bot names, not the one the list named", async () => {
    // Two requests in the Chat, each waiting for its own approval. The Space
    // names r1 as the Run the Chat follows.
    space.messages.push(request("m1", "r1", "first request"));
    space.messages.push(request("m2", "r2", "second request"));
    space.statuses["h1"] = "waiting";
    space.activeRuns["h1"] = "r1";
    space.runs["r1"] = { status: "waiting", approval: "a1" };
    space.runs["r2"] = { status: "waiting", approval: "a2" };
    space.approvals.push({
      approval: "a1",
      run: "r1",
      bot: "b1",
      chat: "h1",
      action: "act1",
      step: 1,
      tool: "shell",
      summary: "printf hi > a.txt",
      reason: "first_use_local",
    });
    const view = await openChat();
    // The page read the list twice by now: at the start, and after the
    // snapshot. Both times it named r1.
    await waitFor(() =>
      expect(space.asked.filter((name) => name === "chat_list").length).toBeGreaterThanOrEqual(2),
    );

    // r1 was answered while the page sat, and the Chat now waits on r2.
    space.activeRuns["h1"] = "r2";
    const read = space.runShows.length;
    fireEvent.click(element(view, ".conv:nth-of-type(1)"));
    await waitFor(() => expect(space.runShows.length).toBeGreaterThan(read));

    // The page reads the record about the Run the Bot names, not the older one
    // the list still carried, and the Chat still waits.
    expect(space.runShows[read]).toBe("r2");
    expect(element(view, ".chat__attention").textContent).toContain("Waiting");
    expect(badges(view)).toEqual(["Waiting"]);
  });

  it("opens the composer when the Bot says the Chat is idle, though the list said Working", async () => {
    // The list read while the Run worked, and it named no Run for the Chat.
    space.statuses["h1"] = "working";
    const view = await openChat();
    // The page read the list twice by now: at the start, and after the
    // snapshot. Both said Working.
    await waitFor(() =>
      expect(space.asked.filter((name) => name === "chat_list").length).toBeGreaterThanOrEqual(2),
    );

    // The Run ended while the page sat, and the Bot's own answer knows it.
    space.statuses["h1"] = "idle";
    fireEvent.click(element(view, ".conv:nth-of-type(1)"));
    await drain();

    // The Bot's word is the newest one the page has, so the Chat is idle.
    expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(false);
    expect(view.container.querySelector(".activity")).toBeNull();
    expect(view.container.querySelector(".chat__attention")).toBeNull();
    expect(badges(view)).toEqual([]);
  });

  it("keeps the composer closed while the Chat's Run works", async () => {
    space.statuses["h1"] = "working";
    space.activeRuns["h1"] = "r1";
    space.runs["r1"] = { status: "running" };
    space.messages.push(request("m1", "r1", "please work"));
    const view = await openChat();

    // The composer says why it takes no Message, and keeps what was typed.
    await waitFor(() => expect(element(view, ".composer__closed")).toBeTruthy());
    expect(element(view, ".composer__closed").textContent).toContain("Scout");
    expect(element<HTMLTextAreaElement>(view, "textarea").disabled).toBe(true);
    expect(view.container.querySelector(".composer__queue")).toBeNull();
  });
});

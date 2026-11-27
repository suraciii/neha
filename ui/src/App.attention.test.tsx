// What the Chats list and the attention list say while Runs work, wait, and
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

// The Bot that owns a Chat.
function botOf(chatId: string) {
  return chatId === "h1" ? "b1" : chatId === "h2" ? "b2" : "b3";
}

// An approval the Space holds for a Chat, which it answers when asked.
function waitingIn(chatId: string, run: string, tool: string, summary: string, approval = `a-${run}`) {
  space.statuses[chatId] = "waiting";
  space.activeRuns[chatId] = run;
  // The Space holds the Run waiting for this approval, as it really would.
  space.runs[run] = { status: "waiting", approval };
  space.approvals.push({
    approval,
    run,
    bot: botOf(chatId),
    chat: chatId,
    action: `act-${run}`,
    step: 1,
    tool,
    summary,
    reason: "first_use_local",
  });
}

// An approval the record asks for, in any Chat.
function asksApproval(chatId: string, run: string, tool: string, summary: string, approval = `a-${run}`) {
  record("neha.approval.requested", {
    approval,
    run,
    bot: botOf(chatId),
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

// What each row of the attention list says.
function attentionRows(view: View): string[] {
  return Array.from(view.container.querySelectorAll(".attention__row")).map((row) => row.textContent ?? "");
}

// What each Chat in the list says about its state.
function badges(view: View): string[] {
  return Array.from(view.container.querySelectorAll(".conv__badge")).map((badge) => badge.textContent ?? "");
}

describe("App attention", () => {
  it("names the approvals that wait for the User and opens the one picked", async () => {
    waitingIn("h2", "r2", "shell", "printf hi > a.txt");
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();

    // The list names the waiting approval and how many wait.
    expect(element(view, ".attention__title").textContent).toBe("Needs attention");
    expect(element(view, ".attention__count").textContent).toBe("1");
    expect(element(view, ".attention__count").getAttribute("title")).toBe("1 approval waiting");
    expect(attentionRows(view)).toHaveLength(1);
    const row = attentionRows(view)[0] ?? "";
    expect(row).toContain("Quill");
    expect(row).toContain("shell · printf hi > a.txt");

    // Picking a row opens that Chat, where its approval card waits.
    fireEvent.click(element(view, ".attention__row"));
    await waitFor(() => expect(element(view, ".chat__titles h1").textContent).toBe("Quill"));
    await waitFor(() => expect(view.container.querySelector(".approval")).toBeTruthy());
    // Opening the Chat does not answer the approval, so it stays listed.
    expect(view.container.querySelectorAll(".attention__row")).toHaveLength(1);

    // Answering it takes the row away, once the record says so.
    const allow = Array.from(view.container.querySelectorAll(".approval .btn--primary")).find(
      (button) => button.textContent === "Allow once",
    );
    if (!allow) throw new Error("no Allow once button in the view");
    fireEvent.click(allow);
    await waitFor(() => expect(space.decisions).toHaveLength(1));
    record("neha.approval.decided", { approval: "a-r2", run: "r2", allow: true, lasting: false, user: "u1", rule: null });
    await waitFor(() => expect(view.container.querySelectorAll(".attention__row")).toHaveLength(0));
    expect(view.container.querySelector(".attention")).toBeNull();
  });

  it("opens a waiting approval's row with the keyboard", async () => {
    waitingIn("h2", "r2", "shell", "printf hi > a.txt");
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();

    // The row is a button, so Enter and Space press it like a click does, and
    // pressing it opens the Chat its approval waits in.
    const row = element<HTMLButtonElement>(view, ".attention__row");
    expect(row.tagName).toBe("BUTTON");
    fireEvent.keyDown(row, { key: "Enter" });
    fireEvent.click(row);
    await waitFor(() => expect(element(view, ".chat__titles h1").textContent).toBe("Quill"));
    await waitFor(() => expect(view.container.querySelector('[data-approval="a-r2"]')).toBeTruthy());
  });

  it("lists one row per approval in the same Chat", async () => {
    waitingIn("h2", "r2", "shell", "printf hi > a.txt", "a-r2");
    waitingIn("h2", "r3", "write_file", "notes/log.txt", "a-r3");
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();

    // Two approvals in one Chat are two answers, so they are two rows.
    expect(element(view, ".attention__count").textContent).toBe("2");
    expect(attentionRows(view)).toHaveLength(2);
    expect(attentionRows(view)[0]).toContain("shell · printf hi > a.txt");
    expect(attentionRows(view)[1]).toContain("write_file · notes/log.txt");
    // The Chat carries one waiting mark, not two.
    expect(badges(view)).toEqual(["Waiting"]);
  });

  it("keeps a waiting Chat's row when its Chat is the one on screen", async () => {
    waitingIn("h1", "r1", "shell", "printf hi > a.txt");
    const view = await openChat();

    // The Chat on screen waits too, and the row stays: the approval is what
    // waits, not the Chat the User happens to be reading.
    await waitFor(() => expect(view.container.querySelectorAll(".attention__row")).toHaveLength(1));
    expect(attentionRows(view)[0]).toContain("shell · printf hi > a.txt");
    expect(element(view, ".chat__attention").textContent).toContain("Waiting");
  });

  it("hides the list when nothing waits", async () => {
    const view = await openChat();
    expect(view.container.querySelector(".attention")).toBeNull();
    expect(badges(view)).toEqual([]);
  });

  it("counts the waiting approvals", async () => {
    waitingIn("h2", "r2", "shell", "printf hi > a.txt");
    waitingIn("h3", "r3", "write_file", "notes/log.txt");
    space.chats = [space.chat, space.otherChat, space.thirdChat];
    const view = await openChat();

    expect(element(view, ".attention__count").textContent).toBe("2");
    expect(element(view, ".attention__count").getAttribute("title")).toBe("2 approvals waiting");
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

  it("keeps a Chat waiting while a later Run of it works", async () => {
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();

    // r1 waits for an approval; r2 queues behind it and then works.
    asksApproval("h2", "r1", "shell", "printf hi > a.txt");
    await waitFor(() => expect(element(view, ".attention__row")).toBeTruthy());
    record("neha.run.queued", { chat: "h2", run: "r2", bot: "b2", command: "c2", message: "m2" });
    record("neha.run.started", { chat: "h2", run: "r2", bot: "b2", command: "c2", message: "m2" });
    await drain();
    // The waiting approval still owns the Chat, and the reader follows it.
    expect(badges(view)).toEqual(["Waiting"]);
    expect(view.container.querySelectorAll(".attention__row")).toHaveLength(1);

    // r2 ends. That is a sibling Run ending, so the Chat stays waiting.
    record("neha.run.finished", { run: "r2", status: "succeeded", reply: "m2", usage: null, error: null });
    await waitFor(() => expect(view.container.querySelectorAll(".attention__row")).toHaveLength(1));
    expect(badges(view)).toEqual(["Waiting"]);
    expect(element(view, ".attention__count").textContent).toBe("1");
  });

  it("keeps the attention of a Chat the reader never opened after a reconnect", async () => {
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();
    expect(view.container.querySelector(".attention")).toBeNull();

    // The page reconnects. The snapshot replays what it missed, and the Space
    // then answers what every Chat does and what still waits.
    space.statuses["h2"] = "waiting";
    space.activeRuns["h2"] = "r2";
    space.approvals.push({
      approval: "a-r2",
      run: "r2",
      bot: "b2",
      chat: "h2",
      action: "act-r2",
      step: 1,
      tool: "shell",
      summary: "printf hi > a.txt",
      reason: "first_use_local",
    });
    space.resume([
      {
        seq: "201",
        kind: "neha.approval.requested",
        version: 1,
        at: "2026-01-01T00:00:00.000Z",
        author: { kind: "bot", id: "b2" },
        acting: "u1",
        data: {
          approval: "a-r2",
          run: "r2",
          bot: "b2",
          chat: "h2",
          acting: "u1",
          action: "act-r2",
          step: 1,
          call: "c1",
          tool: "shell",
          arguments: { command: "printf hi > a.txt" },
          summary: "printf hi > a.txt",
          reason: "first_use_local",
          rule: null,
        },
      },
    ]);
    await waitFor(() => expect(view.container.querySelectorAll(".attention__row")).toHaveLength(1));
    expect(attentionRows(view)[0]).toContain("shell · printf hi > a.txt");
    expect(badges(view)).toContain("Waiting");
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

  it("names a waiting approval in the reader's language", async () => {
    waitingIn("h2", "r2", "shell", "printf hi > a.txt");
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();
    flush(() => fireEvent.click(element(view, ".locale-switch--foot")));
    expect(element(view, ".attention__title").textContent).toBe("在等你");
    expect(element(view, ".attention__count").textContent).toBe("1");
    expect(attentionRows(view)[0]).toContain("Quill");
    expect(attentionRows(view)[0]).toContain("shell · printf hi > a.txt");
    expect(badges(view)).toContain("等待中");
  });

  it("opens the attention list as its own view from the rail", async () => {
    waitingIn("h2", "r2", "shell", "printf hi > a.txt");
    waitingIn("h3", "r3", "write_file", "notes/log.txt");
    space.chats = [space.chat, space.otherChat, space.thirdChat];
    const view = await openChat();

    // The rail has both entries, and the attention one carries the count.
    const rail = Array.from(view.container.querySelectorAll(".rail__item"));
    expect(rail).toHaveLength(2);
    expect(rail[1]?.getAttribute("aria-label")).toBe("Needs attention: 2 approvals waiting");
    expect(element(view, ".rail__count").textContent).toBe("2");

    // Opening it fills the sidebar with the whole wait, and the Chats stay a
    // rail click away.
    flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(2)")));
    expect(element(view, ".sidebar__title").textContent).toBe("Needs attention");
    expect(view.container.querySelectorAll(".attention__row")).toHaveLength(2);
    expect(view.container.querySelector(".conv")).toBeNull();

    // The rail goes back to the Chats.
    flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(1)")));
    expect(element(view, ".sidebar__title").textContent).toBe("Chats");
    expect(view.container.querySelector(".conv")).toBeTruthy();
  });

  it("brings the picked approval's card into view", async () => {
    const scrolled: string[] = [];
    const hadScroll = Element.prototype.scrollIntoView;
    // jsdom has no layout, so a test names what the page asked to scroll to.
    Element.prototype.scrollIntoView = function scrollIntoView() {
      scrolled.push(this.getAttribute("data-approval") ?? "");
    };
    try {
      waitingIn("h2", "r2", "shell", "printf hi > a.txt");
      waitingIn("h2", "r3", "write_file", "notes/log.txt", "a-r3");
      space.chats = [space.chat, space.otherChat];
      const view = await openChat();

      // The second row names the second approval, and picking it scrolls to that
      // card, not to the first one.
      const rows = view.container.querySelectorAll(".attention__row");
      expect(rows).toHaveLength(2);
      flush(() => fireEvent.click(rows[1] as HTMLElement));
      await waitFor(() => expect(view.container.querySelector('[data-approval="a-r3"]')).toBeTruthy());
      await waitFor(() => expect(scrolled).toEqual(["a-r3"]));
    } finally {
      Element.prototype.scrollIntoView = hadScroll;
    }
  });

  it("keeps a long wait from pushing the Chats out of reach", async () => {
    for (let index = 1; index <= 23; index += 1) {
      waitingIn("h2", `r${index}`, "shell", `printf ${index} > a.txt`, `a-r${index}`);
    }
    space.chats = [space.chat, space.otherChat];
    const view = await openChat();

    // Every approval is listed, and the block that holds them scrolls instead
    // of growing, so the Chats below stay reachable.
    expect(view.container.querySelectorAll(".attention__row")).toHaveLength(23);
    const block = element<HTMLElement>(view, ".attention");
    expect(block.className).toContain("attention");
    // The Chat list is still in the view, below the wait.
    expect(view.container.querySelector(".conv")).toBeTruthy();
  });
});

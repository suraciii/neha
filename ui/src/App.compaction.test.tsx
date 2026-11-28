// What a Chat shows where its compaction left the boundary: the summary stays
// the Model's, and the stream marks where it starts without hiding a Message.
import { waitFor } from "@solidjs/testing-library";
import { describe, expect, it, vi } from "vitest";

import { space } from "./test/fakeSpace";
import { element, openChat, renderApp, useView, type View } from "./test/view";
import type { Message } from "./types";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

useView();

// One Message of the Chat's record as chat_read answers it. The boundary
// Message carries the compacted mark; every other Message leaves it away.
function messageOf(
  id: string,
  author: "user" | "bot",
  text: string,
  seq: string,
  compacted = false,
): Message {
  return {
    id,
    chat: "h1",
    author: author === "user" ? { kind: "user", id: "u1" } : { kind: "bot", id: "b1" },
    acting: author === "user" ? null : "u1",
    text,
    at: "2026-01-01T00:00:00.000Z",
    seq,
    run: null,
    file: null,
    ...(compacted ? { compacted: true } : {}),
  };
}

// The three Messages a compacted Chat keeps: the boundary is the second one.
function compactedChat() {
  space.messages = [
    messageOf("m1", "user", "Please plan the launch.", "1"),
    messageOf("m2", "bot", "Here is the plan.", "2", true),
    messageOf("m3", "user", "Start with the first step.", "3"),
  ];
}

// The mark the Chat shows at a compaction boundary, when it shows one.
function marker(view: View): HTMLElement | null {
  return view.container.querySelector<HTMLElement>(".chat__compacted");
}

describe("App chat compaction", () => {
  it("marks the boundary after its Message and keeps every Message", async () => {
    compactedChat();
    const view = await openChat();

    // The mark sits in the boundary Message's row, after it and before the next
    // Message, so the reader sees where the summary starts.
    const boundary = element(view, '[data-message="m2"]');
    const row = boundary.parentElement;
    expect(row?.querySelector(".chat__compacted")?.textContent).toBe(
      "Earlier Messages were compacted",
    );
    const stream = element(view, ".chat__stream").textContent ?? "";
    expect(stream.indexOf("Here is the plan.")).toBeLessThan(
      stream.indexOf("Earlier Messages were compacted"),
    );
    expect(stream.indexOf("Earlier Messages were compacted")).toBeLessThan(
      stream.indexOf("Start with the first step."),
    );
    // The Messages above the boundary still show: compaction changes only what
    // the Model reads.
    expect(stream).toContain("Please plan the launch.");
  });

  it("keeps the mark when the page reloads", async () => {
    compactedChat();
    const view = await openChat();
    expect(marker(view)).toBeTruthy();

    // A reload reads the Chat's record again. The mark comes from the Message's
    // own field, so it is there rather than only in the page's live state.
    const reloaded = renderApp();
    await waitFor(() => expect(marker(reloaded)).toBeTruthy());
    expect(marker(reloaded)?.textContent).toBe("Earlier Messages were compacted");
  });

  it("marks a compaction that lands while the Chat is open", async () => {
    space.messages = [
      messageOf("m1", "user", "Please plan the launch.", "1"),
      messageOf("m2", "bot", "Here is the plan.", "2"),
    ];
    const view = await openChat();
    expect(marker(view)).toBeNull();

    // Another client's Run compacts the Chat. The record writes the boundary,
    // and the page reads the Chat again rather than showing the summary.
    const summary = "Earlier Messages covered the launch plan.";
    space.compact("2", summary);

    await waitFor(() => expect(marker(view)).toBeTruthy());
    expect(marker(view)?.textContent).toBe("Earlier Messages were compacted");
    // The summary is the Model's: the stream never shows it.
    const stream = element(view, ".chat__stream").textContent ?? "";
    expect(stream).not.toContain(summary);
    // The history stays whole around the new mark.
    expect(stream).toContain("Please plan the launch.");
    expect(stream).toContain("Here is the plan.");
  });
});

// What the page asks the Space when it opens, and that every read it sends
// travels in a frame the Space's own decoder accepts. The fake Space refuses a
// frame whose args shape the real decoder would refuse, so a call site that
// names the wrong shape cannot reach its first Chat.
import { waitFor } from "@solidjs/testing-library";
import { describe, expect, it, vi } from "vitest";

import { space } from "./test/fakeSpace";
import { openChat, renderApp, useView } from "./test/view";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

useView();

describe("App startup", () => {
  it("shows an invitation even when the browser is already signed in", async () => {
    history.replaceState({}, "", "/invite#secret1");
    const view = renderApp();
    try {
      // The Space answers `/api/me` with an account, and the invitation page
      // still opens: it joins a new User, not the signed-in one.
      await waitFor(() => expect(view.container.querySelector(".auth")).toBeTruthy());
      expect(view.container.textContent).toContain("Join this Space");
      expect(view.container.querySelector(".rail")).toBeNull();
    } finally {
      history.replaceState({}, "", "/");
    }
  });

  it("loads the workspace through frames the Space's decoder accepts", async () => {
    const view = await openChat();

    // The fake Space refuses a unit command that carries args and an argument
    // command that carries none, so these four reads arriving is the call sites
    // naming the shape the real decoder needs.
    expect(space.asked.slice(0, 4)).toEqual([
      "provider_list",
      "model_list",
      "bot_list",
      "chat_list",
    ]);
    // The lists loaded: the Chat they named is open and nothing failed.
    expect(view.container.querySelector("form.composer")).toBeTruthy();
    expect(view.container.querySelector('[role="alert"]')).toBeNull();
  });
});

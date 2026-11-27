import { fireEvent, waitFor } from "@solidjs/testing-library";
import { describe, expect, it, vi } from "vitest";

import { space } from "./test/fakeSpace";
import { element, renderApp, useView } from "./test/view";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

// The client at a phone width: the Chat list is the page until a Chat opens.
function mobile() {
  Object.defineProperty(window, "matchMedia", {
    writable: true,
    value: (query: string) => ({
      matches: query.includes("max-width: 900px"),
      media: query,
      addEventListener: () => undefined,
      removeEventListener: () => undefined,
    }),
  });
}

describe("the mobile client", () => {
  useView();

  it("gives a Chat a history entry that Back closes", async () => {
    history.replaceState(null, "");
    mobile();
    const view = renderApp();

    await waitFor(() => expect(view.container.querySelector(".conv")).toBeTruthy());
    expect(view.container.querySelector(".app--chat")).toBeNull();
    const before = history.length;

    fireEvent.click(element(view, ".conv"));
    await waitFor(() => expect(view.container.querySelector(".app--chat")).toBeTruthy());
    expect(history.state?.nehaChat).toBe(space.chat.id);
    // A push, not a replace, so the entry has somewhere to go back to.
    expect(history.length).toBe(before + 1);

    history.back();
    await waitFor(() => expect(view.container.querySelector(".app--chat")).toBeNull());

    // Forward reopens the Chat its entry names.
    history.forward();
    await waitFor(() => expect(view.container.querySelector(".app--chat")).toBeTruthy());
  });

  it("restores an open Chat from its history entry after a reload", async () => {
    history.replaceState({ nehaChat: space.chat.id }, "");
    const before = history.length;
    mobile();
    const view = renderApp();

    await waitFor(() => expect(view.container.querySelector(".app--chat")).toBeTruthy());
    expect(history.state?.nehaChat).toBe(space.chat.id);
    expect(history.length).toBe(before);
  });

  it("gives a wide-window Chat an entry when the window becomes narrow", async () => {
    history.replaceState(null, "");
    let narrow = false;
    const listeners = new Set<(event: { matches: boolean }) => void>();
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: (query: string) => ({
        get matches() {
          return narrow;
        },
        media: query,
        addEventListener: (_type: string, listener: (event: { matches: boolean }) => void) => {
          listeners.add(listener);
        },
        removeEventListener: (_type: string, listener: (event: { matches: boolean }) => void) => {
          listeners.delete(listener);
        },
      }),
    });
    const view = renderApp();

    await waitFor(() => expect(view.container.querySelector(".app--chat")).toBeTruthy());
    expect(history.state?.nehaChat ?? null).toBeNull();

    narrow = true;
    for (const listener of listeners) listener({ matches: true });
    await waitFor(() => expect(history.state?.nehaChat).toBe(space.chat.id));
  });
});

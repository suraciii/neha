import { cleanup, fireEvent, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { describe, expect, it, vi } from "vitest";

import { space } from "./test/fakeSpace";
import { button, element, panelText, renderApp, useView } from "./test/view";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

// The client at a phone width: the Chat list is the page until a Chat opens.
// The shell names the layout by what the main area holds rather than by a Chat
// alone: a management page is a detail of its list the same way, so the merged
// client carries one class for both.
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
    expect(view.container.querySelector(".app--detail")).toBeNull();
    const before = history.length;

    fireEvent.click(element(view, ".conv"));
    await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeTruthy());
    expect(history.state?.chat).toBe(space.chat.id);
    // A push, not a replace, so the entry has somewhere to go back to.
    expect(history.length).toBe(before + 1);

    history.back();
    await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeNull());

    // Forward reopens the Chat its entry names.
    history.forward();
    await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeTruthy());
  });

  it("restores an open Chat from its history entry after a reload", async () => {
    history.replaceState({ chat: space.chat.id, view: "chats", section: null, bot: null }, "");
    const before = history.length;
    mobile();
    const view = renderApp();

    await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeTruthy());
    expect(history.state?.chat).toBe(space.chat.id);
    expect(history.length).toBe(before);
  });

  it("opens the Chat and Message a notification names", async () => {
    space.messages = [
      {
        id: "m1",
        chat: space.chat.id,
        author: { kind: "bot", id: space.bot.id },
        acting: space.account.id,
        text: "Done.",
        at: "2026-01-01T00:00:00.000Z",
        seq: "1",
        run: null,
        file: null,
      },
    ];
    history.replaceState(null, "", `/?chat=${space.chat.id}&message=m1`);
    mobile();
    const scroll = vi.fn();
    const original = Element.prototype.scrollIntoView;
    Element.prototype.scrollIntoView = scroll as unknown as typeof Element.prototype.scrollIntoView;
    const view = renderApp();

    try {
      await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeTruthy());
      expect(history.state?.chat).toBe(space.chat.id);
      expect(location.search).toBe("");
      await waitFor(() =>
        expect(view.container.querySelector('[data-message="m1"]')).toBeTruthy(),
      );
      await waitFor(() => expect(scroll).toHaveBeenCalled());
    } finally {
      if (original) Element.prototype.scrollIntoView = original;
    }
    history.replaceState(null, "", "/");
  });

  it("restores the screen its history entry names after a reload", async () => {
    // A phone reloads into the screen it was on rather than into the Chat behind
    // it: the entry names the surface and what was open inside it, and restoring
    // it takes no step of its own, so the stack keeps its length and its Back
    // returns to the screen before.
    history.replaceState(null, "");
    mobile();
    const opened = renderApp();

    await waitFor(() => expect(opened.container.querySelector(".conv")).toBeTruthy());
    fireEvent.click(element(opened, ".conv"));
    await waitFor(() => expect(opened.container.querySelector(".app--detail")).toBeTruthy());
    flush(() => fireEvent.click(element(opened, ".rail__item:nth-of-type(4)")));
    await waitFor(() => expect(element(opened, ".app").className).not.toContain("app--detail"));
    flush(() => fireEvent.click(button(opened, "Models")));
    await waitFor(() => expect(element(opened, ".app").className).toContain("app--detail"));
    expect(panelText(opened, "Models")).toBeTruthy();
    const before = history.length;

    // The reload: a fresh client over the history the steps left behind.
    cleanup();
    const view = renderApp();

    await waitFor(() => expect(panelText(view, "Models")).toBeTruthy());
    expect(element(view, ".app").className).toContain("app--detail");
    // The restore wrote no entry of its own.
    expect(history.length).toBe(before);

    // And the step the entry holds returns to the Settings list.
    history.back();
    await waitFor(() => expect(element(view, ".app").className).not.toContain("app--detail"));
    expect(view.container.querySelectorAll(".conv")).toHaveLength(4);
  });

  it("restores a Chat from an entry the client before this one wrote", async () => {
    // The client before this one wrote an entry that names only its Chat. An
    // installed client that upgrades and reloads must land on that Chat, in the
    // same entry rather than in a new one.
    history.replaceState({ nehaChat: space.chat.id }, "");
    const before = history.length;
    mobile();
    const view = renderApp();

    await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeTruthy());
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

    await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeTruthy());
    expect(history.state?.chat ?? null).toBeNull();

    narrow = true;
    for (const listener of listeners) listener({ matches: true });
    await waitFor(() => expect(history.state?.chat).toBe(space.chat.id));
  });

  it("walks a management detail back out of the phone's history", async () => {
    // A phone shows one surface at a time, so a management detail is a step the
    // system Back must be able to leave. Backing out of the Models section the
    // User reached through a Chat returns to the Settings list that was on
    // screen, and the next Back closes the Chat behind it: the page's own Back
    // and the system Back walk the same steps.
    history.replaceState(null, "");
    mobile();
    const view = renderApp();

    await waitFor(() => expect(view.container.querySelector(".conv")).toBeTruthy());
    fireEvent.click(element(view, ".conv"));
    await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeTruthy());

    // The rail opens the Settings surface, and Models opens inside it.
    flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
    await waitFor(() => expect(element(view, ".app").className).not.toContain("app--detail"));
    flush(() => fireEvent.click(button(view, "Models")));
    await waitFor(() => expect(element(view, ".app").className).toContain("app--detail"));
    expect(panelText(view, "Models")).toBeTruthy();

    // The system Back leaves the detail for the Settings list, which is what the
    // page's own Back does.
    history.back();
    await waitFor(() => expect(element(view, ".app").className).not.toContain("app--detail"));
    expect(view.container.querySelectorAll(".conv")).toHaveLength(4);

    // The next Back returns to the Chat the surface was opened from.
    history.back();
    await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeTruthy());
    expect(history.state?.chat).toBe(space.chat.id);

    // And the one after that closes the Chat, the way a Chat with no surface
    // behind it does.
    history.back();
    await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeNull());
    expect(history.state?.chat ?? null).toBeNull();
  });

  it("takes a management page's Back out of the phone's history", async () => {
    // The page's Back and the system Back are one step: pressing the page's Back
    // leaves the same entry behind, so the system Back then leaves the surface
    // rather than the Chat it was opened from.
    history.replaceState(null, "");
    mobile();
    const view = renderApp();

    await waitFor(() => expect(view.container.querySelector(".conv")).toBeTruthy());
    fireEvent.click(element(view, ".conv"));
    await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeTruthy());

    flush(() => fireEvent.click(element(view, ".rail__item:nth-of-type(4)")));
    await waitFor(() => expect(view.container.querySelectorAll(".conv")).toHaveLength(4));
    flush(() => fireEvent.click(button(view, "Computers")));
    await waitFor(() => expect(element(view, ".app").className).toContain("app--detail"));
    expect(panelText(view, "Computers")).toBeTruthy();

    // The page's Back leaves the section for the list of sections.
    flush(() => fireEvent.click(element(view, ".page__back")));
    await waitFor(() => expect(element(view, ".app").className).not.toContain("app--detail"));

    // And the system Back leaves the surface for the Chat it was opened from: the
    // page's Back took the step the section held, so the system Back takes the
    // next one rather than walking back into the section again.
    history.back();
    await waitFor(() => expect(view.container.querySelector(".app--detail")).toBeTruthy());
    expect(history.state?.chat).toBe(space.chat.id);
  });
});

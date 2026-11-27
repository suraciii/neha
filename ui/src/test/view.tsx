// What a test needs to render the client against the fake Space and read the view.
import { cleanup, render, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { afterEach, beforeAll, beforeEach, expect, vi } from "vitest";

import App from "../App";
import { setLocale } from "../i18n";
import { api } from "../lib/api";
import type { Account, Author } from "../types";
import { space } from "./fakeSpace";

export type View = ReturnType<typeof render>;

export function element<T extends Element>(view: View, selector: string): T {
  const found = view.container.querySelector<T>(selector);
  if (!found) throw new Error(`no ${selector} in the view`);
  return found;
}

// Renders the client, signed in to the fake Space.
export function renderApp(): View {
  vi.spyOn(api, "me").mockResolvedValue(space.account as Account);
  return render(() => <App />);
}

// Renders the client and waits until its first Chat is open, loaded, and followed.
export async function openChat(): Promise<View> {
  const view = renderApp();
  await waitFor(() => expect(view.container.querySelector("form.composer")).toBeTruthy());
  await waitFor(() => expect(space.handlers).not.toBeNull());
  // The header names the Bot only once the Chat's reads have landed.
  await waitFor(() => expect(element(view, ".chat__titles h1").textContent).toBe(space.bot.name));
  flush();
  return view;
}

// One event the page follows from the record. A test can name who wrote it,
// which is how a File event says whose File it is.
export function record(kind: string, data: Record<string, unknown>, author?: Author) {
  space.seq += 1;
  space.handlers?.onEvent?.({
    seq: String(space.seq),
    kind,
    version: 1,
    at: "2026-01-01T00:00:00.000Z",
    author: author ?? { kind: "bot", id: "b1" },
    acting: "u1",
    data,
  });
}

// Lets the microtask chains a settled answer starts run before a test reads the view.
export async function drain() {
  for (let turn = 0; turn < 4; turn += 1) await Promise.resolve();
}

// Starts the fake Space clean before each test and cleans the view after it.
export function useView() {
  beforeAll(() => {
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: (query: string) => ({
        matches: false,
        media: query,
        addEventListener: () => undefined,
        removeEventListener: () => undefined,
      }),
    });
  });

  beforeEach(() => space.reset());

  afterEach(() => {
    cleanup();
    vi.restoreAllMocks();
    flush(() => setLocale("en-US"));
  });
}

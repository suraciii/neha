import { fireEvent, render, waitFor } from "@solidjs/testing-library";
import { describe, expect, it, vi } from "vitest";

import App from "./App";
import { api } from "./lib/api";
import { space } from "./test/fakeSpace";
import { element, useView } from "./test/view";
import type { Account } from "./types";

vi.mock("./lib/ws", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./lib/ws")>()),
  Commands: (await import("./test/fakeSpace")).FakeCommands,
}));

describe("an unreachable Space", () => {
  useView();

  it("keeps the shell and offers a retry instead of the sign-in form", async () => {
    vi.spyOn(api, "me")
      .mockRejectedValueOnce(new TypeError("Failed to fetch"))
      .mockResolvedValue(space.account as Account);
    const view = render(() => <App />);

    await waitFor(() => expect(view.container.querySelector(".offline")).toBeTruthy());
    expect(view.container.querySelector(".auth")).toBeNull();

    fireEvent.click(element(view, ".offline button"));
    await waitFor(() => expect(view.container.querySelector(".app")).toBeTruthy());
    expect(view.container.querySelector(".offline")).toBeNull();
  });
});

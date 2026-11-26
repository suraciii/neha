import { cleanup, fireEvent, render, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { afterEach, describe, expect, it, vi } from "vitest";

import { setLocale } from "../i18n";
import { api, ApiError } from "../lib/api";
import AuthView from "./AuthView";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  setLocale("en-US");
});

describe("AuthView", () => {
  it("re-renders a sign-in error in the selected language", async () => {
    vi.spyOn(api, "login").mockRejectedValue(
      new ApiError(401, "unauthorized", "invalid credentials"),
    );
    setLocale("zh-CN");
    const view = render(() => <AuthView mode="login" secret="" onAuthed={() => undefined} />);
    const inputs = view.container.querySelectorAll("input");
    fireEvent.input(inputs[0]!, { target: { value: "Ada" } });
    fireEvent.input(inputs[1]!, { target: { value: "wrong password" } });
    fireEvent.submit(view.container.querySelector("form")!);

    await waitFor(() => {
      expect(view.container.querySelector('[role="alert"]')?.textContent).toBe(
        "显示名称或密码不正确。",
      );
    });

    flush(() => fireEvent.click(view.container.querySelector(".locale-switch")!));
    expect(view.container.querySelector('[role="alert"]')?.textContent).toBe(
      "The display name or password is not correct.",
    );
  });
});

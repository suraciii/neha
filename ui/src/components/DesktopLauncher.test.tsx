import { cleanup, fireEvent, render, waitFor } from "@solidjs/testing-library";
import { flush } from "solid-js";
import { afterEach, describe, expect, it, vi } from "vitest";

import { setLocale } from "../i18n";
import DesktopLauncher from "./DesktopLauncher";

afterEach(() => {
  cleanup();
  delete (window as { nehaDesktop?: unknown }).nehaDesktop;
  vi.restoreAllMocks();
  setLocale("en-US");
});

function bridge() {
  const chooseLocal = vi.fn().mockResolvedValue({ ok: true });
  const connectRemote = vi.fn().mockResolvedValue({ ok: true });
  (window as { nehaDesktop?: unknown }).nehaDesktop = { chooseLocal, connectRemote };
  return { chooseLocal, connectRemote };
}

describe("DesktopLauncher", () => {
  it("starts a local Space without the local computer by default", async () => {
    const api = bridge();
    const view = render(() => <DesktopLauncher />);
    fireEvent.click(view.container.querySelector(".desktop__choice")!);
    await waitFor(() => expect(api.chooseLocal).toHaveBeenCalledWith(false));
  });

  it("passes explicit consent for the Local Computer", async () => {
    const api = bridge();
    const view = render(() => <DesktopLauncher />);
    fireEvent.click(view.container.querySelector(".desktop__consent input")!);
    flush();
    fireEvent.click(view.container.querySelector(".desktop__choice")!);
    await waitFor(() => expect(api.chooseLocal).toHaveBeenCalledWith(true));
  });

  it("connects to an existing Space with the entered address", async () => {
    const api = bridge();
    const view = render(() => <DesktopLauncher />);
    fireEvent.input(view.container.querySelector('input[name="space_url"]')!, {
      target: { value: "https://space.example.com" },
    });
    flush();
    fireEvent.click(view.container.querySelector('button[type="submit"]')!);
    await waitFor(() =>
      expect(api.connectRemote).toHaveBeenCalledWith("https://space.example.com"),
    );
  });

  it("reports a refused local start", async () => {
    const api = bridge();
    api.chooseLocal.mockResolvedValue({ ok: false });
    const view = render(() => <DesktopLauncher />);
    fireEvent.click(view.container.querySelector(".desktop__choice")!);
    await waitFor(() =>
      expect(view.container.querySelector('[role="alert"]')?.textContent).toBe(
        "The local Space could not start. Check the desktop logs.",
      ),
    );
  });
});
